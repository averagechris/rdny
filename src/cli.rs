//! Command-line surface and dispatch.

use anyhow::{Context, Result};
use clap::{ArgGroup, Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use serde_json::json;
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use crate::browser::{self, BrowserStatus, LaunchOpts};
use crate::commands::OutputFormat;
use crate::input::{
    DEFAULT_DRAG_DURATION_MS, DEFAULT_DRAG_STEPS, KeyChord, MAX_DRAG_DURATION_MS, MAX_DRAG_STEPS,
    MIN_DRAG_DURATION_MS, MIN_DRAG_STEPS, MouseButton, PointerPoint, PointerTarget,
};
use crate::selector::ElementSelector;
use crate::{commands, config, session};

const STRUCTURED_COMMAND_INVENTORY: &str = "status, list, cleanup, open, cookie list, viewport, logs, pages, screenshot, screenshot-el, pdf, download FILE, stop-video";
const TOP_LEVEL_AFTER_HELP: &str = "Structured output commands: status, list, cleanup, open, cookie list, viewport, logs, pages, screenshot, screenshot-el, pdf, download FILE, stop-video.\n\nInput discovery: use `rdny key --help` for key names and `rdny pointer --help` for pointer targets. Trusted key names assume a US keyboard layout.\n\nArtifact commands accept an optional FILE positional. Omit FILE for the default artifact path; use FILE=- only where documented for raw stdout bytes.";

/// Chrome automation from the command line.
#[derive(Debug, Parser)]
#[command(
    name = "rdny",
    version,
    about = "Chrome automation from the command line",
    after_long_help = TOP_LEVEL_AFTER_HELP
)]
pub struct Cli {
    /// Seconds to wait for slow operations before giving up.
    #[arg(long, global = true, default_value_t = BoundedDuration::default())]
    pub timeout: BoundedDuration,

    /// State directory to use, equivalent to RDNY_STATE_DIR and taking precedence over it.
    /// Conflicts with any --instance/RDNY_INSTANCE selector.
    #[arg(long, global = true, conflicts_with = "instance")]
    pub state_dir: Option<PathBuf>,

    /// Registered instance id or unique exact label (env: RDNY_INSTANCE).
    /// Conflicts with --state-dir and RDNY_STATE_DIR.
    #[arg(
        long,
        global = true,
        env = "RDNY_INSTANCE",
        conflicts_with = "state_dir"
    )]
    pub instance: Option<String>,

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

impl OutputFormat {
    fn emit(self, value: &serde_json::Value) -> Result<()> {
        self.emit_json(value)
    }

    fn render_artifact(self, artifact: &commands::artifacts::ProducedArtifact) -> Result<String> {
        match self {
            Self::Human => Ok(artifact.human_summary()),
            Self::Json => Ok(serde_json::to_string_pretty(artifact)?),
            Self::Jsonl => Ok(serde_json::to_string(artifact)?),
        }
    }

    fn emit_artifact(self, artifact: &commands::artifacts::ProducedArtifact) -> Result<()> {
        println!("{}", self.render_artifact(artifact)?);
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
    Html {
        selector: Option<String>,
        /// Split SELECTOR on `>>>` and traverse nested open shadow roots.
        #[arg(long, requires = "selector")]
        pierce: bool,
    },
    /// Print element text.
    Text {
        selector: String,
        /// Split SELECTOR on `>>>` and traverse nested open shadow roots.
        #[arg(long)]
        pierce: bool,
    },
    /// Print an element attribute.
    Attr {
        selector: String,
        name: String,
        /// Split SELECTOR on `>>>` and traverse nested open shadow roots.
        #[arg(long)]
        pierce: bool,
    },
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
    Click {
        selector: String,
        /// Split SELECTOR on `>>>` and traverse nested open shadow roots.
        #[arg(long)]
        pierce: bool,
    },
    /// Press a named key or modifier chord using trusted browser input.
    #[command(
        after_long_help = "Supported forms: named keys (Enter, Escape, ArrowLeft), literal US-layout printable keys, and modifier chords joined with '+'. Physical key codes assume a US keyboard layout.\nExamples:\n  rdny key Enter\n  rdny key 'Control+Shift+K'\n  rdny key 'Meta+ArrowLeft'"
    )]
    Key(KeyArgs),
    /// Send trusted pointer movement, button, and drag input.
    Pointer(PointerArgs),
    /// Type text into an element.
    Input(InputArgs),
    /// Clear an element's value.
    Clear {
        selector: String,
        /// Split SELECTOR on `>>>` and traverse nested open shadow roots.
        #[arg(long)]
        pierce: bool,
    },
    /// Upload a file to an input element.
    File {
        selector: String,
        path: PathBuf,
        /// Split SELECTOR on `>>>` and traverse nested open shadow roots.
        #[arg(long)]
        pierce: bool,
    },
    /// Click and download a linked resource.
    Download {
        /// Link or element selector to click.
        selector: String,
        /// Split SELECTOR on `>>>` and traverse nested open shadow roots.
        #[arg(long)]
        pierce: bool,
        /// Maximum accepted payload bytes (default 268435456; env RDNY_MAX_DOWNLOAD_BYTES).
        #[arg(long)]
        max_bytes: Option<u64>,
        /// Replace an existing output file.
        #[arg(long)]
        force: bool,
        /// Output file. Omit or pass `-` to write raw bytes to stdout in human mode; structured formats require a real file path.
        file: Option<PathBuf>,
    },
    /// Select an option by value.
    Select {
        selector: String,
        value: String,
        /// Split SELECTOR on `>>>` and traverse nested open shadow roots.
        #[arg(long)]
        pierce: bool,
    },
    /// Submit a form.
    Submit {
        selector: String,
        /// Split SELECTOR on `>>>` and traverse nested open shadow roots.
        #[arg(long)]
        pierce: bool,
    },
    /// Hover an element.
    Hover {
        selector: String,
        /// Split SELECTOR on `>>>` and traverse nested open shadow roots.
        #[arg(long)]
        pierce: bool,
    },
    /// Focus an element.
    Focus {
        selector: String,
        /// Split SELECTOR on `>>>` and traverse nested open shadow roots.
        #[arg(long)]
        pierce: bool,
    },
    /// Wait for an element to appear.
    Wait {
        selector: String,
        /// Split SELECTOR on `>>>` and traverse nested open shadow roots.
        #[arg(long)]
        pierce: bool,
    },
    /// Wait for page load.
    Waitload,
    /// Wait for page stability.
    Waitstable(WaitQuietArgs),
    /// Wait for browser idleness.
    Waitidle(WaitQuietArgs),
    /// Sleep for a number of seconds.
    Sleep { seconds: BoundedDuration },
    /// Capture a screenshot. FILE defaults to screenshot.png.
    Screenshot(ScreenshotArgs),
    /// Capture an element screenshot. SELECTOR chooses the element; FILE defaults to screenshot.png.
    ScreenshotEl {
        selector: String,
        /// Split SELECTOR on `>>>` and traverse nested open shadow roots.
        #[arg(long)]
        pierce: bool,
        /// Replace an existing output file.
        #[arg(long)]
        force: bool,
        /// Output file. Omit for screenshot.png.
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
    /// Stop collecting frames and assemble a video file. FILE defaults to recording.mp4.
    StopVideo(ArtifactArgs),
}

