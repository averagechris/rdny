//! Command-line surface and dispatch.

use anyhow::Result;
use clap::Parser;

/// Chrome automation from the command line.
#[derive(Debug, Parser)]
#[command(name = "rdny", version, about)]
pub struct Cli {
    /// Seconds to wait for slow operations before giving up.
    #[arg(long, global = true, default_value_t = 30.0)]
    pub timeout: f64,
}

/// Parse argv and execute the selected command.
pub fn run() -> Result<()> {
    let _cli = Cli::parse();
    anyhow::bail!("unimplemented: command dispatch")
}
