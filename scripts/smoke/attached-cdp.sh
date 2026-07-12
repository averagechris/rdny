#!/usr/bin/env bash
# Small attached-browser scenario: rdny must attach and detach without ownership.

set -euo pipefail

[[ $# -ge 1 ]] || { echo 'usage: attached-cdp.sh /path/to/rdny' >&2; exit 2; }
RDNY=$1
browser=${RDNY_ATTACHED_CHROME:-${RDNY_CHROME:-}}
if [[ -z $browser ]]; then
  browser=$(command -v chromium || command -v chromium-browser || command -v google-chrome || true)
fi
[[ -n $browser ]] || { echo 'attached-cdp: no Chromium; set RDNY_ATTACHED_CHROME' >&2; exit 2; }

workdir=${RDNY_SMOKE_WORKDIR:-$(mktemp -d)}
mkdir -p "$workdir"
state="$workdir/state"
profile="$workdir/profile"
export XDG_STATE_HOME="$workdir/xdg"
unset RDNY_STATE_DIR RDNY_INSTANCE

port=$(python3 - <<'PY'
import socket
with socket.socket() as sock:
    sock.bind(("127.0.0.1", 0))
    print(sock.getsockname()[1])
PY
)

"$browser" --headless --no-sandbox --disable-gpu \
  --remote-debugging-address=127.0.0.1 --remote-debugging-port="$port" \
  --user-data-dir="$profile" about:blank >"$workdir/chromium.log" 2>&1 &
browser_pid=$!
browser_stopped=0

cleanup() {
  local status=$?
  if [[ $browser_stopped -eq 1 ]]; then
    kill -CONT "$browser_pid" >/dev/null 2>&1 || true
  fi
  "$RDNY" --state-dir "$state" stop >/dev/null 2>&1 || true
  kill "$browser_pid" >/dev/null 2>&1 || true
  wait "$browser_pid" >/dev/null 2>&1 || true
  if [[ $status -ne 0 ]]; then
    echo "smoke: preserving attached-cdp evidence: $workdir" >&2
  elif [[ -z ${RDNY_SMOKE_WORKDIR:-} ]]; then
    rm -rf "$workdir"
  fi
}
trap cleanup EXIT

python3 - "$port" <<'PY'
import json, sys, time, urllib.request
url = f"http://127.0.0.1:{sys.argv[1]}/json/version"
for _ in range(200):
    try:
        with urllib.request.urlopen(url, timeout=.2) as response:
            json.load(response)
        break
    except Exception:
        time.sleep(.05)
else:
    raise SystemExit(f"attached Chromium did not become ready at {url}")
PY

"$RDNY" --state-dir "$state" connect "127.0.0.1:$port" >/dev/null
"$RDNY" --state-dir "$state" open 'data:text/html,<title>attached architecture</title><h1>attached</h1>' --allow-data-url >/dev/null
[[ $("$RDNY" --state-dir "$state" title) == 'attached architecture' ]]

# A test-owned stopped browser is alive but temporarily unreachable. Default
# cleanup must preserve this uncertainty. --all may explicitly remove only the
# rdny state; neither path owns or kills the external process.
kill -STOP "$browser_pid"
browser_stopped=1
sleep 0.05
"$RDNY" --timeout 0.01 --format json cleanup >"$workdir/cleanup-default.json"
[[ -f $state/state.json ]]
python3 - "$workdir/cleanup-default.json" preserved_inconclusive_ <<'PY'
import json, pathlib, sys
value = json.loads(pathlib.Path(sys.argv[1]).read_text())
assert value["schemaVersion"] == 1 and value["kind"] == "cleanup", value
assert any(item["action"].startswith(sys.argv[2]) for item in value["results"]), value
PY
kill -CONT "$browser_pid"
browser_stopped=0
kill -0 "$browser_pid"

kill -STOP "$browser_pid"
browser_stopped=1
sleep 0.05
"$RDNY" --timeout 0.01 --format json cleanup --all >"$workdir/cleanup-all.json"
[[ ! -e $state/state.json ]]
python3 - "$workdir/cleanup-all.json" cleaned <<'PY'
import json, pathlib, sys
value = json.loads(pathlib.Path(sys.argv[1]).read_text())
assert any(item["action"].startswith(sys.argv[2]) for item in value["results"]), value
PY
kill -CONT "$browser_pid"
browser_stopped=0
kill -0 "$browser_pid"

"$RDNY" --state-dir "$state" connect "127.0.0.1:$port" >/dev/null
fixture=$(pwd)/scripts/fixtures/browser-programs.html
"$RDNY" --state-dir "$state" open "file://$fixture" --allow-file-url >/dev/null
expected=$("$RDNY" --state-dir "$state" js 'window.makeLargeDownload()')
"$RDNY" --state-dir "$state" download '#download' --max-bytes 10485900 "$workdir/attached-large.bin" >/dev/null
python3 scripts/check-smoke-download.py "$workdir/attached-large.bin" "$expected"

[[ $("$RDNY" --state-dir "$state" stop) == 'detached (browser left running)' ]]
kill -0 "$browser_pid"
echo 'smoke: PASS: attached-cdp'