impl Command {
    fn name(&self) -> &'static str {
        match self {
            Self::Start(_) => "start",
            Self::Connect { .. } => "connect",
            Self::Stop => "stop",
            Self::Status(_) => "status",
            Self::List => "list",
            Self::Completion { .. } => "completion",
            Self::Cleanup(_) => "cleanup",
            Self::Open { .. } => "open",
            Self::Back => "back",
            Self::Forward => "forward",
            Self::Reload(_) => "reload",
            Self::ClearCache => "clear-cache",
            Self::Cookie(_) => "cookie",
            Self::Url => "url",
            Self::Title => "title",
            Self::Html { .. } => "html",
            Self::Text { .. } => "text",
            Self::Attr { .. } => "attr",
            Self::Pdf(_) => "pdf",
            Self::Js { .. } => "js",
            Self::Logs(_) => "logs",
            Self::Viewport(_) => "viewport",
            Self::Click { .. } => "click",
            Self::Key(_) => "key",
            Self::Pointer(_) => "pointer",
            Self::Input(_) => "input",
            Self::Clear { .. } => "clear",
            Self::File { .. } => "file",
            Self::Download { .. } => "download",
            Self::Select { .. } => "select",
            Self::Submit { .. } => "submit",
            Self::Hover { .. } => "hover",
            Self::Focus { .. } => "focus",
            Self::Wait { .. } => "wait",
            Self::Waitload => "waitload",
            Self::Waitstable(_) => "waitstable",
            Self::Waitidle(_) => "waitidle",
            Self::Sleep { .. } => "sleep",
            Self::Screenshot(_) => "screenshot",
            Self::ScreenshotEl { .. } => "screenshot-el",
            Self::Pages => "pages",
            Self::Page { .. } => "page",
            Self::Newpage { .. } => "newpage",
            Self::StartVideo => "start-video",
            Self::StopVideo(_) => "stop-video",
        }
    }

    fn supports_structured_output(&self) -> bool {
        matches!(
            self,
            Self::Status(_)
                | Self::List
                | Self::Cleanup(_)
                | Self::Open { .. }
                | Self::Cookie(CookieArgs {
                    command: CookieCommand::List
                })
                | Self::Pdf(_)
                | Self::Logs(_)
                | Self::Viewport(_)
                | Self::Download { .. }
                | Self::Screenshot(_)
                | Self::ScreenshotEl { .. }
                | Self::Pages
                | Self::StopVideo(_)
        )
    }

    fn targets_registered_instance(&self) -> bool {
        match self {
            Self::Start(_)
            | Self::Connect { .. }
            | Self::List
            | Self::Completion { .. }
            | Self::Cleanup(_)
            | Self::Sleep { .. } => false,
            Self::Stop
            | Self::Status(_)
            | Self::Open { .. }
            | Self::Back
            | Self::Forward
            | Self::Reload(_)
            | Self::ClearCache
            | Self::Cookie(_)
            | Self::Url
            | Self::Title
            | Self::Html { .. }
            | Self::Text { .. }
            | Self::Attr { .. }
            | Self::Pdf(_)
            | Self::Js { .. }
            | Self::Logs(_)
            | Self::Viewport(_)
            | Self::Click { .. }
            | Self::Key(_)
            | Self::Pointer(_)
            | Self::Input(_)
            | Self::Clear { .. }
            | Self::File { .. }
            | Self::Download { .. }
            | Self::Select { .. }
            | Self::Submit { .. }
            | Self::Hover { .. }
            | Self::Focus { .. }
            | Self::Wait { .. }
            | Self::Waitload
            | Self::Waitstable(_)
            | Self::Waitidle(_)
            | Self::Screenshot(_)
            | Self::ScreenshotEl { .. }
            | Self::Pages
            | Self::Page { .. }
            | Self::Newpage { .. }
            | Self::StartVideo
            | Self::StopVideo(_) => true,
        }
    }
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
    /// Permit privileged chrome: URLs and Chromium about: aliases.
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
    /// Also remove state whose liveness probe is inconclusive; attached browsers are never killed.
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
    /// Split SELECTOR on `>>>` and traverse nested open shadow roots.
    #[arg(long)]
    pub pierce: bool,
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

#[derive(Debug, Args)]
pub struct KeyArgs {
    /// Named key, literal US-layout key, or `+`-separated modifier chord (for example Control+Shift+K).
    pub chord: KeyChord,
}

#[derive(Debug, Args)]
#[command(
    after_long_help = "Pointer targets are either selector hit-tested action points or explicit X,Y viewport coordinates.\nExamples:\n  rdny pointer move --selector '#handle'\n  rdny pointer move --at 120,80\n  rdny pointer down --selector '#handle' --button left\n  rdny pointer up --at 420,240 --button left\n  rdny pointer drag --from-selector '#handle' --to-selector '#drop-zone'\n  rdny pointer drag --from-at 100,120 --to-selector '#drop-zone'"
)]
pub struct PointerArgs {
    #[command(subcommand)]
    pub command: PointerCommand,
}

