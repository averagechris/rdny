<!-- Refs #135 #136 #143 #184 -->

# CLI output formats and schema version 1

`rdny --format human|json|jsonl` selects output for commands that expose stable
structured output: `status`, `list`, `cleanup`, `cookie list`, `viewport`,
`logs`, `pages`, and artifact-producing commands. Human output is for terminals;
page-controlled text is stripped of terminal-control bytes before printing.
Progress, warnings, and diagnostics are written to stderr so stdout remains
parseable. Commands without structured output reject unsupported formats rather
than silently printing human text.

Every structured record includes `schemaVersion: 1` and `kind`. `json` prints a
single pretty JSON document. `jsonl` prints one compact JSON document per line;
streaming commands such as `logs --follow` emit one record per event. Empty lists
are represented as `[]`. Finite multi-item commands wrap results in one document
containing an array; `jsonl` still uses one compact line for that document.

## status

`rdny status --check` is a health check: exit 0 when the configured session is
running/reachable and non-zero when missing or stale.

```json
{"schemaVersion":1,"kind":"status","status":"running|missing|stale","healthy":true,"browser":"Chrome","host":"127.0.0.1","port":9222,"pid":123,"instance":"...","target":"...","label":"optional"}
```

## list

```json
{"schemaVersion":1,"kind":"list","instances":[{"dir":"/state","pid":123,"liveness":"alive|attached|dead|unverifiable|unrelated","label":"optional","instance":"...","selector":"...","target":"...","host":"127.0.0.1","port":9222}]}
```

`selector` is the registry-verified stable value to pass to
`rdny --instance VALUE` or `RDNY_INSTANCE=VALUE`; it is `null` for discovered
legacy state that is not registered. Human output prints the same value as
`selector=...`.

## cleanup

Cleanup records preserve the distinction between dead states and inconclusive
probes:

```json
{"schemaVersion":1,"kind":"cleanup","results":[{"dir":"/state","pid":123,"label":"optional","action":"cleaned|preserved_changed|preserved_dead|preserved_inconclusive_expired|preserved_inconclusive_timeout|preserved_inconclusive_unavailable|preserved_live_alive|preserved_live_attached|preserved_live_unverifiable|preserved_live_unrelated","reason":"human diagnostic"}]}
```

## pages

Page list records use `kind: "pages"` with page entries containing `index`,
`current`, `id`, `url`, and `title`.

## cookies

`rdny cookie list` exposes cookie deletion fields explicitly so deletion can be
driven from list output:

```json
{"schemaVersion":1,"kind":"cookies","url":"https://example.com/","cookies":[{"name":"sid","value":"...","domain":"example.com","path":"/","secure":true,"httpOnly":true,"sameSite":"Lax","expires":1999999999}]}
```

Use `rdny cookie delete NAME --domain DOMAIN --path PATH` with the listed
`domain` and `path`.

Cookie/input secrets can be supplied without argv leakage with
`--value-stdin`, `--value-file`, `--value-fd`, `--text-stdin`, `--text-file`, or
`--text-fd`.

## viewport

Viewport records use `kind: "viewport"` and contain width, height, scale, mobile,
and reset/applied state when available.

## logs

Use `rdny logs --duration SECONDS` for an explicit finite capture, or
`rdny logs --follow` for streaming. Structured log records identify the selected
rdny browser instance separately from the selected page target. `instance` is
omitted when the session was not selected from a registered instance;
`cdpSession` is the lower-level flat CDP session id and is present only when
Chrome supplied it. Structured log records contain:

```json
{"schemaVersion":1,"kind":"log","timestamp":1783737600.123,"instance":"optional selected instance id","target":"selected page target id","cdpSession":"optional flat CDP session id","severity":"info|warning|error|log|warn","message":"..."}
```

## artifacts

Artifact-producing commands keep their existing human messages and overwrite
protection. Human output prints `saved PATH` for `screenshot`,
`screenshot-el`, `pdf`, and `download FILE`; `stop-video` prints the bare
`PATH`. Displayed paths are terminal-safe.

Structured artifact records have exactly these required fields:

```json
{
  "schemaVersion": 1,
  "kind": "artifact",
  "path": "canonical absolute path",
  "type": "MIME media type",
  "bytes": 123
}
```

Screenshot records include `width` and `height` when the PNG header can be
parsed. Video records may include them when the first JPEG frame can be parsed;
reported video dimensions include the even-pixel padding applied by the encoder.
`instance`, `target`, and `url` may be included when available and are omitted
otherwise. `json` prints one pretty JSON document. `jsonl` prints one compact
JSON document on one line.

`type` is the artifact's MIME media type: screenshots use `image/png`, PDFs use
`application/pdf`, downloads use the normalized response `Content-Type` (or
`application/octet-stream` when it is absent or invalid), and known video
extensions use their corresponding video media type. `path` is canonicalized
after atomic publication, with an absolute normalized fallback, and `bytes` is
the final published file's metadata size. Page artifacts collect context from
the selected instance, attached target, and `location.href` before capture or
download. Recovered video assembly remains available without a browser, so its
context fields can be absent.

`download SELECTOR` and `download SELECTOR -` write raw bytes to stdout in human
mode. Structured formats for raw stdout downloads are rejected before instance
selection or browser/session I/O and suggest using human output or an explicit
file path instead.
