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
use serde_json::Value;

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