#[derive(Debug, Subcommand)]
pub enum PointerCommand {
    /// Move to a selector's hit-tested action point or an explicit viewport coordinate.
    Move(PointerTargetArgs),
    /// Move and press a mouse button, leaving it pressed for a later `up` command.
    Down(PointerButtonArgs),
    /// Release a mouse button at a selector or explicit viewport coordinate.
    Up(PointerButtonArgs),
    /// Drag between independently explicit selector or coordinate endpoints.
    Drag(DragArgs),
}

#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("pointer_target")
        .required(true)
        .args(["selector", "at"])
))]
pub struct PointerTargetArgs {
    /// Target the first matching CSS selector at a hit-tested action point.
    #[arg(long)]
    pub selector: Option<String>,
    /// Target an explicit `X,Y` viewport coordinate in CSS pixels.
    #[arg(long, value_name = "X,Y")]
    pub at: Option<PointerPoint>,
    /// Split --selector on `>>>` and traverse nested open shadow roots.
    #[arg(long, requires = "selector", conflicts_with = "at")]
    pub pierce: bool,
}

#[derive(Debug, Args)]
pub struct PointerButtonArgs {
    #[command(flatten)]
    pub target: PointerTargetArgs,
    /// Mouse button to press or release.
    #[arg(long, default_value = "left")]
    pub button: MouseButton,
}

#[derive(Debug, Args)]
#[command(
    group(ArgGroup::new("drag_from").required(true).args(["from_selector", "from_at"])),
    group(ArgGroup::new("drag_to").required(true).args(["to_selector", "to_at"])),
    group(ArgGroup::new("drag_selector").multiple(true).args(["from_selector", "to_selector"]))
)]
pub struct DragArgs {
    /// Start at the first matching CSS selector's hit-tested action point.
    #[arg(long)]
    pub from_selector: Option<String>,
    /// Start at an explicit `X,Y` viewport coordinate in CSS pixels.
    #[arg(long, value_name = "X,Y")]
    pub from_at: Option<PointerPoint>,
    /// End at the first matching CSS selector's hit-tested action point.
    #[arg(long)]
    pub to_selector: Option<String>,
    /// End at an explicit `X,Y` viewport coordinate in CSS pixels.
    #[arg(long, value_name = "X,Y")]
    pub to_at: Option<PointerPoint>,
    /// Split selector endpoints on `>>>` and traverse nested open shadow roots.
    #[arg(long, requires = "drag_selector")]
    pub pierce: bool,
    /// Mouse button held during the drag.
    #[arg(long, default_value = "left")]
    pub button: MouseButton,
    /// Number of evenly interpolated mouse movements (1 through 1000).
    #[arg(long, default_value_t = DEFAULT_DRAG_STEPS, value_parser = parse_drag_steps)]
    pub steps: u32,
    /// Total interpolation duration in milliseconds (1 through 30000).
    #[arg(long, default_value_t = DEFAULT_DRAG_DURATION_MS, value_parser = parse_drag_duration_ms)]
    pub duration_ms: u64,
}

fn parse_drag_steps(raw: &str) -> std::result::Result<u32, String> {
    let value: u32 = raw
        .parse()
        .map_err(|_| "drag steps must be an integer".to_string())?;
    if (MIN_DRAG_STEPS..=MAX_DRAG_STEPS).contains(&value) {
        Ok(value)
    } else {
        Err(format!(
            "drag steps must be between {MIN_DRAG_STEPS} and {MAX_DRAG_STEPS}"
        ))
    }
}

fn parse_drag_duration_ms(raw: &str) -> std::result::Result<u64, String> {
    let value: u64 = raw
        .parse()
        .map_err(|_| "drag duration-ms must be an integer".to_string())?;
    if (MIN_DRAG_DURATION_MS..=MAX_DRAG_DURATION_MS).contains(&value) {
        Ok(value)
    } else {
        Err(format!(
            "drag duration-ms must be between {MIN_DRAG_DURATION_MS} and {MAX_DRAG_DURATION_MS}"
        ))
    }
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
    /// Output file. Omit for screenshot.png.
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
    /// Output file. Omit for page.pdf or recording.mp4, depending on command.
    pub file: Option<PathBuf>,
}

