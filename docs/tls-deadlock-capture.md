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
server, and — as the next paragraph shows — not even against the public
googleapis.com frontend. In production it was only ever observed through the
real, authenticated Drive endpoint, so capturing it needs a live, authenticated
Drive session whose OAuth credentials cannot be embedded in a test.

The automated `real_googleapis_nested_topology` test (in the same file, run
with `--ignored --nocapture`) exercises the real frontend using the public,
no-auth discovery endpoint (`https://www.googleapis.com/discovery/v1/apis`),
driving the nested axum-handler topology across eight rounds with escalating
idle pauses (5 s / 15 s / 30 s) under both the pooled and the unpooled client
configs. It has been run, and it **did not reproduce** the hang — both configs
passed every round. The discovery endpoint shares the googleapis.com TLS
frontend but is not Drive itself, so this narrows the trigger to the
authenticated Drive upload host and leaves this runbook as the only remaining
path to a confirmed root cause.

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

Both are logged at `debug` level inside a `drive_request` span, so `RUST_LOG`
must be at least `debug` for the backend crate to see them; `trace` is needed on
top to capture the hyper/reqwest/rustls connection-lifecycle lines (pool reuse,
TLS handshake, idle close) that surround the wedge.

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

`RUST_LOG=trace` over the whole daemon is correct but very noisy. A targeted
filter that keeps the wedge signature and the connection-lifecycle lines while
dropping the rest is usually easier to read:

```bash
RUST_LOG="cascade_backend_gdrive=debug,cascade_presenter_webdav=debug,hyper=trace,hyper_util=trace,reqwest=trace,rustls=trace"
```

The startup log must contain a `WARN` line beginning:

```
CASCADE_GDRIVE_HTTP_DIAG=pooled: Drive HTTP client is using pooled connections …
```

(the full message continues "… This re-introduces the known-bad configuration
and may reproduce the TLS hang. Diagnostic use only." on the same logical line).
If it does not appear, the env var did not take and the run is invalid — the
default unpooled client emits no such warning. Stop the daemon and check your
shell environment.

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

`CASCADE_GDRIVE_HTTP_DIAG` controls **only** the client-side layer: `pooled`
re-enables connection pooling; `pooled-http2` additionally drops `http1_only()`
— but that is a no-op, because the workspace `reqwest` is built without the
`http2` feature, so the client cannot negotiate HTTP/2 either way (see
`build_unpooled_http1_client` in `crates/backend-gdrive/src/client.rs`). So the
client layer that actually matters is `pool_max_idle_per_host(0)` (no pooling),
which `pooled` flips off.

The two server-side layers — `Connection: close` on every WebDAV response and
`run_isolated_blocking` for the backend write — are hardcoded in
`crates/presenter-webdav/src/server.rs`. They are **not** reachable from the
env var; bisecting them means editing that file and rebuilding.

So, holding the pooled client constant (`CASCADE_GDRIVE_HTTP_DIAG=pooled`):

- Client pooling alone: pooled (hang expected) versus the default unpooled (no
  hang). This is steps 2 and 7 above and isolates the client-pooling layer.
- Server `Connection: close`: strip it from the WebDAV response in `server.rs`,
  rebuild, re-run the pooled config.
- Server runtime isolation: replace `run_isolated_blocking` with a direct
  `.await` of the backend write on the axum runtime in `server.rs`, rebuild,
  re-run the pooled config.

The standing hypothesis from the synthetic bisection is that the client-side
`pool_max_idle_per_host(0)` is the load-bearing mitigation and the two
server-side layers are belt-and-braces. Do not remove any layer from production
until the root cause is confirmed and a reproduction passes.

## After capture

Write up the captured wedge (seq, method/url, backtrace, log excerpt) into the
"Google Drive TLS deadlock workaround" section of `docs/design.md` as the first
real-endpoint reproduction. Keep the saved trace log and sample as artefacts.

## Important constraints

**Status (2026-06-07): the default has been flipped to the shared pooled client.**
After two real-endpoint captures passed (see Capture results below — 68 uploads,
no wedge), `pooled-shared` became the production default. The former workaround —
`build_unpooled_http1_client` with `pool_max_idle_per_host(0)` plus
`run_isolated_blocking` in the WebDAV PUT handler — is **not deleted**; it is the
escape hatch, reachable instantly via `CASCADE_GDRIVE_HTTP_DIAG=unpooled-legacy`
should the shared client wedge in production. The escape hatch and the workaround
code stay until the new default has soaked in real use; deleting them is the final
step. The capture procedure above remains valid for re-validating the default and
for diagnosing any regression.

Do not run this diagnostic against a Drive folder that contains data you care
about. The pooled config can stall in-flight uploads, potentially leaving
partial uploads or duplicates.

## Capture results

### 2026-06-07 — `pooled-shared` against real authenticated Drive: PASS (no hang)

First real-endpoint capture of the architectural-fix mode. Setup: a debug daemon
authenticated against a real Google Drive account, started with
`CASCADE_PRESENTER=webdav CASCADE_GDRIVE_HTTP_DIAG=pooled-shared … start --no-mount`
(WebDAV HTTP server on localhost, no OS mount), driving the write path with HTTP
PUTs directly at the server — so every PUT runs the handler's `skip_isolation`
branch and uploads through the single daemon-owned pooled client on the main
runtime.

- **28 sequential uploads** into a throwaway folder: one warm-up, seven with
  escalating idle gaps (0/0/5/12/20/35/50 s — spanning the remote idle-close
  window), then a 20-upload back-to-back burst (maximal connection reuse). Every
  PUT returned `201`, ~2 s each; the daemon's `before backend upload` /
  `after backend upload` ledger was **28 → 28** with no `before-send` left
  without its `after-headers`, and no timeout/stall/wedge in the trace.
- **Control** (default mode, isolation on): six uploads, all `201`, ledger
  `6 → 6`.

So the shared pooled client survived exactly the write-path-with-connection-reuse
scenario the original deadlock struck on ("after ~2 handshakes"), at 14× that
threshold and across idle gaps — no hang. The throwaway folder was trashed
afterwards.

A second, heavier run immediately after: **40 uploads** (files to 512 KB, idle
gaps of 25 / 40 s), ledger `40 → 40`, again no wedge — **68 uploads total** across
the two runs, zero wedges.

On the strength of those two captures the **default was flipped** to
`pooled-shared`, confirmed by a final run with no env var that showed the daemon
using the shared client (`skip_isolation=true`) and uploading cleanly. The
former workaround is kept as the `unpooled-legacy` escape hatch (see Status
above), not deleted.

Residual caveat: the original hang was **not** reproduced in any configuration,
so the captures prove the fix works in those runs but not that the harness would
catch a regression. The escape hatch is the safeguard for that uncertainty; the
workaround code is not deleted until the new default has soaked in real use,
ideally re-validated across sessions / networks.
