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
  <head>
    <title>rdny smoke</title>
    <style>
      .interaction-fixture { display: none; position: fixed; z-index: 100; }
      #child-hit { left: 20px; top: 100px; width: 140px; height: 44px; }
      #child-hit > span { position: absolute; inset: 0; display: grid; place-items: center; }
      #partially-visible { left: -80px; top: 180px; width: 100px; height: 44px; }
      #occluded-fixture { left: 20px; top: 100px; width: 140px; height: 44px; }
      #occluded-button, #occluder { position: absolute; inset: 0; }
      #occluder { z-index: 1; background: rgba(0, 0, 0, 0.1); }
      #pointer-fixture { left: 20px; top: 100px; width: 140px; height: 44px; }
      #pointer-underlay, #pointer-none { position: absolute; inset: 0; }
      #pointer-none { z-index: 1; pointer-events: none; }
      #detached-target { left: 20px; top: 100px; width: 140px; height: 44px; }
      #trusted-pointer { left: 220px; top: 100px; width: 140px; height: 60px; }
      #drag-source { left: 220px; top: 220px; width: 100px; height: 60px; background: #9cf; }
      #drop-target { left: 440px; top: 220px; width: 120px; height: 80px; background: #cfc; }
      outer-shell, slot-shell { display: inline-block; }
    </style>
  </head>
  <body>
    <h1 id="heading">Smoke Page</h1>
    <a id="link" href="data:text/plain;base64,U21va2UgZG93bmxvYWQK">a link</a>
    <button id="btn">press</button>
    <form id="f" onsubmit="event.preventDefault(); document.title = 'submitted'">
      <input id="name" type="text">
      <select id="pet">
        <option value="cat">cat</option>
        <option value="dog">dog</option>
      </select>
    </form>
    <button id="child-hit" class="interaction-fixture"><span>child covers button</span></button>
    <button id="partially-visible" class="interaction-fixture">visible edge</button>
    <div id="occluded-fixture" class="interaction-fixture">
      <button id="occluded-button" class="blocked primary">blocked target</button>
      <div id="occluder" class="cover modal"></div>
    </div>
    <div id="pointer-fixture" class="interaction-fixture">
      <button id="pointer-underlay">underlay</button>
      <button id="pointer-none" class="disabled-target">no pointer events</button>
    </div>
    <button id="detached-target" class="interaction-fixture">detach on move</button>
    <input id="key-target" aria-label="trusted keyboard target">
    <button id="trusted-pointer" class="interaction-fixture">pointer target</button>
    <div id="drag-source" class="interaction-fixture" draggable="true">drag source</div>
    <div id="drop-target" class="interaction-fixture">drop target</div>
    <slot-shell id="slot-host"><button id="slotted-button"><span>slotted child</span></button></slot-shell>
    <outer-shell id="shadow-outer"></outer-shell>
    <script>
      window.interactions = {
        ordinaryTrusted: false,
        hoverTrusted: false,
        childTrusted: false,
        partialTrusted: false,
        occludedClicks: 0,
        occluderClicks: 0,
        pointerNoneClicks: 0,
        pointerUnderlayClicks: 0,
        detachedMoves: 0,
        detachedClicks: 0,
        slotTrusted: false,
        keys: [],
        pointerEvents: [],
        drag: {start: false, over: false, drop: false, target: '', data: ''},
      };
      const ordinary = document.querySelector('#btn');
      ordinary.addEventListener('click', (event) => {
        window.interactions.ordinaryTrusted = event.isTrusted;
        document.title = 'clicked';
      });
      ordinary.addEventListener('mouseover', (event) => {
        window.interactions.hoverTrusted = event.isTrusted;
      });
      document.querySelector('#child-hit').addEventListener('click', (event) => {
        window.interactions.childTrusted = event.isTrusted && event.target.localName === 'span';
      });
      document.querySelector('#partially-visible').addEventListener('click', (event) => {
        window.interactions.partialTrusted = event.isTrusted;
      });
      document.querySelector('#occluded-button').addEventListener('click', () => window.interactions.occludedClicks++);
      document.querySelector('#occluder').addEventListener('click', () => window.interactions.occluderClicks++);
      document.querySelector('#pointer-none').addEventListener('click', () => window.interactions.pointerNoneClicks++);
      document.querySelector('#pointer-underlay').addEventListener('click', () => window.interactions.pointerUnderlayClicks++);
      const detached = document.querySelector('#detached-target');
      detached.addEventListener('mousemove', () => {
        window.interactions.detachedMoves++;
        detached.remove();
      });
      detached.addEventListener('click', () => window.interactions.detachedClicks++);
      for (const type of ['keydown', 'keyup']) {
        document.querySelector('#key-target').addEventListener(type, (event) => {
          window.interactions.keys.push({
            type,
            trusted: event.isTrusted,
            key: event.key,
            code: event.code,
            control: event.ctrlKey,
            shift: event.shiftKey,
            alt: event.altKey,
            meta: event.metaKey,
          });
        });
      }
      const trustedPointer = document.querySelector('#trusted-pointer');
      for (const type of ['pointermove', 'pointerdown', 'pointerup']) {
        trustedPointer.addEventListener(type, (event) => {
          window.interactions.pointerEvents.push({
            type,
            trusted: event.isTrusted,
            target: event.target.id,
            buttons: event.buttons,
            button: event.button,
          });
        });
      }
      const dragSource = document.querySelector('#drag-source');
      const dropTarget = document.querySelector('#drop-target');
      dragSource.addEventListener('dragstart', (event) => {
        window.interactions.drag.start = event.isTrusted;
        event.dataTransfer.setData('text/plain', 'rdny-smoke');
      });
      dropTarget.addEventListener('dragover', (event) => {
        event.preventDefault();
        window.interactions.drag.over = window.interactions.drag.over || event.isTrusted;
      });
      dropTarget.addEventListener('drop', (event) => {
        event.preventDefault();
        window.interactions.drag.drop = event.isTrusted;
        window.interactions.drag.target = event.target.id;
        window.interactions.drag.data = event.dataTransfer.getData('text/plain');
      });
      document.querySelector('#slot-host').attachShadow({mode: 'open'}).innerHTML = '<slot style="display: inline-block"></slot>';
      document.querySelector('#slotted-button').addEventListener('click', (event) => {
        window.interactions.slotTrusted = event.isTrusted && event.target.localName === 'span';
      });
      const outerRoot = document.querySelector('#shadow-outer').attachShadow({mode: 'open'});
      outerRoot.innerHTML = '<inner-shell id="shadow-inner"></inner-shell>';
      const innerRoot = outerRoot.querySelector('#shadow-inner').attachShadow({mode: 'open'});
      window.installShadowButton = () => {
        const button = document.createElement('button');
        button.id = 'shadow-button';
        button.innerHTML = '<span>Nested shadow ready</span>';
        button.onclick = () => { document.title = 'shadow clicked'; };
        innerRoot.appendChild(button);
      };
    </script>
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