/// Parse argv and execute the selected command.
pub fn run() -> Result<()> {
    let cli = Cli::parse_adjusted();
    if let Command::Download { file, .. } = &cli.command {
        // Validate stdout ownership before instance selection or browser I/O.
        validate_download_output(cli.format, file.as_deref())?;
    }
    let timeout = cli.timeout.get();
    let command_budget = match &cli.command {
        Command::Logs(args) if !args.follow => args.duration.unwrap_or(cli.timeout).get(),
        Command::Sleep { seconds } => seconds.get(),
        _ => timeout,
    };
    let deadline = session::Deadline::after(command_budget);
    configure_instance_selection(&cli)?;
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
        Command::List => commands::instances::list_format_until(cli.format, deadline)?,
        Command::Completion { shell } => {
            let mut cmd = adjusted_command();
            clap_complete::generate(shell, &mut cmd, "rdny", &mut std::io::stdout());
        }
        Command::Cleanup(args) => {
            commands::instances::cleanup_until(args.all, cli.format, deadline)?
        }
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
            CookieCommand::List => commands::cookie::list_format(sess!(), cli.format)?,
            CookieCommand::Get { name } => commands::cookie::get(sess!(), &name)?,
            CookieCommand::Delete(args) => {
                commands::cookie::delete(sess!(), &args.name, &args.domain, &args.path)?
            }
        },
        Command::Url => commands::pageinfo::url(sess!())?,
        Command::Title => commands::pageinfo::title(sess!())?,
        Command::Html { selector, pierce } => {
            let selector = selector
                .map(|selector| ElementSelector::parse(selector, pierce))
                .transpose()?;
            commands::pageinfo::html(sess!(), selector.as_ref())?
        }
        Command::Text { selector, pierce } => {
            let selector = ElementSelector::parse(selector, pierce)?;
            commands::pageinfo::text(sess!(), &selector)?
        }
        Command::Attr {
            selector,
            name,
            pierce,
        } => {
            let selector = ElementSelector::parse(selector, pierce)?;
            commands::pageinfo::attr(sess!(), &selector, &name)?
        }
        Command::Pdf(args) => {
            let artifact = commands::pageinfo::pdf(sess!(), args.file.as_deref(), args.force)?;
            cli.format.emit_artifact(&artifact)?;
        }
        Command::Js { expression } => {
            let stdin = std::io::stdin();
            let stdin_is_tty = stdin.is_terminal();
            let expression = resolve_js_expression(expression, stdin, stdin_is_tty)?;
            commands::interact::js(sess!(), &expression)?
        }
        Command::Logs(args) => commands::logs::logs_format(sess!(), args.follow, cli.format)?,
        Command::Viewport(args) => commands::viewport::viewport_format(
            sess!(),
            args.width,
            args.height,
            args.scale,
            args.mobile,
            args.reset,
            cli.format,
        )?,
        Command::Click { selector, pierce } => {
            let selector = ElementSelector::parse(selector, pierce)?;
            commands::interact::click(sess!(), &selector)?
        }
        Command::Key(args) => crate::input::key(sess!(), &args.chord)?,
        Command::Pointer(args) => match args.command {
            PointerCommand::Move(args) => {
                let target = pointer_target(args.selector, args.at, args.pierce)?;
                crate::input::pointer_move(sess!(), &target)?
            }
            PointerCommand::Down(args) => {
                let target =
                    pointer_target(args.target.selector, args.target.at, args.target.pierce)?;
                crate::input::pointer_down(sess!(), &target, args.button)?
            }
            PointerCommand::Up(args) => {
                let target =
                    pointer_target(args.target.selector, args.target.at, args.target.pierce)?;
                crate::input::pointer_up(sess!(), &target, args.button)?
            }
            PointerCommand::Drag(args) => {
                let from = pointer_target(args.from_selector, args.from_at, args.pierce)?;
                let to = pointer_target(args.to_selector, args.to_at, args.pierce)?;
                crate::input::drag(
                    sess!(),
                    &from,
                    &to,
                    args.button,
                    args.steps,
                    Duration::from_millis(args.duration_ms),
                )?
            }
        },
        Command::Input(args) => {
            let selector = ElementSelector::parse(args.selector, args.pierce)?;
            let text = resolve_secret(
                args.text.as_deref(),
                args.text_stdin,
                args.text_file.as_deref(),
                args.text_fd,
                "input text",
            )?;
            commands::interact::input(sess!(), &selector, &text)?
        }
        Command::Clear { selector, pierce } => {
            let selector = ElementSelector::parse(selector, pierce)?;
            commands::interact::clear(sess!(), &selector)?
        }
        Command::File {
            selector,
            path,
            pierce,
        } => {
            let selector = ElementSelector::parse(selector, pierce)?;
            commands::interact::file(sess!(), &selector, &path)?
        }
        Command::Download {
            selector,
            pierce,
            max_bytes,
            force,
            file,
        } => {
            let selector = ElementSelector::parse(selector, pierce)?;
            if let Some(artifact) =
                commands::interact::download(sess!(), &selector, file.as_deref(), force, max_bytes)?
            {
                cli.format.emit_artifact(&artifact)?;
            }
        }
        Command::Select {
            selector,
            value,
            pierce,
        } => {
            let selector = ElementSelector::parse(selector, pierce)?;
            commands::interact::select(sess!(), &selector, &value)?
        }
        Command::Submit { selector, pierce } => {
            let selector = ElementSelector::parse(selector, pierce)?;
            commands::interact::submit(sess!(), &selector)?
        }
        Command::Hover { selector, pierce } => {
            let selector = ElementSelector::parse(selector, pierce)?;
            commands::interact::hover(sess!(), &selector)?
        }
        Command::Focus { selector, pierce } => {
            let selector = ElementSelector::parse(selector, pierce)?;
            commands::interact::focus(sess!(), &selector)?
        }
        Command::Wait { selector, pierce } => {
            let selector = ElementSelector::parse(selector, pierce)?;
            commands::wait::wait(sess!(), &selector)?
        }
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
            let artifact = commands::shot::screenshot(
                sess!(),
                args.width,
                args.height,
                args.file.as_deref(),
                args.force,
                state.viewport.as_ref(),
            )?;
            cli.format.emit_artifact(&artifact)?;
        }
        Command::ScreenshotEl {
            selector,
            pierce,
            force,
            file,
        } => {
            let selector = ElementSelector::parse(selector, pierce)?;
            let artifact =
                commands::shot::screenshot_el(sess!(), &selector, file.as_deref(), force)?;
            cli.format.emit_artifact(&artifact)?;
        }
        Command::Pages => commands::tabs::pages_format_until(cli.format, deadline)?,
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
            let artifact = commands::video::stop(live.as_mut(), args.file.as_deref(), args.force)?;
            cli.format.emit_artifact(&artifact)?;
        }
    }
    if drain_after_dispatch && let Some(session) = page_session.as_mut() {
        session.drain_events(std::time::Duration::from_millis(300))?;
    }
    Ok(())
}

impl Cli {
    fn parse_adjusted() -> Self {
        Self::try_parse_adjusted_from(std::env::args_os()).unwrap_or_else(|err| err.exit())
    }

    fn try_parse_adjusted_from<I, T>(args: I) -> std::result::Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        let mut matches = adjusted_command().try_get_matches_from(args)?;
        let cli = Self::from_arg_matches_mut(&mut matches)?;
        validate_format_support_clap(cli.format, &cli.command)?;
        Ok(cli)
    }
}

fn adjusted_command() -> clap::Command {
    let mut command = Cli::command();
    command.build();
    hide_instance_for_non_target_subcommands(&mut command);
    command
}

fn hide_instance_for_non_target_subcommands(command: &mut clap::Command) {
    for name in ["start", "connect", "list", "completion", "cleanup", "sleep"] {
        if let Some(subcommand) = command.find_subcommand_mut(name)
            && subcommand
                .get_arguments()
                .any(|arg| arg.get_id() == "instance")
        {
            let updated = subcommand.clone().mut_arg("instance", |arg| arg.hide(true));
            *subcommand = updated;
        }
    }
}

fn pointer_target(
    selector: Option<String>,
    point: Option<PointerPoint>,
    pierce: bool,
) -> Result<PointerTarget> {
    match (selector, point) {
        (Some(selector), None) => Ok(PointerTarget::selector(ElementSelector::parse(
            selector, pierce,
        )?)),
        (None, Some(point)) => Ok(PointerTarget::point(point)),
        _ => anyhow::bail!("choose exactly one selector or coordinate pointer target"),
    }
}

