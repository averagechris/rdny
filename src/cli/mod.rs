//! Command-line façade and dependency boundary.

mod arguments;
mod dispatch;
mod output;
mod validation;

use anyhow::{Context, Result};
use clap::{CommandFactory, FromArgMatches};
use serde_json::json;
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::browser::{self, BrowserStatus, LaunchOpts};
use crate::commands::OutputFormat;
use crate::input::{MouseButton, PointerPoint, PointerTarget};
use crate::selector::ElementSelector;
use crate::{commands, config, session};

use arguments::*;
use validation::*;

/// Parse argv, validate process-wide policy, and dispatch the selected command.
pub fn run() -> Result<()> {
    let cli = arguments::parse();
    validation::before_dispatch(&cli)?;
    dispatch::run(cli)
}
