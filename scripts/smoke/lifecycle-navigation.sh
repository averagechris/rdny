#!/usr/bin/env bash
set -euo pipefail
source scripts/smoke/lib.sh
smoke_init lifecycle-navigation "$@"

second="$workdir/second"
"${RDNY[@]}" --state-dir "$state" start --label smoke-nav-a >/dev/null
"${RDNY[@]}" --state-dir "$second" start --label smoke-nav-b >/dev/null
trap '"${RDNY[@]}" --state-dir "$second" stop >/dev/null 2>&1 || true; smoke_cleanup' EXIT
list=$("${RDNY[@]}" list)
expect_contains 'list first instance' 'label=smoke-nav-a' "$list"
expect_contains 'list second instance' 'label=smoke-nav-b' "$list"
R=("${RDNY[@]}" --instance smoke-nav-a)
"${R[@]}" open "file://$(pwd)/scripts/fixtures/browser-programs.html" --allow-file-url >/dev/null
"${R[@]}" wait '#heading'
"${R[@]}" waitload
"${R[@]}" waitstable
"${R[@]}" waitidle
expect 'fixture title' 'rdny browser programs' "$("${R[@]}" title)"
selector='#outer >>> #inner >>> #action'
"${R[@]}" wait --pierce "$selector"
expect 'shadow traversal text' 'action' "$("${R[@]}" text --pierce "$selector")"

"${R[@]}" open about:blank >/dev/null
expect 'about blank default allow' 'about:blank' "$("${R[@]}" url)"
"${R[@]}" open "file://$(pwd)/scripts/fixtures/browser-programs.html" --allow-file-url >/dev/null
before=$("${R[@]}" url)
if error=$("${R[@]}" open about:version 2>&1); then exit 1; fi
expect_contains 'about version default denial' '--allow-chrome-url' "$error"
expect 'denied about open leaves page unchanged' "$before" "$("${R[@]}" url)"
"${R[@]}" open about:version --allow-chrome-url >/dev/null
expect_contains 'about version explicit open' 'version' "$("${R[@]}" url)"

"${R[@]}" open "file://$(pwd)/scripts/fixtures/browser-programs.html" --allow-file-url >/dev/null
before=$("${R[@]}" url)
if error=$("${R[@]}" newpage about:version 2>&1); then exit 1; fi
expect_contains 'about version newpage default denial' '--allow-chrome-url' "$error"
expect 'denied about newpage leaves page unchanged' "$before" "$("${R[@]}" url)"
"${R[@]}" newpage about:version --allow-chrome-url >/dev/null
expect_contains 'about version explicit newpage' 'version' "$("${R[@]}" url)"

"${R[@]}" page 0 >/dev/null
before=$("${R[@]}" url)
if error=$("${R[@]}" open chrome:///version --allow-chrome-url 2>&1); then exit 1; fi
expect_contains 'malformed internal URL denied' 'malformed chrome: URL' "$error"
expect 'malformed internal URL leaves page unchanged' "$before" "$("${R[@]}" url)"
"${RDNY[@]}" --state-dir "$second" stop >/dev/null
"${R[@]}" stop >/dev/null
smoke_pass
