# Drive upload regression-capture runbook

Background on the historical TLS deadlock and the architecture that resolved it
is in the "Google Drive HTTP client (resolved TLS deadlock)" section of
`docs/design.md`. The per-request/unpooled-client and isolated-runtime
workaround has been removed: every Drive API call and `OAuth2` refresh now goes
through a single daemon-owned pooled `reqwest::Client`, and the WebDAV PUT
handler awaits the backend write directly on the main runtime.

This document is the human-in-the-loop procedure for re-validating that path
against the real authenticated Drive endpoint, and for capturing evidence if a
regression ever surfaces. It exists because the original hang only ever appeared
through the real, authenticated Drive host — not against synthetic TLS servers
or the public no-auth `www.googleapis.com` frontend (the `#[ignore]`d
reproductions in `crates/presenter-webdav/tests/tls_topology_repro.rs` and
`tls_repro_harness.rs`), whose OAuth credentials cannot be embedded in a test.

## Prerequisites

- A Google Drive account with the Cascade backend already configured and
  authenticated (`cascade backend add gdrive --name personal` + device-code
  flow). On macOS the tokens live in the Keychain (`security`, service
  `com.cascade.gdrive`), not a JSON file.
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
  If `cascade` on PATH is the Homebrew binary, use `./target/release/cascade`.

## Tracing spans to watch

Each Drive API request goes through `authenticated_get` or `authenticated_write`
in `crates/backend-gdrive/src/client.rs`, and the WebDAV PUT handler logs a
`before backend upload` / `after backend upload` pair around each upload. A
healthy run keeps the ledger balanced: every `before backend upload` is followed
by its matching `after backend upload`, with no upload left in flight. A wedged
upload would show a `before` with no matching `after`, and the client would be
stuck at the same moment.

`RUST_LOG` must be at least `debug` for the backend and presenter spans; `trace`
adds the hyper/reqwest/rustls connection-lifecycle lines (pool reuse, TLS
handshake, idle close) that surround any wedge.

## Steps

**1. Stop any running daemon.**

```bash
cascade stop
ps aux | grep cascade   # confirm the process is gone
```

**2. Start the daemon against a test Drive folder.**

Use a throwaway folder, not production data. `CASCADE_PRESENTER=webdav` forces
the WebDAV presenter on any platform and `--no-mount` runs the WebDAV HTTP
server without an OS mount, so the write path can be driven by HTTP PUT directly
with no Finder/FSKit in the loop:

```bash
CASCADE_PRESENTER=webdav \
RUST_LOG="cascade_backend_gdrive=debug,cascade_presenter_webdav=debug,hyper=trace,hyper_util=trace,reqwest=trace,rustls=trace" \
cascade start --no-mount 2>&1 | tee ~/cascade-drive-capture-$(date +%Y%m%d-%H%M%S).log
```

Note the WebDAV port from the startup log (or `cascade status`).

**3. Drive the write path with connection reuse.**

PUT several files into the WebDAV server, with idle gaps between bursts so a
pooled keep-alive connection goes idle and gets reused on the next write — the
scenario the original deadlock struck on ("after ~2 handshakes"):

- A back-to-back burst of small files (maximal connection reuse).
- Then single writes with escalating idle gaps spanning the remote idle-close
  window (e.g. 5 / 15 / 30 / 50 s).

**4. Watch the ledger.**

```bash
grep -nE 'before backend upload|after backend upload' ~/cascade-drive-capture-*.log
```

Every `before` must have its matching `after`. A `before` with no `after` is a
wedged upload — capture the surrounding hyper/reqwest/rustls trace lines, the
wall-clock time, and a thread sample (`sample $(pgrep cascade) 5`), then file it
against `docs/design.md`'s HTTP-client section as the first reproduction.

**5. Clean up.**

Trash the throwaway folder (never hard-delete) and `cascade stop`.

## Capture results

### 2026-06-07 — shared pooled client against real authenticated Drive: PASS

The architectural fix was validated against a real Google Drive account before
the per-request/isolation machinery was removed. Driving the WebDAV write path
with HTTP PUTs directly at the server (`CASCADE_PRESENTER=webdav … --no-mount`):
sequential uploads with escalating idle gaps (spanning the remote idle-close
window) plus a back-to-back burst, then a heavier run with larger files — every
PUT returned `201`, the `before`/`after` ledger stayed balanced, and no
timeout, stall, or wedge appeared. The original hang was **not** reproduced in
any configuration, synthetic or real-endpoint, so the captures prove the fix
works in those runs but not that a harness would catch a future regression — the
purpose of this runbook.
