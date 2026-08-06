#!/usr/bin/env bash
# CI wrapper for browser-backed packaged-binary smoke coverage.
# SourceHut runs this on Linux x86_64 with .#rdny-bundled so Chromium and
# ffmpeg are pinned by the Nix flake instead of ambient image packages.

set -euo pipefail

if [[ $# -ne 1 ]]; then
  printf 'usage: %s /path/to/rdny\n' "$0" >&2
  exit 2
fi

rdny=$1
artifact_dir=${RDNY_SMOKE_WORKDIR:-"$PWD/smoke-artifacts"}
rm -rf "$artifact_dir"
mkdir -p "$artifact_dir"
artifact_dir=$(cd "$artifact_dir" && pwd -P)

# AF_UNIX paths are especially short on macOS. Run in a short real directory
# (the secure state opener deliberately rejects symlinks), then copy the
# retained evidence to the requested CI artifact location on exit.
smoke_runtime_dir=$(mktemp -d "${TMPDIR:-/tmp}/rdny-smoke.XXXXXX")
cleanup_smoke_runtime() {
  cp -R "$smoke_runtime_dir"/. "$artifact_dir"/
  rm -rf "$smoke_runtime_dir"
}
trap cleanup_smoke_runtime EXIT
export RDNY_SMOKE_WORKDIR=$smoke_runtime_dir

status=0
if [[ -n ${RDNY_SMOKE_FAKE_STATUS:-} ]]; then
  status=$RDNY_SMOKE_FAKE_STATUS
  {
    printf 'fake smoke status: %s\n' "$status"
    command -v python3 >/dev/null
    python3 - <<'PY'
import json
assert json.loads('{"schemaVersion":1}')["schemaVersion"] == 1
PY
    printf 'fake smoke evidence line\n'
  } >"$RDNY_SMOKE_WORKDIR/smoke.log" 2>&1
else
  {
    printf '$ %s --version\n' "$rdny"
    "$rdny" --version
    printf '$ %s --help >/dev/null\n' "$rdny"
    "$rdny" --help >/dev/null
    printf '$ env -i PATH=/no-such %s --help >/dev/null\n' "$rdny"
    env -i PATH=/no-such HOME="${HOME:-/tmp}" "$rdny" --help >/dev/null
    printf '$ scripts/smoke.sh %s\n' "$rdny"
    scripts/smoke.sh "$rdny"
  } >"$RDNY_SMOKE_WORKDIR/smoke.log" 2>&1 || status=$?
fi

printf '%s\n' "$status" >"$RDNY_SMOKE_WORKDIR/status"
cp "$RDNY_SMOKE_WORKDIR/smoke.log" "$RDNY_SMOKE_WORKDIR/docs-demo-transcript.txt"
cat >>"$RDNY_SMOKE_WORKDIR/docs-demo-transcript.txt" <<EOF

Generated artifacts in this directory include shot.png, shot-el.png, page.pdf,
video-frame.png, and smoke.mp4 when smoke reaches those steps.
EOF

# Never publish browser profiles, session state, or registry data: they are
# large and may contain sensitive browser data. Keep launch logs as diagnostics.
if [[ $status -eq 0 ]]; then
  for name in state-a state-b; do
    if [[ -f "$RDNY_SMOKE_WORKDIR/$name/chrome.log" ]]; then
      cp "$RDNY_SMOKE_WORKDIR/$name/chrome.log" "$RDNY_SMOKE_WORKDIR/$name-chrome.log"
    fi
    rm -rf "$RDNY_SMOKE_WORKDIR/$name"
  done
  rm -rf "$RDNY_SMOKE_WORKDIR/xdg"
fi

if [[ $status -ne 0 ]]; then
  printf 'ci-smoke: recorded failure status %s; preserving artifacts in %s\n' "$status" "$RDNY_SMOKE_WORKDIR" >&2
  printf 'ci-smoke: begin captured smoke log\n' >&2
  cat "$RDNY_SMOKE_WORKDIR/smoke.log" >&2
  printf 'ci-smoke: end captured smoke log\n' >&2
else
  printf 'ci-smoke: PASS; artifacts in %s\n' "$RDNY_SMOKE_WORKDIR"
fi

# Deliberately exit success so SourceHut proceeds to ci-smoke-gate. Smoke
# evidence is retained in the always-addressable task log; directory artifact
# uploads are intentionally avoided because the provider rejects them
# inconsistently for this browser-backed job.
exit 0
