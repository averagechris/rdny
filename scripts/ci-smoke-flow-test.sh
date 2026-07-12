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
import pathlib
assert json.loads('{"schemaVersion":1}')["schemaVersion"] == 1
for path in pathlib.Path("scripts").glob("check-smoke-*.py"):
    compile(path.read_text(), str(path), "exec")
PY
for helper in scripts/smoke/lib.sh scripts/check-smoke-architecture.py scripts/check-smoke-artifacts.py scripts/check-smoke-download.py; do
  [[ -s $helper ]]
done
scenarios=$(scripts/smoke.sh --list-scenarios)
for scenario in lifecycle-navigation output-artifacts trusted-input-browser-programs video-tabs attached-cdp; do
  [[ $scenarios == *"$scenario"* ]]
  route_dir="$tmp/routes"
  RDNY_SMOKE_ROUTE_ONLY=1 RDNY_SMOKE_WORKDIR="$route_dir" scripts/smoke.sh --scenario "$scenario" /bin/true
  [[ $(<"$route_dir/$scenario/status") == 0 ]]
  grep -q "routed scenario: $scenario" "$route_dir/$scenario/smoke.log"
done
if RDNY_SMOKE_ROUTE_ONLY=1 RDNY_SMOKE_FAKE_STATUS=7 RDNY_SMOKE_WORKDIR="$tmp/route-failure" \
  scripts/smoke.sh --scenario lifecycle-navigation /bin/false; then
  printf 'expected routed scenario status propagation to fail\n' >&2
  exit 1
fi
[[ $(<"$tmp/route-failure/lifecycle-navigation/status") == 7 ]]
if scripts/smoke.sh --scenario unknown /bin/true >/dev/null 2>&1; then
  printf 'expected unknown named smoke scenario to fail\n' >&2
  exit 1
fi

export RDNY_SMOKE_WORKDIR="$tmp/failure"
RDNY_SMOKE_FAKE_STATUS=7 scripts/ci-smoke.sh /bin/false
if scripts/ci-smoke-gate.sh; then
  printf 'expected ci-smoke-gate to fail for recorded failure\n' >&2
  exit 1
fi
[[ $(<"$RDNY_SMOKE_WORKDIR/status") == 7 ]]
grep -q 'fake smoke evidence line' "$RDNY_SMOKE_WORKDIR/smoke.log"

printf 'ci-smoke flow test: PASS\n'
