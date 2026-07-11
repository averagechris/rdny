#!/usr/bin/env bash
set -euo pipefail
source scripts/smoke/lib.sh
smoke_init output-artifacts "$@"
smoke_start

capture_formats() {
  local name=$1
  shift
  "${R[@]}" "$@" >"$workdir/$name.human"
  "${R[@]}" --format json "$@" >"$workdir/$name.json"
  "${R[@]}" --format jsonl "$@" >"$workdir/$name.jsonl"
}

capture_formats status status
capture_formats list list
capture_formats open open "file://$fixture" --allow-file-url
capture_formats pages pages
capture_formats cookies cookie list
capture_formats viewport viewport

for format in human json jsonl; do
  "${R[@]}" js "setTimeout(() => console.log('rdny-smoke-$format'), 250)" >/dev/null
  if [[ $format == human ]]; then
    "${R[@]}" logs --duration 0.7 >"$workdir/logs.$format"
  else
    "${R[@]}" --format "$format" logs --duration 0.7 >"$workdir/logs.$format"
  fi
done

"${R[@]}" screenshot "$workdir/shot-human.png" >"$workdir/screenshot.human"
"${R[@]}" --format json screenshot "$workdir/shot-json.png" >"$workdir/screenshot.json"
"${R[@]}" --format jsonl screenshot "$workdir/shot-jsonl.png" >"$workdir/screenshot.jsonl"
"${R[@]}" pdf "$workdir/page-human.pdf" >"$workdir/pdf.human"
"${R[@]}" --format json pdf "$workdir/page-json.pdf" >"$workdir/pdf.json"
"${R[@]}" --format jsonl pdf "$workdir/page-jsonl.pdf" >"$workdir/pdf.jsonl"
"${R[@]}" download '#download' "$workdir/download-human.txt" >"$workdir/download.human"
"${R[@]}" --format json download '#download' "$workdir/download-json.txt" >"$workdir/download.json"
"${R[@]}" --format jsonl download '#download' "$workdir/download-jsonl.txt" >"$workdir/download.jsonl"

python3 scripts/check-smoke-artifacts.py "$workdir"

expected=$("${R[@]}" js 'window.makeLargeDownload()')
"${R[@]}" download '#download' --max-bytes 10485900 "$workdir/download-large.bin" >/dev/null
python3 scripts/check-smoke-download.py "$workdir/download-large.bin" "$expected"
if "${R[@]}" download '#download' --max-bytes 1048576 "$workdir/download-too-large.bin" >"$workdir/download-max.out" 2>"$workdir/download-max.err"; then
  echo 'smoke: large download max should fail' >&2
  exit 1
fi
expect_contains 'large download max failure' 'exceeds max' "$(<"$workdir/download-max.err")"
[[ ! -e $workdir/download-too-large.bin ]]
"${R[@]}" download '#download' --max-bytes 10485900 - >"$workdir/download-large-raw.bin"
python3 scripts/check-smoke-download.py "$workdir/download-large-raw.bin" "$expected"
smoke_pass