# Explicit traversal through two nested open shadow roots. Install the final
# target after the command begins so the shared wait resolver must poll it.
shadow_selector='#shadow-outer >>> #shadow-inner >>> #shadow-button'
"${RDNY[@]}" js 'setTimeout(window.installShadowButton, 500)' >/dev/null
"${RDNY[@]}" wait --pierce "$shadow_selector"
expect "nested shadow text" "Nested shadow ready" "$("${RDNY[@]}" text --pierce "$shadow_selector")"
"${RDNY[@]}" click --pierce "$shadow_selector"
expect "nested shadow click" "shadow clicked" "$("${RDNY[@]}" title)"
"${RDNY[@]}" js "document.title = 'rdny smoke'" >/dev/null

# Chromium internal URLs stay denied by default, fail without changing the
# current page when malformed, and navigate only with the explicit privilege.
"${RDNY[@]}" open "about:blank" >/dev/null
expect "allowed safe about blank" "about:blank" "$("${RDNY[@]}" url)"
"${RDNY[@]}" open "file://$page" --allow-file-url >/dev/null
if about_error=$("${RDNY[@]}" open "about:version" 2>&1); then
  fail "privileged about alias without opt-in should fail"
fi
expect_contains "about alias denial retry flag" "--allow-chrome-url" "$about_error"
expect "denied about alias leaves page unchanged" "file://$page" "$("${RDNY[@]}" url)"
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
"${RDNY[@]}" open "about:version" --allow-chrome-url >/dev/null
allowed_about_open_url=$("${RDNY[@]}" url)
[[ -n "$allowed_about_open_url" ]] || fail "allowed about alias open left an empty URL"
echo "smoke: ok: allowed about alias open"
"${RDNY[@]}" open "file://$page" --allow-file-url >/dev/null

