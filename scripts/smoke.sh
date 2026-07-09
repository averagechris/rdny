#!/usr/bin/env bash
# End-to-end smoke test for rdny against a real local Chrome/Chromium.
# Covers the ticket #76 acceptance path:
#   start -> open a local test page -> click/input/text/screenshot -> stop
# plus the rest of the parity surface at a smoke level.
#
# Usage: scripts/smoke.sh [path-to-rdny-binary]
# With no argument, runs via `cargo run -q --`.

set -euo pipefail

if [[ $# -ge 1 ]]; then
  RDNY=("$1")
else
  RDNY=(cargo run -q --)
fi

workdir=${RDNY_SMOKE_WORKDIR:-$(mktemp -d)}
export RDNY_STATE_DIR="$workdir/state"
failed=0

cleanup() {
  local status=$?
  if [[ $status -ne 0 || $failed -ne 0 ]]; then
    printf 'smoke: preserving workdir after failure: %s\n' "$workdir" >&2
    "${RDNY[@]}" status >"$workdir/final-status.txt" 2>&1 || true
  else
    "${RDNY[@]}" stop >/dev/null 2>&1 || true
    if [[ -z ${RDNY_SMOKE_WORKDIR:-} ]]; then
      rm -rf "$workdir"
    fi
  fi
}
trap cleanup EXIT

fail() {
  failed=1
  echo "smoke: FAIL: $*" >&2
  exit 1
}

expect() {
  local desc=$1 want=$2 got=$3
  if [[ "$got" != "$want" ]]; then
    fail "$desc: expected '$want', got '$got'"
  fi
  echo "smoke: ok: $desc"
}

expect_contains() {
  local desc=$1 want=$2 got=$3
  if [[ "$got" != *"$want"* ]]; then
    fail "$desc: expected to contain '$want', got '$got'"
  fi
  echo "smoke: ok: $desc"
}

page="$workdir/page.html"
cat >"$page" <<'HTML'
<!DOCTYPE html>
<html>
  <head><title>rdny smoke</title></head>
  <body>
    <h1 id="heading">Smoke Page</h1>
    <a id="link" href="https://example.com/dl">a link</a>
    <button id="btn" onclick="document.title = 'clicked'">press</button>
    <form id="f" onsubmit="event.preventDefault(); document.title = 'submitted'">
      <input id="name" type="text">
      <select id="pet">
        <option value="cat">cat</option>
        <option value="dog">dog</option>
      </select>
    </form>
  </body>
</html>
HTML

# --- lifecycle -------------------------------------------------------
"${RDNY[@]}" start
expect_contains "status running" "running:" "$("${RDNY[@]}" status)"

# --- navigation + waiting -------------------------------------------
"${RDNY[@]}" open "file://$page"
"${RDNY[@]}" wait "#heading"
"${RDNY[@]}" waitload
"${RDNY[@]}" waitstable
"${RDNY[@]}" waitidle
expect "url" "file://$page" "$("${RDNY[@]}" url)"
expect "title" "rdny smoke" "$("${RDNY[@]}" title)"

# --- page info -------------------------------------------------------
expect "text" "Smoke Page" "$("${RDNY[@]}" text '#heading')"
expect "attr" "https://example.com/dl" "$("${RDNY[@]}" attr '#link' href)"
expect_contains "html selector" '<h1 id="heading">' "$("${RDNY[@]}" html '#heading')"
expect_contains "html page" "</html>" "$("${RDNY[@]}" html)"

# --- interaction -----------------------------------------------------
"${RDNY[@]}" click "#btn"
expect "click fired" "clicked" "$("${RDNY[@]}" js 'document.title')"
"${RDNY[@]}" input "#name" "hello smoke"
expect "input typed" "hello smoke" "$("${RDNY[@]}" js "document.querySelector('#name').value")"
"${RDNY[@]}" clear "#name"
expect "input cleared" "" "$("${RDNY[@]}" js "document.querySelector('#name').value")"
"${RDNY[@]}" select "#pet" dog
expect "select" "dog" "$("${RDNY[@]}" js "document.querySelector('#pet').value")"
"${RDNY[@]}" focus "#name"
expect "focus" "name" "$("${RDNY[@]}" js 'document.activeElement.id')"
"${RDNY[@]}" hover "#btn"
"${RDNY[@]}" submit "#f"
expect "submit" "submitted" "$("${RDNY[@]}" js 'document.title')"
expect "js math" "3" "$("${RDNY[@]}" js '1+2')"

# --- screenshots + pdf ----------------------------------------------
"${RDNY[@]}" screenshot "$workdir/shot.png" >/dev/null
LC_ALL=C grep -a -q 'PNG' "$workdir/shot.png" || fail "screenshot is not a PNG"
echo "smoke: ok: screenshot"
"${RDNY[@]}" screenshot-el "#heading" "$workdir/shot-el.png" >/dev/null
LC_ALL=C grep -a -q 'PNG' "$workdir/shot-el.png" || fail "screenshot-el is not a PNG"
echo "smoke: ok: screenshot-el"
"${RDNY[@]}" pdf "$workdir/page.pdf" >/dev/null
LC_ALL=C grep -a -q '%PDF' "$workdir/page.pdf" || fail "pdf is not a PDF"
echo "smoke: ok: pdf"

# --- video ------------------------------------------------------------
"${RDNY[@]}" start-video
"${RDNY[@]}" open "file://$page"
"${RDNY[@]}" wait "#heading"
"${RDNY[@]}" click "#btn"
"${RDNY[@]}" screenshot "$workdir/video-frame.png" >/dev/null
"${RDNY[@]}" stop-video "$workdir/smoke.mp4" >/dev/null
[[ -s "$workdir/smoke.mp4" ]] || fail "video was not created"
LC_ALL=C grep -a -q 'ftyp' "$workdir/smoke.mp4" || fail "video is not an MP4"
echo "smoke: ok: video"

# --- tabs ------------------------------------------------------------
"${RDNY[@]}" newpage "file://$page" >/dev/null
expect_contains "pages lists two" "1: " "$("${RDNY[@]}" pages)"
"${RDNY[@]}" page 0 >/dev/null
expect_contains "pages marker moved" "* 0:" "$("${RDNY[@]}" pages)"

# --- error path ------------------------------------------------------
if "${RDNY[@]}" --timeout 1 wait "#never-exists" 2>/dev/null; then
  fail "wait for missing element should fail"
fi
echo "smoke: ok: wait timeout exits nonzero"

# --- shutdown --------------------------------------------------------
"${RDNY[@]}" stop
expect "status cleared" "no session" "$("${RDNY[@]}" status)"

echo "smoke: PASS"
