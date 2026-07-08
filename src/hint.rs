//! Actionable error formatting.
//!
//! Errors that reach the user follow a fixed three-line layering
//! (one diagnostic, one action, one docs link — see tracker #80):
//!
//! ```text
//! <what went wrong, one line>
//! hint: <one concrete next action>
//! docs: <one URL>
//! ```

/// Base URL for evergreen docs pages referenced from hints.
pub const DOCS_BASE: &str = "https://averagechris.srht.site/wiki";

/// Build an error carrying the diagnostic/action/docs layering.
pub fn hint_error(
    diagnostic: impl std::fmt::Display,
    action: impl std::fmt::Display,
    docs_slug: Option<&str>,
) -> anyhow::Error {
    let mut msg = format!("{diagnostic}\nhint: {action}");
    if let Some(slug) = docs_slug {
        msg.push_str(&format!("\ndocs: {DOCS_BASE}/{slug}/"));
    }
    anyhow::anyhow!(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_line_layering() {
        let err = hint_error(
            "no browser found",
            "install Google Chrome or set RDNY_CHROME",
            Some("chromium-remote-debugging"),
        );
        let text = format!("{err}");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[1].starts_with("hint: "));
        assert!(lines[2].starts_with("docs: https://"));
    }

    #[test]
    fn docs_line_optional() {
        let err = hint_error("boom", "try again", None);
        assert_eq!(format!("{err}").lines().count(), 2);
    }
}