# --- page info -------------------------------------------------------
expect "text" "Smoke Page" "$("${RDNY[@]}" text '#heading')"
expect "attr" "data:text/plain;base64,U21va2UgZG93bmxvYWQK" "$("${RDNY[@]}" attr '#link' href)"
expect_contains "html selector" '<h1 id="heading">' "$("${RDNY[@]}" html '#heading')"
expect_contains "html page" "</html>" "$("${RDNY[@]}" html)"

# --- interaction -----------------------------------------------------
"${RDNY[@]}" click "#btn"
expect "click fired" "clicked" "$("${RDNY[@]}" js 'document.title')"
expect "click stays trusted" "true" "$("${RDNY[@]}" js 'window.interactions.ordinaryTrusted')"
"${RDNY[@]}" js "document.querySelector('#child-hit').style.display = 'block'" >/dev/null
"${RDNY[@]}" click "#child-hit"
expect "composed child hit accepted" "true" "$("${RDNY[@]}" js 'window.interactions.childTrusted')"
"${RDNY[@]}" click --pierce "#slot-host >>> slot"
expect "slotted composed child hit accepted" "true" "$("${RDNY[@]}" js 'window.interactions.slotTrusted')"
"${RDNY[@]}" js "document.querySelector('#child-hit').style.display = 'none'; document.querySelector('#partially-visible').style.display = 'block'" >/dev/null
"${RDNY[@]}" click "#partially-visible"
expect "viewport-clipped visible point clicked" "true" "$("${RDNY[@]}" js 'window.interactions.partialTrusted')"
"${RDNY[@]}" js "document.querySelector('#partially-visible').style.display = 'none'; document.querySelector('#occluded-fixture').style.display = 'block'" >/dev/null
if occluded_error=$("${RDNY[@]}" click "#occluded-button" 2>&1); then
  fail "fully occluded target should fail before dispatch"
fi
expect_contains "occlusion names selected target" "button#occluded-button.blocked.primary" "$occluded_error"
expect_contains "occlusion names intercepting target" "div#occluder.cover.modal" "$occluded_error"
expect "occlusion dispatches no click" "0,0" "$("${RDNY[@]}" js '[window.interactions.occludedClicks, window.interactions.occluderClicks].join()')"
"${RDNY[@]}" js "document.querySelector('#occluded-fixture').style.display = 'none'; document.querySelector('#pointer-fixture').style.display = 'block'" >/dev/null
if pointer_error=$("${RDNY[@]}" click "#pointer-none" 2>&1); then
  fail "pointer-events none target should fail before dispatch"
fi
expect_contains "pointer-events failure names selected" "button#pointer-none.disabled-target" "$pointer_error"
expect_contains "pointer-events failure names underlay" "button#pointer-underlay" "$pointer_error"
expect "pointer-events failure dispatches no click" "0,0" "$("${RDNY[@]}" js '[window.interactions.pointerNoneClicks, window.interactions.pointerUnderlayClicks].join()')"
"${RDNY[@]}" js "document.querySelector('#pointer-fixture').style.display = 'none'; document.querySelector('#detached-target').style.display = 'block'" >/dev/null
if detached_error=$("${RDNY[@]}" click "#detached-target" 2>&1); then
  fail "target detached by mouse movement should fail before press"
