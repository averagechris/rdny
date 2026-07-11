//! Trusted parsing and CDP dispatch for keyboard and pointer input.
//!
//! Values in this module are deliberately typed before they reach CDP.  The
//! command-line layer can therefore parse untrusted strings once and cannot
//! accidentally construct a partly-valid keyboard or mouse event payload.

use std::fmt;
use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::interaction_target::ActionPoint;
use crate::selector::ElementSelector;
use crate::session::{Deadline, PageSession};

pub(crate) const MIN_DRAG_STEPS: u32 = 1;
pub(crate) const MAX_DRAG_STEPS: u32 = 1_000;
pub(crate) const DEFAULT_DRAG_STEPS: u32 = 20;
pub(crate) const MIN_DRAG_DURATION_MS: u64 = 1;
pub(crate) const MAX_DRAG_DURATION_MS: u64 = 30_000;
pub(crate) const DEFAULT_DRAG_DURATION_MS: u64 = 500;

const CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
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

/// A CDP mouse button independent of the CLI framework.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

impl MouseButton {
    pub(crate) const fn buttons_mask(self) -> u8 {
        match self {
            Self::Left => 1,
            Self::Right => 2,
            Self::Middle => 4,
            Self::Back => 8,
            Self::Forward => 16,
        }
    }

    const fn as_cdp(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Middle => "middle",
            Self::Back => "back",
            Self::Forward => "forward",
        }
    }
}

impl FromStr for MouseButton {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        if value.eq_ignore_ascii_case("left") {
            Ok(Self::Left)
        } else if value.eq_ignore_ascii_case("right") {
            Ok(Self::Right)
        } else if value.eq_ignore_ascii_case("middle") {
            Ok(Self::Middle)
        } else if value.eq_ignore_ascii_case("back") {
            Ok(Self::Back)
        } else if value.eq_ignore_ascii_case("forward") {
            Ok(Self::Forward)
        } else {
            bail!("unknown mouse button `{value}`; expected left, right, middle, back, or forward")
        }
    }
}

impl fmt::Display for MouseButton {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_cdp())
    }
}

/// A finite CSS viewport coordinate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PointerPoint {
    x: f64,
    y: f64,
}

impl PointerPoint {
    pub(crate) fn new(x: f64, y: f64) -> Result<Self> {
        if !x.is_finite() || !y.is_finite() {
            bail!("pointer coordinates must be finite numbers");
        }
        Ok(Self { x, y })
    }
}

impl FromStr for PointerPoint {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let mut parts = value.split(',');
        let x = parts
            .next()
            .filter(|part| !part.trim().is_empty())
            .context("pointer coordinates must use `x,y`")?;
        let y = parts
            .next()
            .filter(|part| !part.trim().is_empty())
            .context("pointer coordinates must use `x,y`")?;
        if parts.next().is_some() {
            bail!("pointer coordinates must contain exactly two values as `x,y`");
        }
        let x = x
            .trim()
            .parse::<f64>()
            .with_context(|| format!("invalid pointer x coordinate `{}`", x.trim()))?;
        let y = y
            .trim()
            .parse::<f64>()
            .with_context(|| format!("invalid pointer y coordinate `{}`", y.trim()))?;
        Self::new(x, y)
    }
}

/// Selector-backed or explicit-coordinate pointer destination.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PointerTarget {
    Selector(ElementSelector),
    Point(PointerPoint),
}

impl PointerTarget {
    pub(crate) fn selector(selector: ElementSelector) -> Self {
        Self::Selector(selector)
    }

    #[cfg(test)]
    pub(crate) fn coordinates(x: f64, y: f64) -> Result<Self> {
        Ok(Self::Point(PointerPoint::new(x, y)?))
    }

    pub(crate) const fn point(point: PointerPoint) -> Self {
        Self::Point(point)
    }
}

#[derive(Debug)]
struct ResolvedPointerTarget {
    point: ActionPoint,
    object_id: Option<String>,
}

#[derive(Debug)]
enum PointerTargetIdentity {
    Selector(String),
    Point(ActionPoint),
}

impl PointerTargetIdentity {
    fn resolve(sess: &mut PageSession, target: &PointerTarget) -> Result<Self> {
        match target {
            PointerTarget::Selector(selector) => Ok(Self::Selector(sess.element(selector)?)),
            PointerTarget::Point(point) => Ok(Self::Point(ActionPoint {
                x: point.x,
                y: point.y,
            })),
        }
    }

    fn action_point(&self, sess: &mut PageSession) -> Result<ResolvedPointerTarget> {
        match self {
            Self::Selector(object_id) => Ok(ResolvedPointerTarget {
                point: sess.element_action_point(object_id)?,
                object_id: Some(object_id.clone()),
            }),
            Self::Point(point) => Ok(ResolvedPointerTarget {
                point: sess.coordinate_action_point(*point)?,
                object_id: None,
            }),
        }
    }
}

