#!/usr/bin/env bash
set -euo pipefail
source scripts/smoke/lib.sh
smoke_init trusted-input-browser-programs "$@"
smoke_start

selector='#outer >>> #inner >>> #action'
"${R[@]}" click --pierce "$selector"
"${R[@]}" click '#plain-action'
"${R[@]}" hover '#plain-action'
"${R[@]}" js "document.querySelector('#composed-child').style.display = 'block'" >/dev/null
"${R[@]}" click '#composed-child'
"${R[@]}" js "document.querySelector('#composed-child').style.display = 'none'; document.querySelector('#partially-visible').style.display = 'block'" >/dev/null
"${R[@]}" click '#partially-visible'
"${R[@]}" js "document.querySelector('#partially-visible').style.display = 'none'; document.querySelector('#occluded-fixture').style.display = 'block'" >/dev/null
if occluded_error=$("${R[@]}" click '#occluded-button' 2>&1); then
  fail 'fully occluded target should fail before dispatch'
fi
expect_contains 'occlusion names selected target' 'button#occluded-button.blocked.primary' "$occluded_error"
expect_contains 'occlusion names intercepting target' 'div#occluder.cover.modal' "$occluded_error"
expect 'occlusion dispatches no click' '0,0' "$("${R[@]}" js '[window.result.actionability.occludedClicks, window.result.actionability.occluderClicks].join()')"
"${R[@]}" js "document.querySelector('#occluded-fixture').style.display = 'none'; document.querySelector('#pointer-fixture').style.display = 'block'" >/dev/null
if pointer_error=$("${R[@]}" click '#pointer-none' 2>&1); then
  fail 'pointer-events none target should fail before dispatch'
fi
expect_contains 'pointer-events failure names selected' 'button#pointer-none.disabled-target' "$pointer_error"
expect_contains 'pointer-events failure names underlay' 'button#pointer-underlay' "$pointer_error"
expect 'pointer-events failure dispatches no click' '0,0' "$("${R[@]}" js '[window.result.actionability.pointerNoneClicks, window.result.actionability.pointerUnderlayClicks].join()')"
"${R[@]}" js "document.querySelector('#pointer-fixture').style.display = 'none'; document.querySelector('#detached-target').style.display = 'block'" >/dev/null
if detached_error=$("${R[@]}" click '#detached-target' 2>&1); then
  fail 'target detached by mouse movement should fail before press'
fi
expect_contains 'detached target fails actionably' 'button#detached-target.actionability-fixture became detached' "$detached_error"
expect 'detachment happened before click' '1,0' "$("${R[@]}" js '[window.result.actionability.detachedMoves, window.result.actionability.detachedClicks].join()')"
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
