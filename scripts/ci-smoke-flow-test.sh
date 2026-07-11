#!/usr/bin/env bash
# Exercise ci-smoke/ci-smoke-gate status propagation without launching a browser.

set -euo pipefail

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

export RDNY_SMOKE_WORKDIR="$tmp/success"
RDNY_SMOKE_FAKE_STATUS=0 scripts/ci-smoke.sh /bin/true
scripts/ci-smoke-gate.sh
grep -q 'fake smoke evidence line' "$RDNY_SMOKE_WORKDIR/smoke.log"
command -v python3 >/dev/null
python3 - <<'PY'
import json
assert json.loads('{"schemaVersion":1}')["schemaVersion"] == 1
PY

export RDNY_SMOKE_WORKDIR="$tmp/failure"
RDNY_SMOKE_FAKE_STATUS=7 scripts/ci-smoke.sh /bin/false
if scripts/ci-smoke-gate.sh; then
  printf 'expected ci-smoke-gate to fail for recorded failure\n' >&2
  exit 1
fi
[[ $(<"$RDNY_SMOKE_WORKDIR/status") == 7 ]]
grep -q 'fake smoke evidence line' "$RDNY_SMOKE_WORKDIR/smoke.log"

printf 'ci-smoke flow test: PASS\n'
