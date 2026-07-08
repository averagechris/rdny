# rdny project guidance

Use `jj` for version-control actions in this repository.

## Development

- Enter the toolchain with `direnv allow` or `nix develop`.
- Nix formatting uses wrapped `alejandra -q`; run `nix fmt` or `nix fmt -- --check .`.
- Prefer local checks: `nix run .#static-checks` (fmt + clippy), `nix run .#ci-test`, `nix run .#ci-machete`, `nix run .#ci-sort`, `nix run .#ci-deny`, `nix run .#ci-audit`.
- `.builds/ci.yml` runs fmt, clippy, test, and the package build on every push.

## sccache

The host sets `RUSTC_WRAPPER=sccache` globally, and it must never be unset to "fix"
build failures. If builds fail with sccache connection or compiler errors, run
`sccache --stop-server` and retry; the supervised launchd agent restarts a healthy
server.

## Release workflow

This repo uses the standard averagechris fleet interface:

```sh
nix run .#prepare-release -- --version X.Y.Z
nix run .#release-tag
nix build .#release-artifact
nix run .#static-checks
nix run .#release -- --version X.Y.Z --submit-linux-build
```

`.builds/ci.yml` runs automatically on every push. `builds/release-linux-x86_64.yml`
is explicit-submit only; do not move it to `.builds/`.
