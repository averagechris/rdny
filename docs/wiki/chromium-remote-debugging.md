# Chromium remote debugging

rdny talks to Chrome-family browsers through the Chrome DevTools Protocol (CDP). If rdny says it could not reach Chrome DevTools, the browser either was not launched with a remote-debugging port, is listening on a different host/port, or started but never exposed the DevTools endpoint.

## Quick fix

Fully quit the browser first. CDP remote debugging is enabled only at browser launch time; adding the flag to a browser that is already running will not open the port for that existing session.

On macOS, launch a new browser instance with `open -na` and pass Chromium flags after `--args`:

```sh
open -na "Google Chrome" --args --remote-debugging-port=9333 --user-data-dir=/tmp/rdny-chrome
```

For Chromium, Brave, Edge, Helium, or another app, replace the app name:

```sh
open -na Helium --args --remote-debugging-port=9333 --user-data-dir=/tmp/rdny-helium
```

On Linux, run the browser binary directly:

```sh
chromium --remote-debugging-port=9333 --user-data-dir=/tmp/rdny-chromium
```

Then attach rdny to the same port:

```sh
rdny connect 127.0.0.1:9333
```

## Chromium 136+ needs a separate profile

Recent Chromium builds ignore `--remote-debugging-port` on the default user data directory for security. Always use a separate `--user-data-dir` when you launch a personal browser for rdny:

```sh
open -na "Google Chrome" --args \
  --remote-debugging-port=9333 \
  --user-data-dir=/tmp/rdny-chrome-debug
```

This creates an isolated browser profile for automation and leaves your normal profile closed to remote debugging.

## Verify the port

Before running `rdny connect`, confirm the DevTools HTTP endpoint responds:

```sh
curl http://127.0.0.1:9333/json/version
```

If that request fails, rdny will fail too. Check that the browser is still running, that the port number matches, and that you used a non-default `--user-data-dir`.

## rdny-specific notes

- `rdny start` launches its own managed browser with `--remote-debugging-port=0` and an isolated profile under rdny's state directory, then records the chosen port automatically.
- `rdny connect` is for a browser you launched yourself. Pass `<host>:<port>` directly, or configure named targets in the rdny config file:

  ```toml
  [connect]
  default = "personal"

  [connect.targets]
  personal = "127.0.0.1:9333"
  ```

  After that, `rdny connect personal` or just `rdny connect` uses the configured target.
- If rdny cannot find a browser binary for managed launches, set `RDNY_CHROME` to a Chrome/Chromium executable path or set `binaries.chrome` in the rdny config file.
- `RDNY_CHROME_ARGS` appends extra flags when rdny launches Chrome itself. It is split on ASCII whitespace, so shell-style quoting inside the variable is not supported.