impl ResolvedPointerTarget {
    fn resolve(sess: &mut PageSession, target: &PointerTarget) -> Result<Self> {
        PointerTargetIdentity::resolve(sess, target)?.action_point(sess)
    }

    fn revalidate(&self, sess: &mut PageSession) -> Result<()> {
        if let Some(object_id) = &self.object_id {
            sess.revalidate_element_action_point(object_id, self.point)
        } else {
            sess.revalidate_coordinate_action_point(self.point)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MouseEventKind {
    Moved {
        button: Option<MouseButton>,
        buttons: u8,
    },
    Pressed {
        button: MouseButton,
    },
    Released {
        button: MouseButton,
    },
}

fn mouse_payload(kind: MouseEventKind, point: ActionPoint) -> Value {
    match kind {
        MouseEventKind::Moved { button, buttons } => json!({
            "type": "mouseMoved",
            "x": point.x,
            "y": point.y,
            "button": button.map(MouseButton::as_cdp).unwrap_or("none"),
            "buttons": buttons,
        }),
        MouseEventKind::Pressed { button } => json!({
            "type": "mousePressed",
            "x": point.x,
            "y": point.y,
            "button": button.as_cdp(),
            "buttons": button.buttons_mask(),
            "clickCount": 1,
        }),
        MouseEventKind::Released { button } => json!({
            "type": "mouseReleased",
            "x": point.x,
            "y": point.y,
            "button": button.as_cdp(),
            "buttons": 0,
            "clickCount": 1,
        }),
    }
}

fn dispatch_mouse(
    sess: &mut PageSession,
    kind: MouseEventKind,
    point: ActionPoint,
    deadline: Deadline,
) -> Result<()> {
    sess.call_until(
        "Input.dispatchMouseEvent",
        mouse_payload(kind, point),
        deadline,
    )?;
    Ok(())
}

fn set_drag_interception(sess: &mut PageSession, enabled: bool, deadline: Deadline) -> Result<()> {
    sess.call_until(
        "Input.setInterceptDrags",
        json!({"enabled": enabled}),
        deadline,
    )?;
    Ok(())
}

fn take_intercepted_drag_data(sess: &mut PageSession) -> Result<Option<Value>> {
    while let Some(event) = sess.next_buffered_event(sess.deadline())? {
        if event.method == "Input.dragIntercepted" {
            return event
                .params
                .get("data")
                .cloned()
                .context("Input.dragIntercepted event omitted drag data")
                .map(Some);
        }
    }
    Ok(None)
}

fn wait_for_intercepted_drag_data(
    sess: &mut PageSession,
    maximum: Duration,
) -> Result<Option<Value>> {
    if let Some(data) = take_intercepted_drag_data(sess)? {
        return Ok(Some(data));
    }
    let wait_until = Instant::now()
        .checked_add(maximum)
        .unwrap_or_else(|| sess.deadline().instant())
        .min(sess.deadline().instant());
    let deadline = Deadline::at(wait_until);
    while let Some(event) = sess.next_event_until(deadline)? {
        if event.method == "Input.dragIntercepted" {
            return event
                .params
                .get("data")
                .cloned()
                .context("Input.dragIntercepted event omitted drag data")
                .map(Some);
        }
    }
    Ok(None)
}

fn dispatch_drag_event(
    sess: &mut PageSession,
    event_type: &str,
    point: ActionPoint,
    data: &Value,
) -> Result<()> {
    sess.call(
        "Input.dispatchDragEvent",
        json!({
            "type": event_type,
            "x": point.x,
            "y": point.y,
            "data": data,
        }),
    )?;
    Ok(())
}

const DRAG_CAPTURE_KEY: &str = "rdny.input.drag-capture.v1";

enum PageDragCapture {
    NotSeen,
    Canceled,
    Data(Value),
}

fn install_page_drag_capture(sess: &mut PageSession) -> Result<()> {
    let expression = format!(
        r#"(() => {{
            const key = Symbol.for({key});
            const prior = window[key];
            if (prior && prior.listener) window.removeEventListener('dragstart', prior.listener);
            const state = {{seen: false, settled: false, canceled: false, effectAllowed: 'none', items: [], listener: null}};
            state.listener = (event) => {{
                const transfer = event.dataTransfer;
                state.seen = true;
                state.effectAllowed = transfer ? String(transfer.effectAllowed || 'none') : 'none';
                state.items = transfer ? Array.from(transfer.types || [])
                    .filter((mimeType) => mimeType !== 'Files')
                    .map((mimeType) => ({{
                        mimeType: String(mimeType),
                        data: String(transfer.getData(mimeType) || ''),
                        title: '',
                        baseURL: String(location.href),
                    }})) : [];
                queueMicrotask(() => {{
                    state.canceled = event.defaultPrevented;
                    state.settled = true;
                }});
            }};
            window.addEventListener('dragstart', state.listener);
            window[key] = state;
            return true;
        }})()"#,
        key = serde_json::to_string(DRAG_CAPTURE_KEY).expect("static capture key serializes"),
    );
    if sess.eval(&expression)?.as_bool() != Some(true) {
        bail!("browser did not install dragstart data capture");
    }
    Ok(())
}

