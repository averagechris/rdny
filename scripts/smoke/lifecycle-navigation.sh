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
expect 'prop live value differs from attr' 'live-value' "$("${R[@]}" prop '#prop-input' value)"
expect 'attr remains initial value' 'attribute-value' "$("${R[@]}" attr '#prop-input' value)"
expect 'prop boolean' 'true' "$("${R[@]}" prop '#prop-check' checked)"
expect 'prop number' '0' "$("${R[@]}" prop '#prop-input' tabIndex)"
expect 'prop object' '{"count":2,"name":"plain","nested":{"ok":true}}' "$("${R[@]}" prop '#prop-object' state)"
expect 'prop preserves own __proto__ key' '{"__proto__":"own-proto-key"}' "$("${R[@]}" prop '#prop-object' protoState)"
expect 'prop array' '["one",2,false,null]' "$("${R[@]}" prop '#prop-array' items)"
expect 'prop shadow pierce' 'shadow-live' "$("${R[@]}" prop --pierce '#outer >>> #inner >>> #shadow-prop' value)"
expect 'prop human sanitizes controls' 'safe[31mtext' "$("${R[@]}" prop '#prop-sanitize' dirty)"
expect 'prop image complete' 'true' "$("${R[@]}" prop '#prop-image' complete)"
expect 'prop image naturalWidth' '1' "$("${R[@]}" prop '#prop-image' naturalWidth)"
json_prop=$("${R[@]}" --format json prop '#prop-input' value)
expect_contains 'prop json schema' '"schemaVersion":1' "$json_prop"
expect_contains 'prop json kind' '"kind":"prop"' "$json_prop"
expect_contains 'prop json value' '"value":"live-value"' "$json_prop"
jsonl_prop=$("${R[@]}" --format jsonl prop '#prop-check' checked)
expect_contains 'prop jsonl bool' '"value":true' "$jsonl_prop"
if [[ $(printf '%s\n' "$jsonl_prop" | wc -l | tr -d ' ') != 1 ]]; then fail 'prop jsonl one line'; fi
if error=$("${R[@]}" prop '#prop-errors' boom 2>&1); then exit 1; fi
expect_contains 'prop error boom' 'getter for property `boom` threw: fixture boom' "$error"
for prop_error in undef nanValue fnValue cycle huge hugeKey manyNodes deep; do
  if error=$("${R[@]}" prop '#prop-errors' "$prop_error" 2>&1); then exit 1; fi
  expect_contains "prop error $prop_error" 'not JSON-compatible' "$error"
done
if error=$("${R[@]}" prop '#prop-errors' escBoom 2>&1); then exit 1; fi
expect_contains 'prop getter esc sanitized' 'getter �[31mboom' "$error"
if [[ "$error" == *$'\e'* ]]; then fail 'prop getter esc not sanitized'; fi
if error=$("${R[@]}" prop '#prop-revalidate' detachOnRead 2>&1); then exit 1; fi
expect_contains 'prop rejects getter detach' 'detached while reading property' "$error"
"${R[@]}" open "file://$(pwd)/scripts/fixtures/browser-programs.html" --allow-file-url >/dev/null
if error=$("${R[@]}" prop '#prop-revalidate' replaceOnRead 2>&1); then exit 1; fi
expect_contains 'prop rejects getter replace' 'while reading property' "$error"
if error=$("${R[@]}" prop '#prop-input' 'value.length' 2>&1); then exit 1; fi
expect_contains 'prop rejects expression property' 'literal property name' "$error"

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
