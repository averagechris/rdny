//! Interaction: js, click, input, clear, file, download, select,
//! submit, hover, focus.

use std::path::Path;

use anyhow::{Result, bail};

use crate::commands::print_value;
use crate::session::PageSession;

/// Evaluate a JavaScript expression and print its result.
pub fn js(sess: &mut PageSession, expression: &str) -> Result<()> {
    let value = sess.eval(expression)?;
    print_value(&value);
    Ok(())
}

/// Click the first selector match (real mouse events).
pub fn click(_sess: &mut PageSession, _selector: &str) -> Result<()> {
    bail!("click: not implemented yet")
}

/// Type text into the first selector match.
pub fn input(_sess: &mut PageSession, _selector: &str, _text: &str) -> Result<()> {
    bail!("input: not implemented yet")
}

/// Clear the value of the first selector match.
pub fn clear(_sess: &mut PageSession, _selector: &str) -> Result<()> {
    bail!("clear: not implemented yet")
}

/// Set a file on a file input; path "-" reads the payload from stdin.
pub fn file(_sess: &mut PageSession, _selector: &str, _path: &Path) -> Result<()> {
    bail!("file: not implemented yet")
}

/// Download the href/src target of the first selector match; file "-"
/// (or no file) streams to stdout.
pub fn download(_sess: &mut PageSession, _selector: &str, _file: Option<&Path>) -> Result<()> {
    bail!("download: not implemented yet")
}

/// Select a dropdown option by value.
pub fn select(_sess: &mut PageSession, _selector: &str, _value: &str) -> Result<()> {
    bail!("select: not implemented yet")
}

/// Submit the form containing (or matching) the selector.
pub fn submit(_sess: &mut PageSession, _selector: &str) -> Result<()> {
    bail!("submit: not implemented yet")
}

/// Hover over the first selector match (real mouse events).
pub fn hover(_sess: &mut PageSession, _selector: &str) -> Result<()> {
    bail!("hover: not implemented yet")
}

/// Focus the first selector match.
pub fn focus(_sess: &mut PageSession, _selector: &str) -> Result<()> {
    bail!("focus: not implemented yet")
}