fn validate_download_output(format: OutputFormat, file: Option<&Path>) -> Result<()> {
    if format != OutputFormat::Human && file.is_none_or(|path| path == Path::new("-")) {
        anyhow::bail!(
            "download to stdout (omitted FILE or `FILE=-`) writes raw bytes and is incompatible with --format json/jsonl; use --format human or choose a file path"
        );
    }
    Ok(())
}

fn validate_format_support_clap(
    format: OutputFormat,
    command: &Command,
) -> std::result::Result<(), clap::Error> {
    if format == OutputFormat::Human || command.supports_structured_output() {
        return Ok(());
    }
    Err(clap::Error::raw(
        clap::error::ErrorKind::ValueValidation,
        format!(
            "--format json/jsonl is not supported for `{}`; supported structured commands are {}",
            command.name(),
            STRUCTURED_COMMAND_INVENTORY
        ),
    ))
}

fn configure_instance_selection(cli: &Cli) -> Result<()> {
    let env_state_dir = std::env::var_os("RDNY_STATE_DIR");
    validate_selection_inputs(
        cli.instance.as_deref(),
        cli.state_dir.as_deref(),
        env_state_dir.as_deref(),
        cli.command.targets_registered_instance(),
    )?;

    let selected = cli
        .instance
        .as_deref()
        .map(commands::instances::resolve_registered)
        .transpose()?;
    if let Some(selected) = &selected {
        // Bind state reads/mutations to the identity verified by the registry,
        // even if the directory is replaced between lookup and dispatch.
        crate::state::select_instance(&selected.instance_id)?;
    }
    let effective_dir = selected
        .as_ref()
        .map(|selected| selected.dir.as_path())
        .or(cli.state_dir.as_deref());
    if let Some(state_dir) = effective_dir {
        // SAFETY: rdny is still single-threaded here, before any command dispatch or
        // background work, so mutating the process environment cannot race other threads.
        unsafe { std::env::set_var("RDNY_STATE_DIR", state_dir) };
    }
    Ok(())
}

