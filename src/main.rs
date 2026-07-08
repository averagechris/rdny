// TODO(#76): remove once all command groups land and every module
// entry point is wired into dispatch.
#![allow(dead_code)]

mod browser;
mod cdp;
mod cli;
mod commands;
mod hint;
mod session;
mod state;

use std::process::ExitCode;

/// Exit codes follow the rodney convention:
/// 0 success, 1 reserved for check-failures, 2 error.
fn main() -> ExitCode {
    match cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("rdny: {err:#}");
            ExitCode::from(2)
        }
    }
}
