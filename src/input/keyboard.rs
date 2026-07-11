//! Trusted key parsing, payload construction, dispatch, and release tests.

use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use super::cleanup::{CLEANUP_TIMEOUT, finish_cleanup};
use crate::session::{Deadline, PageSession};

const MOD_ALT: u8 = 1;
const MOD_CONTROL: u8 = 2;
const MOD_META: u8 = 4;
const MOD_SHIFT: u8 = 8;

/// A strictly parsed keyboard chord.  Its fields remain private so callers
/// cannot bypass canonical modifier checks or key-name validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KeyChord {
    modifiers: u8,
    primary: KeyDefinition,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct KeyDefinition {
    key: String,
    shifted_key: Option<String>,
    code: String,
    windows_virtual_key_code: u16,
    text: String,
    shifted_text: Option<String>,
    location: u8,
    required_modifiers: u8,
}

impl KeyDefinition {
    fn special(key: &str, code: &str, vk: u16) -> Self {
        Self {
            key: key.into(),
            shifted_key: None,
            code: code.into(),
            windows_virtual_key_code: vk,
            text: String::new(),
            shifted_text: None,
            location: 0,
            required_modifiers: 0,
        }
    }

    fn printable(key: char, shifted: Option<char>, code: &str, vk: u16) -> Self {
        Self {
            key: key.to_string(),
            shifted_key: shifted.map(|value| value.to_string()),
            code: code.into(),
            windows_virtual_key_code: vk,
            text: key.to_string(),
            shifted_text: shifted.map(|value| value.to_string()),
            location: 0,
            required_modifiers: 0,
        }
    }

    fn direct_printable(key: char, code: &str, vk: u16) -> Self {
        Self::printable(key, None, code, vk)
    }

    fn effective_key(&self, modifiers: u8) -> &str {
        if modifiers & MOD_SHIFT != 0 {
            self.shifted_key.as_deref().unwrap_or(&self.key)
        } else {
            &self.key
        }
    }

    fn effective_text(&self, modifiers: u8) -> &str {
        if modifiers & (MOD_CONTROL | MOD_ALT | MOD_META) != 0 {
            return "";
        }
        if modifiers & MOD_SHIFT != 0 {
            self.shifted_text.as_deref().unwrap_or(&self.text)
        } else {
            &self.text
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Modifier {
    Control,
    Alt,
    Shift,
    Meta,
}

impl Modifier {
    const SAFE_ORDER: [Self; 4] = [Self::Control, Self::Alt, Self::Shift, Self::Meta];

    const fn mask(self) -> u8 {
        match self {
            Self::Alt => MOD_ALT,
            Self::Control => MOD_CONTROL,
            Self::Meta => MOD_META,
            Self::Shift => MOD_SHIFT,
        }
    }

    const fn key(self) -> &'static str {
        match self {
            Self::Alt => "Alt",
            Self::Control => "Control",
            Self::Meta => "Meta",
            Self::Shift => "Shift",
        }
    }

    const fn code(self) -> &'static str {
        match self {
            Self::Alt => "AltLeft",
            Self::Control => "ControlLeft",
            Self::Meta => "MetaLeft",
            Self::Shift => "ShiftLeft",
        }
    }

    const fn windows_virtual_key_code(self) -> u16 {
        match self {
            Self::Alt => 18,
            Self::Control => 17,
            Self::Meta => 91,
            Self::Shift => 16,
        }
    }
}

impl FromStr for KeyChord {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> Result<Self> {
        if raw.is_empty() {
            bail!("key chord is empty");
        }

        let mut modifiers = 0_u8;
        let mut primary = None;
        for (index, raw_segment) in raw.split('+').enumerate() {
            let segment = raw_segment.trim();
            if segment.is_empty() {
                bail!(
                    "key chord segment {} is empty; use the named key `Plus` instead of a literal `+`",
                    index + 1
                );
            }
            if let Some(modifier) = parse_modifier(segment) {
                let mask = modifier.mask();
                if modifiers & mask != 0 {
                    bail!("duplicate {} modifier in key chord", modifier.key());
                }
                modifiers |= mask;
                continue;
            }

            let parsed = parse_primary_key(segment)
                .with_context(|| format!("invalid key chord segment `{segment}`"))?;
            modifiers |= parsed.required_modifiers;
            if primary.replace(parsed).is_some() {
                bail!("key chord contains more than one non-modifier key");
            }
        }

