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
ffmpeg uses `RDNY_FFMPEG` > `binaries.ffmpeg` > `ffmpeg` from `PATH`. The
`connect` section is parsed now for named targets; command wiring will arrive in
a follow-up release.

>>>>>>> conflict 1 of 1 ends
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
