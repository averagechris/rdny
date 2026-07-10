<!-- Refs #135 #136 #143 -->

# CLI output formats and schema version 1

`rdny --format human|json|jsonl` selects output for commands that expose stable
structured output. Human output is for terminals; page-controlled text is stripped
of terminal-control bytes before printing. Progress, warnings, and diagnostics are
written to stderr so stdout remains parseable.

Every structured record includes `schemaVersion: 1` and `kind`. `json` prints a
single pretty JSON document. `jsonl` prints one compact JSON document per line;
streaming commands such as `logs --follow` emit one record per event.

## status

`rdny status --check` is a health check: exit 0 when the configured session is
running/reachable and non-zero when missing or stale.

```json
{"schemaVersion":1,"kind":"status","status":"running|missing|stale","healthy":true,"browser":"Chrome","host":"127.0.0.1","port":9222,"pid":123,"instance":"...","target":"...","label":"optional"}
```

## list

```json
{"schemaVersion":1,"kind":"list","instances":[{"dir":"/state","pid":123,"liveness":"alive|attached|dead|unverifiable|unrelated","label":"optional","instance":"...","target":"...","host":"127.0.0.1","port":9222}]}
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
`rdny logs --follow` for streaming. Structured log records contain:

```json
{"schemaVersion":1,"kind":"log","timestamp":1783737600.123,"instance":"...","target":"...","severity":"info|warning|error|log|warn","message":"..."}
```

## artifacts

Artifact-producing commands keep their existing human messages and overwrite
protection. Structured artifact records use `kind: "artifact"` with `path`,
`type`, and byte size as available.
