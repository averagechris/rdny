//! Waiting: wait, waitload, waitstable, waitidle, sleep.

use anyhow::{Result, bail};

use crate::session::PageSession;

/// Wait for a selector to match an element.
pub fn wait(_sess: &mut PageSession, _selector: &str) -> Result<()> {
    bail!("wait: not implemented yet")
}

/// Wait for the page load event.
pub fn waitload(_sess: &mut PageSession) -> Result<()> {
    bail!("waitload: not implemented yet")
}

/// Wait for the DOM to stop mutating.
pub fn waitstable(_sess: &mut PageSession) -> Result<()> {
    bail!("waitstable: not implemented yet")
}

/// Wait for the network to go idle.
pub fn waitidle(_sess: &mut PageSession) -> Result<()> {
    bail!("waitidle: not implemented yet")
}

/// Sleep for a number of seconds (fractions allowed).
pub fn sleep(seconds: f64) -> Result<()> {
    if !seconds.is_finite() || seconds < 0.0 {
        bail!("sleep: seconds must be a non-negative number");
    }
    std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
    Ok(())
}
