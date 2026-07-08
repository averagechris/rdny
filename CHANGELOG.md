# Changelog

## Unreleased

### Added

- Optional config file for third-party binary paths and future named connect
  targets (~averagechris/projects#110).
- `rdny start-video` / `rdny stop-video` records video via CDP screencast
  frames (~averagechris/projects#109).
- Instance labels plus `rdny list` and `rdny cleanup` for discovering and
  reaping per-state-dir browser sessions (~averagechris/projects#108).
- `rdny cookie` manages browser cookies with set, list, get, and delete
  subcommands (~averagechris/projects#98).
- `rdny viewport` prints, persists, resets, and reapplies viewport/mobile
  emulation overrides across commands (~averagechris/projects#97).
- `rdny logs` captures console messages, thrown exceptions, and browser
  log entries; `--follow` streams until interrupted, otherwise events
  drain for the global `--timeout` window (~averagechris/projects#95).
- `rdny js -` (or `rdny js` with piped stdin) reads the JavaScript
  expression from stdin, so multi-line programs pipe cleanly
  (~averagechris/projects#94).
- Optional Linux-only `.#rdny-bundled` flake output wrapping rdny with
  ungoogled-chromium via a default `RDNY_CHROME`; browser provisioning
  decision recorded in `docs/decisions/0002-browser-provisioning.md`
  (~averagechris/projects#77).
- Core rodney command parity over CDP (~averagechris/projects#76):
  - lifecycle: `start` (headless by default, `--show`, `--insecure`/`-k`),
    `connect <host:port>`, `stop`, `status`, with session state persisted
    under `$RDNY_STATE_DIR` / `$XDG_STATE_HOME/rdny` /
    `~/Library/Application Support/rdny`
  - navigation: `open`, `back`, `forward`, `reload [--hard]`, `clear-cache`
  - page info: `url`, `title`, `html [sel]`, `text`, `attr`, `pdf`
  - interaction: `js`, `click`, `input`, `clear`, `file`, `download`,
    `select`, `submit`, `hover`, `focus`
  - waiting: `wait`, `waitload`, `waitstable`, `waitidle`, `sleep`, with a
    global `--timeout`
  - screenshots: `screenshot [-w N] [-h N] [file]`, `screenshot-el`
  - tabs: `pages`, `page <idx>`, `newpage [url]`
- Hand-rolled blocking CDP client over tungstenite (no tokio); decision
  record in `docs/decisions/0001-cdp-client.md`.
- Browser discovery: `RDNY_CHROME` override, then well-known macOS/Linux
  locations; errors follow a one-diagnostic/one-action/one-docs-link
  hint format, including the Chromium 136+ silent
  `--remote-debugging-port` caveat.
- Launch-behavior fixes from upstream rodney PRs (~averagechris/projects#78):
  `--single-process` is never added and is filtered on macOS,
  `RDNY_CHROME_ARGS` passes extra Chrome flags, and CLI flag parsing is
  locked in by tests.
- End-to-end smoke script (`scripts/smoke.sh`) exercising the full
  surface against a real local Chrome/Helium.
- Initial project scaffold.
