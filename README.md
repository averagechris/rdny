# rdny

Chrome automation CLI in Rust, deeply inspired by
[rodney](https://github.com/simonw/rodney) by Simon Willison.

rdny is a clean-room rewrite: it shares rodney's command-line surface and
persistent-browser philosophy, but no code. Where rodney wraps
[go-rod](https://github.com/go-rod/rod), rdny speaks the Chrome DevTools
Protocol from Rust.

## Browser provisioning

The default `rdny` package does not bundle a browser. At runtime rdny looks for
Chrome/Chromium in this order:

1. `RDNY_CHROME`, when set.
2. `binaries.chrome` in the rdny config file.
3. Well-known macOS and Linux Chrome/Chromium locations.
3. A clear error with the paths it tried and instructions for setting
   `RDNY_CHROME` or `binaries.chrome`.

Set `RDNY_CHROME_ARGS` to append extra launch flags when rdny starts Chrome, for
example:

```sh
RDNY_CHROME=/Applications/Google\ Chrome.app/Contents/MacOS/Google\ Chrome \
RDNY_CHROME_ARGS="--headless=new --disable-gpu" \
rdny screenshot https://example.com out.png
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
chrome = "/Applications/Helium.app/Contents/MacOS/Helium"
ffmpeg = "/opt/homebrew/bin/ffmpeg"

[connect]
default = "helium"

[connect.targets]
helium = "127.0.0.1:9333"
```

Binary path precedence is environment first, then config, then the built-in
fallback: Chrome uses `RDNY_CHROME` > `binaries.chrome` > well-known discovery;
ffmpeg uses `RDNY_FFMPEG` > `binaries.ffmpeg` > `ffmpeg` from `PATH`.

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
```

Both sessions coexist because state is separated. Attached sessions record no
browser pid, so `rdny stop` and `rdny cleanup` never kill your personal browser;
they only detach rdny from it or clear rdny state.

### State, discovery, and lifecycle safety

Every state directory successfully used by `start` or `connect` is recorded in
an owner-private persistent registry below the default rdny state directory.
Consequently `rdny list` and `rdny cleanup` discover arbitrary custom
`--state-dir` sessions after the shell that created them exits; they do not scan
`/tmp`. For example, the custom session above can later be managed with:

```sh
rdny list
rdny --state-dir ~/.local/state/rdny-personal status
rdny --state-dir ~/.local/state/rdny-personal stop
```

State and registry updates use bounded interprocess locks and atomic, synced
owner-only files. State, profile, log, and frame directories must be owned by
the effective user, must not be symlinks, and must not grant group or other
permissions. rdny rejects unsafe pre-existing paths rather than weakening
their permissions. New directories and files use modes `0700` and `0600`.

`status` and `list` report malformed or truncated state instead of hiding it.
`cleanup` moves malformed `state.json` files to a unique
`state.corrupt-*` file in the same directory and prunes missing, malformed, or
unsafe registry entries. `start` and `connect` refuse corrupt state until it is
cleaned, and refuse to replace either a live rdny-managed browser or a live
attached browser; run `rdny stop` first.

New managed-session state records a process birth token plus executable/profile
correlation. rdny validates all of these immediately before sending a signal.
For compatibility, old state without this identity still loads: if its PID is
dead, cleanup can remove it, but if that PID is live `rdny stop` refuses to
signal it and preserves state. Stop that browser manually, then run
`rdny cleanup`.

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
