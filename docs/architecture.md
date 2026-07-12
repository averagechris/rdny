# Architecture and dependency direction

`rdny` keeps command-line policy, browser protocol mechanics, and persistent
session ownership separate so refactors do not change the public CLI or CDP
sequences.

## Ownership

- `cli/arguments.rs` owns Clap argument DTOs, parsers, and adjusted help;
  `cli/mod.rs` is only the façade. Pre-I/O semantic validation and selection
  (`validation.rs`), output rendering (`output.rs`), and dispatch
  (`dispatch.rs`). Dispatch may depend on commands, never the reverse.
- `commands/` owns user-visible operations and output DTO conversion. Artifact
  commands convert session-owned `PageIdentity` into `ArtifactContext`; the
  session layer does not depend on command DTOs.
- `session.rs` owns attachment, deadlines, event routing, page identity, and
  recording ingestion (`session/recording.rs`). It depends only on CDP, state,
  selector, and browser-program primitives.
- `input/` exposes typed key chords, pointer targets, buttons, and drag options.
  Keyboard, pointer, and drag entry points are separate modules; shared cleanup
  and protocol payload mechanics remain private to the input boundary.
- `browser_programs/` is the leaf registry for page-side JavaScript. Rust owners
  serialize dynamic values before interpolation and decode bounded plain-data
  results. Selector traversal, actionability/hit testing, drag capture,
  download streaming, and wait instrumentation are exercised by source tests,
  fake-CDP protocol tests, and real-browser smoke fixtures.

The intended direction is `cli -> commands/input -> session -> cdp/state`.
`browser_programs` is a leaf program registry used by selector, input, wait,
download, and actionability owners; it does not perform CDP I/O.

## Integration scenarios

`scripts/smoke.sh --list-scenarios` lists independently runnable scenarios.
Managed scenarios each receive isolated state, work, fixture, and evidence
directories. Running `scripts/smoke.sh` aggregates `lifecycle-navigation`,
`output-artifacts`, `trusted-input-browser-programs`, and `video-tabs`. CI also
runs `attached-cdp`, which launches a small external Chromium debugger, verifies
`connect`, and proves `stop` detaches without terminating the browser.