fn validate_selection_inputs(
    instance: Option<&str>,
    cli_state_dir: Option<&std::path::Path>,
    env_state_dir: Option<&std::ffi::OsStr>,
    targets_registered_instance: bool,
) -> Result<()> {
    let Some(instance) = instance else {
        return Ok(());
    };
    if cli_state_dir.is_some() || env_state_dir.is_some() {
        anyhow::bail!(
            "instance selector `{instance}` conflicts with --state-dir/RDNY_STATE_DIR; unset the state-directory override or unset RDNY_INSTANCE/omit --instance"
        );
    }
    if !targets_registered_instance {
        anyhow::bail!(
            "--instance/RDNY_INSTANCE does not apply to this command; start/connect choose their destination with --state-dir, list/cleanup inspect the registry, and completion/sleep need no instance"
        );
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

    fn output_artifact() -> commands::artifacts::ProducedArtifact {
        commands::artifacts::ProducedArtifact::new(
            commands::artifacts::PublishedArtifact {
                path: PathBuf::from("/tmp/example.png"),
                bytes: 42,
            },
            PathBuf::from("example.png"),
            commands::artifacts::HumanArtifactOutput::Saved,
            "image/png",
            Some((10, 20)),
            commands::artifacts::ArtifactContext::default(),
        )
    }

    fn parse(args: &[&str]) -> Command {
        Cli::try_parse_from(args).unwrap().command
    }

    fn parse_cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap()
    }

    fn help_for(path: &[&str]) -> String {
        let mut command = adjusted_command();
        for name in path {
            command = command.find_subcommand(name).unwrap().clone();
        }
        let mut buf = Vec::new();
        command.write_long_help(&mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn adjusted_help_hides_instance_only_where_inapplicable() {
        for name in ["start", "connect", "list", "completion", "cleanup", "sleep"] {
            let help = help_for(&[name]);
            assert!(!help.contains("--instance <INSTANCE>"), "{name}\n{help}");
        }
        for name in ["status", "open", "title", "pages", "screenshot"] {
            let help = help_for(&[name]);
            assert!(help.contains("--instance <INSTANCE>"), "{name}\n{help}");
        }
        Cli::try_parse_adjusted_from(["rdny", "--instance", "x", "start"]).unwrap();
    }

    #[test]
    fn command_specific_help_documents_exact_defaults_and_input_forms() {
        let download = help_for(&["download"]);
        assert!(download.contains("Omit or pass `-` to write raw bytes to stdout"));
        assert!(download.contains("structured formats require a real file path"));
        let screenshot = help_for(&["screenshot"]);
        assert!(screenshot.contains("FILE defaults to screenshot.png"));
        assert!(screenshot.contains("Omit for screenshot.png"));
        let screenshot_el = help_for(&["screenshot-el"]);
        assert!(screenshot_el.contains("SELECTOR chooses the element"));
        assert!(screenshot_el.contains("FILE defaults to screenshot.png"));
        let pdf = help_for(&["pdf"]);
        assert!(pdf.contains("page.pdf"));
        let stop_video = help_for(&["stop-video"]);
        assert!(stop_video.contains("FILE defaults to recording.mp4"));
        let key = help_for(&["key"]);
        assert!(key.contains("US-layout"));
        assert!(key.contains("modifier chord"));
        let pointer = help_for(&["pointer"]);
        assert!(pointer.contains("selector hit-tested action points"));
        assert!(pointer.contains("X,Y viewport coordinates"));
    }

    #[test]
    fn unsupported_structured_formats_are_clap_exit_2_errors() {
        for format in ["json", "jsonl"] {
            let cli = Cli::try_parse_adjusted_from([
                "rdny",
                "--format",
                format,
                "open",
                "https://example.com/",
            ])
            .expect("open supports structured output");
            assert!(matches!(cli.command, Command::Open { .. }));
        }

        let err = Cli::try_parse_adjusted_from(["rdny", "--format", "json", "title"])
            .expect_err("unsupported format should be a clap validation error");
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
        assert_eq!(err.exit_code(), 2);
        let message = err.to_string();
        assert!(message.contains("--format json/jsonl is not supported for `title`"));
        assert!(message.contains(STRUCTURED_COMMAND_INVENTORY));
    }

    #[test]
    fn documented_structured_schema_contract_matrix() {
        let contracts: Vec<(&str, serde_json::Value, &str)> = vec![
            (
                "status",
                serde_json::json!({"schemaVersion":1,"kind":"status","status":"missing","healthy":false}),
                "no session",
            ),
            (
                "list",
                serde_json::json!({"schemaVersion":1,"kind":"list","instances":[]}),
                "",
            ),
            (
                "cleanup",
                serde_json::json!({"schemaVersion":1,"kind":"cleanup","results":[{"dir":"/state/a","pid":null,"label":null,"action":"cleaned","reason":"cleaned"},{"dir":"/state/b","pid":42,"label":"b","action":"preserved_inconclusive_timeout","reason":"probe timed out"}]}),
                "cleaned: /state/a",
            ),
            (
                "open",
                serde_json::json!({"schemaVersion":1,"kind":"open","url":"https://example.com/"}),
                "Example — https://example.com/",
            ),
            (
                "cookie list",
                serde_json::json!({"schemaVersion":1,"kind":"cookies","url":"https://example.com/","cookies":[]}),
                "",
            ),
            (
                "viewport",
                serde_json::json!({"schemaVersion":1,"kind":"viewport","applied":false,"width":800,"height":600,"scale":1.0,"mobile":false}),
                "800x600",
            ),
            (
                "logs",
                serde_json::json!({"schemaVersion":1,"kind":"log","timestamp":1.0,"instance":"inst","target":"target","cdpSession":"cdp","severity":"log","message":"[log] hi"}),
                "[log] hi",
            ),
            (
                "pages",
                serde_json::json!({"schemaVersion":1,"kind":"pages","pages":[{"index":0,"current":true,"id":"target","url":"https://example.com/","title":"Example"},{"index":1,"current":false,"id":"target-2","url":"about:blank","title":""}]}),
                "* 0: https://example.com/ (Example)",
            ),
            (
                "screenshot",
                serde_json::from_str(
                    &OutputFormat::Jsonl
                        .render_artifact(&output_artifact())
                        .unwrap(),
                )
                .unwrap(),
                "saved example.png",
            ),
            (
                "screenshot-el",
                serde_json::from_str(
                    &OutputFormat::Jsonl
                        .render_artifact(&output_artifact())
                        .unwrap(),
                )
                .unwrap(),
                "saved example.png",
            ),
            (
                "pdf",
                serde_json::from_str(
                    &OutputFormat::Jsonl
                        .render_artifact(&output_artifact())
                        .unwrap(),
                )
                .unwrap(),
                "saved example.png",
            ),
            (
                "download FILE",
                serde_json::from_str(
                    &OutputFormat::Jsonl
                        .render_artifact(&output_artifact())
                        .unwrap(),
                )
                .unwrap(),
                "saved example.png",
            ),
            (
                "stop-video",
                serde_json::from_str(
                    &OutputFormat::Jsonl
                        .render_artifact(&output_artifact())
                        .unwrap(),
                )
                .unwrap(),
                "saved example.png",
            ),
        ];
        assert_eq!(
            contracts
                .iter()
                .map(|(name, _, _)| *name)
                .collect::<Vec<_>>()
                .join(", "),
            STRUCTURED_COMMAND_INVENTORY
        );
        for (name, value, human_hint) in contracts {
            assert_eq!(value["schemaVersion"], 1, "{name}");
            assert!(value["kind"].is_string(), "{name}");
            let pretty = OutputFormat::Json.render_json(&value).unwrap();
            assert!(pretty.contains('\n'), "{name} json should be pretty");
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&pretty).unwrap(),
                value
            );
            let jsonl = OutputFormat::Jsonl.render_json(&value).unwrap();
            assert!(
                !jsonl.contains('\n'),
                "{name} jsonl should be compact one record"
            );
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&jsonl).unwrap(),
                value
            );
            if !human_hint.is_empty() {
                assert!(!human_hint.contains('\u{1b}'), "{name} human stdout purity");
            }
        }

        let log_one = serde_json::json!({"schemaVersion":1,"kind":"log","timestamp":1.0,"target":"t","severity":"log","message":"one"});
        let log_two = serde_json::json!({"schemaVersion":1,"kind":"log","timestamp":2.0,"target":"t","severity":"error","message":"two"});
        let stream = format!(
            "{}\n{}\n",
            OutputFormat::Jsonl.render_json(&log_one).unwrap(),
            OutputFormat::Jsonl.render_json(&log_two).unwrap()
        );
        assert_eq!(
            stream.lines().count(),
            2,
            "multi-event logs are one record per line"
        );
        let empty_finite = OutputFormat::Jsonl
            .render_json(&serde_json::json!({"schemaVersion":1,"kind":"list","instances":[]}))
            .unwrap();
        assert!(empty_finite.contains("\"instances\":[]"));
        let multi_finite = OutputFormat::Jsonl.render_json(&serde_json::json!({"schemaVersion":1,"kind":"pages","pages":[{"index":0},{"index":1}]})).unwrap();
        assert_eq!(
            multi_finite.lines().count(),
            1,
            "finite multi-item jsonl stays one envelope"
        );
    }

    #[test]
    fn artifact_output_formats_are_executable_schema_snapshots() {
        let artifact = output_artifact();
        assert_eq!(
            OutputFormat::Human.render_artifact(&artifact).unwrap(),
            "saved example.png"
        );

        let json = OutputFormat::Json.render_artifact(&artifact).unwrap();
        assert!(json.contains('\n'));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap(),
            serde_json::json!({
                "schemaVersion": 1,
                "kind": "artifact",
                "path": "/tmp/example.png",
                "type": "image/png",
                "bytes": 42,
                "width": 10,
                "height": 20
            })
        );

        let jsonl = OutputFormat::Jsonl.render_artifact(&artifact).unwrap();
        assert_eq!(
            jsonl,
            r#"{"schemaVersion":1,"kind":"artifact","path":"/tmp/example.png","type":"image/png","bytes":42,"width":10,"height":20}"#
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&jsonl).unwrap()["kind"],
            "artifact"
        );
    }

    #[test]
    fn structured_raw_download_is_rejected_before_dispatch() {
        for format in [OutputFormat::Json, OutputFormat::Jsonl] {
            let error = validate_download_output(format, Some(Path::new("-"))).unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains("raw bytes"));
            assert!(message.contains("--format human"));
            assert!(message.contains("file path"));
        }
        assert!(validate_download_output(OutputFormat::Human, Some(Path::new("-"))).is_ok());
        assert!(validate_download_output(OutputFormat::Json, Some(Path::new("out.bin"))).is_ok());
        assert!(validate_download_output(OutputFormat::Human, None).is_ok());
        assert!(validate_download_output(OutputFormat::Jsonl, None).is_err());
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
            matches!(parse(&["rdny", "attr", "a", "href"]), Command::Attr { selector, name, pierce: false } if selector == "a" && name == "href")
        );
        assert!(matches!(
            parse(&["rdny", "reload", "--hard"]),
            Command::Reload(ReloadArgs { hard: true })
        ));
        assert!(matches!(
            parse(&["rdny", "html"]),
            Command::Html {
                selector: None,
                pierce: false
            }
        ));
        assert!(matches!(
            parse(&["rdny", "js"]),
            Command::Js { expression: None }
        ));
        assert!(
            matches!(parse(&["rdny", "js", "1 + 1"]), Command::Js { expression: Some(expression) } if expression == "1 + 1")
        );
        assert!(
            matches!(parse(&["rdny", "html", "div"]), Command::Html { selector: Some(selector), pierce: false } if selector == "div")
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
            matches!(parse(&["rdny", "download", "a.link", "-"]), Command::Download { selector, file: Some(file), force: false, max_bytes: None, pierce: false } if selector == "a.link" && file.as_os_str() == "-")
        );
    }

    #[test]
    fn parses_key_and_unambiguous_pointer_targets() {
        assert!(matches!(
            parse(&["rdny", "key", "Control+Shift+K"]),
            Command::Key(_)
        ));
        assert!(Cli::try_parse_from(["rdny", "key", "Control++K"]).is_err());

        assert!(matches!(
            parse(&["rdny", "pointer", "move", "--selector", "#handle"]),
            Command::Pointer(PointerArgs {
                command: PointerCommand::Move(PointerTargetArgs {
                    selector: Some(selector),
                    at: None,
                    pierce: false,
                })
            }) if selector == "#handle"
        ));
        assert!(matches!(
            parse(&["rdny", "pointer", "move", "--at", "12.5,40"]),
            Command::Pointer(PointerArgs {
                command: PointerCommand::Move(PointerTargetArgs {
                    selector: None,
                    at: Some(_),
                    pierce: false,
                })
            })
        ));
        assert!(matches!(
            parse(&[
                "rdny",
                "pointer",
                "down",
                "--selector",
                "button",
                "--button",
                "right"
            ]),
            Command::Pointer(PointerArgs {
                command: PointerCommand::Down(PointerButtonArgs {
                    button: MouseButton::Right,
                    ..
                })
            })
        ));
        assert!(matches!(
            parse(&["rdny", "pointer", "up", "--at", "10,20"]),
            Command::Pointer(PointerArgs {
                command: PointerCommand::Up(PointerButtonArgs {
                    button: MouseButton::Left,
                    ..
                })
            })
        ));

        for args in [
            vec!["rdny", "pointer", "move"],
            vec!["rdny", "pointer", "move", "--selector", "#x", "--at", "1,2"],
            vec!["rdny", "pointer", "move", "--at", "1,2", "--pierce"],
            vec![
                "rdny", "pointer", "down", "--at", "1,2", "--button", "primary",
            ],
            vec!["rdny", "pointer", "up", "--at", "NaN,2"],
        ] {
            assert!(Cli::try_parse_from(&args).is_err(), "accepted {args:?}");
        }
    }

    #[test]
    fn parses_every_drag_endpoint_combination_and_bounds_options() {
        for endpoints in [
            vec!["--from-selector", "#source", "--to-selector", "#target"],
            vec!["--from-selector", "#source", "--to-at", "20,30"],
            vec!["--from-at", "10,15", "--to-selector", "#target"],
            vec!["--from-at", "10,15", "--to-at", "20,30"],
        ] {
            let mut args = vec!["rdny", "pointer", "drag"];
            args.extend(endpoints);
            assert!(matches!(
                parse(&args),
                Command::Pointer(PointerArgs {
                    command: PointerCommand::Drag(_)
                })
            ));
        }

        let command = parse(&[
            "rdny",
            "pointer",
            "drag",
            "--from-selector",
            "outer >>> #source",
            "--to-at",
            "20,30",
            "--pierce",
            "--button",
            "middle",
            "--steps",
            "1000",
            "--duration-ms",
            "30000",
        ]);
        assert!(matches!(
            command,
            Command::Pointer(PointerArgs {
                command: PointerCommand::Drag(DragArgs {
                    pierce: true,
                    button: MouseButton::Middle,
                    steps: 1000,
                    duration_ms: 30000,
                    ..
                })
            })
        ));

        for args in [
            vec![
                "rdny",
                "pointer",
                "drag",
                "--from-at",
                "1,2",
                "--to-at",
                "3,4",
                "--pierce",
            ],
            vec![
                "rdny",
                "pointer",
                "drag",
                "--from-at",
                "1,2",
                "--to-at",
                "3,4",
                "--steps",
                "0",
            ],
            vec![
                "rdny",
                "pointer",
                "drag",
                "--from-at",
                "1,2",
                "--to-at",
                "3,4",
                "--steps",
                "1001",
            ],
            vec![
                "rdny",
                "pointer",
                "drag",
                "--from-at",
                "1,2",
                "--to-at",
                "3,4",
                "--duration-ms",
                "0",
            ],
            vec![
                "rdny",
                "pointer",
                "drag",
                "--from-at",
                "1,2",
                "--to-at",
                "3,4",
                "--duration-ms",
                "30001",
            ],
            vec![
                "rdny",
                "pointer",
                "drag",
                "--from-at",
                "1,2",
                "--from-selector",
                "#x",
                "--to-at",
                "3,4",
            ],
            vec!["rdny", "pointer", "drag", "--from-at", "1,2"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }

    #[test]
    fn parses_pierced_selectors_for_every_element_command() {
        const SHADOW: &str = "outer-host >>> inner-host >>> .target";

        assert!(matches!(
            parse(&["rdny", "html", "--pierce", SHADOW]),
            Command::Html { selector: Some(selector), pierce: true } if selector == SHADOW
        ));
        assert!(matches!(
            parse(&["rdny", "text", SHADOW, "--pierce"]),
            Command::Text { selector, pierce: true } if selector == SHADOW
        ));
        assert!(matches!(
            parse(&["rdny", "attr", "--pierce", SHADOW, "data-state"]),
            Command::Attr { selector, name, pierce: true }
                if selector == SHADOW && name == "data-state"
        ));
        assert!(matches!(
            parse(&["rdny", "click", "--pierce", SHADOW]),
            Command::Click { selector, pierce: true } if selector == SHADOW
        ));
        assert!(matches!(
            parse(&["rdny", "input", SHADOW, "hello", "--pierce"]),
            Command::Input(InputArgs { selector, text: Some(text), pierce: true, .. })
                if selector == SHADOW && text == "hello"
        ));
        assert!(matches!(
            parse(&["rdny", "clear", "--pierce", SHADOW]),
            Command::Clear { selector, pierce: true } if selector == SHADOW
        ));
        assert!(matches!(
            parse(&["rdny", "file", "--pierce", SHADOW, "upload.txt"]),
            Command::File { selector, path, pierce: true }
                if selector == SHADOW && path.as_os_str() == "upload.txt"
        ));
        assert!(matches!(
            parse(&["rdny", "download", SHADOW, "--pierce", "out.txt"]),
            Command::Download { selector, pierce: true, file: Some(file), .. }
                if selector == SHADOW && file.as_os_str() == "out.txt"
        ));
        assert!(matches!(
            parse(&["rdny", "select", "--pierce", SHADOW, "dog"]),
            Command::Select { selector, value, pierce: true }
                if selector == SHADOW && value == "dog"
        ));
        assert!(matches!(
            parse(&["rdny", "submit", SHADOW, "--pierce"]),
            Command::Submit { selector, pierce: true } if selector == SHADOW
        ));
        assert!(matches!(
            parse(&["rdny", "hover", "--pierce", SHADOW]),
            Command::Hover { selector, pierce: true } if selector == SHADOW
        ));
        assert!(matches!(
            parse(&["rdny", "focus", SHADOW, "--pierce"]),
            Command::Focus { selector, pierce: true } if selector == SHADOW
        ));
        assert!(matches!(
            parse(&["rdny", "wait", "--pierce", SHADOW]),
            Command::Wait { selector, pierce: true } if selector == SHADOW
        ));
        assert!(matches!(
            parse(&["rdny", "screenshot-el", SHADOW, "--pierce", "shadow.png"]),
            Command::ScreenshotEl { selector, pierce: true, file: Some(file), .. }
                if selector == SHADOW && file.as_os_str() == "shadow.png"
        ));

        assert!(Cli::try_parse_from(["rdny", "html", "--pierce"]).is_err());
        assert!(matches!(
            parse(&["rdny", "text", SHADOW]),
            Command::Text { selector, pierce: false } if selector == SHADOW
        ));
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

    #[test]
    fn parses_global_instance_and_rejects_cli_state_dir_conflict() {
        let cli = parse_cli(&["rdny", "status", "--instance", "agent-a"]);
        assert_eq!(cli.instance.as_deref(), Some("agent-a"));
        assert!(
            Cli::try_parse_from([
                "rdny",
                "--instance",
                "agent-a",
                "--state-dir",
                "/tmp/state",
                "status",
            ])
            .is_err()
        );
    }

    #[test]
    fn cli_instance_env_process_helper() {
        let Ok(mode) = std::env::var("RDNY_CLI_INSTANCE_ENV_HELPER") else {
            return;
        };
        match mode.as_str() {
            "env" => {
                let cli = parse_cli(&["rdny", "status"]);
                assert_eq!(cli.instance.as_deref(), Some("from-env"));
            }
            "override" => {
                let cli = parse_cli(&["rdny", "--instance", "from-cli", "status"]);
                assert_eq!(cli.instance.as_deref(), Some("from-cli"));
            }
            _ => panic!("unknown helper mode"),
        }
    }

    #[test]
    fn rdny_instance_is_equivalent_and_cli_overrides_it() {
        for mode in ["env", "override"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("cli::tests::cli_instance_env_process_helper")
                .arg("--exact")
                .env("RDNY_INSTANCE", "from-env")
                .env("RDNY_CLI_INSTANCE_ENV_HELPER", mode)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "helper mode {mode} failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn effective_instance_conflicts_with_environment_state_dir() {
        let error = validate_selection_inputs(
            Some("agent-a"),
            None,
            Some(std::ffi::OsStr::new("/tmp/state")),
            true,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("conflicts with --state-dir/RDNY_STATE_DIR"));

        assert!(
            validate_selection_inputs(
                None,
                Some(std::path::Path::new("/cli")),
                Some(std::ffi::OsStr::new("/env")),
                true,
            )
            .is_ok()
        );
    }

    #[test]
    fn non_target_commands_reject_instance_selector() {
        for command in [
            parse(&["rdny", "start"]),
            parse(&["rdny", "connect", "127.0.0.1:9222"]),
            parse(&["rdny", "list"]),
            parse(&["rdny", "cleanup"]),
            parse(&["rdny", "completion", "bash"]),
            parse(&["rdny", "sleep", "1"]),
        ] {
            assert!(!command.targets_registered_instance());
            let error = validate_selection_inputs(Some("agent-a"), None, None, false).unwrap_err();
            assert!(format!("{error:#}").contains("does not apply to this command"));
        }
        for command in [
            parse(&["rdny", "status"]),
            parse(&["rdny", "stop"]),
            parse(&["rdny", "title"]),
            parse(&["rdny", "screenshot"]),
            parse(&["rdny", "pages"]),
            parse(&["rdny", "start-video"]),
            parse(&["rdny", "stop-video"]),
            parse(&["rdny", "key", "Enter"]),
            parse(&["rdny", "pointer", "move", "--at", "1,2"]),
        ] {
            assert!(command.targets_registered_instance());
        }
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
