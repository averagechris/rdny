//! Command-line surface and dispatch.

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

use crate::browser::{self, BrowserStatus, LaunchOpts};

/// Chrome automation from the command line.
#[derive(Debug, Parser)]
#[command(
    name = "rdny",
    version,
    about = "Chrome automation from the command line"
)]
pub struct Cli {
    /// Seconds to wait for slow operations before giving up.
    #[arg(long, global = true, default_value_t = 30.0)]
    pub timeout: f64,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start a new browser session.
    Start(StartArgs),
    /// Connect to an existing browser debugger endpoint.
    Connect { address: String },
    /// Stop the current browser session.
    Stop,
    /// Show current browser session status.
    Status,
    /// Open a URL in the current page.
    Open { url: String },
    /// Go back in page history.
    Back,
    /// Go forward in page history.
    Forward,
    /// Reload the current page.
    Reload(ReloadArgs),
    /// Clear the browser cache.
    ClearCache,
    /// Print the current page URL.
    Url,
    /// Print the current page title.
    Title,
    /// Print page or element HTML.
    Html { selector: Option<String> },
    /// Print element text.
    Text { selector: String },
    /// Print an element attribute.
    Attr { selector: String, name: String },
    /// Save the current page as PDF.
    Pdf { file: Option<PathBuf> },
    /// Evaluate JavaScript in the current page.
    Js { expression: String },
    /// Click an element.
    Click { selector: String },
    /// Type text into an element.
    Input { selector: String, text: String },
    /// Clear an element's value.
    Clear { selector: String },
    /// Upload a file to an input element.
    File { selector: String, path: PathBuf },
    /// Click and download a linked resource.
    Download {
        selector: String,
        file: Option<PathBuf>,
    },
    /// Select an option by value.
    Select { selector: String, value: String },
    /// Submit a form.
    Submit { selector: String },
    /// Hover an element.
    Hover { selector: String },
    /// Focus an element.
    Focus { selector: String },
    /// Wait for an element to appear.
    Wait { selector: String },
    /// Wait for page load.
    Waitload,
    /// Wait for page stability.
    Waitstable,
    /// Wait for browser idleness.
    Waitidle,
    /// Sleep for a number of seconds.
    Sleep { seconds: f64 },
    /// Capture a screenshot.
    Screenshot(ScreenshotArgs),
    /// Capture an element screenshot.
    ScreenshotEl {
        selector: String,
        file: Option<PathBuf>,
    },
    /// List open pages.
    Pages,
    /// Switch to a page by index.
    Page { index: usize },
    /// Open a new page.
    Newpage { url: Option<String> },
}

#[derive(Debug, Parser)]
pub struct StartArgs {
    /// Show a visible window instead of headless.
    #[arg(long)]
    pub show: bool,
    /// Ignore TLS certificate errors.
    #[arg(short = 'k', long)]
    pub insecure: bool,
}

#[derive(Debug, Parser)]
pub struct ReloadArgs {
    /// Bypass cache while reloading.
    #[arg(long)]
    pub hard: bool,
}

#[derive(Debug, Parser)]
#[command(disable_help_flag = true)]
pub struct ScreenshotArgs {
    /// Screenshot width.
    #[arg(short = 'w')]
    pub width: Option<u32>,
    /// Screenshot height.
    #[arg(short = 'h', long)]
    pub height: Option<u32>,
    /// Show help.
    #[arg(long, action = clap::ArgAction::Help)]
    pub help: Option<bool>,
    /// Output file.
    pub file: Option<PathBuf>,
}

