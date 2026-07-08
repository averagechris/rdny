# 0002: Browser provisioning — discover by default, bundle only as opt-in

Date: 2026-07-08
Status: accepted
Ticket: ~averagechris/projects#77

## Context

rdny drives Chrome through the Chrome DevTools Protocol, but Chrome itself is a
large, platform-specific dependency. go-rod can automatically fetch browser
snapshots, but rdny's default package should stay small, predictable, and free
of implicit runtime downloads.

## Options evaluated

| Option | Notes |
|--------|-------|
| Auto-fetch Chrome snapshots by default | Seamless first run, but surprising network and disk activity; adds provenance, cache, update, and platform policy questions to every install. |
| Require an explicit Chrome path only | Predictable, but too sharp for users with Chrome in standard locations. |
| Discover local Chrome, with an opt-in bundled Nix output | Keeps the default closure small, preserves explicit overrides, and gives Linux/Nix users a fully provisioned package when they ask for it. |

## Decision

Do not implement go-rod-style automatic browser snapshot fetching as default
behavior for now.

Runtime browser discovery is:

1. `RDNY_CHROME` when set.
2. Well-known Chrome/Chromium locations on macOS and Linux.
3. A clear, actionable error explaining how to install Chrome or set
   `RDNY_CHROME`.

The default flake package remains browser-free. A separate opt-in package,
`.#rdny-bundled`, wraps `rdny` with Nixpkgs `ungoogled-chromium` and sets
`RDNY_CHROME` by default through the wrapper. Because Nixpkgs Chromium support
is effectively Linux-only, `rdny-bundled` is only exposed for Linux systems.

On Darwin, seamlessness relies on discovering an installed system Chrome or
Chromium application. A future opt-in fetch of Chrome-for-Testing snapshots may
be considered if macOS users need a Nix-managed browser path without making
network downloads implicit.

## Consequences

- Default installs and `.#rdny` builds do not include or download a browser.
- Users can override any discovered or bundled browser with `RDNY_CHROME`.
- Linux/Nix users who want one closure containing both the CLI and browser can
  use `.#rdny-bundled`.
- Darwin users need system Chrome/Chromium today, or an explicit
  `RDNY_CHROME` path.

## Addendum: ffmpeg provisioning

`stop-video` shells out to ffmpeg, but the default package should remain minimal
and avoid bundling media tooling for commands that do not need it. Runtime
ffmpeg lookup is explicit and predictable: `RDNY_FFMPEG` overrides all other
sources, config can provide a binary path, and otherwise rdny falls back to
`PATH` lookup.

Nix users who want a managed ffmpeg binary can opt into `.#rdny-ffmpeg`, which
wraps the default CLI and sets `RDNY_FFMPEG` to Nixpkgs `ffmpeg-headless` only as
a default. The Linux-only `.#rdny-bundled` output now provisions both browser and
ffmpeg defaults, while `.#rdny` remains browser- and ffmpeg-free.
