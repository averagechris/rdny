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
RDNY_BASE=("${RDNY[@]}")

workdir=${RDNY_SMOKE_WORKDIR:-$(mktemp -d)}
export XDG_STATE_HOME="$workdir/xdg"
unset RDNY_STATE_DIR RDNY_INSTANCE
state_a="$workdir/state-a"
state_b="$workdir/state-b"
failed=0

cleanup() {
  local status=$?
  if [[ $status -ne 0 || $failed -ne 0 ]]; then
    printf 'smoke: preserving workdir after failure: %s\n' "$workdir" >&2
    "${RDNY_BASE[@]}" --state-dir "$state_a" status >"$workdir/final-status.txt" 2>&1 || true
  else
    "${RDNY_BASE[@]}" --state-dir "$state_a" stop >/dev/null 2>&1 || true
    "${RDNY_BASE[@]}" --state-dir "$state_b" stop >/dev/null 2>&1 || true
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

expect_artifact_json() {
  local desc=$1 format=$2 output=$3 want_path=$4 want_type=$5
  ARTIFACT_FORMAT=$format ARTIFACT_OUTPUT=$output ARTIFACT_PATH=$want_path ARTIFACT_TYPE=$want_type ARTIFACT_URL="file://$page" python3 - <<'PY'
import json
import os
import pathlib
import sys

fmt = os.environ["ARTIFACT_FORMAT"]
text = os.environ["ARTIFACT_OUTPUT"]
want_path = pathlib.Path(os.environ["ARTIFACT_PATH"]).resolve()
want_type = os.environ["ARTIFACT_TYPE"]
want_url = os.environ["ARTIFACT_URL"]

try:
    if fmt == "jsonl":
        if "\n" in text:
            raise AssertionError("jsonl output must be exactly one line")
        obj = json.loads(text)
        if json.dumps(obj, separators=(",", ":")) != text:
            raise AssertionError("jsonl output must be compact")
    else:
        obj = json.loads(text)
        if "\n" not in text:
            raise AssertionError("json output must be pretty-printed")
except Exception as exc:
    print(f"invalid structured artifact output: {exc}: {text!r}", file=sys.stderr)
    sys.exit(1)

required = {
    "schemaVersion": 1,
    "kind": "artifact",
    "path": str(want_path),
    "type": want_type,
}
for key, value in required.items():
    if obj.get(key) != value:
        print(f"{key}: expected {value!r}, got {obj.get(key)!r}; object={obj!r}", file=sys.stderr)
        sys.exit(1)
for key in ("instance", "target"):
    if not isinstance(obj.get(key), str) or not obj[key]:
        print(f"{key} must identify the live page; object={obj!r}", file=sys.stderr)
        sys.exit(1)
if obj.get("url") != want_url:
    print(f"url: expected {want_url!r}, got {obj.get('url')!r}; object={obj!r}", file=sys.stderr)
    sys.exit(1)
if not isinstance(obj.get("bytes"), int) or obj["bytes"] <= 0:
    print(f"bytes must be a positive integer; object={obj!r}", file=sys.stderr)
    sys.exit(1)
if not want_path.is_file() or want_path.stat().st_size != obj["bytes"]:
    print(f"bytes does not match file size for {want_path}; object={obj!r}", file=sys.stderr)
    sys.exit(1)
for key in ("width", "height"):
    if key in obj and (not isinstance(obj[key], int) or obj[key] <= 0):
        print(f"{key} must be a positive integer when present; object={obj!r}", file=sys.stderr)
        sys.exit(1)
if want_type == "image/png" and not all(isinstance(obj.get(key), int) and obj[key] > 0 for key in ("width", "height")):
    print(f"PNG artifacts must report dimensions; object={obj!r}", file=sys.stderr)
    sys.exit(1)
PY
  echo "smoke: ok: $desc"
}

page="$workdir/page.html"
cat >"$page" <<'HTML'
<!DOCTYPE html>
<html>
  <head><title>rdny smoke</title></head>
  <body>
    <h1 id="heading">Smoke Page</h1>
    <a id="link" href="data:text/plain;base64,U21va2UgZG93bmxvYWQK">a link</a>
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
"${RDNY_BASE[@]}" --state-dir "$state_a" start --label smoke-a
"${RDNY_BASE[@]}" --state-dir "$state_b" start --label smoke-b
list_output=$("${RDNY_BASE[@]}" list)
expect_contains "list prints label a" "label=smoke-a" "$list_output"
expect_contains "list prints copyable selector" "selector=" "$list_output"
instance_b=""
while IFS= read -r line; do
  if [[ $line == *"label=smoke-b"* && $line =~ selector=([^[:space:]]+) ]]; then
    instance_b=${BASH_REMATCH[1]}
  fi
done <<<"$list_output"
[[ -n $instance_b && $instance_b != "-" ]] || fail "could not extract instance-b selector"

# All remaining parity checks target A by its unique label. B is independently
# addressed by exact id to exercise both selector forms against live browsers.
RDNY=("${RDNY_BASE[@]}" --instance smoke-a)
"${RDNY_BASE[@]}" --instance "$instance_b" open 'data:text/html,<title>instance b</title><h1>B</h1>' --allow-data-url >/dev/null
expect "exact-id selects instance b" "instance b" "$("${RDNY_BASE[@]}" --instance "$instance_b" title)"
"${RDNY[@]}" open 'data:text/html,<title>instance a</title><h1>A</h1>' --allow-data-url >/dev/null
expect "label selects instance a" "instance a" "$("${RDNY[@]}" title)"
expect_contains "status running" "running:" "$("${RDNY[@]}" status)"

# --- navigation + waiting -------------------------------------------
"${RDNY[@]}" open "file://$page" --allow-file-url
"${RDNY[@]}" wait "#heading"
"${RDNY[@]}" waitload
"${RDNY[@]}" waitstable
"${RDNY[@]}" waitidle
expect "url" "file://$page" "$("${RDNY[@]}" url)"
expect "title" "rdny smoke" "$("${RDNY[@]}" title)"

# Chromium internal URLs stay denied by default, fail without changing the
# current page when malformed, and navigate only with the explicit privilege.
if internal_error=$("${RDNY[@]}" open "chrome://version" 2>&1); then
  fail "chrome URL without opt-in should fail"
fi
expect_contains "chrome denial retry flag" "--allow-chrome-url" "$internal_error"
expect "denied chrome URL leaves page unchanged" "file://$page" "$("${RDNY[@]}" url)"
if extension_error=$("${RDNY[@]}" open "chrome-extension://abcdefghijklmnopabcdefghijklmnop/page.html" 2>&1); then
  fail "chrome-extension URL without opt-in should fail"
fi
expect_contains "chrome-extension denial retry flag" "--allow-chrome-extension-url" "$extension_error"
expect "denied chrome-extension URL leaves page unchanged" "file://$page" "$("${RDNY[@]}" url)"
if malformed_error=$("${RDNY[@]}" open "chrome:///version" --allow-chrome-url 2>&1); then
  fail "malformed chrome URL should fail"
fi
expect_contains "malformed chrome URL" "malformed chrome: URL" "$malformed_error"
expect "malformed chrome URL leaves page unchanged" "file://$page" "$("${RDNY[@]}" url)"
if malformed_extension_error=$("${RDNY[@]}" open "chrome-extension://too-short/page.html" --allow-chrome-extension-url 2>&1); then
  fail "malformed chrome-extension URL should fail"
fi
expect_contains "malformed chrome-extension URL" "malformed chrome-extension: URL" "$malformed_extension_error"
expect "malformed chrome-extension URL leaves page unchanged" "file://$page" "$("${RDNY[@]}" url)"
"${RDNY[@]}" open "chrome://version" --allow-chrome-url >/dev/null
expect_contains "allowed chrome open" "chrome://version" "$("${RDNY[@]}" url)"
"${RDNY[@]}" open "file://$page" --allow-file-url >/dev/null

# --- page info -------------------------------------------------------
expect "text" "Smoke Page" "$("${RDNY[@]}" text '#heading')"
expect "attr" "data:text/plain;base64,U21va2UgZG93bmxvYWQK" "$("${RDNY[@]}" attr '#link' href)"
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
shot_json=$("${RDNY[@]}" --format json screenshot --force "$workdir/shot-json.png")
expect_artifact_json "screenshot json artifact" json "$shot_json" "$workdir/shot-json.png" "image/png"
shot_jsonl=$("${RDNY[@]}" --format jsonl screenshot --force "$workdir/shot-jsonl.png")
expect_artifact_json "screenshot jsonl artifact" jsonl "$shot_jsonl" "$workdir/shot-jsonl.png" "image/png"
"${RDNY[@]}" screenshot-el "#heading" "$workdir/shot-el.png" >/dev/null
LC_ALL=C grep -a -q 'PNG' "$workdir/shot-el.png" || fail "screenshot-el is not a PNG"
echo "smoke: ok: screenshot-el"
shot_el_json=$("${RDNY[@]}" --format json screenshot-el "#heading" --force "$workdir/shot-el-json.png")
expect_artifact_json "screenshot-el json artifact" json "$shot_el_json" "$workdir/shot-el-json.png" "image/png"
shot_el_jsonl=$("${RDNY[@]}" --format jsonl screenshot-el "#heading" --force "$workdir/shot-el-jsonl.png")
expect_artifact_json "screenshot-el jsonl artifact" jsonl "$shot_el_jsonl" "$workdir/shot-el-jsonl.png" "image/png"
"${RDNY[@]}" pdf "$workdir/page.pdf" >/dev/null
LC_ALL=C grep -a -q '%PDF' "$workdir/page.pdf" || fail "pdf is not a PDF"
echo "smoke: ok: pdf"
pdf_json=$("${RDNY[@]}" --format json pdf --force "$workdir/page-json.pdf")
expect_artifact_json "pdf json artifact" json "$pdf_json" "$workdir/page-json.pdf" "application/pdf"
pdf_jsonl=$("${RDNY[@]}" --format jsonl pdf --force "$workdir/page-jsonl.pdf")
expect_artifact_json "pdf jsonl artifact" jsonl "$pdf_jsonl" "$workdir/page-jsonl.pdf" "application/pdf"

# --- downloads --------------------------------------------------------
"${RDNY[@]}" download "#link" "$workdir/download-human.txt" >/dev/null
expect "download human file" "Smoke download" "$(<"$workdir/download-human.txt")"
download_json=$("${RDNY[@]}" --format json download "#link" --force "$workdir/download-json.txt")
expect_artifact_json "download explicit json artifact" json "$download_json" "$workdir/download-json.txt" "text/plain"
download_jsonl=$("${RDNY[@]}" --format jsonl download "#link" --force "$workdir/download-jsonl.txt")
expect_artifact_json "download explicit jsonl artifact" jsonl "$download_jsonl" "$workdir/download-jsonl.txt" "text/plain"
"${RDNY[@]}" download "#link" >"$workdir/download-raw-omitted.txt"
expect "download omitted FILE raw stdout" "Smoke download" "$(<"$workdir/download-raw-omitted.txt")"
"${RDNY[@]}" download "#link" - >"$workdir/download-raw.txt"
expect "download raw stdout" "Smoke download" "$(<"$workdir/download-raw.txt")"
for structured_format in json jsonl; do
  if raw_structured_error=$("${RDNY[@]}" --format "$structured_format" download "#link" - 2>&1); then
    fail "$structured_format raw download should fail"
  fi
  expect_contains "$structured_format raw download suggests human" "human" "$raw_structured_error"
  expect_contains "$structured_format raw download suggests file path" "file path" "$raw_structured_error"
done
for structured_format in json jsonl; do
  if raw_structured_error=$("${RDNY[@]}" --format "$structured_format" download "#link" 2>&1); then
    fail "$structured_format omitted-FILE raw download should fail"
  fi
  expect_contains "$structured_format omitted-FILE rejection" "raw bytes" "$raw_structured_error"
done

# --- video ------------------------------------------------------------
"${RDNY[@]}" start-video
"${RDNY[@]}" open "file://$page" --allow-file-url
"${RDNY[@]}" wait "#heading"
"${RDNY[@]}" click "#btn"
"${RDNY[@]}" screenshot "$workdir/video-frame.png" >/dev/null
expect "stop-video human path" "$workdir/smoke.mp4" "$("${RDNY[@]}" stop-video "$workdir/smoke.mp4")"
[[ -s "$workdir/smoke.mp4" ]] || fail "video was not created"
LC_ALL=C grep -a -q 'ftyp' "$workdir/smoke.mp4" || fail "video is not an MP4"
echo "smoke: ok: video"
"${RDNY[@]}" start-video
"${RDNY[@]}" screenshot "$workdir/video-frame-json.png" >/dev/null
video_json=$("${RDNY[@]}" --format json stop-video "$workdir/smoke-json.mp4")
expect_artifact_json "stop-video json artifact" json "$video_json" "$workdir/smoke-json.mp4" "video/mp4"
"${RDNY[@]}" start-video
"${RDNY[@]}" screenshot "$workdir/video-frame-jsonl.png" >/dev/null
video_jsonl=$("${RDNY[@]}" --format jsonl stop-video "$workdir/smoke-jsonl.mp4")
expect_artifact_json "stop-video jsonl artifact" jsonl "$video_jsonl" "$workdir/smoke-jsonl.mp4" "video/mp4"

# --- tabs ------------------------------------------------------------
"${RDNY[@]}" newpage "file://$page" --allow-file-url >/dev/null
expect_contains "pages lists two" "1: " "$("${RDNY[@]}" pages)"
"${RDNY[@]}" newpage "chrome://version" --allow-chrome-url >/dev/null
expect_contains "allowed chrome newpage" "chrome://version" "$("${RDNY[@]}" url)"
"${RDNY[@]}" page 0 >/dev/null
expect_contains "pages marker moved" "* 0:" "$("${RDNY[@]}" pages)"

# --- error path ------------------------------------------------------
if "${RDNY[@]}" --timeout 1 wait "#never-exists" 2>/dev/null; then
  fail "wait for missing element should fail"
fi
echo "smoke: ok: wait timeout exits nonzero"

# --- shutdown --------------------------------------------------------
"${RDNY_BASE[@]}" --instance "$instance_b" stop
expect_contains "selected stop preserves other instance" "running:" "$("${RDNY[@]}" status)"
"${RDNY[@]}" stop
if missing_error=$("${RDNY[@]}" status 2>&1); then
  fail "stopped selector should no longer resolve"
fi
expect_contains "stopped selector is unregistered" "no registered rdny instance matches" "$missing_error"

echo "smoke: PASS"
