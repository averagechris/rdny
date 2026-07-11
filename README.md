# rdny

Chrome automation CLI in Rust, deeply inspired by
[rodney](https://github.com/simonw/rodney) by Simon Willison.

rdny is a clean-room rewrite: it shares rodney's command-line surface and
persistent-browser philosophy, but no code. Where rodney wraps
[go-rod](https://github.com/go-rod/rod), rdny speaks the Chrome DevTools
Protocol from Rust.

## Installation

rdny currently builds through the Nix flake for `x86_64-linux`,
`aarch64-linux`, `x86_64-darwin`, and `aarch64-darwin`. SourceHut CI currently
runs Linux x86_64 jobs for the full check and browser-smoke path and evaluates
all four system outputs. Native aarch64 and Darwin CI runners are not currently
part of the automatic build matrix.

The most reproducible install path is Nix:

```sh
nix run git+https://git.sr.ht/~averagechris/rdny -- --help
nix profile install git+https://git.sr.ht/~averagechris/rdny#rdny
```

Release artifacts contain `rdny`, `README.md`, `CHANGELOG.md`, and `LICENSE`.
They are produced by the Nix release workflow for matching Nix systems and are
not currently promised as portable, copy-to-`PATH` binaries for hosts without
the required Nix runtime closure. For Linux x86_64 releases, CI extracts the
tarball, verifies the SHA-256 sidecar, and runs `rdny --version` and
`rdny --help` outside the build directory before upload. Inspect one with:

```sh
sha256sum -c rdny-v0.1.0-x86_64-linux.tar.gz.sha256
tar -xzf rdny-v0.1.0-x86_64-linux.tar.gz
rdny-v0.1.0-x86_64-linux/rdny --version
```

From source, use the Rust toolchain supplied by the flake or your local Cargo:

```sh
nix develop
cargo build --release
target/release/rdny --help
```

rdny needs a Chrome/Chromium-compatible browser for commands that launch or
inspect pages. Browser-backed CI currently uses Nixpkgs `ungoogled-chromium` on
Linux x86_64; local development has also been smoke-tested with Helium on macOS.
Other Chrome/Chromium-compatible builds should work through the Chrome DevTools
Protocol, but are not yet part of the automated support matrix. Video export
additionally requires ffmpeg. Provide paths with
`RDNY_CHROME` and `RDNY_FFMPEG`, config file entries, or use the Nix wrappers
described below.

The smoke wrapper records status and evidence, prints captured smoke logs inline
on failure, exits successfully, and lets a following gate task fail the build
from the recorded status. Smoke evidence is retained in the SourceHut task log;
the provider's directory-artifact upload is not used because it is unreliable for
this browser-backed job.

## Quickstart

These commands use the packaged binary shape, so they work with `nix run`, a
profile install, or `result/bin/rdny` after `nix build`:

```sh
rdny start
rdny open https://example.com
rdny title
rdny text h1
rdny screenshot example.png
rdny stop
```

### Selecting elements in open shadow DOM

Element commands use ordinary CSS selectors by default. To traverse open shadow
roots explicitly, pass `--pierce` and separate each host/target CSS selector with
`>>>`:

```sh
rdny wait --pierce 'app-shell >>> user-card >>> button.save'
rdny text --pierce 'app-shell >>> user-card >>> .status'
rdny click --pierce 'app-shell >>> user-card >>> button.save'
```

With `--pierce`, rdny queries the first segment in the document, enters that
element's open `shadowRoot`, queries the next segment there, and repeats. This is
deterministic for nested open roots and retains first-match CSS semantics within
each root. Every segment must be non-empty and is validated as CSS in its own
root. Errors identify the failing host or target segment, invalid CSS, a missing
match, or a host that does not expose an open root.

The `>>>` delimiter has no special meaning without `--pierce`; the complete
argument is passed to `document.querySelector` as ordinary CSS. This preserves
the default CSS interface and prevents accidental traversal. Shadow traversal is
available on `wait`, `html` (when given a selector), `text`, `attr`, `click`,
`input`, `clear`, `hover`, `focus`, `submit`, `select`, `screenshot-el`, `file`,
and `download`.

Closed shadow roots are intentionally unsupported because page JavaScript cannot
access them. Shadow traversal also does not cross iframe boundaries; in
particular, cross-origin frames are unsupported. Selectors operate only in the
current page document.

Use an isolated state directory while experimenting or in tests:

```sh
export RDNY_STATE_DIR="$(mktemp -d)"
rdny start --label quickstart
rdny open https://example.com
rdny status
rdny stop
```

### Selecting registered instances

Every started or connected lifecycle is published in the owner-private global
instance registry. `rdny list` prints a stable `selector=<instance-id>` for each
registered instance. Target status, stop, page, artifact, tab, and recording
commands with that id or with a unique exact label:

```sh
rdny --state-dir /tmp/rdny-work start --label work
rdny --state-dir /tmp/rdny-docs start --label docs
rdny list
rdny --instance work open https://example.com
rdny --instance work screenshot work.png
rdny --instance 0000019abc... status
RDNY_INSTANCE=docs rdny title
```

An exact instance id wins if it is also another instance's label. Duplicate
labels fail and print every matching id and state directory so the command can
be retried unambiguously. Selection verifies the secured registry entry against
the selected `state.json`; missing, malformed, stale, inaccessible, or unrelated
state fails before browser dispatch or mutation.

Selector precedence is intentionally strict:

1. CLI `--instance` overrides `RDNY_INSTANCE`.
2. Any effective instance selector conflicts with either CLI `--state-dir` or
   `RDNY_STATE_DIR`; rdny never silently chooses one addressing mechanism.
3. With no instance selector, CLI `--state-dir` continues to override
   `RDNY_STATE_DIR`, which continues to override the platform default.

`--state-dir` remains the low-level destination override for `start` and
`connect`. Instance selectors apply only to commands targeting an existing
instance. `start`, `connect`, `list`, `cleanup`, `completion`, and `sleep` reject
an effective selector instead of ignoring it; unset `RDNY_INSTANCE` when using
those commands.

## Browser provisioning

### Managed-session isolation

`rdny start` does **not** open a DevTools TCP port. It starts the exact current
`rdny` executable in a hidden broker mode through an inherited anonymous startup
socket, then launches Chrome with `--remote-debugging-pipe`. The broker keeps the
Chrome child handle and both pipe ends for the browser's lifetime. Commands use
an owner-only Unix socket and authenticate its filesystem owner/mode, OS peer
UID, broker process identity, protocol version, instance id, and a random token
stored only in the private state file. There is no managed TCP or WebSocket
fallback and no `DevToolsActivePort` file.

Startup is armed until state and the instance registry are atomically published:
the CLI then sends commit and requires the broker's explicit acknowledgement as
the transaction's final fallible action. If the starting CLI exits, either
publication write fails, or commit/ack fails, exact state and registry snapshots
are restored and the armed guard closes Chrome and removes its socket. Killing a later CLI does not stop the browser;
killing the broker closes Chrome's sole debugging pipe and the broker-owned child
is fail-closed. `stop` authenticates to the broker, asks Chrome to close, and the
broker escalates through its owned `Child` handle if needed. Multiple command
processes, including `logs --follow`, may connect concurrently; per-client event
queues are bounded and target sessions are private to their creating client.
Queue-full, EOF, authentication, and socket-write disconnect paths all remove
routing and detach client-owned target sessions, including late attach replies.

Managed launch rejects every user-provided `--remote-debugging-port` or
`--remote-debugging-pipe` argument. Remove those flags from `RDNY_CHROME_ARGS` or
configuration; only the single broker-owned pipe flag is permitted.

Legacy managed state containing a loopback debug port remains readable and can
be stopped or cleaned during migration. Newly started sessions never use it.
External `rdny connect` intentionally remains the separately hardened loopback
HTTP/WebSocket mode described below.

The broker is detached from the invoking terminal session with `setsid`, and its
stdout/stderr share the owner-only `chrome.log`. Terminal interrupt or a client
logout therefore affects that client, not the managed browser. This is not a
promise that every OS login-session manager will preserve detached processes:
administrative logout cleanup, reboot, SIGKILL, or resource pressure can kill the
broker, in which case Chrome is expected to exit when its pipe closes. See
[`docs/threat-model.md`](docs/threat-model.md).

The default `rdny` package does not bundle a browser. At runtime rdny looks for
Chrome/Chromium in this order:

1. `RDNY_CHROME`, when set.
2. `binaries.chrome` paths in the rdny config file, in listed order.
3. Well-known macOS and Linux Chrome/Chromium locations.
4. A clear error with the paths it tried and instructions for setting
   `RDNY_CHROME` or `binaries.chrome`.

Set `RDNY_CHROME_ARGS` to append extra launch flags when rdny starts Chrome, for
example:

```sh
RDNY_CHROME=/Applications/Google\ Chrome.app/Contents/MacOS/Google\ Chrome \
RDNY_CHROME_ARGS="--headless=new --disable-gpu" \
rdny start --label docs
```

Linux/Nix users can opt into a browser-containing closure with
`.#rdny-bundled`, which wraps rdny with Nixpkgs `ungoogled-chromium` and
`ffmpeg-headless`, setting `RDNY_CHROME` and `RDNY_FFMPEG` only as defaults:

```sh
nix run .#rdny-bundled -- --help
```

`rdny-bundled` is exposed on Linux systems only; Darwin builds use system
Chrome/Chromium discovery or an explicit `RDNY_CHROME` path.

The `stop-video` command shells out to ffmpeg. By default rdny resolves ffmpeg
from configuration or `PATH`; set `RDNY_FFMPEG` to force a specific binary:

```sh
RDNY_FFMPEG=/opt/homebrew/bin/ffmpeg rdny stop-video video.mp4
```

Commands that create artifacts (`download`, `pdf`, `screenshot`,
`screenshot-el`, and `stop-video`) refuse to overwrite existing paths by
default, including symlinks. Pass `--force` to replace an output intentionally.
`download SELECTOR FILE` writes a protected artifact; omitting `FILE` (or using
`-`) streams raw bytes to stdout.
Downloads perform a best-effort `HEAD` preflight and reject payloads larger than
256 MiB by default before fetching when `Content-Length` is available. Use
`rdny download --max-bytes N ...` or `RDNY_MAX_DOWNLOAD_BYTES=N` to tune this.
Fetched chunks are decoded and written incrementally to the reserved artifact (or
stdout) instead of materializing additional full output copies in Rust.

Machine-readable command output is selected globally with
`--format human|json|jsonl`. Supported schemas are versioned with
`schemaVersion: 1` and documented in [`docs/cli-output-schemas.md`](docs/cli-output-schemas.md).
Human page/content output strips terminal-control bytes before printing, and
warnings/progress are written to stderr so structured stdout remains parseable.
Use `rdny status --check` as a health check, and `rdny completion SHELL` to
generate shell completions.

`rdny file SELECTOR -` reads upload data from stdin into a private temporary file
with owner-only permissions, keeps it until Chrome accepts the upload, and then
removes it on success or normal error. Stdin uploads are limited to 64 MiB; pass
a real file path for larger payloads.

Each `start-video` creates a recording id and writes frames under a dedicated
state subdirectory (`recordings/<id>/frames`). A second `start-video` is rejected
while a recording is active. `stop-video` marks the active recording inactive and
adds it to recoverable state before assembly, so browser disconnects, empty
captures, and ffmpeg failures do not mix frame sets or block a new recording. If
several recordings are recoverable, `stop-video` retries the oldest recoverable
recording first; the error text prints the selected id and frame directory.
Successful assembly clears that id from state before best-effort frame deletion,
so a deletion warning leaves an explicit manual cleanup path without state
pointing at deleted frames. Older state using the legacy singular
`recoverable_recording` field or `frames/` directory is still readable.
Recording capture is bounded: each incoming screencast frame is size-checked
before decode, and active recordings stop into recoverable state when frame
count, duration, total frame bytes, or minimum free disk quotas are exceeded.
Tune with `RDNY_MAX_SCREENCAST_FRAME_BYTES` (default 8 MiB),
`RDNY_MAX_RECORDING_FRAMES` (18,000), `RDNY_MAX_RECORDING_SECONDS` (1,800),
`RDNY_MAX_RECORDING_BYTES` (512 MiB), and `RDNY_MIN_FREE_DISK_BYTES` (256 MiB).

All Nix systems can opt into an ffmpeg-containing closure without a bundled
browser using `.#rdny-ffmpeg`:

```sh
nix run .#rdny-ffmpeg -- --help
```

## Configuration

rdny reads an optional TOML config file from the first configured location:

1. `RDNY_CONFIG`, interpreted as the exact config file path.
2. `$XDG_CONFIG_HOME/rdny/config.toml`.
3. `~/.config/rdny/config.toml` on all platforms, including macOS.

All fields are optional, and unknown keys are rejected so typos fail loudly:

```toml
[binaries]
chrome = [
  "/Applications/Helium.app/Contents/MacOS/Helium",
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
]
ffmpeg = "/opt/homebrew/bin/ffmpeg"

[connect]
default = "helium"

[connect.targets]
helium = "127.0.0.1:9333"
```

Binary path precedence is environment first, then config, then the built-in
fallback: Chrome uses `RDNY_CHROME` > `binaries.chrome` (an ordered array of
paths) > well-known discovery; ffmpeg uses `RDNY_FFMPEG` > `binaries.ffmpeg` >
`ffmpeg` from `PATH`. For compatibility, `binaries.chrome = "/path/to/chrome"`
is still accepted as a single-item list. When multiple configured or well-known
Chrome paths exist, rdny prefers the first and falls back to later paths if an
earlier candidate cannot be launched.

## Driving your own browser

`rdny connect` can attach to a browser you launched yourself, as long as that
browser was started with Chrome DevTools Protocol remote debugging enabled. CDP
can only be enabled at browser launch time. On macOS, for example:

```sh
open -na Helium --args --remote-debugging-port=9333
open -na "Google Chrome" --args --remote-debugging-port=9333
```

Chromium 136+ silently ignores `--remote-debugging-port` when using the default
user data directory, so some browser builds require a separate profile directory:

```sh
open -na Helium --args --remote-debugging-port=9333 --user-data-dir=/tmp/rdny-helium
```

Name personal browser targets in the rdny config file:

```toml
[connect]
default = "helium"

[connect.targets]
helium = "127.0.0.1:9333"
```

Then use `rdny connect helium`, or just `rdny connect` when `default` is set.
The default rdny state directory can continue to hold an isolated managed
browser, while a separate state directory tracks your personal browser:

```sh
rdny start --label agent
rdny --state-dir ~/.local/state/rdny-personal connect helium
rdny list
rdny --instance agent title
```

Both sessions coexist because state is separated. Attached sessions record no
browser pid, so `rdny stop` and `rdny cleanup` never kill your personal browser;
they only detach rdny from it or clear rdny state.

For security, debugger connections are loopback-only. `localhost`, names below
`.localhost`, and literal loopback addresses retain the convenient plaintext
HTTP/WS workflow above. rdny does not support direct remote CDP because Chrome's
discovery endpoint is unauthenticated and plaintext; even `--allow-remote` will
reject that downgrade and print tunnel guidance. Use an authenticated SSH
tunnel instead, then connect rdny to its local end:

```sh
ssh -N -L 9222:127.0.0.1:9222 browser-host.example
rdny connect 127.0.0.1:9222
```

The WebSocket URL returned by Chrome must use `ws://` and match the approved
host and port (equivalent loopback spellings are accepted). Redirects and
cross-host/cross-port debugger URLs are rejected.

## Transport and URL safety limits

DevTools HTTP responses are bounded to 32 KiB of headers and 8 MiB of decoded
body data, with bounded chunk metadata/trailers. CDP WebSockets allow at most 2
MiB per frame, 8 MiB per message, and 1,024 events buffered while waiting for a
command response.

`--timeout` accepts 0.001 through 86400 seconds. A command creates one monotonic
deadline: discovery, TCP connection, HTTP, WebSocket handshake and I/O, CDP
setup/calls/events, waits, recording acknowledgement, and shutdown all consume
that same budget rather than starting fresh timers. Under normal local scheduler
load, a blocked transport returns within 250 ms of the requested deadline (the
tests use this tolerance); non-network filesystem and process cleanup can add
small platform-dependent overhead. `logs --duration` remains the explicit
capture budget when supplied, `logs --follow` remains unbounded after bounded
setup, and wait quiet-window values are unchanged.

Navigation uses standards-based URL parsing. Bare `localhost` and
`*.localhost` targets intentionally become `http://` for local development;
other bare hosts become `https://`. Control characters, malformed URLs,
embedded credentials, and unsupported schemes are rejected. `file:`, `data:`,
RFC1918/ULA literals, link-local literals, and `.local` names require their
corresponding explicit flags: `--allow-file-url`, `--allow-data-url`,
`--allow-private-url`, `--allow-link-local-url`, or `--allow-local-url`.
Loopback remains allowed by default.

Chromium's internal pages are a separate privilege boundary and are denied by
default. Use `--allow-chrome-url` on `open` or a URL-bearing `newpage` to permit
a trusted `chrome://` page. Use the separate `--allow-chrome-extension-url`
flag for `chrome-extension://` pages: those pages can execute with the installed
extension's permissions, not ordinary web-page permissions. The URL authority
must be the extension's exact 32-character lowercase ID (letters `a` through
`p`), for example
`chrome-extension://abcdefghijklmnopabcdefghijklmnop/page.html`. The flag does
not install or load an extension; an unknown or unavailable ID can still be
rejected by Chromium. Do not pass either opt-in for untrusted input.

All URL policy checks happen before browser I/O. Pages can still redirect, and
hostname DNS can change after validation, so use an isolated browser
profile/network when automating untrusted pages. Internal-URL opt-ins do not
weaken the independent private-network, file, or data URL policies.

## Wait semantics

`rdny waitstable` waits for a quiet DOM window, not for two equal DOM snapshots.
It installs a page-side `MutationObserver` and succeeds once no observed
child-list, attribute, or character-data mutation has occurred for the quiet
window. This catches same-length replacements that fingerprint polling can miss.

`rdny waitidle` waits for a quiet network window using CDP `Network` events.
rdny enables the Network domain when it attaches to a page and installs a
page-resident fetch/XMLHttpRequest counter before `open`, `reload`, and
URL-bearing `newpage` navigations. A request started by one rdny command can
therefore keep the page non-idle when a later rdny command attaches and runs
`waitidle`. CDP events track document and subresource requests through
`loadingFinished` or `loadingFailed`, including redirects and failures; the page
counter tracks fetch/XHR that remain active across CLI process detach/reattach.
`waitidle` also requires the current document to be at least `interactive`.
The instrumentation is versioned and rdny records the CDP script identifier for
the current target so later attaches remove the prior registration when Chrome
accepts the identifier; the bootstrap also upgrades existing wrappers in-place so
an older registered script cannot keep winning after a newer rdny attaches.

Irreducible limits: traffic that began before rdny installed instrumentation and
enabled Network for the target cannot be reconstructed. WebSocket lifecycle
events are intentionally ignored by `waitidle`; CDP does not expose them as
ordinary request lifecycles and the page instrumentation does not wrap
`WebSocket`. HTTP(S) long-polls made with document loading, fetch, or XHR are
tracked and keep the page non-idle until Chrome/the page reports completion.
Service-worker-internal traffic that does not surface as page fetch/XHR or target
Network events is not counted.

Both commands default to a 500 ms quiet window and accept `--quiet-ms` to tune it:

```sh
rdny waitstable --quiet-ms 750
rdny waitidle --quiet-ms 250
```

## Development

```sh
direnv allow   # or: nix develop
nix run . -- --help
nix run .#ci-fmt
nix run .#ci-clippy
nix run .#static-checks
nix run .#ci-test
```

## Release

```sh
nix run .#release -- --version X.Y.Z --submit-linux-build
```

The shared release interface comes from
`git+https://git.sr.ht/~averagechris/averagechris.srht.site#lib.fleet.presets.rust`.