        let primary =
            primary.ok_or_else(|| anyhow!("key chord must include a non-modifier key"))?;
        Ok(Self { modifiers, primary })
    }
}

fn parse_modifier(value: &str) -> Option<Modifier> {
    if value.eq_ignore_ascii_case("control") || value.eq_ignore_ascii_case("ctrl") {
        Some(Modifier::Control)
    } else if value.eq_ignore_ascii_case("alt") || value.eq_ignore_ascii_case("option") {
        Some(Modifier::Alt)
    } else if value.eq_ignore_ascii_case("shift") {
        Some(Modifier::Shift)
    } else if value.eq_ignore_ascii_case("meta")
        || value.eq_ignore_ascii_case("cmd")
        || value.eq_ignore_ascii_case("command")
    {
        Some(Modifier::Meta)
    } else {
        None
    }
}

fn parse_primary_key(value: &str) -> Result<KeyDefinition> {
    let mut chars = value.chars();
    if let (Some(character), None) = (chars.next(), chars.next()) {
        if character.is_ascii_alphabetic() {
            let lower = character.to_ascii_lowercase();
            return Ok(KeyDefinition::printable(
                lower,
                Some(lower.to_ascii_uppercase()),
                &format!("Key{}", lower.to_ascii_uppercase()),
                lower.to_ascii_uppercase() as u16,
            ));
        }
        if character.is_ascii_digit() {
            let shifted = match character {
                '0' => ')',
                '1' => '!',
                '2' => '@',
                '3' => '#',
                '4' => '$',
                '5' => '%',
                '6' => '^',
                '7' => '&',
                '8' => '*',
                '9' => '(',
                _ => unreachable!(),
            };
            return Ok(KeyDefinition::printable(
                character,
                Some(shifted),
                &format!("Digit{character}"),
                character as u16,
            ));
        }
        if character != '+'
            && let Some(key) = punctuation_key(character)
        {
            return Ok(key);
        }
    }

    let folded = value.to_ascii_lowercase();
    let special = match folded.as_str() {
        "enter" | "return" => {
            let mut key = KeyDefinition::special("Enter", "Enter", 13);
            key.text = "\r".into();
            key
        }
        "tab" => KeyDefinition::special("Tab", "Tab", 9),
        "escape" | "esc" => KeyDefinition::special("Escape", "Escape", 27),
        "backspace" => KeyDefinition::special("Backspace", "Backspace", 8),
        "delete" | "del" => KeyDefinition::special("Delete", "Delete", 46),
        "insert" | "ins" => KeyDefinition::special("Insert", "Insert", 45),
        "arrowleft" | "left" => KeyDefinition::special("ArrowLeft", "ArrowLeft", 37),
        "arrowup" | "up" => KeyDefinition::special("ArrowUp", "ArrowUp", 38),
        "arrowright" | "right" => KeyDefinition::special("ArrowRight", "ArrowRight", 39),
        "arrowdown" | "down" => KeyDefinition::special("ArrowDown", "ArrowDown", 40),
        "home" => KeyDefinition::special("Home", "Home", 36),
        "end" => KeyDefinition::special("End", "End", 35),
        "pageup" => KeyDefinition::special("PageUp", "PageUp", 33),
        "pagedown" => KeyDefinition::special("PageDown", "PageDown", 34),
        "space" | "spacebar" => KeyDefinition::direct_printable(' ', "Space", 32),
        "f1" | "f2" | "f3" | "f4" | "f5" | "f6" | "f7" | "f8" | "f9" | "f10" | "f11" | "f12" => {
            let number: u16 = folded[1..].parse().expect("matched function-key number");
            KeyDefinition::special(&format!("F{number}"), &format!("F{number}"), 111 + number)
        }
        _ => return parse_named_punctuation(&folded),
    };
    Ok(special)
}

fn punctuation_key(character: char) -> Option<KeyDefinition> {
    let (shifted, code, vk) = match character {
        '`' => ('~', "Backquote", 192),
        '-' => ('_', "Minus", 189),
        '=' => ('+', "Equal", 187),
        '[' => ('{', "BracketLeft", 219),
        ']' => ('}', "BracketRight", 221),
        '\\' => ('|', "Backslash", 220),
        ';' => (':', "Semicolon", 186),
        '\'' => ('"', "Quote", 222),
        ',' => ('<', "Comma", 188),
        '.' => ('>', "Period", 190),
        '/' => ('?', "Slash", 191),
        _ => return None,
    };
    Some(KeyDefinition::printable(character, Some(shifted), code, vk))
}

fn parse_named_punctuation(value: &str) -> Result<KeyDefinition> {
    let base = match value {
        "backquote" | "grave" => punctuation_key('`'),
        "minus" | "hyphen" => punctuation_key('-'),
        "equal" | "equals" => punctuation_key('='),
        "bracketleft" | "leftbracket" => punctuation_key('['),
        "bracketright" | "rightbracket" => punctuation_key(']'),
        "backslash" => punctuation_key('\\'),
        "semicolon" => punctuation_key(';'),
        "quote" | "apostrophe" => punctuation_key('\''),
        "comma" => punctuation_key(','),
        "period" | "dot" => punctuation_key('.'),
        "slash" => punctuation_key('/'),
        _ => None,
    };
    if let Some(key) = base {
        return Ok(key);
    }

    let (base, shifted, code, vk) = match value {
        "plus" => ('=', '+', "Equal", 187),
        "tilde" => ('`', '~', "Backquote", 192),
        "underscore" => ('-', '_', "Minus", 189),
        "braceleft" | "leftbrace" => ('[', '{', "BracketLeft", 219),
        "braceright" | "rightbrace" => (']', '}', "BracketRight", 221),
        "pipe" => ('\\', '|', "Backslash", 220),
        "colon" => (';', ':', "Semicolon", 186),
        "doublequote" => ('\'', '"', "Quote", 222),
        "lessthan" => (',', '<', "Comma", 188),
        "greaterthan" => ('.', '>', "Period", 190),
        "question" | "questionmark" => ('/', '?', "Slash", 191),
        "exclamation" | "exclamationmark" => ('1', '!', "Digit1", 49),
        "at" | "atsign" => ('2', '@', "Digit2", 50),
        "hash" | "numbersign" => ('3', '#', "Digit3", 51),
        "dollar" => ('4', '$', "Digit4", 52),
        "percent" => ('5', '%', "Digit5", 53),
        "caret" => ('6', '^', "Digit6", 54),
        "ampersand" => ('7', '&', "Digit7", 55),
        "asterisk" => ('8', '*', "Digit8", 56),
        "parenthesisleft" | "leftparen" => ('9', '(', "Digit9", 57),
        "parenthesisright" | "rightparen" => ('0', ')', "Digit0", 48),
        _ => bail!("unknown key name `{value}`"),
    };
    let mut key = KeyDefinition::printable(base, Some(shifted), code, vk);
    key.required_modifiers = MOD_SHIFT;
    Ok(key)
}

fn modifier_key_payload(event_type: &str, modifier: Modifier, modifiers: u8) -> Value {
    json!({
        "type": event_type,
        "modifiers": modifiers,
        "key": modifier.key(),
        "code": modifier.code(),
        "windowsVirtualKeyCode": modifier.windows_virtual_key_code(),
        "text": "",
        "unmodifiedText": "",
        "location": 1,
    })
}

fn primary_key_payload(event_type: &str, chord: &KeyChord, include_text: bool) -> Value {
    let text = if include_text {
        chord.primary.effective_text(chord.modifiers)
    } else {
        ""
    };
    json!({
        "type": event_type,
        "modifiers": chord.modifiers,
        "key": chord.primary.effective_key(chord.modifiers),
        "code": chord.primary.code,
        "windowsVirtualKeyCode": chord.primary.windows_virtual_key_code,
        "text": text,
        "unmodifiedText": if include_text {
            &chord.primary.text
        } else {
            ""
        },
        "location": chord.primary.location,
    })
}

fn dispatch_key_payload(sess: &mut PageSession, payload: Value, deadline: Deadline) -> Result<()> {
    sess.call_until("Input.dispatchKeyEvent", payload, deadline)?;
    Ok(())
}

/// Dispatch one complete key chord.  Modifier presses are canonicalized to a
/// stable order and every attempted press receives a best-effort release, even
/// when CDP leaves the outcome of a failed request ambiguous.
pub(crate) fn key(sess: &mut PageSession, chord: &KeyChord) -> Result<()> {
    let modifiers: Vec<_> = Modifier::SAFE_ORDER
        .into_iter()
        .filter(|modifier| chord.modifiers & modifier.mask() != 0)
        .collect();
    let mut attempted = Vec::new();
    let mut active = 0_u8;
    let mut primary_attempted = false;
    let mut primary_error = None;
    let mut cleanup_errors = Vec::new();

    for modifier in &modifiers {
        active |= modifier.mask();
        attempted.push(*modifier);
        if let Err(error) = dispatch_key_payload(
            sess,
            modifier_key_payload("rawKeyDown", *modifier, active),
            sess.deadline(),
        ) {
            primary_error = Some(error.context(format!("pressing {} modifier", modifier.key())));
            break;
        }
    }

    if primary_error.is_none() {
        primary_attempted = true;
        let event_type = if chord.primary.effective_text(chord.modifiers).is_empty() {
            "rawKeyDown"
        } else {
            "keyDown"
        };
        if let Err(error) = dispatch_key_payload(
            sess,
            primary_key_payload(event_type, chord, true),
            sess.deadline(),
        ) {
            primary_error = Some(error.context("pressing primary key"));
        }
    }

    let cleanup_deadline = Deadline::after(CLEANUP_TIMEOUT);
    if primary_attempted
        && let Err(error) = dispatch_key_payload(
            sess,
            primary_key_payload("keyUp", chord, false),
            cleanup_deadline,
        )
    {
        if primary_error.is_some() {
            cleanup_errors.push(error.context("releasing primary key"));
        } else {
            primary_error = Some(error.context("releasing primary key"));
        }
    }

    for modifier in attempted.into_iter().rev() {
        active &= !modifier.mask();
        if let Err(error) = dispatch_key_payload(
            sess,
            modifier_key_payload("keyUp", modifier, active),
            cleanup_deadline,
        ) {
            if primary_error.is_some() {
                cleanup_errors
                    .push(error.context(format!("releasing {} modifier", modifier.key())));
            } else {
                primary_error =
                    Some(error.context(format!("releasing {} modifier", modifier.key())));
            }
        }
    }

    finish_cleanup(primary_error, cleanup_errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::PageSession;
    use serde_json::{Value, json};
    use std::net::TcpListener;
    use std::thread;
    use std::time::{Duration, Instant};
    use tungstenite::{Message, accept};
    #[test]
    fn parses_aliases_and_canonicalizes_modifiers() {
        let chord: KeyChord = "Cmd+option+CTRL+shift+K".parse().unwrap();
        assert_eq!(
            chord.modifiers,
            MOD_META | MOD_ALT | MOD_CONTROL | MOD_SHIFT
        );
        assert_eq!(chord.primary.key, "k");
        assert_eq!(chord.primary.code, "KeyK");
    }

    #[test]
    fn parses_required_named_keys_case_insensitively() {
        for name in [
            "ENTER",
            "tab",
            "Escape",
            "Backspace",
            "delete",
            "INSERT",
            "ArrowLeft",
            "up",
            "right",
            "ArrowDown",
            "Home",
            "End",
            "PageUp",
            "PageDown",
            "Space",
            "F1",
            "f12",
        ] {
            name.parse::<KeyChord>()
                .unwrap_or_else(|error| panic!("expected {name} to parse, got {error:#}"));
        }
    }

    #[test]
    fn parses_named_and_unambiguous_literal_punctuation() {
        for name in [
            "Plus",
            "Minus",
            "Equal",
            "Comma",
            "Period",
            "Slash",
            "Backslash",
            "Semicolon",
            "Quote",
            "Backquote",
            "BracketLeft",
            "BracketRight",
            "QuestionMark",
            "Ampersand",
            "-",
            "/",
            "]",
        ] {
            name.parse::<KeyChord>()
                .unwrap_or_else(|error| panic!("expected {name} to parse, got {error:#}"));
        }
        let plus: KeyChord = "Plus".parse().unwrap();
        assert_eq!(plus.primary.effective_key(plus.modifiers), "+");
        assert_eq!(plus.primary.code, "Equal");
        assert_eq!(plus.modifiers, MOD_SHIFT);
    }

    #[test]
    fn shifted_named_punctuation_uses_physical_base_key_plus_shift() {
        for (name, base, shifted, code, vk) in [
            ("Plus", "=", "+", "Equal", 187),
            ("Tilde", "`", "~", "Backquote", 192),
            ("QuestionMark", "/", "?", "Slash", 191),
            ("AtSign", "2", "@", "Digit2", 50),
            ("LeftBrace", "[", "{", "BracketLeft", 219),
        ] {
            let chord: KeyChord = name.parse().unwrap();
            assert_eq!(chord.modifiers, MOD_SHIFT, "{name}");
            assert_eq!(chord.primary.key, base, "{name}");
            assert_eq!(
                chord.primary.shifted_key.as_deref(),
                Some(shifted),
                "{name}"
            );
            assert_eq!(chord.primary.code, code, "{name}");
            assert_eq!(chord.primary.windows_virtual_key_code, vk, "{name}");
            let payload = primary_key_payload("keyDown", &chord, true);
            assert_eq!(payload["key"], shifted, "{name}");
            assert_eq!(payload["text"], shifted, "{name}");
            assert_eq!(payload["unmodifiedText"], base, "{name}");
        }

        assert!("SectionSign".parse::<KeyChord>().is_err());
    }

    #[test]
    fn rejects_malformed_or_ambiguous_chords() {
        for raw in [
            "",
            "+",
            "Control+",
            "+K",
            "Control++K",
            "Control+Ctrl+K",
            "Option+Alt+K",
            "Cmd+Command+K",
            "Control+Shift",
            "K+Enter",
            "DefinitelyNotAKey",
        ] {
            assert!(raw.parse::<KeyChord>().is_err(), "expected `{raw}` to fail");
        }
    }

    #[test]
    fn key_payloads_map_shift_and_text_suppression() {
        let shifted: KeyChord = "Shift+K".parse().unwrap();
        assert_eq!(
            primary_key_payload("keyDown", &shifted, true),
            json!({
                "type": "keyDown",
                "modifiers": 8,
                "key": "K",
                "code": "KeyK",
                "windowsVirtualKeyCode": 75,
                "text": "K",
                "unmodifiedText": "k",
                "location": 0,
            })
        );

        let control_shift: KeyChord = "Control+Shift+K".parse().unwrap();
        let down = primary_key_payload("keyDown", &control_shift, true);
        assert_eq!(down["key"], "K");
        assert_eq!(down["text"], "");
        assert_eq!(down["unmodifiedText"], "k");
        assert_eq!(down["modifiers"], 10);

        let up = primary_key_payload("keyUp", &control_shift, false);
        assert_eq!(up["text"], "");
        assert_eq!(up["unmodifiedText"], "");
    }

    #[test]
    fn modifier_payload_has_post_transition_mask_and_left_location() {
        assert_eq!(
            modifier_key_payload("rawKeyDown", Modifier::Control, MOD_CONTROL),
            json!({
                "type": "rawKeyDown",
                "modifiers": 2,
                "key": "Control",
                "code": "ControlLeft",
                "windowsVirtualKeyCode": 17,
                "text": "",
                "unmodifiedText": "",
                "location": 1,
            })
        );
        assert_eq!(
            modifier_key_payload("keyUp", Modifier::Control, 0)["modifiers"],
            0
        );
    }

    #[test]
    fn shifted_digits_and_punctuation_have_physical_codes() {
        let digit: KeyChord = "Shift+1".parse().unwrap();
        let payload = primary_key_payload("keyDown", &digit, true);
        assert_eq!(payload["key"], "!");
        assert_eq!(payload["code"], "Digit1");
        assert_eq!(payload["windowsVirtualKeyCode"], 49);
        assert_eq!(payload["text"], "!");
        assert_eq!(payload["unmodifiedText"], "1");

        let slash: KeyChord = "Shift+Slash".parse().unwrap();
        let payload = primary_key_payload("keyDown", &slash, true);
        assert_eq!(payload["key"], "?");
        assert_eq!(payload["code"], "Slash");
    }

    #[test]
    fn key_failure_releases_primary_and_every_attempted_modifier_in_reverse_order() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = accept(stream).unwrap();
            let expected = [
                ("rawKeyDown", "Control", 2),
                ("rawKeyDown", "Shift", 10),
                ("rawKeyDown", "K", 10),
                ("keyUp", "K", 10),
                ("keyUp", "Shift", 2),
                ("keyUp", "Control", 0),
            ];
            for (index, (event_type, key, modifiers)) in expected.into_iter().enumerate() {
                let command = read_json_message(&mut socket);
                assert_eq!(command["method"], "Input.dispatchKeyEvent");
                assert_eq!(command["params"]["type"], event_type);
                assert_eq!(command["params"]["key"], key);
                assert_eq!(command["params"]["modifiers"], modifiers);
                let response = if index == 2 {
                    json!({
                        "id": command["id"],
                        "error": {"code": -32000, "message": "injected key failure"}
                    })
                } else if index == 3 {
                    json!({
                        "id": command["id"],
                        "error": {"code": -32000, "message": "injected key release failure"}
                    })
                } else {
                    json!({"id": command["id"], "result": {}})
                };
                socket
                    .send(Message::Text(response.to_string().into()))
                    .unwrap();
            }
        });

