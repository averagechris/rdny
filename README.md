# rdny

Chrome automation CLI in Rust, deeply inspired by
[rodney](https://github.com/simonw/rodney) by Simon Willison.

rdny is a clean-room rewrite: it shares rodney's command-line surface and
persistent-browser philosophy, but no code. Where rodney wraps
[go-rod](https://github.com/go-rod/rod), rdny speaks the Chrome DevTools
Protocol from Rust.

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
