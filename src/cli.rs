//! Command-line surface and dispatch.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::io::{IsTerminal, Read};
use std::path::PathBuf;

use crate::browser::{self, BrowserStatus, LaunchOpts};
use crate::{commands, session};

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
    /// Evaluate JavaScript in the current page. Pass `-` or omit EXPRESSION to read stdin.
    Js {
        /// JavaScript expression to evaluate.
        expression: Option<String>,
    },
    /// Capture console and browser logs.
    Logs(LogsArgs),
    /// Print or set viewport/mobile emulation.
    Viewport(ViewportArgs),
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
pub struct LogsArgs {
    /// Keep streaming log events until interrupted.
    #[arg(long)]
    pub follow: bool,
}

#[derive(Debug, Parser)]
pub struct ViewportArgs {
    /// Viewport width.
    #[arg(conflicts_with = "reset", requires = "height")]
    pub width: Option<u32>,
    /// Viewport height.
    #[arg(conflicts_with = "reset", requires = "width")]
    pub height: Option<u32>,
    /// Device scale factor.
    #[arg(long, default_value_t = 1.0)]
    pub scale: f64,
    /// Enable mobile emulation.
    #[arg(long)]
    pub mobile: bool,
    /// Clear persisted viewport/mobile emulation.
    #[arg(long)]
    pub reset: bool,
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
        Command::Open { url } => commands::nav::open(&mut session::connect(cli.timeout)?, &url)?,
        Command::Back => commands::nav::back(&mut session::connect(cli.timeout)?)?,
        Command::Forward => commands::nav::forward(&mut session::connect(cli.timeout)?)?,
        Command::Reload(args) => {
            commands::nav::reload(&mut session::connect(cli.timeout)?, args.hard)?
        }
        Command::ClearCache => commands::nav::clear_cache(&mut session::connect(cli.timeout)?)?,
        Command::Url => commands::pageinfo::url(&mut session::connect(cli.timeout)?)?,
        Command::Title => commands::pageinfo::title(&mut session::connect(cli.timeout)?)?,
        Command::Html { selector } => {
            commands::pageinfo::html(&mut session::connect(cli.timeout)?, selector.as_deref())?
        }
        Command::Text { selector } => {
            commands::pageinfo::text(&mut session::connect(cli.timeout)?, &selector)?
        }
        Command::Attr { selector, name } => {
            commands::pageinfo::attr(&mut session::connect(cli.timeout)?, &selector, &name)?
        }
        Command::Pdf { file } => {
            commands::pageinfo::pdf(&mut session::connect(cli.timeout)?, file.as_deref())?
        }
        Command::Js { expression } => {
            let stdin = std::io::stdin();
            let stdin_is_tty = stdin.is_terminal();
            let expression = resolve_js_expression(expression, stdin, stdin_is_tty)?;
            commands::interact::js(&mut session::connect(cli.timeout)?, &expression)?
        }
        Command::Logs(args) => {
            commands::logs::logs(&mut session::connect(cli.timeout)?, args.follow)?
        }
        Command::Viewport(args) => commands::viewport::viewport(
            &mut session::connect(cli.timeout)?,
            args.width,
            args.height,
            args.scale,
            args.mobile,
            args.reset,
        )?,
        Command::Click { selector } => {
            commands::interact::click(&mut session::connect(cli.timeout)?, &selector)?
        }
        Command::Input { selector, text } => {
            commands::interact::input(&mut session::connect(cli.timeout)?, &selector, &text)?
        }
        Command::Clear { selector } => {
            commands::interact::clear(&mut session::connect(cli.timeout)?, &selector)?
        }
        Command::File { selector, path } => {
            commands::interact::file(&mut session::connect(cli.timeout)?, &selector, &path)?
        }
        Command::Download { selector, file } => commands::interact::download(
            &mut session::connect(cli.timeout)?,
            &selector,
            file.as_deref(),
        )?,
        Command::Select { selector, value } => {
            commands::interact::select(&mut session::connect(cli.timeout)?, &selector, &value)?
        }
        Command::Submit { selector } => {
            commands::interact::submit(&mut session::connect(cli.timeout)?, &selector)?
        }
        Command::Hover { selector } => {
            commands::interact::hover(&mut session::connect(cli.timeout)?, &selector)?
        }
        Command::Focus { selector } => {
            commands::interact::focus(&mut session::connect(cli.timeout)?, &selector)?
        }
        Command::Wait { selector } => {
            commands::wait::wait(&mut session::connect(cli.timeout)?, &selector)?
        }
        Command::Waitload => commands::wait::waitload(&mut session::connect(cli.timeout)?)?,
        Command::Waitstable => commands::wait::waitstable(&mut session::connect(cli.timeout)?)?,
        Command::Waitidle => commands::wait::waitidle(&mut session::connect(cli.timeout)?)?,
        Command::Sleep { seconds } => commands::wait::sleep(seconds)?,
        Command::Screenshot(args) => {
            let state = crate::state::require()?;
            commands::shot::screenshot(
                &mut session::connect(cli.timeout)?,
                args.width,
                args.height,
                args.file.as_deref(),
                state.viewport.as_ref(),
            )?
        }
        Command::ScreenshotEl { selector, file } => commands::shot::screenshot_el(
            &mut session::connect(cli.timeout)?,
            &selector,
            file.as_deref(),
        )?,
        Command::Pages => commands::tabs::pages()?,
        Command::Page { index } => commands::tabs::page(index)?,
        Command::Newpage { url } => commands::tabs::newpage(url.as_deref())?,
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

fn resolve_js_expression(
    expression: Option<String>,
    reader: impl Read,
    stdin_is_tty: bool,
) -> Result<String> {
    match expression {
        Some(expression) if expression == "-" => read_js_expression_from_stdin(reader),
        Some(expression) => Ok(expression),
        None if stdin_is_tty => Err(crate::hint::hint_error(
            "missing JavaScript expression",
            "pass an expression or pipe one on stdin",
            None,
        )),
        None => read_js_expression_from_stdin(reader),
    }
}

fn read_js_expression_from_stdin(reader: impl Read) -> Result<String> {
    std::io::read_to_string(reader).context("failed to read JavaScript expression from stdin")
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
        assert!(matches!(
            parse(&["rdny", "js"]),
            Command::Js { expression: None }
        ));
        assert!(
            matches!(parse(&["rdny", "js", "1 + 1"]), Command::Js { expression: Some(expression) } if expression == "1 + 1")
        );
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
            parse(&["rdny", "logs", "--follow"]),
            Command::Logs(LogsArgs { follow: true })
        ));
        assert!(matches!(
            parse(&["rdny", "viewport"]),
            Command::Viewport(ViewportArgs {
                width: None,
                height: None,
                scale: 1.0,
                mobile: false,
                reset: false,
            })
        ));
        assert!(matches!(
            parse(&["rdny", "page", "2"]),
            Command::Page { index: 2 }
        ));
        assert!(
            matches!(parse(&["rdny", "download", "a.link", "-"]), Command::Download { selector, file: Some(file) } if selector == "a.link" && file.as_os_str() == "-")
        );
    }

    #[test]
    fn parses_viewport_args() {
        assert!(matches!(
            parse(&["rdny", "viewport", "375", "812"]),
            Command::Viewport(ViewportArgs {
                width: Some(375),
                height: Some(812),
                scale: 1.0,
                mobile: false,
                reset: false,
            })
        ));
        assert!(matches!(
            parse(&["rdny", "viewport", "375", "812", "--scale", "2", "--mobile"]),
            Command::Viewport(ViewportArgs {
                width: Some(375),
                height: Some(812),
                scale: 2.0,
                mobile: true,
                reset: false,
            })
        ));
        assert!(matches!(
            parse(&["rdny", "viewport", "--reset"]),
            Command::Viewport(ViewportArgs {
                width: None,
                height: None,
                scale: 1.0,
                mobile: false,
                reset: true,
            })
        ));
        assert!(Cli::try_parse_from(["rdny", "viewport", "375"]).is_err());
        assert!(Cli::try_parse_from(["rdny", "viewport", "375", "812", "--reset"]).is_err());
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

    #[test]
    fn resolves_js_expression_from_argument() {
        let expression = resolve_js_expression(
            Some("document.title".to_string()),
            "ignored".as_bytes(),
            true,
        )
        .unwrap();

        assert_eq!(expression, "document.title");
    }

    #[test]
    fn resolves_js_expression_from_dash_stdin() {
        let expression =
            resolve_js_expression(Some("-".to_string()), "a\nb\n".as_bytes(), true).unwrap();

        assert_eq!(expression, "a\nb\n");
    }

    #[test]
    fn resolves_js_expression_from_omitted_non_tty_stdin() {
        let expression =
            resolve_js_expression(None, "(() => {\n  return 42;\n})()".as_bytes(), false).unwrap();

        assert_eq!(expression, "(() => {\n  return 42;\n})()");
    }

    #[test]
    fn rejects_omitted_js_expression_on_tty() {
        let err = resolve_js_expression(None, "ignored".as_bytes(), true).unwrap_err();

        assert!(format!("{err}").contains("pass an expression or pipe one on stdin"));
    }
}