        let mut session =
            PageSession::connect_for_input_test(&format!("ws://127.0.0.1:{port}")).unwrap();
        let chord: KeyChord = "Control+Shift+K".parse().unwrap();
        let error = key(&mut session, &chord).unwrap_err();
        assert!(format!("{error:#}").contains("pressing primary key"));
        assert!(format!("{error:#}").contains("injected key failure"));
        assert!(format!("{error:#}").contains("input cleanup also failed"));
        assert!(format!("{error:#}").contains("injected key release failure"));
        server.join().unwrap();
    }

    #[test]
    fn key_cleanup_is_bounded_when_release_reply_never_arrives() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = accept(stream).unwrap();
            for index in 0..4 {
                let command = read_json_message(&mut socket);
                let response = if index == 2 {
                    json!({
                        "id": command["id"],
                        "error": {"code": -32000, "message": "primary failed before missing cleanup reply"}
                    })
                } else {
                    json!({"id": command["id"], "result": {}})
                };
                if index < 3 {
                    socket
                        .send(Message::Text(response.to_string().into()))
                        .unwrap();
                } else {
                    thread::sleep(CLEANUP_TIMEOUT + Duration::from_millis(500));
                }
            }
        });

        let mut session =
            PageSession::connect_for_input_test(&format!("ws://127.0.0.1:{port}")).unwrap();
        let chord: KeyChord = "Control+Shift+K".parse().unwrap();
        let started = Instant::now();
        let error = key(&mut session, &chord).unwrap_err();
        assert!(started.elapsed() < CLEANUP_TIMEOUT + Duration::from_millis(750));
        assert!(format!("{error:#}").contains("pressing primary key"));
        assert!(format!("{error:#}").contains("input cleanup also failed"));
        server.join().unwrap();
    }

    #[test]
    fn slow_ambiguous_key_press_does_not_preexpire_cleanup_budget() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = accept(stream).unwrap();
            let expected = [
                ("rawKeyDown", "Control", 2),
                ("rawKeyDown", "Shift", 10),
                ("rawKeyDown", "K", 10),
                ("keyUp", "K", 10),
                ("keyUp", "Shift", 2),
                ("keyUp", "Control", 0),
            ];
            for (index, (event_type, key, modifiers)) in expected.into_iter().enumerate() {
                let command = read_json_message(&mut socket);
                assert_eq!(command["method"], "Input.dispatchKeyEvent");
                assert_eq!(command["params"]["type"], event_type);
                assert_eq!(command["params"]["key"], key);
                assert_eq!(command["params"]["modifiers"], modifiers);

                if index == 2 {
                    thread::sleep(CLEANUP_TIMEOUT - Duration::from_millis(250));
                    socket
                        .send(Message::Text(
                            json!({
                                "id": command["id"],
                                "error": {"code": -32000, "message": "slow primary failure"}
                            })
                            .to_string()
                            .into(),
                        ))
                        .unwrap();
                } else if index == 3 {
                    thread::sleep(Duration::from_millis(400));
                    socket
                        .send(Message::Text(
                            json!({"id": command["id"], "result": {}})
                                .to_string()
                                .into(),
                        ))
                        .unwrap();
                } else {
                    socket
                        .send(Message::Text(
                            json!({"id": command["id"], "result": {}})
                                .to_string()
                                .into(),
                        ))
                        .unwrap();
                }
            }
        });

        let mut session =
            PageSession::connect_for_input_test(&format!("ws://127.0.0.1:{port}")).unwrap();
        let chord: KeyChord = "Control+Shift+K".parse().unwrap();
        let error = key(&mut session, &chord).unwrap_err();
        assert!(format!("{error:#}").contains("pressing primary key"));
        assert!(format!("{error:#}").contains("slow primary failure"));
        assert!(!format!("{error:#}").contains("input cleanup also failed"));
        server.join().unwrap();
    }
    fn read_json_message(socket: &mut tungstenite::WebSocket<std::net::TcpStream>) -> Value {
        match socket.read().unwrap() {
            Message::Text(text) => serde_json::from_str(&text).unwrap(),
            message => panic!("unexpected command message {message:?}"),
        }
    }
}
