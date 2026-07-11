//! Command-line surface and dispatch.

use anyhow::{Context, Result};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use serde_json::json;
use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use crate::browser::{self, BrowserStatus, LaunchOpts};
use crate::{commands, config, session};

/// Chrome automation from the command line.
#[derive(Debug, Parser)]
#[command(
    name = "rdny",
    version,
    about = "Chrome automation from the command line"
)]
pub struct Cli {
    /// Seconds to wait for slow operations before giving up.
    #[arg(long, global = true, default_value_t = BoundedDuration::default())]
    pub timeout: BoundedDuration,

    /// State directory to use, equivalent to RDNY_STATE_DIR and taking precedence over it.
    #[arg(long, global = true)]
    pub state_dir: Option<PathBuf>,

    /// Output format for supported commands (schemaVersion 1 for structured output).
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Human)]
    pub format: OutputFormat,

    #[command(subcommand)]
    pub command: Command,
}

/// A user-supplied duration which is safe to convert to `Duration`.
///
/// One day is intentionally generous for interactive automation while still
/// preventing accidental effectively-unbounded commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoundedDuration(Duration);

impl BoundedDuration {
    pub const MIN: Duration = Duration::from_millis(1);
    pub const MAX: Duration = Duration::from_secs(24 * 60 * 60);

    pub fn get(self) -> Duration {
        self.0
    }
}

impl Default for BoundedDuration {
    fn default() -> Self {
        Self(Duration::from_secs(30))
    }
}

impl std::fmt::Display for BoundedDuration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0.as_secs_f64())
    }
}

impl FromStr for BoundedDuration {
    type Err = String;