fn take_page_drag_data(sess: &mut PageSession, deadline: Deadline) -> Result<PageDragCapture> {
    let expression = format!(
        r#"(() => {{
            const key = Symbol.for({key});
            const state = window[key];
            if (!state) return null;
            if (state.listener) window.removeEventListener('dragstart', state.listener);
            delete window[key];
            return {{
                seen: Boolean(state.seen),
                settled: Boolean(state.settled),
                canceled: Boolean(state.canceled),
                effectAllowed: String(state.effectAllowed || 'none'),
                items: Array.isArray(state.items) ? state.items : [],
            }};
        }})()"#,
        key = serde_json::to_string(DRAG_CAPTURE_KEY).expect("static capture key serializes"),
    );
    let capture = sess.eval_until(&expression, deadline)?;
    if capture.is_null() || capture["seen"].as_bool() != Some(true) {
        return Ok(PageDragCapture::NotSeen);
    }
    if capture["settled"].as_bool() != Some(true) || capture["canceled"].as_bool() != Some(false) {
        return Ok(PageDragCapture::Canceled);
    }
    let items = capture["items"]
        .as_array()
        .context("browser returned invalid captured drag items")?
        .clone();
    let mask = match capture["effectAllowed"].as_str().unwrap_or("none") {
        "copy" => 1,
        "link" => 2,
        "move" => 16,
        "copyLink" => 1 | 2,
        "copyMove" => 1 | 16,
        "linkMove" => 2 | 16,
        "all" | "uninitialized" => 1 | 2 | 16,
        "none" => 0,
        _ => 0,
    };
    Ok(PageDragCapture::Data(
        json!({"items": items, "dragOperationsMask": mask}),
    ))
}

pub(crate) fn pointer_move(sess: &mut PageSession, target: &PointerTarget) -> Result<()> {
    let target = ResolvedPointerTarget::resolve(sess, target)?;
    dispatch_mouse(
        sess,
        MouseEventKind::Moved {
            button: None,
            buttons: 0,
        },
        target.point,
        sess.deadline(),
    )
}

/// Move and press a button.  A successful call intentionally leaves the button
/// down for a later `pointer_up`; an ambiguous failed press is released.
pub(crate) fn pointer_down(
    sess: &mut PageSession,
    target: &PointerTarget,
    button: MouseButton,
) -> Result<()> {
    let target = ResolvedPointerTarget::resolve(sess, target)?;
    dispatch_mouse(
        sess,
        MouseEventKind::Moved {
            button: None,
            buttons: 0,
        },
        target.point,
        sess.deadline(),
    )?;
    target.revalidate(sess)?;
    if let Err(primary) = dispatch_mouse(
        sess,
        MouseEventKind::Pressed { button },
        target.point,
        sess.deadline(),
    ) {
        let cleanup_deadline = Deadline::after(CLEANUP_TIMEOUT);
        let cleanup = dispatch_mouse(
            sess,
            MouseEventKind::Released { button },
            target.point,
            cleanup_deadline,
        )
        .context("releasing mouse button after ambiguous press failure")
        .err()
        .into_iter()
        .collect();
        return finish_cleanup(Some(primary.context("pressing mouse button")), cleanup);
    }
    Ok(())
}

pub(crate) fn pointer_up(
    sess: &mut PageSession,
    target: &PointerTarget,
    button: MouseButton,
) -> Result<()> {
    let target = ResolvedPointerTarget::resolve(sess, target)?;
    target.revalidate(sess)?;
    dispatch_mouse(
        sess,
        MouseEventKind::Released { button },
        target.point,
        sess.deadline(),
    )
}

