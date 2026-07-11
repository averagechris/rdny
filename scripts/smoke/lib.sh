#!/usr/bin/env bash
# Shared managed-browser scenario lifecycle and assertions.

smoke_init() {
  SMOKE_NAME=$1
  shift
  if [[ $# -ge 1 ]]; then RDNY=("$1"); else RDNY=(cargo run -q --); fi
  workdir=${RDNY_SMOKE_WORKDIR:-$(mktemp -d)}
  mkdir -p "$workdir"
  state="$workdir/state"
  export XDG_STATE_HOME="$workdir/xdg"
  unset RDNY_STATE_DIR RDNY_INSTANCE
  fixture=$(pwd)/scripts/fixtures/browser-programs.html
  trap smoke_cleanup EXIT
}

smoke_cleanup() {
  local status=$?
  "${RDNY[@]}" --state-dir "$state" stop >/dev/null 2>&1 || true
  if [[ $status -ne 0 ]]; then
    printf 'smoke: preserving %s evidence: %s\n' "$SMOKE_NAME" "$workdir" >&2
  elif [[ -z ${RDNY_SMOKE_WORKDIR:-} ]]; then
    rm -rf "$workdir"
  fi
}

smoke_start() {
  "${RDNY[@]}" --state-dir "$state" start --label "smoke-$SMOKE_NAME" >/dev/null
  R=("${RDNY[@]}" --state-dir "$state")
  "${R[@]}" open "file://$fixture" --allow-file-url >/dev/null
  "${R[@]}" wait '#heading'
}

expect() {
  local description=$1 expected=$2 actual=$3
  [[ $actual == "$expected" ]] || {
    printf "smoke: FAIL: %s: expected '%s', got '%s'\n" "$description" "$expected" "$actual" >&2
    return 1
  }
  printf 'smoke: ok: %s\n' "$description"
}

expect_contains() {
  local description=$1 expected=$2 actual=$3
  [[ $actual == *"$expected"* ]] || {
    printf "smoke: FAIL: %s: expected '%s' in '%s'\n" "$description" "$expected" "$actual" >&2
    return 1
  }
  printf 'smoke: ok: %s\n' "$description"
}

smoke_pass() { printf 'smoke: PASS: %s\n' "$SMOKE_NAME"; }
