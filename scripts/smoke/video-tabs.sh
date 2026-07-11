#!/usr/bin/env bash
set -euo pipefail
source scripts/smoke/lib.sh
smoke_init video-tabs "$@"
smoke_start

"${R[@]}" start-video
"${R[@]}" click '#plain-action'
"${R[@]}" screenshot "$workdir/frame.png" >/dev/null
video=$("${R[@]}" stop-video "$workdir/video.mp4")
expect 'video output path' "$workdir/video.mp4" "$video"
[[ -s $workdir/video.mp4 ]]
"${R[@]}" newpage "file://$fixture" --allow-file-url >/dev/null
expect_contains 'second tab listed' '1: ' "$("${R[@]}" pages)"
"${R[@]}" page 0 >/dev/null
expect_contains 'tab selection marker' '* 0:' "$("${R[@]}" pages)"
smoke_pass
