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
2. Well-known macOS and Linux Chrome/Chromium locations.
3. A clear error with the paths it tried and instructions for setting
   `RDNY_CHROME`.

Set `RDNY_CHROME_ARGS` to append extra launch flags when rdny starts Chrome, for
example:

```sh
RDNY_CHROME=/Applications/Google\ Chrome.app/Contents/MacOS/Google\ Chrome \
RDNY_CHROME_ARGS="--headless=new --disable-gpu" \
rdny screenshot https://example.com out.png
```

Linux/Nix users can opt into a browser-containing closure with
`.#rdny-bundled`, which wraps rdny with Nixpkgs `ungoogled-chromium` and sets
`RDNY_CHROME` only as a default:

```sh
nix run .#rdny-bundled -- --help
```

`rdny-bundled` is exposed on Linux systems only; Darwin builds use system
Chrome/Chromium discovery or an explicit `RDNY_CHROME` path.

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
