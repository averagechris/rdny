# 0001: CDP client — hand-rolled minimal client over blocking tungstenite

Date: 2026-07-07
Status: accepted
Ticket: ~averagechris/projects#76

## Context

rdny needs a Chrome DevTools Protocol client covering a modest surface:
navigation, DOM query/interaction, input events, screenshots/PDF,
network-idle signals, and multiple targets/tabs. The process model is
one-shot: every command loads session state, connects, acts, and exits.
No long-lived daemon, no concurrency inside a command.

## Options evaluated

Transitive dependency counts measured with `cargo tree -e normal` on
2026-07-07 (empty bin crate + the single dependency):

| Option                | Version | Transitive crates | Notes |
|-----------------------|---------|-------------------|-------|
| chromiumoxide         | 0.9.1   | 147               | Async (tokio/async-std), full CDP codegen. The generated `chromiumoxide_cdp` types dominate compile time. API is far larger than we need; maintenance is sporadic. |
| headless_chrome       | 1.0.22  | 108               | Sync, but bundles its own launcher/fetcher philosophy that fights our launch architecture (we own discovery/launch per tickets #76/#77) and our no-bundled-browser policy. |
| hand-rolled           | tungstenite 0.29.0 | 33 | Blocking WebSocket + serde_json. We own request ids, flat-protocol `sessionId` routing, and event buffering — roughly a few hundred lines for the surface we need. |

## Decision

Hand-roll a minimal CDP client on blocking `tungstenite` + `serde_json`,
and drop `tokio` entirely.

- The one-shot command model needs no async runtime; blocking reads with
  socket read-timeouts express waiting (`waitload`, `waitidle`, …) simply
  and debuggably.
- CDP's JSON framing is small: auto-incrementing `id`, `method`, `params`,
  optional `sessionId` (flat session protocol via
  `Target.attachToTarget { flatten: true }`). Events are buffered while
  waiting for responses.
- The `/json/version`, `/json/list`, `/json/new` HTTP endpoints are
  served by Chrome's trivial built-in HTTP server; a ~100-line HTTP/1.1
  GET/PUT helper over `std::net::TcpStream` avoids an HTTP client crate.
- Smallest closure, fastest compile, no maintenance coupling to a large
  third-party CDP surface.

## Consequences

- We maintain our own protocol layer (`src/cdp/`): connection, call/reply
  matching, event polling, target attach. Mitigated by unit tests against
  an in-process fake WebSocket server.
- New CDP domains cost a method-name string and a `serde_json::json!`
  params literal — no codegen step.
- If scope ever balloons (browser contexts, fetch interception, workers),
  revisit chromiumoxide; this decision only commits us while the surface
  stays modest.