/// Click helper retained for the existing command while sharing the same typed
/// payload and cleanup guarantees as the new pointer primitives.
pub(crate) fn pointer_click(
    sess: &mut PageSession,
    target: &PointerTarget,
    button: MouseButton,
) -> Result<()> {
    let target = ResolvedPointerTarget::resolve(sess, target)?;
    dispatch_mouse(
        sess,
        MouseEventKind::Moved {
            button: None,
            buttons: 0,
        },
        target.point,
        sess.deadline(),
    )?;
    target.revalidate(sess)?;

    let mut primary_error = dispatch_mouse(
        sess,
        MouseEventKind::Pressed { button },
        target.point,
        sess.deadline(),
    )
    .err()
    .map(|error| error.context("pressing mouse button"));
    // Preserve the established selector-click sequence: the exact selected
    // node is revalidated immediately before press, then press/release are
    // adjacent. Explicit coordinates have no remote identity and are instead
    // revalidated against the live viewport before both state changes.
    if primary_error.is_none()
        && target.object_id.is_none()
        && let Err(error) = target.revalidate(sess)
    {
        primary_error = Some(error.context("revalidating click target before release"));
    }
    let cleanup_deadline = Deadline::after(CLEANUP_TIMEOUT);
    let release = dispatch_mouse(
        sess,
        MouseEventKind::Released { button },
        target.point,
        cleanup_deadline,
    )
    .context("releasing mouse button");

    let mut cleanup_errors = Vec::new();
    if let Err(error) = release {
        if primary_error.is_some() {
            cleanup_errors.push(error);
        } else {
            primary_error = Some(error);
        }
    }
    finish_cleanup(primary_error, cleanup_errors)
}

