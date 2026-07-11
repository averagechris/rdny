#!/usr/bin/env bash
set -euo pipefail
source scripts/smoke/lib.sh
smoke_init trusted-input-browser-programs "$@"
smoke_start

selector='#outer >>> #inner >>> #action'
"${R[@]}" click --pierce "$selector"
"${R[@]}" focus '#key-target'
"${R[@]}" key 'Control+Shift+K'
"${R[@]}" key Enter
"${R[@]}" key Plus
"${R[@]}" key a
"${R[@]}" pointer move --selector '#pointer-target'
"${R[@]}" pointer down --at 260,130 --button left
"${R[@]}" pointer up --selector '#pointer-target' --button left
"${R[@]}" pointer drag --from-selector '#drag-source' --to-selector '#drop-target' --steps 8 --duration-ms 80
"${R[@]}" js 'window.captureReleasedPointer()' >/dev/null
"${R[@]}" pointer move --selector '#drop-target'
result=$("${R[@]}" js 'JSON.stringify(window.result)')
python3 scripts/check-smoke-architecture.py "$result"
smoke_pass
