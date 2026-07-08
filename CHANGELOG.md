# Changelog

## Unreleased

### Added

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