/// Drag between two independently trusted targets.
pub(crate) fn drag(
    sess: &mut PageSession,
    from: &PointerTarget,
    to: &PointerTarget,
    button: MouseButton,
    steps: u32,
    duration: Duration,
) -> Result<()> {
    validate_drag_options(steps, duration)?;

    // Resolve selector identities before pressing, then prove the destination
    // is actionable. Resolve the source point last because destination
    // validation may scroll. After press, derive the destination point again
    // from the same exact node so distant endpoints and pointer handlers cannot
    // leave us moving toward stale geometry.
    let from_identity = PointerTargetIdentity::resolve(sess, from)?;
    let to_identity = PointerTargetIdentity::resolve(sess, to)?;
    to_identity.action_point(sess)?;
    let from = from_identity.action_point(sess)?;

    // A preceding pointer primitive may have been sent through a now-detached
    // CDP session. Clear Chromium's drag controller before this self-contained
    // convenience action; this does not release or press any mouse button.
    sess.call("Input.cancelDragging", json!({}))
        .context("resetting prior browser drag state")?;

    if let Err(primary) = set_drag_interception(sess, true, sess.deadline()) {
        let cleanup_deadline = Deadline::after(CLEANUP_TIMEOUT);
        let cleanup = set_drag_interception(sess, false, cleanup_deadline)
            .context("disabling drag interception after ambiguous enable failure")
            .err()
            .into_iter()
            .collect();
        return finish_cleanup(
            Some(primary.context("enabling native drag interception")),
            cleanup,
        );
    }
    if let Err(primary) = install_page_drag_capture(sess) {
        let mut cleanup = Vec::new();
        let cleanup_deadline = Deadline::after(CLEANUP_TIMEOUT);
        if let Err(error) = take_page_drag_data(sess, cleanup_deadline)
            .context("removing possibly installed dragstart capture after setup failure")
        {
            cleanup.push(error);
        }
        if let Err(error) = set_drag_interception(sess, false, cleanup_deadline)
            .context("disabling drag interception after capture setup failure")
        {
            cleanup.push(error);
        }
        return finish_cleanup(
            Some(primary.context("installing native dragstart data capture")),
            cleanup,
        );
    }

    let source_setup = (|| {
        dispatch_mouse(
            sess,
            MouseEventKind::Moved {
                button: None,
                buttons: 0,
            },
            from.point,
            sess.deadline(),
        )?;
        // This must remain the final browser round trip before press: hover
        // handlers can detach, move, hide, or cover the exact selected node.
        from.revalidate(sess)
    })();
    if let Err(primary) = source_setup {
        let mut cleanup = Vec::new();
        let cleanup_deadline = Deadline::after(CLEANUP_TIMEOUT);
        if let Err(error) = take_page_drag_data(sess, cleanup_deadline)
            .context("removing dragstart capture after source setup failure")
        {
            cleanup.push(error);
        }
        if let Err(error) = set_drag_interception(sess, false, cleanup_deadline)
            .context("disabling drag interception after source setup failure")
        {
            cleanup.push(error);
        }
        return finish_cleanup(
            Some(primary.context("preparing and revalidating drag source")),
            cleanup,
        );
    }

    let mut last_safe_point = from.point;
    let mut primary_error = dispatch_mouse(
        sess,
        MouseEventKind::Pressed { button },
        from.point,
        sess.deadline(),
    )
    .err()
    .map(|error| error.context("pressing mouse button to begin drag"));

    let mut destination = None;
    let mut drag_data = None;
    let mut drag_entered = false;
    let mut drop_dispatched = false;
    if primary_error.is_none() {
        match to_identity.action_point(sess) {
            Ok(target) => destination = Some(target),
            Err(error) => {
                primary_error = Some(error.context("resolving drag destination after press"));
            }
        }
    }

    if primary_error.is_none()
        && let Some(to) = &destination
    {
        let started = Instant::now();
        let threshold_steps = steps.saturating_sub(1).min(5);
        for (index, point) in interpolation_points(from.point, to.point, steps)
            .into_iter()
            .enumerate()
        {
            let target_elapsed = duration.mul_f64((index + 1) as f64 / steps as f64);
            let target_time = started + target_elapsed;
            if let Some(wait) = target_time.checked_duration_since(Instant::now()) {
                sess.deadline().sleep(wait);
            }
            match dispatch_mouse(
                sess,
                MouseEventKind::Moved {
                    // Chromium needs the held button on the initial movements
                    // to cross the native drag threshold. Later movement is
                    // related to no button transition while `buttons` retains
                    // the held-state bitmask.
                    button: ((index as u32) < threshold_steps).then_some(button),
                    buttons: button.buttons_mask(),
                },
                point,
                sess.deadline(),
            ) {
                Ok(()) => {
                    last_safe_point = point;
                    if drag_data.is_none() {
                        match take_intercepted_drag_data(sess) {
                            Ok(data) => drag_data = data,
                            Err(error) => {
                                primary_error =
                                    Some(error.context("reading intercepted native drag data"));
                                break;
                            }
                        }
                    }
                    if let Some(data) = &drag_data {
                        let event_type = if drag_entered {
                            "dragOver"
                        } else {
                            drag_entered = true;
                            "dragEnter"
                        };
                        if let Err(error) = dispatch_drag_event(sess, event_type, point, data) {
                            primary_error = Some(error.context(format!(
                                "dispatching native {event_type} at drag movement step {} of {steps}",
                                index + 1
                            )));
                            break;
                        }
                    }
                }
                Err(error) => {
                    primary_error = Some(error.context(format!(
                        "dispatching drag movement step {} of {steps}",
                        index + 1
                    )));
                    break;
                }
            }
        }
    }

    // Chromium may emit dragIntercepted just after the final movement response;
    // wait briefly for actual browser drag data before deciding whether this is
    // an HTML drop or an ordinary held-button pointer drag.
    if primary_error.is_none() && drag_data.is_none() {
        match wait_for_intercepted_drag_data(sess, Duration::from_millis(250)) {
            Ok(data) => drag_data = data,
            Err(error) => {
                primary_error = Some(error.context("reading final intercepted native drag data"));
            }
        }
    }
    if primary_error.is_none() {
        match take_page_drag_data(sess, sess.deadline()) {
            Ok(PageDragCapture::Data(captured)) => {
                if drag_data.is_none() {
                    drag_data = Some(captured);
                }
            }
            Ok(PageDragCapture::Canceled) => drag_data = None,
            Ok(PageDragCapture::NotSeen) => {}
            Err(error) => {
                primary_error = Some(error.context("reading trusted page dragstart data"));
            }
        }
    }

    if primary_error.is_none()
        && let Some(data) = &drag_data
    {
        if !drag_entered
            && let Err(error) = dispatch_drag_event(sess, "dragEnter", last_safe_point, data)
        {
            primary_error = Some(error.context("dispatching native dragEnter before drop"));
        }
        if primary_error.is_none()
            && let Err(error) = dispatch_drag_event(sess, "dragOver", last_safe_point, data)
        {
            primary_error = Some(error.context("dispatching native dragOver before drop"));
        }
        if primary_error.is_none()
            && let Some(to) = &destination
            && let Err(error) = to.revalidate(sess)
        {
            primary_error =
                Some(error.context("revalidating drag destination immediately before native drop"));
        }
        if primary_error.is_none()
            && let Err(error) = dispatch_drag_event(sess, "drop", last_safe_point, data)
        {
            primary_error = Some(error.context("dispatching native drop"));
        } else if primary_error.is_none() {
            drop_dispatched = true;
        }
    }
    if primary_error.is_none()
        && drag_data.is_none()
        && let Some(to) = &destination
        && let Err(error) = to.revalidate(sess)
    {
        primary_error =
            Some(error.context("revalidating drag destination immediately before release"));
    }

    // The press request may have reached Chromium even when its response did
    // not.  Always send release on a fresh, short budget and at the last point
    // whose move was successfully dispatched.
    let cleanup_deadline = Deadline::after(CLEANUP_TIMEOUT);
    let release = dispatch_mouse(
        sess,
        MouseEventKind::Released { button },
        last_safe_point,
        cleanup_deadline,
    )
    .context("releasing mouse button after drag");
    let mut cleanup_errors = Vec::new();
    if let Err(error) = release {
        if primary_error.is_some() {
            cleanup_errors.push(error);
        } else {
            primary_error = Some(error);
        }
    }
    if let Err(error) = take_page_drag_data(sess, cleanup_deadline) {
        let error = error.context("removing dragstart capture after drag");
        if primary_error.is_some() {
            cleanup_errors.push(error);
        } else {
            primary_error = Some(error);
        }
    }
    if !drop_dispatched
        && let Err(error) = sess
            .call_until("Input.cancelDragging", json!({}), cleanup_deadline)
            .context("canceling intercepted drag after unsuccessful drop")
    {
        if primary_error.is_some() {
            cleanup_errors.push(error);
        } else {
            primary_error = Some(error);
        }
    }
    if let Err(error) = set_drag_interception(sess, false, cleanup_deadline)
        .context("disabling drag interception after drag")
    {
        if primary_error.is_some() {
            cleanup_errors.push(error);
        } else {
            primary_error = Some(error);
        }
    }
    finish_cleanup(primary_error, cleanup_errors)
}

