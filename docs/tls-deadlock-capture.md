# TLS deadlock capture runbook

Background on the bug and its current workaround is in the "Google Drive TLS
deadlock workaround" section of `docs/design.md`. This document covers the
human-in-the-loop steps needed to capture a reproduction against the real Drive
endpoint, which is the only remaining path to a confirmed root cause.

## Why a manual runbook

Every automated reproduction attempt in
`crates/presenter-webdav/tests/tls_topology_repro.rs` — synthetic local TLS
server, idle-drop-and-reuse, concurrent requests, varying worker counts — passed
across all configurations. The deadlock does not reproduce against a synthetic
server; only the real googleapis.com TLS frontend triggers it. That frontend
requires a live, authenticated Drive session whose OAuth credentials cannot be
embedded in a test.

The automated `real_googleapis_nested_topology` test (in the same file, run
with `--ignored --nocapture`) partially exercises the real frontend using the
public, no-auth discovery endpoint. If that test stalls on the pooled config and
passes on the unpooled config, that is strong evidence the googleapis frontend
is the trigger. But the discovery endpoint is not Drive itself. If it does not
reproduce, the only remaining path is this runbook.

## Prerequisites

- A Google Drive account with the Cascade backend already configured and
  authenticated (`cascade backend add gdrive --name personal` + device-code
  flow). Verify the token file exists:
  `~/.config/cascade/gdrive-tokens/personal.json`.
- The daemon built from source, not from Homebrew, so the diagnostic tracing
  spans in `crates/backend-gdrive/src/client.rs` are present:
  ```bash
  ~/.cargo/bin/cargo build --release
  ```
  Confirm the binary is the freshly built one:
  ```bash
  which cascade            # must not point to /opt/homebrew/…
  cascade --version        # cross-check with Cargo.toml version
  ```
  If `cascade` on PATH is the Homebrew binary, use the full path
  `./target/release/cascade` or add `./target/release` to PATH for this
  session.
- A terminal with the cascade-dev tmux window available (or a spare terminal
  for the daemon). Do not run two daemon instances simultaneously; trace output
  from both will interleave in the log.

## Tracing spans to watch

Each Drive API request goes through `authenticated_get` or `authenticated_write`
in `crates/backend-gdrive/src/client.rs`. Each request logs:

- `before-send` — logged before `.send().await`, with `seq=N`, `method`, and
  `url`.
- `after-headers` — logged after `.send().await` returns, same `seq`.

A wedged request shows `before-send` for some `seq=N` and **no** matching
`after-headers` for that `seq`. The Finder copy will be stuck at the same
moment.

## Steps

**1. Stop any running daemon.**

```bash
cascade stop
```

Check the cascade-dev tmux window if one is in use and confirm the process is
gone (`ps aux | grep cascade`).

**2. Start the daemon in the pooled diagnostic mode with full trace logging.**

Use a test Drive folder, not production data — the pooled config can wedge
in-flight uploads.

```bash
CASCADE_GDRIVE_HTTP_DIAG=pooled \
RUST_LOG=trace \
cascade start 2>&1 | tee ~/cascade-tls-deadlock-$(date +%Y%m%d-%H%M%S).log
```

The startup log must contain the line:

```
CASCADE_GDRIVE_HTTP_DIAG=pooled: Drive HTTP client is using pooled connections.
This re-introduces the known-bad configuration and may reproduce the TLS hang.
Diagnostic use only.
```

If it does not appear, the env var did not take and the run is invalid. Stop
the daemon and check your shell environment.

**3. Confirm the WebDAV mount came up.**

```bash
cascade status
```

Note the mount path (usually something like `/Volumes/Cascade` on macOS).

**4. Drive the WebDAV write path the way Finder does.**

Copy several files into the mounted volume, then overwrite them, working in
Finder (or `cp` into the mount). Use repeated writes with short idle gaps:

- Copy a handful of small files (a few KB each) into the mount.
- Pause 10–30 seconds.
- Overwrite them (`cp` again, or save from an editor).
- Pause again.
- Repeat a few times.

The idle gaps are essential: you need a pooled keep-alive connection that went
idle and gets reused on the next write. A single burst with no pauses is less
likely to trigger the window.

**5. Watch the live trace for the wedge signature.**

In the log (or the terminal running the daemon):

```bash
grep -n 'before-send\|after-headers\|drive_request' \
    ~/cascade-tls-deadlock-*.log
```

Look for a `seq=N` that has a `before-send` line but **no** matching
`after-headers` line. That is the wedged request. Finder will be stuck at the
same moment.

**6. Capture the evidence.**

When the hang reproduces, record:

- The wedged `seq`, its `method` and `url` from the `before-send` line.
- The wall-clock time.
- Surrounding hyper/reqwest trace lines (connection pool reuse, TLS events,
  idle-close).

If possible, sample the wedged daemon thread (macOS):

```bash
sample $(pgrep cascade) 5 > ~/cascade-tls-deadlock-sample.txt
```

This shows where the async await is parked.

**7. Run the control.**

Stop the daemon:

```bash
cascade stop
```

Restart with the default (workaround) client — either unset
`CASCADE_GDRIVE_HTTP_DIAG` or set it explicitly to `unpooled-http1` — keeping
`RUST_LOG=trace`:

```bash
RUST_LOG=trace \
cascade start 2>&1 | tee ~/cascade-tls-deadlock-control-$(date +%Y%m%d-%H%M%S).log
```

Repeat the identical Finder write sequence. Every `before-send` must be
followed by its `after-headers`. This pins the wedge to the pooled config and
confirms the workaround mitigates it.

**8. Optional: bisect the load-bearing mitigation.**

If you want to know which of the three workaround layers is individually
sufficient, re-run the pooled config with one layer re-enabled at a time:

- `pool_max_idle_per_host(0)` with pooled config → only this knob re-enabled.
- Runtime isolation (`run_isolated_blocking`) stripped from pooled config.
- `Connection: close` removed from the WebDAV response, pooled config.

The standing hypothesis from the synthetic bisection is that
`pool_max_idle_per_host(0)` is the load-bearing mitigation. The others may be
belt-and-braces. Do not remove any layer from production until the root cause
is confirmed and a reproduction passes.

## After capture

Write up the captured wedge (seq, method/url, backtrace, log excerpt) into the
"Google Drive TLS deadlock workaround" section of `docs/design.md` as the first
real-endpoint reproduction. Keep the saved trace log and sample as artefacts.

## Important constraints

The workaround — `build_unpooled_http1_client` with `pool_max_idle_per_host(0)`
in `crates/backend-gdrive/src/client.rs`, plus `run_isolated_blocking` in the
WebDAV PUT handler — must remain in place until a root cause is confirmed **and**
a reproduction passes. Neither a green automated test run nor a single runbook
capture is sufficient grounds to remove it. The production default is
`CASCADE_GDRIVE_HTTP_DIAG` unset (unpooled-http1).

Do not run this diagnostic against a Drive folder that contains data you care
about. The pooled config can stall in-flight uploads, potentially leaving
partial uploads or duplicates.
