//! Command implementations, grouped to mirror the CLI surface.

pub mod artifacts;
pub mod cookie;
pub mod instances;
pub mod interact;
pub mod logs;
pub mod nav;
pub mod pageinfo;
pub mod shot;
pub mod tabs;
pub mod video;
pub mod viewport;
pub mod wait;

use anyhow::{Context, Result};
use base64::Engine as _;
use clap::ValueEnum;
use serde_json::Value;
use std::io::Write;

/// User-selected output format. Structured commands must receive this value
/// directly so json/jsonl semantics stay consistent everywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Human,
    Json,
    Jsonl,
}

impl OutputFormat {
    pub fn is_structured(self) -> bool {
        !matches!(self, Self::Human)
    }

    pub fn emit_json(self, value: &Value) -> Result<()> {
        self.write_json(value, &mut std::io::stdout())
    }

    pub fn write_json(self, value: &Value, mut writer: impl Write) -> Result<()> {
        match self {
            Self::Human => writeln!(writer, "{}", human_sanitize(&value.to_string()))?,
            Self::Json => writeln!(writer, "{}", serde_json::to_string_pretty(value)?)?,
            Self::Jsonl => writeln!(writer, "{}", serde_json::to_string(value)?)?,
        }
        Ok(())
    }

    pub fn render_json(self, value: &Value) -> Result<String> {
        Ok(match self {
            Self::Human => human_sanitize(&value.to_string()),
            Self::Json => serde_json::to_string_pretty(value)?,
            Self::Jsonl => serde_json::to_string(value)?,
        })
    }
}

/// Print an evaluation result the way a shell user expects: strings
/// raw, null/undefined as nothing, everything else as compact JSON.
pub fn print_value(value: &Value) {
    match value {
        Value::Null => {}
        Value::String(s) => println!("{}", human_sanitize(s)),
        other => println!("{other}"),
    }
}

/// Strip terminal-control bytes from browser/page controlled text before human output.
pub fn human_sanitize(s: &str) -> String {
    s.chars()
        .filter(|&c| c == '\n' || c == '\t' || c == '\r' || (!c.is_control() && c != '\u{7f}'))
        .collect()
}

/// Decode a base64 payload as returned by Chrome (screenshots, PDF,
/// downloads).
pub fn decode_base64(data: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .context("decoding base64 payload from Chrome")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn output_format_writer_semantics_are_exact() {
        let value = json!({"schemaVersion":1,"kind":"x","items":[{"a":1},{"a":2}]});
        let mut json_out = Vec::new();
        OutputFormat::Json
            .write_json(&value, &mut json_out)
            .unwrap();
        let json_text = String::from_utf8(json_out).unwrap();
        assert!(json_text.contains("\n  \"items\": [\n"));
        assert_eq!(serde_json::from_str::<Value>(&json_text).unwrap(), value);

        let mut jsonl_out = Vec::new();
        OutputFormat::Jsonl
            .write_json(&value, &mut jsonl_out)
            .unwrap();
        let jsonl_text = String::from_utf8(jsonl_out).unwrap();
        assert_eq!(jsonl_text.lines().count(), 1);
        assert!(!jsonl_text.trim_end().contains('\n'));
        assert_eq!(serde_json::from_str::<Value>(&jsonl_text).unwrap(), value);

        let empty = json!({"schemaVersion":1,"kind":"x","items":[]});
        let mut empty_out = Vec::new();
        OutputFormat::Jsonl
            .write_json(&empty, &mut empty_out)
            .unwrap();
        assert!(
            String::from_utf8(empty_out)
                .unwrap()
                .contains("\"items\":[]")
        );
    }
}