/// Parse argv and execute the selected command.
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Start(args) => {
            if let Some(state) = crate::state::load()?
                && let Ok(BrowserStatus::Running { .. }) = browser::status(&state)
            {
                let pid = state
                    .pid
                    .map(|pid| pid.to_string())
                    .unwrap_or_else(|| "attached".to_string());
                return Err(crate::hint::hint_error(
                    format!(
                        "a browser session is already running (pid {pid}/port {})",
                        state.port
                    ),
                    "run `rdny stop` first",
                    None,
                ));
            }
            let opts = LaunchOpts {
                show: args.show,
                insecure: args.insecure,
                extra_args: vec![],
            };
            let state = browser::launch(&opts, &crate::state::state_dir()?)?;
            crate::state::save(&state)?;
            let browser = state
                .browser_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "browser".to_string());
            let pid = state
                .pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "attached".to_string());
            println!("started {browser} pid {pid} on port {}", state.port);
        }
        Command::Connect { address } => {
            let (host, port) = parse_address(&address)?;
            let state = browser::connect(&host, port)?;
            crate::state::save(&state)?;
            println!("connected to {host}:{port}");
        }
        Command::Stop => {
            let state = crate::state::require()?;
            browser::stop(&state)?;
            crate::state::clear()?;
            println!("stopped");
        }
        Command::Status => match crate::state::load()? {
            None => println!("no session"),
            Some(state) => match browser::status(&state)? {
                BrowserStatus::Running { browser } => {
                    let pid = state
                        .pid
                        .map(|pid| pid.to_string())
                        .unwrap_or_else(|| "attached".to_string());
                    println!(
                        "running: {browser} on {}:{} (pid {pid})",
                        state.host, state.port
                    );
                }
                BrowserStatus::Stale => {
                    println!("stale: state file exists but browser is not responding");
                }
            },
        },
        Command::Open { .. } => anyhow::bail!("open: not implemented yet"),
        Command::Back => anyhow::bail!("back: not implemented yet"),
        Command::Forward => anyhow::bail!("forward: not implemented yet"),
        Command::Reload(_) => anyhow::bail!("reload: not implemented yet"),
        Command::ClearCache => anyhow::bail!("clear-cache: not implemented yet"),
        Command::Url => anyhow::bail!("url: not implemented yet"),
        Command::Title => anyhow::bail!("title: not implemented yet"),
        Command::Html { .. } => anyhow::bail!("html: not implemented yet"),
        Command::Text { .. } => anyhow::bail!("text: not implemented yet"),
        Command::Attr { .. } => anyhow::bail!("attr: not implemented yet"),
        Command::Pdf { .. } => anyhow::bail!("pdf: not implemented yet"),
        Command::Js { .. } => anyhow::bail!("js: not implemented yet"),
        Command::Click { .. } => anyhow::bail!("click: not implemented yet"),
        Command::Input { .. } => anyhow::bail!("input: not implemented yet"),
        Command::Clear { .. } => anyhow::bail!("clear: not implemented yet"),
        Command::File { .. } => anyhow::bail!("file: not implemented yet"),
        Command::Download { .. } => anyhow::bail!("download: not implemented yet"),
        Command::Select { .. } => anyhow::bail!("select: not implemented yet"),
        Command::Submit { .. } => anyhow::bail!("submit: not implemented yet"),
        Command::Hover { .. } => anyhow::bail!("hover: not implemented yet"),
        Command::Focus { .. } => anyhow::bail!("focus: not implemented yet"),
        Command::Wait { .. } => anyhow::bail!("wait: not implemented yet"),
        Command::Waitload => anyhow::bail!("waitload: not implemented yet"),
        Command::Waitstable => anyhow::bail!("waitstable: not implemented yet"),
        Command::Waitidle => anyhow::bail!("waitidle: not implemented yet"),
        Command::Sleep { .. } => anyhow::bail!("sleep: not implemented yet"),
        Command::Screenshot(_) => anyhow::bail!("screenshot: not implemented yet"),
        Command::ScreenshotEl { .. } => anyhow::bail!("screenshot-el: not implemented yet"),
        Command::Pages => anyhow::bail!("pages: not implemented yet"),
        Command::Page { .. } => anyhow::bail!("page: not implemented yet"),
        Command::Newpage { .. } => anyhow::bail!("newpage: not implemented yet"),
    }
    Ok(())
}