fi
expect_contains "detached target fails actionably" "button#detached-target.interaction-fixture became detached" "$detached_error"
expect "detachment happened before click" "1,0" "$("${RDNY[@]}" js '[window.interactions.detachedMoves, window.interactions.detachedClicks].join()')"
"${RDNY[@]}" input "#name" "hello smoke"
expect "input typed" "hello smoke" "$("${RDNY[@]}" js "document.querySelector('#name').value")"
"${RDNY[@]}" clear "#name"
expect "input cleared" "" "$("${RDNY[@]}" js "document.querySelector('#name').value")"
"${RDNY[@]}" select "#pet" dog
expect "select" "dog" "$("${RDNY[@]}" js "document.querySelector('#pet').value")"
"${RDNY[@]}" focus "#name"
expect "focus" "name" "$("${RDNY[@]}" js 'document.activeElement.id')"
"${RDNY[@]}" hover "#btn"
expect "hover stays trusted" "true" "$("${RDNY[@]}" js 'window.interactions.hoverTrusted')"
"${RDNY[@]}" focus "#key-target"
"${RDNY[@]}" key 'Control+Shift+K'
"${RDNY[@]}" key Enter
"${RDNY[@]}" key Plus
"${RDNY[@]}" key a
expect "keyboard events are trusted with key/code/modifiers and release" "keydown:true:Control:ControlLeft:true:false,keydown:true:Shift:ShiftLeft:true:true,keydown:true:K:KeyK:true:true,keyup:true:K:KeyK:true:true,keyup:true:Shift:ShiftLeft:true:false,keyup:true:Control:ControlLeft:false:false,keydown:true:Enter:Enter:false:false,keyup:true:Enter:Enter:false:false,keydown:true:Shift:ShiftLeft:false:true,keydown:true:+:Equal:false:true,keyup:true:+:Equal:false:true,keyup:true:Shift:ShiftLeft:false:false,keydown:true:a:KeyA:false:false,keyup:true:a:KeyA:false:false" "$("${RDNY[@]}" js 'window.interactions.keys.map(e => [e.type,e.trusted,e.key,e.code,e.control,e.shift].join(":")).join()')"
"${RDNY[@]}" pointer move --pierce --selector '#slot-host >>> slot'
"${RDNY[@]}" js "document.querySelector('#trusted-pointer').style.display = 'block'" >/dev/null
"${RDNY[@]}" pointer move --selector '#trusted-pointer'
"${RDNY[@]}" pointer down --at 290,130 --button left
"${RDNY[@]}" pointer up --selector '#trusted-pointer' --button left
expect "pointer primitives are trusted and hit the real target" "pointermove:true:trusted-pointer:0,pointermove:true:trusted-pointer:0,pointerdown:true:trusted-pointer:1,pointerup:true:trusted-pointer:0" "$("${RDNY[@]}" js "window.interactions.pointerEvents.map(e => [e.type,e.trusted,e.target,e.buttons].join(':')).join()")"
# Reload to isolate native HTML drag state from the preceding cross-process
# down/up primitive smoke while preserving a deterministic fixture.
"${RDNY[@]}" open "file://$page" --allow-file-url >/dev/null
"${RDNY[@]}" js "document.querySelector('#drag-source').style.display = 'block'; document.querySelector('#drop-target').style.display = 'block'" >/dev/null
"${RDNY[@]}" pointer drag --from-selector '#drag-source' --to-selector '#drop-target' --steps 20 --duration-ms 300
expect "native drag/drop is trusted and reaches actual target" "true,true,true,drop-target,rdny-smoke" "$("${RDNY[@]}" js 'const d = window.interactions.drag; [d.start,d.over,d.drop,d.target,d.data].join()')"
expect "drag leaves mouse button released" "0" "$("${RDNY[@]}" js 'window.__releaseButtons = null; const t = document.querySelector("#drop-target"); t.addEventListener("pointermove", e => window.__releaseButtons = e.buttons, {once:true}); 0' >/dev/null; "${RDNY[@]}" pointer move --selector '#drop-target'; "${RDNY[@]}" js 'window.__releaseButtons')"
if drag_bounds_error=$("${RDNY[@]}" pointer drag --from-at 1,1 --to-at 999999,999999 2>&1); then
  fail "out-of-bounds drag should fail before dispatch"
