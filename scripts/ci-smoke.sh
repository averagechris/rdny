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
export RDNY_SMOKE_WORKDIR=${RDNY_SMOKE_WORKDIR:-"$PWD/smoke-artifacts"}
rm -rf "$RDNY_SMOKE_WORKDIR"
mkdir -p "$RDNY_SMOKE_WORKDIR"

status=0
if [[ -n ${RDNY_SMOKE_FAKE_STATUS:-} ]]; then
  status=$RDNY_SMOKE_FAKE_STATUS
  {
    printf 'fake smoke status: %s\n' "$status"
    printf 'fake smoke evidence line\n'
  } >"$RDNY_SMOKE_WORKDIR/smoke.log" 2>&1
else
  {
    printf '$ %s --version\n' "$rdny"
    "$rdny" --version
    printf '$ %s --help >/dev/null\n' "$rdny"
    "$rdny" --help >/dev/null
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

# Never publish the browser profile or session state: they are large and may
# contain sensitive browser data. Keep the launch log as a small diagnostic.
if [[ $status -eq 0 && -d "$RDNY_SMOKE_WORKDIR/state" ]]; then
  if [[ -f "$RDNY_SMOKE_WORKDIR/state/chrome.log" ]]; then
    cp "$RDNY_SMOKE_WORKDIR/state/chrome.log" "$RDNY_SMOKE_WORKDIR/chrome.log"
  fi
  rm -rf "$RDNY_SMOKE_WORKDIR/state"
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
