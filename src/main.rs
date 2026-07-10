mod browser;
mod cdp;
mod cli;
mod commands;
mod config;
mod hint;
mod session;
mod state;

use std::process::ExitCode;

/// Exit codes follow the rodney convention:
/// 0 success, 1 reserved for check-failures, 2 error.
fn main() -> ExitCode {
    if let Err(err) = state::capture_initial_cwd() {
        eprintln!("rdny: could not capture initial working directory: {err:#}");
        return ExitCode::from(2);
    }
    match cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("rdny: {err:#}");
            ExitCode::from(2)
        }
    }
}