fn validate_drag_options(steps: u32, duration: Duration) -> Result<()> {
    if !(MIN_DRAG_STEPS..=MAX_DRAG_STEPS).contains(&steps) {
        bail!("drag steps must be between {MIN_DRAG_STEPS} and {MAX_DRAG_STEPS}");
    }
    if duration < Duration::from_millis(MIN_DRAG_DURATION_MS)
        || duration > Duration::from_millis(MAX_DRAG_DURATION_MS)
    {
        bail!(
            "drag duration must be between {MIN_DRAG_DURATION_MS} and {MAX_DRAG_DURATION_MS} milliseconds"
        );
    }
    Ok(())
}

fn interpolation_points(from: ActionPoint, to: ActionPoint, steps: u32) -> Vec<ActionPoint> {
    (1..=steps)
        .map(|step| {
            if step == steps {
                return to;
            }
            let fraction = f64::from(step) / f64::from(steps);
            ActionPoint {
                x: from.x + (to.x - from.x) * fraction,
                y: from.y + (to.y - from.y) * fraction,
            }
        })
        .collect()
}

fn finish_cleanup(
    primary_error: Option<anyhow::Error>,
    cleanup_errors: Vec<anyhow::Error>,
) -> Result<()> {
    match (primary_error, cleanup_errors.is_empty()) {
        (None, true) => Ok(()),
        (Some(error), true) => Err(error),
        (Some(error), false) => {
            let cleanup = cleanup_errors
                .iter()
                .map(|error| format!("{error:#}"))
                .collect::<Vec<_>>()
                .join("; ");
            Err(error.context(format!("input cleanup also failed: {cleanup}")))
        }
        (None, false) => unreachable!("cleanup errors always promote the first failure"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;
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
    fn mouse_buttons_parse_display_and_map_to_cdp_masks() {
        for (name, button, mask) in [
            ("left", MouseButton::Left, 1),
            ("RIGHT", MouseButton::Right, 2),
            ("middle", MouseButton::Middle, 4),
            ("back", MouseButton::Back, 8),
            ("forward", MouseButton::Forward, 16),
        ] {
            let parsed: MouseButton = name.parse().unwrap();
            assert_eq!(parsed, button);
            assert_eq!(parsed.buttons_mask(), mask);
            assert_eq!(parsed.to_string(), name.to_ascii_lowercase());
        }
        assert!("primary".parse::<MouseButton>().is_err());
    }

    #[test]
    fn mouse_payloads_include_cdp_button_state() {
        let point = ActionPoint { x: 10.5, y: 20.25 };
        assert_eq!(
            mouse_payload(
                MouseEventKind::Moved {
                    button: None,
                    buttons: 0,
                },
                point
            ),
            json!({
                "type": "mouseMoved", "x": 10.5, "y": 20.25,
                "button": "none", "buttons": 0
            })
        );
        assert_eq!(
            mouse_payload(
                MouseEventKind::Moved {
                    button: Some(MouseButton::Left),
                    buttons: 1,
                },
                point
            ),
            json!({
                "type": "mouseMoved", "x": 10.5, "y": 20.25,
                "button": "left", "buttons": 1
            })
        );
        assert_eq!(
            mouse_payload(
                MouseEventKind::Pressed {
                    button: MouseButton::Back
                },
                point
            ),
            json!({
                "type": "mousePressed", "x": 10.5, "y": 20.25,
                "button": "back", "buttons": 8, "clickCount": 1
            })
        );
        assert_eq!(
            mouse_payload(
                MouseEventKind::Released {
                    button: MouseButton::Back
                },
                point
            ),
            json!({
                "type": "mouseReleased", "x": 10.5, "y": 20.25,
                "button": "back", "buttons": 0, "clickCount": 1
            })
        );
    }

    #[test]
    fn coordinate_parser_is_strict_and_rejects_nonfinite_values() {
        assert_eq!(
            " 12.5, -4 ".parse::<PointerPoint>().unwrap(),
            PointerPoint { x: 12.5, y: -4.0 }
        );
        for raw in ["", "1", "1,", ",2", "1,2,3", "x,2", "NaN,2", "inf,2"] {
            assert!(
                raw.parse::<PointerPoint>().is_err(),
                "expected `{raw}` to fail"
            );
        }
        assert!(PointerPoint::new(f64::INFINITY, 0.0).is_err());
        assert!(PointerTarget::coordinates(0.0, f64::NAN).is_err());
    }

    #[test]
    fn interpolation_is_exact_and_bounded_by_step_count() {
        let points = interpolation_points(
            ActionPoint { x: 0.0, y: 10.0 },
            ActionPoint { x: 10.0, y: -10.0 },
            4,
        );
        assert_eq!(points.len(), 4);
        assert_eq!(points[0], ActionPoint { x: 2.5, y: 5.0 });
        assert_eq!(points[1], ActionPoint { x: 5.0, y: 0.0 });
        assert_eq!(points[3], ActionPoint { x: 10.0, y: -10.0 });
    }

    #[test]
    fn drag_limits_include_only_documented_bounds() {
        assert!(
            validate_drag_options(MIN_DRAG_STEPS, Duration::from_millis(MIN_DRAG_DURATION_MS))
                .is_ok()
        );
        assert!(
            validate_drag_options(MAX_DRAG_STEPS, Duration::from_millis(MAX_DRAG_DURATION_MS))
                .is_ok()
        );
        assert!(validate_drag_options(0, Duration::from_millis(1)).is_err());
        assert!(validate_drag_options(MAX_DRAG_STEPS + 1, Duration::from_millis(1)).is_err());
        assert!(validate_drag_options(1, Duration::ZERO).is_err());
        assert!(validate_drag_options(1, Duration::from_millis(MAX_DRAG_DURATION_MS + 1)).is_err());
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

    #[test]
    fn drag_preserves_press_and_cleanup_release_failures_at_last_safe_point() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = accept(stream).unwrap();
            loop {
                let command = read_json_message(&mut socket);
                let id = command["id"].clone();
                match command["method"].as_str().unwrap() {
                    "Runtime.evaluate" => {
                        let expression = command["params"]["expression"].as_str().unwrap();
                        let value = if expression.contains("window.addEventListener('dragstart'") {
                            json!(true)
                        } else if expression.contains("delete window[key]") {
                            Value::Null
                        } else {
                            json!({"status": "ready"})
                        };
                        socket
                            .send(Message::Text(
                                json!({
                                    "id": id,
                                    "result": {"result": {
                                        "type": "object",
                                        "value": value
                                    }}
                                })
                                .to_string()
                                .into(),
                            ))
                            .unwrap();
                    }
                    "Input.dispatchMouseEvent" if command["params"]["type"] == "mousePressed" => {
                        assert_eq!(command["params"]["x"], 1.0);
                        assert_eq!(command["params"]["y"], 2.0);
                        socket
                            .send(Message::Text(
                                json!({
                                    "id": id,
                                    "error": {"code": -32000, "message": "injected press failure"}
                                })
                                .to_string()
                                .into(),
                            ))
                            .unwrap();
                    }
                    "Input.dispatchMouseEvent" if command["params"]["type"] == "mouseReleased" => {
                        assert_eq!(command["params"]["x"], 1.0);
                        assert_eq!(command["params"]["y"], 2.0);
                        assert_eq!(command["params"]["buttons"], 0);
                        socket
                            .send(Message::Text(
                                json!({
                                    "id": id,
                                    "error": {"code": -32000, "message": "injected release failure"}
                                })
                                .to_string()
                                .into(),
                            ))
                            .unwrap();
                    }
                    "Input.setInterceptDrags" => {
                        let disabling = command["params"]["enabled"] == false;
                        socket
                            .send(Message::Text(
                                json!({"id": id, "result": {}}).to_string().into(),
                            ))
                            .unwrap();
                        if disabling {
                            break;
                        }
                    }
                    "Input.cancelDragging" => socket
                        .send(Message::Text(
                            json!({"id": id, "result": {}}).to_string().into(),
                        ))
                        .unwrap(),
                    "Input.dispatchMouseEvent" => {
                        socket
                            .send(Message::Text(
                                json!({"id": id, "result": {}}).to_string().into(),
                            ))
                            .unwrap();
                    }
                    method => panic!("unexpected CDP method {method}"),
                }
            }
        });

        let mut session =
            PageSession::connect_for_input_test(&format!("ws://127.0.0.1:{port}")).unwrap();
        let from = PointerTarget::coordinates(1.0, 2.0).unwrap();
        let to = PointerTarget::coordinates(11.0, 12.0).unwrap();
        let error = drag(
            &mut session,
            &from,
            &to,
            MouseButton::Left,
            2,
            Duration::from_millis(2),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("pressing mouse button to begin drag"));
        assert!(format!("{error:#}").contains("injected press failure"));
        assert!(format!("{error:#}").contains("input cleanup also failed"));
        assert!(format!("{error:#}").contains("injected release failure"));
        server.join().unwrap();
    }

    #[test]
    fn drag_releases_after_an_intermediate_movement_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = accept(stream).unwrap();
            let mut pressed = false;
            let mut failed_move = false;
            loop {
                let command = read_json_message(&mut socket);
                let id = command["id"].clone();
                match command["method"].as_str().unwrap() {
                    "Runtime.evaluate" => socket
                        .send(Message::Text({
                            let expression = command["params"]["expression"].as_str().unwrap();
                            let value =
                                if expression.contains("window.addEventListener('dragstart'") {
                                    json!(true)
                                } else if expression.contains("delete window[key]") {
                                    Value::Null
                                } else {
                                    json!({"status": "ready"})
                                };
                            json!({
                                "id": id,
                                "result": {"result": {"type": "object", "value": value}}
                            })
                            .to_string()
                            .into()
                        }))
                        .unwrap(),
                    "Input.dispatchMouseEvent" if command["params"]["type"] == "mousePressed" => {
                        pressed = true;
                        socket
                            .send(Message::Text(
                                json!({"id": id, "result": {}}).to_string().into(),
                            ))
                            .unwrap();
                    }
                    "Input.dispatchMouseEvent"
                        if pressed
                            && !failed_move
                            && command["params"]["type"] == "mouseMoved"
                            && command["params"]["buttons"] == 1 =>
                    {
                        failed_move = true;
                        socket
                            .send(Message::Text(
                                json!({
                                    "id": id,
                                    "error": {"code": -32000, "message": "injected movement failure"}
                                })
                                .to_string()
                                .into(),
                            ))
                            .unwrap();
                    }
                    "Input.dispatchMouseEvent" if command["params"]["type"] == "mouseReleased" => {
                        assert!(pressed);
                        assert!(failed_move);
                        assert_eq!(command["params"]["x"], 1.0);
                        assert_eq!(command["params"]["y"], 2.0);
                        assert_eq!(command["params"]["buttons"], 0);
                        socket
                            .send(Message::Text(
                                json!({"id": id, "result": {}}).to_string().into(),
                            ))
                            .unwrap();
                    }
                    "Input.setInterceptDrags" => {
                        let disabling = command["params"]["enabled"] == false;
                        socket
                            .send(Message::Text(
                                json!({"id": id, "result": {}}).to_string().into(),
                            ))
                            .unwrap();
                        if disabling {
                            break;
                        }
                    }
                    "Input.cancelDragging" => socket
                        .send(Message::Text(
                            json!({"id": id, "result": {}}).to_string().into(),
                        ))
                        .unwrap(),
                    "Input.dispatchMouseEvent" => socket
                        .send(Message::Text(
                            json!({"id": id, "result": {}}).to_string().into(),
                        ))
                        .unwrap(),
                    method => panic!("unexpected CDP method {method}"),
                }
            }
        });

        let mut session =
            PageSession::connect_for_input_test(&format!("ws://127.0.0.1:{port}")).unwrap();
        let from = PointerTarget::coordinates(1.0, 2.0).unwrap();
        let to = PointerTarget::coordinates(11.0, 12.0).unwrap();
        let error = drag(
            &mut session,
            &from,
            &to,
            MouseButton::Left,
            2,
            Duration::from_millis(2),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("dispatching drag movement step 1 of 2"));
        assert!(format!("{error:#}").contains("injected movement failure"));
        server.join().unwrap();
    }

    fn read_json_message(socket: &mut tungstenite::WebSocket<std::net::TcpStream>) -> Value {
        match socket.read().unwrap() {
            Message::Text(text) => serde_json::from_str(&text).unwrap(),
            message => panic!("unexpected command message {message:?}"),
        }
    }
}
