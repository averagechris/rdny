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
nix run .#static-checks
nix run .#release -- --version X.Y.Z --check
nix run .#release -- --version X.Y.Z
```

The preflight is non-mutating and fails fast unless the requested GitHub tag is
available and the empty jj working-copy commit's parent, local `main`, and
`main@origin` agree. The release runs fmt/clippy/test plus deny, machete, sort,
the evaluated help/docs contract, and `ci-release-facing`, then atomically
publishes `main` and the annotated tag. A read-only GitHub workflow builds and
verifies the unbundled CLI archive/checksum pairs for both platforms. Follow
`docs/release.md` to verify identities and checksums, manually publish four
assets, and refresh Pages. Browser-backed smoke remains a separate gate.

`.builds/ci.yml` still runs automatically on SourceHut pushes. Historical
SourceHut assets and `builds/release-linux-x86_64.yml` are archival only: do not
submit that manifest or dual-publish a new release.
