//! Shared bounded cleanup policy for ambiguous input failures.

use std::time::Duration;

use anyhow::Result;

pub(super) const CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) fn finish_cleanup(
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
