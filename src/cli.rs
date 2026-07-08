//! Command-line surface and dispatch.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
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
    /// List discovered rdny instances.
    List,
    /// Clean up stale instance state files.
    Cleanup(CleanupArgs),
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
    /// Manage cookies for the current browser session.
    Cookie(CookieArgs),
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
    /// Start collecting video frames from commands.
    StartVideo,
    /// Stop collecting frames and assemble a video file.
    StopVideo { file: Option<PathBuf> },
}

#[derive(Debug, Parser)]
pub struct StartArgs {
    /// Show a visible window instead of headless.
    #[arg(long)]
    pub show: bool,
    /// Ignore TLS certificate errors.
    #[arg(short = 'k', long)]
    pub insecure: bool,
    /// Human-readable label for this instance.
    #[arg(long)]
    pub label: Option<String>,
}

#[derive(Debug, Parser)]
pub struct CleanupArgs {
    /// Stop live instances before removing their state files.
    #[arg(long)]
    pub all: bool,
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
pub struct CookieArgs {
    #[command(subcommand)]
    pub command: CookieCommand,
}

#[derive(Debug, Subcommand)]
pub enum CookieCommand {
    /// Set a cookie by name and value.
    Set(CookieSetArgs),
    /// List cookies visible to the current page.
    List,
    /// Print a cookie value by name.
    Get { name: String },
    /// Delete a cookie by name, domain, and path.
    Delete(CookieDeleteArgs),
}

#[derive(Debug, Parser)]
pub struct CookieSetArgs {
    /// Cookie name.
    pub name: String,
    /// Cookie value.
    pub value: String,
    /// Cookie domain.
    #[arg(long)]
    pub domain: String,
    /// Cookie path.
    #[arg(long, default_value = "/")]
    pub path: String,
    /// Mark the cookie as secure.
    #[arg(long)]
    pub secure: bool,
    /// Mark the cookie as HTTP-only.
    #[arg(long = "http-only")]
    pub http_only: bool,
    /// SameSite policy: strict, lax, or none.
    #[arg(long = "same-site", value_enum, ignore_case = true)]
    pub same_site: Option<CookieSameSiteArg>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum CookieSameSiteArg {
    Strict,
    Lax,
    None,
}

impl From<CookieSameSiteArg> for commands::cookie::SameSite {
    fn from(value: CookieSameSiteArg) -> Self {
        match value {
            CookieSameSiteArg::Strict => Self::Strict,
            CookieSameSiteArg::Lax => Self::Lax,
            CookieSameSiteArg::None => Self::None,
        }
    }
}

#[derive(Debug, Parser)]
pub struct CookieDeleteArgs {
    /// Cookie name.
    pub name: String,
    /// Cookie domain.
    #[arg(long)]
    pub domain: String,
    /// Cookie path.
    #[arg(long, default_value = "/")]
    pub path: String,
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
    let recording_active = crate::state::load()?.is_some_and(|state| state.recording);
    let drain_after_dispatch =
        recording_active && !matches!(cli.command, Command::StopVideo { .. });
    let mut page_session = None;
    macro_rules! sess {
        () => {{
            if page_session.is_none() {
                page_session = Some(session::connect(cli.timeout)?);
            }
            page_session.as_mut().expect("session just connected")
        }};
    }
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
                label: args.label,
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
                    let label = state
                        .label
                        .as_deref()
                        .map(|label| format!(" label={label}"))
                        .unwrap_or_default();
                    println!(
                        "running: {browser} on {}:{} (pid {pid}){label}",
                        state.host, state.port
                    );
                }
                BrowserStatus::Stale => {
                    println!("stale: state file exists but browser is not responding");
                }
            },
        },
        Command::List => commands::instances::list()?,
        Command::Cleanup(args) => commands::instances::cleanup(args.all)?,
        Command::Open { url } => commands::nav::open(sess!(), &url)?,
        Command::Back => commands::nav::back(sess!())?,
        Command::Forward => commands::nav::forward(sess!())?,
        Command::Reload(args) => commands::nav::reload(sess!(), args.hard)?,
        Command::ClearCache => commands::nav::clear_cache(sess!())?,
        Command::Cookie(args) => match args.command {
            CookieCommand::Set(args) => commands::cookie::set(
                sess!(),
                &commands::cookie::SetCookie {
                    name: &args.name,
                    value: &args.value,
                    domain: &args.domain,
                    path: &args.path,
                    secure: args.secure,
                    http_only: args.http_only,
                    same_site: args.same_site.map(Into::into),
                },
            )?,
            CookieCommand::List => commands::cookie::list(sess!())?,
            CookieCommand::Get { name } => commands::cookie::get(sess!(), &name)?,
            CookieCommand::Delete(args) => {
                commands::cookie::delete(sess!(), &args.name, &args.domain, &args.path)?
            }
        },
        Command::Url => commands::pageinfo::url(sess!())?,
        Command::Title => commands::pageinfo::title(sess!())?,
        Command::Html { selector } => commands::pageinfo::html(sess!(), selector.as_deref())?,
        Command::Text { selector } => commands::pageinfo::text(sess!(), &selector)?,
        Command::Attr { selector, name } => commands::pageinfo::attr(sess!(), &selector, &name)?,
        Command::Pdf { file } => commands::pageinfo::pdf(sess!(), file.as_deref())?,
        Command::Js { expression } => {
            let stdin = std::io::stdin();
            let stdin_is_tty = stdin.is_terminal();
            let expression = resolve_js_expression(expression, stdin, stdin_is_tty)?;
            commands::interact::js(sess!(), &expression)?
        }
        Command::Logs(args) => commands::logs::logs(sess!(), args.follow)?,
        Command::Viewport(args) => commands::viewport::viewport(
            sess!(),
            args.width,
            args.height,
            args.scale,
            args.mobile,
            args.reset,
        )?,
        Command::Click { selector } => commands::interact::click(sess!(), &selector)?,
        Command::Input { selector, text } => commands::interact::input(sess!(), &selector, &text)?,
        Command::Clear { selector } => commands::interact::clear(sess!(), &selector)?,
        Command::File { selector, path } => commands::interact::file(sess!(), &selector, &path)?,
        Command::Download { selector, file } => {
            commands::interact::download(sess!(), &selector, file.as_deref())?
        }
        Command::Select { selector, value } => {
            commands::interact::select(sess!(), &selector, &value)?
        }
        Command::Submit { selector } => commands::interact::submit(sess!(), &selector)?,
        Command::Hover { selector } => commands::interact::hover(sess!(), &selector)?,
        Command::Focus { selector } => commands::interact::focus(sess!(), &selector)?,
        Command::Wait { selector } => commands::wait::wait(sess!(), &selector)?,
        Command::Waitload => commands::wait::waitload(sess!())?,
        Command::Waitstable => commands::wait::waitstable(sess!())?,
        Command::Waitidle => commands::wait::waitidle(sess!())?,
        Command::Sleep { seconds } => commands::wait::sleep(seconds)?,
        Command::Screenshot(args) => {
            let state = crate::state::require()?;
            commands::shot::screenshot(
                sess!(),
                args.width,
                args.height,
                args.file.as_deref(),
                state.viewport.as_ref(),
            )?
        }
        Command::ScreenshotEl { selector, file } => {
            commands::shot::screenshot_el(sess!(), &selector, file.as_deref())?
        }
        Command::Pages => commands::tabs::pages()?,
        Command::Page { index } => commands::tabs::page(index)?,
        Command::Newpage { url } => commands::tabs::newpage(url.as_deref())?,
        Command::StartVideo => commands::video::start()?,
        Command::StopVideo { file } => commands::video::stop(sess!(), file.as_deref())?,
    }
    if drain_after_dispatch && let Some(session) = page_session.as_mut() {
        session.drain_events(std::time::Duration::from_millis(300))?;
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
                insecure: false,
                label: None
            })
        ));
        assert!(matches!(
            parse(&["rdny", "start", "-k"]),
            Command::Start(StartArgs {
                show: false,
                insecure: true,
                label: None
            })
        ));
        assert!(matches!(
            parse(&["rdny", "start", "--show", "--insecure"]),
            Command::Start(StartArgs {
                show: true,
                insecure: true,
                label: None
            })
        ));
        assert!(matches!(
            parse(&["rdny", "start", "--label", "work"]),
            Command::Start(StartArgs {
                show: false,
                insecure: false,
                label: Some(label)
            }) if label == "work"
        ));
        assert!(Cli::try_parse_from(["rdny", "start", "--bogus"]).is_err());
    }

    #[test]
    fn parses_instance_commands() {
        assert!(matches!(parse(&["rdny", "list"]), Command::List));
        assert!(matches!(
            parse(&["rdny", "cleanup"]),
            Command::Cleanup(CleanupArgs { all: false })
        ));
        assert!(matches!(
            parse(&["rdny", "cleanup", "--all"]),
            Command::Cleanup(CleanupArgs { all: true })
        ));
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
            parse(&["rdny", "start-video"]),
            Command::StartVideo
        ));
        assert!(matches!(
            parse(&["rdny", "stop-video"]),
            Command::StopVideo { file: None }
        ));
        assert!(
            matches!(parse(&["rdny", "stop-video", "out.mp4"]), Command::StopVideo { file: Some(file) } if file.as_os_str() == "out.mp4")
        );
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
    fn parses_cookie_commands() {
        match parse(&[
            "rdny",
            "cookie",
            "set",
            "sid",
            "abc",
            "--domain",
            "example.com",
        ]) {
            Command::Cookie(CookieArgs {
                command: CookieCommand::Set(args),
            }) => {
                assert_eq!(args.name, "sid");
                assert_eq!(args.value, "abc");
                assert_eq!(args.domain, "example.com");
                assert_eq!(args.path, "/");
                assert!(!args.secure);
                assert!(!args.http_only);
                assert_eq!(args.same_site, None);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        assert!(matches!(
            parse(&["rdny", "cookie", "list"]),
            Command::Cookie(CookieArgs {
                command: CookieCommand::List
            })
        ));
        assert!(matches!(
            parse(&["rdny", "cookie", "get", "sid"]),
            Command::Cookie(CookieArgs {
                command: CookieCommand::Get { name }
            }) if name == "sid"
        ));
    }

    #[test]
    fn parses_cookie_set_all_flags() {
        match parse(&[
            "rdny",
            "cookie",
            "set",
            "sid",
            "abc",
            "--domain",
            "example.com",
            "--path",
            "/app",
            "--secure",
            "--http-only",
            "--same-site",
            "LaX",
        ]) {
            Command::Cookie(CookieArgs {
                command: CookieCommand::Set(args),
            }) => {
                assert_eq!(args.path, "/app");
                assert!(args.secure);
                assert!(args.http_only);
                assert_eq!(args.same_site, Some(CookieSameSiteArg::Lax));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn rejects_invalid_cookie_args() {
        assert!(Cli::try_parse_from(["rdny", "cookie", "set", "sid", "abc"]).is_err());
        assert!(Cli::try_parse_from(["rdny", "cookie", "delete", "sid"]).is_err());
        assert!(
            Cli::try_parse_from([
                "rdny",
                "cookie",
                "set",
                "sid",
                "abc",
                "--domain",
                "example.com",
                "--same-site",
                "invalid",
            ])
            .is_err()
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
