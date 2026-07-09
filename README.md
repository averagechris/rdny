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

Commands that create artifacts (`download`, `pdf`, `screenshot`,
`screenshot-el`, and `stop-video`) refuse to overwrite existing paths by
default, including symlinks. Pass `--force` to replace an output intentionally.
When `download` infers a filename from page-controlled URLs, rdny keeps the file
in the current directory and sanitizes separators, dotfiles, control characters,
`.`/`..`, and overlong names before creating it on Linux/macOS.

`rdny file SELECTOR -` reads upload data from stdin into a private temporary file
with owner-only permissions, keeps it until Chrome accepts the upload, and then
removes it on success or normal error. Stdin uploads are limited to 64 MiB; pass
a real file path for larger payloads.

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