pub fn parse_address(address: &str) -> Result<(String, u16)> {
    let (host, port) = address.split_once(':').ok_or_else(|| {
        crate::hint::hint_error(
            format!("invalid address `{address}`"),
            "use `<host>:<port>`, for example `127.0.0.1:9222`",
            None,
        )
    })?;
    if host.is_empty() || port.is_empty() || port.contains(':') {
        return Err(crate::hint::hint_error(
            format!("invalid address `{address}`"),
            "use `<host>:<port>`, for example `127.0.0.1:9222`",
            None,
        ));
    }
    let port = port.parse::<u16>().map_err(|_| {
        crate::hint::hint_error(
            format!("invalid port in address `{address}`"),
            "use a numeric TCP port, for example `127.0.0.1:9222`",
            None,
        )
    })?;
    Ok((host.to_string(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Command {
        Cli::try_parse_from(args).unwrap().command
    }

    #[test]
    fn parses_start_flags() {
        assert!(matches!(
            parse(&["rdny", "start", "--show"]),
            Command::Start(StartArgs {
                show: true,
                insecure: false
            })
        ));
        assert!(matches!(
            parse(&["rdny", "start", "-k"]),
            Command::Start(StartArgs {
                show: false,
                insecure: true
            })
        ));
        assert!(matches!(
            parse(&["rdny", "start", "--show", "--insecure"]),
            Command::Start(StartArgs {
                show: true,
                insecure: true
            })
        ));
        assert!(Cli::try_parse_from(["rdny", "start", "--bogus"]).is_err());
    }

    #[test]
    fn parses_required_commands() {
        assert!(
            matches!(parse(&["rdny", "connect", "127.0.0.1:9222"]), Command::Connect { address } if address == "127.0.0.1:9222")
        );
        assert!(
            matches!(parse(&["rdny", "sleep", "1.5"]), Command::Sleep { seconds } if seconds == 1.5)
        );
        assert!(
            matches!(parse(&["rdny", "attr", "a", "href"]), Command::Attr { selector, name } if selector == "a" && name == "href")
        );
        assert!(matches!(
            parse(&["rdny", "reload", "--hard"]),
            Command::Reload(ReloadArgs { hard: true })
        ));
        assert!(matches!(
            parse(&["rdny", "html"]),
            Command::Html { selector: None }
        ));
        assert!(
            matches!(parse(&["rdny", "html", "div"]), Command::Html { selector: Some(selector) } if selector == "div")
        );
        assert!(matches!(parse(&["rdny", "waitload"]), Command::Waitload));
        assert!(matches!(
            parse(&["rdny", "waitstable"]),
            Command::Waitstable
        ));
        assert!(matches!(parse(&["rdny", "waitidle"]), Command::Waitidle));
        assert!(matches!(
            parse(&["rdny", "page", "2"]),
            Command::Page { index: 2 }
        ));
        assert!(
            matches!(parse(&["rdny", "download", "a.link", "-"]), Command::Download { selector, file: Some(file) } if selector == "a.link" && file.as_os_str() == "-")
        );
    }

    #[test]
    fn parses_screenshot_height_short() {
        match parse(&["rdny", "screenshot", "-w", "1280", "-h", "720", "out.png"]) {
            Command::Screenshot(args) => {
                assert_eq!(args.width, Some(1280));
                assert_eq!(args.height, Some(720));
                assert_eq!(args.file, Some(PathBuf::from("out.png")));
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(matches!(
            parse(&["rdny", "screenshot"]),
            Command::Screenshot(_)
        ));
    }

    #[test]
    fn address_parsing() {
        assert_eq!(
            parse_address("127.0.0.1:9222").unwrap(),
            ("127.0.0.1".to_string(), 9222)
        );
        assert!(parse_address("9222").is_err());
        assert!(parse_address("foo:bar").is_err());
    }
}