fi
expect_contains "drag release/out-of-bounds reports viewport bound" "viewport" "$drag_bounds_error"
"${RDNY[@]}" js "document.querySelector('#drag-source').style.display = 'none'; document.querySelector('#drop-target').style.display = 'none'" >/dev/null
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
"${RDNY[@]}" js 'window.largeDownloadBytes = 9 * 1024 * 1024 + 123; const bytes = new Uint8Array(window.largeDownloadBytes); for (let i = 0; i < bytes.length; i++) bytes[i] = i % 251; const blob = new Blob([bytes], {type: "application/octet-stream"}); const link = document.querySelector("#link"); if (window.largeDownloadUrl) URL.revokeObjectURL(window.largeDownloadUrl); window.largeDownloadUrl = URL.createObjectURL(blob); link.href = window.largeDownloadUrl;' >/dev/null
"${RDNY[@]}" download "#link" --max-bytes 10485900 "$workdir/download-large.bin" >/dev/null
python3 - "$workdir/download-large.bin" <<'PY'
import pathlib, sys
path = pathlib.Path(sys.argv[1])
data = path.read_bytes()
expected = 9 * 1024 * 1024 + 123
if len(data) != expected:
    raise SystemExit(f"large download length: expected {expected}, got {len(data)}")
for index in (0, 1, 1024, len(data) - 1):
    if data[index] != index % 251:
        raise SystemExit(f"large download byte {index}: expected {index % 251}, got {data[index]}")
PY
echo "smoke: ok: large download exact artifact bytes"
if large_max_error=$("${RDNY[@]}" download "#link" --max-bytes 1048576 "$workdir/download-too-large.bin" 2>&1); then
  fail "large download over max should fail"
fi
expect_contains "large download max failure names limit" "exceeds max" "$large_max_error"
[[ ! -e "$workdir/download-too-large.bin" ]] || fail "large download max failure published a file"
"${RDNY[@]}" download "#link" --max-bytes 10485900 - >"$workdir/download-large-raw.bin"
python3 - "$workdir/download-large-raw.bin" <<'PY'
import pathlib, sys
path = pathlib.Path(sys.argv[1])
data = path.read_bytes()
expected = 9 * 1024 * 1024 + 123
if len(data) != expected:
    raise SystemExit(f"raw large download length: expected {expected}, got {len(data)}")
if data[:4] != bytes([0, 1, 2, 3]) or data[-1] != (expected - 1) % 251:
    raise SystemExit("raw large download bytes were not pure payload")
PY
echo "smoke: ok: large raw stdout is pure payload"

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
if newpage_about_error=$("${RDNY[@]}" newpage "about:version" 2>&1); then
  fail "privileged about newpage without opt-in should fail"
fi
expect_contains "newpage about alias denial retry flag" "--allow-chrome-url" "$newpage_about_error"
expect_contains "newpage denied about leaves page unchanged" "file://$page" "$("${RDNY[@]}" url)"
"${RDNY[@]}" newpage "chrome://version" --allow-chrome-url >/dev/null
expect_contains "allowed chrome newpage" "chrome://version" "$("${RDNY[@]}" url)"
"${RDNY[@]}" newpage "about:version" --allow-chrome-url >/dev/null
allowed_about_newpage_url=$("${RDNY[@]}" url)
[[ -n "$allowed_about_newpage_url" ]] || fail "allowed about alias newpage left an empty URL"
echo "smoke: ok: allowed about alias newpage"
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