    fn from_str(raw: &str) -> std::result::Result<Self, Self::Err> {
        let seconds: f64 = raw
            .parse()
            .map_err(|_| "duration must be a number of seconds".to_string())?;
        if !seconds.is_finite() || seconds < Self::MIN.as_secs_f64() {
            return Err(format!(
                "duration must be finite and at least {} seconds",
                Self::MIN.as_secs_f64()
            ));
        }
        if seconds > Self::MAX.as_secs_f64() {
            return Err(format!(
                "duration must not exceed {} seconds",
                Self::MAX.as_secs()
            ));
        }
        Ok(Self(Duration::from_secs_f64(seconds)))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Human,
    Json,
    Jsonl,
}

impl OutputFormat {
    fn emit(self, value: &serde_json::Value) -> Result<()> {
        match self {
            Self::Human => println!("{}", crate::commands::human_sanitize(&value.to_string())),
            Self::Json => println!("{}", serde_json::to_string_pretty(value)?),
            Self::Jsonl => println!("{}", serde_json::to_string(value)?),
        }
        Ok(())
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start a new browser session.
    Start(StartArgs),
    /// Connect to an existing browser debugger endpoint.
    ///
    /// ADDRESS is `<host>:<port>`, or a named target from
    /// `[connect.targets]` in the rdny config file. Omit it to use the
    /// `[connect] default` target. The browser must have been launched
    /// with `--remote-debugging-port=<port>`; on macOS, for example:
    /// `open -na Helium --args --remote-debugging-port=9333`. See the
    /// README section "Driving your own browser".
    Connect {
        /// `<host>:<port>`, or a named `[connect.targets]` entry;
        /// omitted: the configured `[connect] default`
        address: Option<String>,
        /// Acknowledge remote-CDP risk. Direct plaintext remote transport is
        /// still rejected; use a verified SSH tunnel to a loopback address.
        #[arg(long)]
        allow_remote: bool,
    },
    /// Stop the current browser session.
    Stop,
    /// Show current browser session status.
    Status(StatusArgs),
    /// List discovered rdny instances.
    List,
    /// Generate shell completion script for a supported shell.
    Completion { shell: clap_complete::Shell },
    /// Clean up stale instance state files.
    Cleanup(CleanupArgs),
    /// Open a URL in the current page.
    Open {
        url: String,
        #[command(flatten)]
        policy: UrlPolicyArgs,
    },
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
    Pdf(ArtifactArgs),
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
    Input(InputArgs),
    /// Clear an element's value.
    Clear { selector: String },
    /// Upload a file to an input element.
    File { selector: String, path: PathBuf },
    /// Click and download a linked resource.
    Download {
        /// Link or element selector to click.
        selector: String,
        /// Maximum accepted payload bytes (default 268435456; env RDNY_MAX_DOWNLOAD_BYTES).
        #[arg(long)]
        max_bytes: Option<u64>,
        /// Replace an existing output file.
        #[arg(long)]
        force: bool,
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
    Waitstable(WaitQuietArgs),
    /// Wait for browser idleness.
    Waitidle(WaitQuietArgs),
    /// Sleep for a number of seconds.
    Sleep { seconds: BoundedDuration },
    /// Capture a screenshot.
    Screenshot(ScreenshotArgs),
    /// Capture an element screenshot.
    ScreenshotEl {
        selector: String,
        /// Replace an existing output file.
        #[arg(long)]
        force: bool,
        file: Option<PathBuf>,
    },
    /// List open pages.
    Pages,
    /// Switch to a page by index.
    Page { index: usize },
    /// Open a new page.
    Newpage {
        url: Option<String>,
        #[command(flatten)]
        policy: UrlPolicyArgs,
    },
    /// Start collecting video frames from commands.
    StartVideo,
    /// Stop collecting frames and assemble a video file.
    StopVideo(ArtifactArgs),
}

/// Opt-ins for navigation targets that can expose local browser privileges.
#[derive(Debug, Clone, Default, Args)]
pub struct UrlPolicyArgs {
    /// Permit file: URLs.
    #[arg(long)]
    allow_file_url: bool,
    /// Permit data: URLs.
    #[arg(long)]
    allow_data_url: bool,
    /// Permit privileged chrome: browser-internal URLs.
    #[arg(long)]
    allow_chrome_url: bool,
    /// Permit chrome-extension: URLs for installed 32-character extension IDs.
    #[arg(long)]
    allow_chrome_extension_url: bool,
    /// Permit RFC1918/ULA literal addresses (loopback stays enabled).
    #[arg(long)]
    allow_private_url: bool,
    /// Permit link-local literal addresses.
    #[arg(long)]
    allow_link_local_url: bool,
    /// Permit mDNS/local hostnames such as printer.local.
    #[arg(long)]
    allow_local_url: bool,
}

impl From<&UrlPolicyArgs> for commands::nav::UrlPolicy {
    fn from(args: &UrlPolicyArgs) -> Self {
        Self {
            allow_file: args.allow_file_url,
            allow_data: args.allow_data_url,
            allow_chrome: args.allow_chrome_url,
            allow_chrome_extension: args.allow_chrome_extension_url,
            allow_private: args.allow_private_url,
            allow_link_local: args.allow_link_local_url,
            allow_local: args.allow_local_url,
        }
    }
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
pub struct WaitQuietArgs {
    /// Quiet-window duration in milliseconds before the wait succeeds.
    #[arg(long, default_value_t = 500, value_parser = parse_quiet_ms)]
    pub quiet_ms: u64,
}

fn parse_quiet_ms(raw: &str) -> std::result::Result<u64, String> {
    let value: u64 = raw
        .parse()
        .map_err(|_| "quiet-ms must be an integer number of milliseconds".to_string())?;
    if (1..=60_000).contains(&value) {
        Ok(value)
    } else {
        Err("quiet-ms must be between 1 and 60000".to_string())
    }
}

#[derive(Debug, Parser)]
pub struct CleanupArgs {
    /// Stop live instances before removing their state files.
    #[arg(long)]
    pub all: bool,
}

#[derive(Debug, Parser)]
pub struct StatusArgs {
    /// Exit 0 only for a healthy running/reachable session, 1 otherwise.
    #[arg(long)]
    pub check: bool,
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
    /// Capture duration in seconds when not following (default: global --timeout).
    #[arg(long)]
    pub duration: Option<BoundedDuration>,
}

#[derive(Debug, Parser)]
pub struct ViewportArgs {
    /// Viewport width.
    #[arg(conflicts_with = "reset", requires = "height", value_parser = parse_dimension)]
    pub width: Option<u32>,
    /// Viewport height.
    #[arg(conflicts_with = "reset", requires = "width", value_parser = parse_dimension)]
    pub height: Option<u32>,
    /// Device scale factor.
    #[arg(long, default_value_t = 1.0, value_parser = parse_scale)]
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
    /// Cookie value. Prefer --value-stdin/--value-file/--value-fd for secrets.
    pub value: Option<String>,
    /// Read cookie value from stdin to avoid argv leakage.
    #[arg(long, conflicts_with_all = ["value", "value_file", "value_fd"])]
    pub value_stdin: bool,
    /// Read cookie value from this file to avoid argv leakage.
    #[arg(long = "value-file", conflicts_with_all = ["value", "value_stdin", "value_fd"])]
    pub value_file: Option<PathBuf>,
    /// Read cookie value from this file descriptor to avoid argv leakage.
    #[arg(long = "value-fd", conflicts_with_all = ["value", "value_stdin", "value_file"])]
    pub value_fd: Option<i32>,
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

#[derive(Debug, Parser)]
pub struct InputArgs {
    /// CSS selector.
    pub selector: String,
    /// Text to type. Prefer --text-stdin/--text-file/--text-fd for secrets.
    pub text: Option<String>,
    /// Read input text from stdin to avoid argv leakage.
    #[arg(long, conflicts_with_all = ["text", "text_file", "text_fd"])]
    pub text_stdin: bool,
    /// Read input text from this file to avoid argv leakage.
    #[arg(long = "text-file", conflicts_with_all = ["text", "text_stdin", "text_fd"])]
    pub text_file: Option<PathBuf>,
    /// Read input text from this file descriptor to avoid argv leakage.
    #[arg(long = "text-fd", conflicts_with_all = ["text", "text_stdin", "text_file"])]
    pub text_fd: Option<i32>,
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
pub struct ScreenshotArgs {
    /// Screenshot width.
    #[arg(short = 'w', value_parser = parse_dimension)]
    pub width: Option<u32>,
    /// Screenshot height. Short -H replaces legacy -h so -h shows help.
    #[arg(short = 'H', long, alias = "legacy-height", value_parser = parse_dimension)]
    pub height: Option<u32>,
    /// Replace an existing output file.
    #[arg(long)]
    pub force: bool,
    /// Output file.
    pub file: Option<PathBuf>,
}

const MAX_DIMENSION: u32 = 16_384;
const MIN_SCALE: f64 = 0.1;
const MAX_SCALE: f64 = 10.0;

fn parse_dimension(raw: &str) -> std::result::Result<u32, String> {
    let value: u32 = raw
        .parse()
        .map_err(|_| "dimension must be a positive integer".to_string())?;
    if (1..=MAX_DIMENSION).contains(&value) {
        Ok(value)
    } else {
        Err(format!("dimension must be between 1 and {MAX_DIMENSION}"))
    }
}

fn parse_scale(raw: &str) -> std::result::Result<f64, String> {
    let value: f64 = raw
        .parse()
        .map_err(|_| "scale must be a number".to_string())?;
    if value.is_finite() && (MIN_SCALE..=MAX_SCALE).contains(&value) {
        Ok(value)
    } else {
        Err(format!(
            "scale must be finite and between {MIN_SCALE} and {MAX_SCALE}"
        ))
    }
}

#[derive(Debug, Parser)]
pub struct ArtifactArgs {
    /// Replace an existing output file.
    #[arg(long)]
    pub force: bool,
    /// Output file.
    pub file: Option<PathBuf>,
}

/// Parse argv and execute the selected command.
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let timeout = cli.timeout.get();
    let command_budget = match &cli.command {
        Command::Logs(args) if !args.follow => args.duration.unwrap_or(cli.timeout).get(),
        Command::Sleep { seconds } => seconds.get(),
        _ => timeout,
    };
    let deadline = session::Deadline::after(command_budget);
    if let Some(state_dir) = &cli.state_dir {
        // SAFETY: rdny is still single-threaded here, before any command dispatch or
        // background work, so mutating the process environment cannot race other threads.
        unsafe { std::env::set_var("RDNY_STATE_DIR", state_dir) };
    }
    // Avoid unconditional lifecycle state reads before lifecycle commands have
    // taken .lifecycle.lock. Page commands may opt into draining once connected.
    let mut drain_after_dispatch = false;
    let mut page_session = None;
    macro_rules! sess {
        () => {{
            if page_session.is_none() {
                page_session = Some(session::connect(deadline, timeout)?);
                drain_after_dispatch = page_session
                    .as_ref()
                    .is_some_and(session::PageSession::is_recording);
            }
            page_session.as_mut().expect("session just connected")
        }};
    }
    match cli.command {
        Command::Start(args) => {
            let _lifecycle = crate::state::lifecycle_lock()?;
            refuse_occupied_lifecycle(&_lifecycle, "start", deadline)?;
            let opts = LaunchOpts {
                show: args.show,
                insecure: args.insecure,
                extra_args: vec![],
                label: args.label,
            };
            let instance_id = crate::state::new_instance_id();
            let mut launched = crate::broker::launch_armed(
                &opts,
                crate::state::browser_storage()?,
                instance_id,
                deadline,
            )?;
            #[cfg(debug_assertions)]
            if let Some(marker) = std::env::var_os("RDNY_TEST_CRASH_BEFORE_BROKER_COMMIT") {
                // Test-only abrupt client death: do not run destructors, so the
                // broker must observe startup-socket EOF and clean itself up.
                let endpoint = launched.state.endpoint.as_ref().expect("managed endpoint");
                let evidence = json!({
                    "broker": endpoint.broker_pid,
                    "browser": launched.state.pid,
                    "socket": endpoint.socket,
                });
                std::fs::write(marker, serde_json::to_vec(&evidence)?)?;
                unsafe { libc::_exit(86) }
            }
            let publish_state = launched.state.clone();
            if let Err(err) =
                crate::state::replace_lifecycle_and_commit(&_lifecycle, &publish_state, || {
                    launched.commit()
                })
            {
                return Err(err.context("publishing and committing managed broker ownership; browser was terminated and previous state preserved"));
            }
            let state = launched.state.clone();
            let browser = state
                .browser_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "browser".to_string());
            let pid = state
                .pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "attached".to_string());
            println!("started {browser} pid {pid} via authenticated local broker");
        }
        Command::Connect {
            address,
            allow_remote,
        } => {
            let _lifecycle = crate::state::lifecycle_lock()?;
            refuse_occupied_lifecycle(&_lifecycle, "connect", deadline)?;
            let config = config::load()?;
            let (host, port) = resolve_connect_target(address.as_deref(), &config)?;
            let state = browser::connect_with_policy_until(&host, port, allow_remote, deadline)?;
            crate::state::replace_lifecycle(&_lifecycle, &state)
                .context("publishing attached browser ownership; previous state preserved")?;
            println!("connected to {host}:{port}");
        }
        Command::Stop => {
            let _lifecycle = crate::state::lifecycle_lock()?;
            let (state, generation) = match crate::state::inspect_for_lifecycle(&_lifecycle)? {
                crate::state::Inspection::Valid(state, generation) => (state, generation),
                crate::state::Inspection::Missing => {
                    return Err(crate::state::require().unwrap_err());
                }
                crate::state::Inspection::Malformed(m) => anyhow::bail!(
                    "state file contains malformed JSON: {}; not stopping or clearing; quarantine or remove it explicitly",
                    m.message()
                ),
                crate::state::Inspection::Incompatible(e, _) => anyhow::bail!(
                    "state file has incompatible schema: {e}; not stopping or clearing"
                ),
            };
            let observed_id = state.instance_id.clone();
            let outcome = browser::stop_until(&state, deadline)?;
            if !crate::state::clear_observed_lifecycle(
                &_lifecycle,
                observed_id.as_deref(),
                &generation,
            )? {
                anyhow::bail!(
                    "state changed while stopping; preserved concurrent replacement and did not clear it"
                )
            }
            match outcome {
                browser::StopOutcome::Stopped => println!("stopped"),
                browser::StopOutcome::Detached => println!("detached (browser left running)"),
            }
        }
        Command::Status(args) => match crate::state::load()? {
            None => {
                if cli.format == OutputFormat::Human {
                    println!("no session")
                } else {
                    cli.format.emit(&json!({"schemaVersion":1,"kind":"status","status":"missing","healthy":false}))?;
                }
                if args.check {
                    std::process::exit(1);
                }
            }
            Some(state) => match browser::status_until(&state, deadline)? {
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
                    if cli.format == OutputFormat::Human {
                        if state.endpoint.is_some() {
                            println!(
                                "running: {browser} via authenticated local broker (pid {pid}){label}"
                            );
                        } else {
                            println!(
                                "running: {browser} on {}:{} (pid {pid}){label}",
                                state.host, state.port
                            );
                        }
                    } else {
                        cli.format.emit(&json!({"schemaVersion":1,"kind":"status","status":"running","healthy":true,"browser":browser,"host":state.host,"port":state.port,"pid":state.pid,"instance":state.instance_id,"target":state.target_id,"label":state.label}))?;
                    }
                }
                BrowserStatus::Stale => {
                    if cli.format == OutputFormat::Human {
                        println!("stale: state file exists but browser is not responding");
                    } else {
                        cli.format.emit(&json!({"schemaVersion":1,"kind":"status","status":"stale","healthy":false,"host":state.host,"port":state.port,"pid":state.pid,"instance":state.instance_id,"target":state.target_id}))?;
                    }
                    if args.check {
                        std::process::exit(1);
                    }
                }
            },
        },
        Command::List => {
            commands::instances::list_format_until(cli.format != OutputFormat::Human, deadline)?
        }
        Command::Completion { shell } => {
            let mut cmd = Cli::command();
            clap_complete::generate(shell, &mut cmd, "rdny", &mut std::io::stdout());
        }
        Command::Cleanup(args) => commands::instances::cleanup_until(args.all, deadline)?,
        Command::Open { url, policy } => {
            let policy = (&policy).into();
            // Validate before `sess!()` connects so rejected input causes no browser I/O.
            commands::nav::normalize_url(&url, &policy)?;
            let opened = commands::nav::open_with_policy(sess!(), &url, &policy)?;
            if cli.format == OutputFormat::Human {
                println!("{}", opened.human_summary());
            }
        }
        Command::Back => commands::nav::back(sess!())?,
        Command::Forward => commands::nav::forward(sess!())?,
        Command::Reload(args) => commands::nav::reload(sess!(), args.hard)?,
        Command::ClearCache => commands::nav::clear_cache(sess!())?,
        Command::Cookie(args) => match args.command {
            CookieCommand::Set(args) => {
                let value = resolve_secret(
                    args.value.as_deref(),
                    args.value_stdin,
                    args.value_file.as_deref(),
                    args.value_fd,
                    "cookie value",
                )?;
                commands::cookie::set(
                    sess!(),
                    &commands::cookie::SetCookie {
                        name: &args.name,
                        value: &value,
                        domain: &args.domain,
                        path: &args.path,
                        secure: args.secure,
                        http_only: args.http_only,
                        same_site: args.same_site.map(Into::into),
                    },
                )?
            }
            CookieCommand::List => {
                commands::cookie::list_format(sess!(), cli.format != OutputFormat::Human)?
            }
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
        Command::Pdf(args) => commands::pageinfo::pdf(sess!(), args.file.as_deref(), args.force)?,
        Command::Js { expression } => {
            let stdin = std::io::stdin();
            let stdin_is_tty = stdin.is_terminal();
            let expression = resolve_js_expression(expression, stdin, stdin_is_tty)?;
            commands::interact::js(sess!(), &expression)?
        }
        Command::Logs(args) => {
            commands::logs::logs_format(sess!(), args.follow, cli.format != OutputFormat::Human)?
        }
        Command::Viewport(args) => commands::viewport::viewport_format(
            sess!(),
            args.width,
            args.height,
            args.scale,
            args.mobile,
            args.reset,
            cli.format != OutputFormat::Human,
        )?,
        Command::Click { selector } => commands::interact::click(sess!(), &selector)?,
        Command::Input(args) => {
            let text = resolve_secret(
                args.text.as_deref(),
                args.text_stdin,
                args.text_file.as_deref(),
                args.text_fd,
                "input text",
            )?;
            commands::interact::input(sess!(), &args.selector, &text)?
        }
        Command::Clear { selector } => commands::interact::clear(sess!(), &selector)?,
        Command::File { selector, path } => commands::interact::file(sess!(), &selector, &path)?,
        Command::Download {
            selector,
            max_bytes,
            force,
            file,
        } => commands::interact::download(sess!(), &selector, file.as_deref(), force, max_bytes)?,
        Command::Select { selector, value } => {
            commands::interact::select(sess!(), &selector, &value)?
        }
        Command::Submit { selector } => commands::interact::submit(sess!(), &selector)?,
        Command::Hover { selector } => commands::interact::hover(sess!(), &selector)?,
        Command::Focus { selector } => commands::interact::focus(sess!(), &selector)?,
        Command::Wait { selector } => commands::wait::wait(sess!(), &selector)?,
        Command::Waitload => commands::wait::waitload(sess!())?,
        Command::Waitstable(args) => {
            if args.quiet_ms == 500 {
                commands::wait::waitstable(sess!())?
            } else {
                commands::wait::waitstable_quiet(
                    sess!(),
                    std::time::Duration::from_millis(args.quiet_ms),
                )?
            }
        }
        Command::Waitidle(args) => {
            if args.quiet_ms == 500 {
                commands::wait::waitidle(sess!())?
            } else {
                commands::wait::waitidle_quiet(
                    sess!(),
                    std::time::Duration::from_millis(args.quiet_ms),
                )?
            }
        }
        Command::Sleep { .. } => commands::wait::sleep(deadline)?,
        Command::Screenshot(args) => {
            let state = crate::state::require()?;
            commands::shot::screenshot(
                sess!(),
                args.width,
                args.height,
                args.file.as_deref(),
                args.force,
                state.viewport.as_ref(),
            )?
        }
        Command::ScreenshotEl {
            selector,
            force,
            file,
        } => commands::shot::screenshot_el(sess!(), &selector, file.as_deref(), force)?,
        Command::Pages => {
            commands::tabs::pages_format_until(cli.format != OutputFormat::Human, deadline)?
        }
        Command::Page { index } => commands::tabs::page_until(index, deadline)?,
        Command::Newpage { url, policy } => commands::tabs::newpage_with_policy(
            url.as_deref(),
            timeout,
            deadline,
            &(&policy).into(),
        )?,
        Command::StartVideo => commands::video::start()?,
        Command::StopVideo(args) => {
            // Assemble even when the browser is gone: frames on disk
            // should never be stranded behind a dead session.
            let mut live = session::connect(deadline, timeout).ok();
            commands::video::stop(live.as_mut(), args.file.as_deref(), args.force)?
        }
    }
    if drain_after_dispatch && let Some(session) = page_session.as_mut() {
        session.drain_events(std::time::Duration::from_millis(300))?;
    }
    Ok(())
}

fn refuse_occupied_lifecycle(
    lock: &crate::state::LifecycleLock,
    action: &str,
    deadline: session::Deadline,
) -> Result<()> {
    match crate::state::inspect_for_lifecycle(lock)? {
        crate::state::Inspection::Missing => Ok(()),
        crate::state::Inspection::Malformed(m) => anyhow::bail!(
            "refusing to {action}: state file contains malformed JSON: {}; run `rdny cleanup` or remove/quarantine the corrupt state after verifying no browser owns it",
            m.message()
        ),
        crate::state::Inspection::Incompatible(e, _) => anyhow::bail!(
            "refusing to {action}: state file has incompatible schema: {e}; upgrade rdny or inspect/remove the state explicitly"
        ),
        crate::state::Inspection::Valid(state, _) => {
            if state.endpoint.is_some() {
                if crate::broker::ping(&state, deadline).is_ok() {
                    return Err(crate::hint::hint_error(
                        format!(
                            "refusing to {action}: live managed broker already owns this state dir"
                        ),
                        "run `rdny stop` first, or use a different --state-dir",
                        None,
                    ));
                }
                return Ok(());
            }
            if let Some(id) = &state.process_identity {
                crate::process_identity::validate_persisted(
                    state.pid,
                    state.browser_path.as_deref(),
                    state.user_data_dir.as_deref(),
                    id,
                )
                .with_context(|| {
                    format!("refusing to {action}: existing managed state identity is inconsistent")
                })?;
            }
            let reachable =
                crate::cdp::http::version_until(&state.host, state.port, deadline.instant())
                    .is_ok();
            match crate::process_identity::classify(
                state.pid,
                state.process_identity.as_ref(),
                reachable,
            ) {
                crate::process_identity::ProcessClass::ManagedMatching
                | crate::process_identity::ProcessClass::AttachedReachable
                | crate::process_identity::ProcessClass::LegacyUnverifiable => {
                    let owner = state
                        .pid
                        .map(|pid| format!("managed pid {pid}"))
                        .unwrap_or_else(|| "attached browser".to_string());
                    Err(crate::hint::hint_error(
                        format!(
                            "refusing to {action}: live {owner} already owns state dir on port {}",
                            state.port
                        ),
                        "run `rdny stop` first, or use a different --state-dir",
                        None,
                    ))
                }
                crate::process_identity::ProcessClass::ManagedDead
                | crate::process_identity::ProcessClass::AttachedDead
                | crate::process_identity::ProcessClass::PidReusedOrUnrelated => Ok(()),
            }
        }
    }
}

fn resolve_secret(
    value: Option<&str>,
    stdin: bool,
    file: Option<&std::path::Path>,
    fd: Option<i32>,
    label: &str,
) -> Result<String> {
    if let Some(value) = value {
        return Ok(value.to_string());
    }
    if stdin {
        let mut out = String::new();
        std::io::stdin()
            .read_to_string(&mut out)
            .with_context(|| format!("reading {label} from stdin"))?;
        return Ok(trim_one_trailing_newline(out));
    }
    if let Some(file) = file {
        return Ok(trim_one_trailing_newline(
            std::fs::read_to_string(file)
                .with_context(|| format!("reading {label} from {}", file.display()))?,
        ));
    }
    if let Some(fd) = fd {
        if fd < 0 {
            anyhow::bail!("invalid fd {fd} for {label}");
        }
        let path = std::path::PathBuf::from(format!("/dev/fd/{fd}"));
        return Ok(trim_one_trailing_newline(
            std::fs::read_to_string(&path)
                .with_context(|| format!("reading {label} from fd {fd}"))?,
        ));
    }
    anyhow::bail!(
        "missing {label}; pass a value or --{}-stdin/--{}-file/--{}-fd",
        label.replace(' ', "-"),
        label.replace(' ', "-"),
        label.replace(' ', "-")
    )
}

fn trim_one_trailing_newline(mut s: String) -> String {
    if s.ends_with('\n') {
        s.pop();
        if s.ends_with('\r') {
            s.pop();
        }
    }
    s
}

pub fn parse_address(address: &str) -> Result<(String, u16)> {
    if let Ok(socket) = address.parse::<std::net::SocketAddr>() {
        return Ok((socket.ip().to_string(), socket.port()));
    }
    let (host, port) = address.rsplit_once(':').ok_or_else(|| {
        crate::hint::hint_error(
            format!("invalid address `{address}`"),
            "use `<host>:<port>`, for example `127.0.0.1:9222`",
            None,
        )
    })?;
    if host.is_empty() || port.is_empty() || host.contains(':') {
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

pub fn resolve_connect_target(arg: Option<&str>, config: &config::Config) -> Result<(String, u16)> {
    let Some(target) = arg else {
        let default = config
            .connect
            .as_ref()
            .and_then(|connect| connect.default.as_deref())
            .ok_or_else(|| {
                crate::hint::hint_error(
                    "missing connect target",
                    "pass `<host>:<port>` or configure [connect] default in the rdny config file",
                    None,
                )
            })?;
        return resolve_named_connect_target(default, config, true);
    };

    if target.contains(':') {
        parse_address(target)
    } else {
        resolve_named_connect_target(target, config, false)
    }
}

fn resolve_named_connect_target(
    name: &str,
    config: &config::Config,
    from_default: bool,
) -> Result<(String, u16)> {
    if let Some(address) = config
        .connect
        .as_ref()
        .and_then(|connect| connect.targets.as_ref())
        .and_then(|targets| targets.get(name))
    {
        return parse_address(address);
    }

    let known = config
        .connect
        .as_ref()
        .and_then(|connect| connect.targets.as_ref())
        .map(|targets| targets.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let hint = if known.is_empty() {
        "pass `<host>:<port>` or add [connect.targets] entries to the rdny config file".to_string()
    } else {
        format!("known connect targets: {}", known.join(", "))
    };
    let message = if from_default {
        format!("default connect target `{name}` is not configured")
    } else {
        format!("unknown connect target `{name}`")
    };
    Err(crate::hint::hint_error(message, hint, None))
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
    use std::collections::BTreeMap;

    fn parse(args: &[&str]) -> Command {
        Cli::try_parse_from(args).unwrap().command
    }

    fn parse_cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap()
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
    fn rejects_unsafe_duration_values_during_clap_parsing() {
        for value in [
            "NaN", "inf", "-inf", "0", "-1", "0.0001", "86400.1", "1e300",
        ] {
            let error =
                Cli::try_parse_from(["rdny", "--timeout", value, "status"]).expect_err(value);
            assert_eq!(error.exit_code(), 2, "{value}");
        }
        assert_eq!(
            Cli::try_parse_from(["rdny", "--timeout", "0.001", "status"])
                .unwrap()
                .timeout
                .get(),
            Duration::from_millis(1)
        );
        assert_eq!(
            Cli::try_parse_from(["rdny", "--timeout", "86400", "status"])
                .unwrap()
                .timeout
                .get(),
            BoundedDuration::MAX
        );
    }

    #[test]
    fn rejects_unsafe_explicit_durations_dimensions_and_scales() {
        for value in ["NaN", "inf", "0", "-1", "86401"] {
            assert!(Cli::try_parse_from(["rdny", "logs", "--duration", value]).is_err());
            assert!(Cli::try_parse_from(["rdny", "sleep", value]).is_err());
        }
        for value in ["0", "16385"] {
            assert!(Cli::try_parse_from(["rdny", "viewport", value, "100"]).is_err());
            assert!(Cli::try_parse_from(["rdny", "screenshot", "--width", value]).is_err());
        }
        for value in ["NaN", "inf", "0", "0.01", "-1", "10.1"] {
            assert!(
                Cli::try_parse_from(["rdny", "viewport", "100", "100", "--scale", value]).is_err()
            );
        }
    }

    #[test]
    fn parses_required_commands() {
        assert!(matches!(
            parse(&["rdny", "connect"]),
            Command::Connect { address: None, .. }
        ));
        assert!(
            matches!(parse(&["rdny", "connect", "helium"]), Command::Connect { address: Some(address), .. } if address == "helium")
        );
        assert!(
            matches!(parse(&["rdny", "connect", "127.0.0.1:9222"]), Command::Connect { address: Some(address), .. } if address == "127.0.0.1:9222")
        );
        assert!(
            matches!(parse(&["rdny", "sleep", "1.5"]), Command::Sleep { seconds } if seconds.get() == Duration::from_secs_f64(1.5))
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
            Command::Waitstable(_)
        ));
        assert!(matches!(
            parse(&["rdny", "waitstable", "--quiet-ms", "750"]),
            Command::Waitstable(WaitQuietArgs { quiet_ms: 750 })
        ));
        assert!(matches!(parse(&["rdny", "waitidle"]), Command::Waitidle(_)));
        assert!(Cli::try_parse_from(["rdny", "waitidle", "--quiet-ms", "0"]).is_err());
        assert!(Cli::try_parse_from(["rdny", "waitidle", "--quiet-ms", "60001"]).is_err());
        assert!(matches!(
            parse(&["rdny", "start-video"]),
            Command::StartVideo
        ));
        assert!(matches!(
            parse(&["rdny", "stop-video"]),
            Command::StopVideo(ArtifactArgs {
                file: None,
                force: false
            })
        ));
        assert!(
            matches!(parse(&["rdny", "stop-video", "out.mp4"]), Command::StopVideo(ArtifactArgs { file: Some(file), force: false }) if file.as_os_str() == "out.mp4")
        );
        assert!(matches!(
            parse(&["rdny", "logs", "--follow"]),
            Command::Logs(LogsArgs { follow: true, .. })
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
            matches!(parse(&["rdny", "download", "a.link", "-"]), Command::Download { selector, file: Some(file), force: false, max_bytes: None } if selector == "a.link" && file.as_os_str() == "-")
        );
    }

    #[test]
    fn parses_separate_chromium_internal_url_opt_ins() {
        match parse(&["rdny", "open", "chrome://version", "--allow-chrome-url"]) {
            Command::Open { policy, .. } => {
                assert!(policy.allow_chrome_url);
                assert!(!policy.allow_chrome_extension_url);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        match parse(&[
            "rdny",
            "newpage",
            "chrome-extension://abcdefghijklmnopabcdefghijklmnop/page.html",
            "--allow-chrome-extension-url",
        ]) {
            Command::Newpage { policy, .. } => {
                assert!(!policy.allow_chrome_url);
                assert!(policy.allow_chrome_extension_url);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_download_max_bytes() {
        assert!(
            matches!(parse(&["rdny", "download", "a.link", "--max-bytes", "1024"]), Command::Download { selector, max_bytes: Some(1024), .. } if selector == "a.link")
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
                assert_eq!(args.value.as_deref(), Some("abc"));
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
        assert!(Cli::try_parse_from(["rdny", "screenshot", "-h"]).is_err());
        match parse(&["rdny", "screenshot", "-w", "1280", "-H", "720", "out.png"]) {
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
        assert_eq!(
            parse_address("[::1]:9222").unwrap(),
            ("::1".to_string(), 9222)
        );
        assert!(parse_address("9222").is_err());
        assert!(parse_address("foo:bar").is_err());
    }

    #[test]
    fn parses_global_state_dir() {
        let cli = parse_cli(&["rdny", "--state-dir", "/tmp/x", "connect", "foo"]);
        assert_eq!(cli.state_dir, Some(PathBuf::from("/tmp/x")));
        assert!(
            matches!(cli.command, Command::Connect { address: Some(address), .. } if address == "foo")
        );
    }

    fn connect_config(default: Option<&str>, targets: &[(&str, &str)]) -> config::Config {
        config::Config {
            binaries: None,
            connect: Some(config::Connect {
                default: default.map(str::to_string),
                targets: Some(
                    targets
                        .iter()
                        .map(|(name, address)| (name.to_string(), address.to_string()))
                        .collect::<BTreeMap<_, _>>(),
                ),
            }),
        }
    }

    #[test]
    fn resolves_connect_target_explicit_address() {
        assert_eq!(
            resolve_connect_target(Some("127.0.0.1:9333"), &config::Config::default()).unwrap(),
            ("127.0.0.1".to_string(), 9333)
        );
    }

    #[test]
    fn resolves_connect_target_named_hit() {
        let config = connect_config(None, &[("helium", "127.0.0.1:9333")]);
        assert_eq!(
            resolve_connect_target(Some("helium"), &config).unwrap(),
            ("127.0.0.1".to_string(), 9333)
        );
    }

    #[test]
    fn resolves_connect_target_named_miss_lists_known_names() {
        let config = connect_config(None, &[("helium", "127.0.0.1:9333")]);
        let err = resolve_connect_target(Some("chrome"), &config).unwrap_err();
        assert!(format!("{err}").contains("helium"));
    }

    #[test]
    fn resolves_connect_target_default() {
        let config = connect_config(Some("helium"), &[("helium", "127.0.0.1:9333")]);
        assert_eq!(
            resolve_connect_target(None, &config).unwrap(),
            ("127.0.0.1".to_string(), 9333)
        );
    }

    #[test]
    fn rejects_connect_target_without_default() {
        let err = resolve_connect_target(None, &config::Config::default()).unwrap_err();
        assert!(format!("{err}").contains("default"));
    }

    #[test]
    fn rejects_connect_default_pointing_at_missing_target() {
        let config = connect_config(Some("helium"), &[]);
        let err = resolve_connect_target(None, &config).unwrap_err();
        assert!(format!("{err}").contains("default connect target `helium`"));
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
