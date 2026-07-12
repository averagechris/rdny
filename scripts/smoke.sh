#!/usr/bin/env bash
# Aggregate and named smoke-scenario entry point.

set -euo pipefail

scenario=
binary=
while [[ $# -gt 0 ]]; do
  case $1 in
    --scenario)
      [[ $# -ge 2 ]] || { echo 'smoke: --scenario requires a name' >&2; exit 2; }
      scenario=$2
      shift 2
      ;;
    --list-scenarios)
      printf '%s\n' lifecycle-navigation output-artifacts trusted-input-browser-programs video-tabs attached-cdp
      exit 0
      ;;
    *)
      [[ -z $binary ]] || { echo "smoke: unexpected argument: $1" >&2; exit 2; }
      binary=$1
      shift
      ;;
  esac
done

run_scenario() {
  local name=$1
  local script="scripts/smoke/$name.sh"
  [[ -f $script ]] || { echo "smoke: unknown scenario: $name" >&2; exit 2; }
  echo "smoke: scenario: $name"
  if [[ -n ${RDNY_SMOKE_ROUTE_ONLY:-} ]]; then
    local route_dir=${RDNY_SMOKE_WORKDIR:-$(mktemp -d)}/$name
    mkdir -p "$route_dir"
    printf 'routed scenario: %s\n' "$name" >"$route_dir/smoke.log"
    printf '%s\n' "${RDNY_SMOKE_FAKE_STATUS:-0}" >"$route_dir/status"
    return "${RDNY_SMOKE_FAKE_STATUS:-0}"
  fi
  if [[ -n ${RDNY_SMOKE_WORKDIR:-} ]]; then
    mkdir -p "$RDNY_SMOKE_WORKDIR/$name"
    if [[ -n $binary ]]; then
      RDNY_SMOKE_WORKDIR="$RDNY_SMOKE_WORKDIR/$name" bash "$script" "$binary"
    else
      RDNY_SMOKE_WORKDIR="$RDNY_SMOKE_WORKDIR/$name" bash "$script"
    fi
  elif [[ -n $binary ]]; then
    bash "$script" "$binary"
  else
    bash "$script"
  fi
}

if [[ -n $scenario ]]; then
  run_scenario "$scenario"
else
  run_scenario lifecycle-navigation
  run_scenario output-artifacts
  run_scenario trusted-input-browser-programs
  run_scenario video-tabs
fi
