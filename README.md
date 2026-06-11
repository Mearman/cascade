# Cascade

[![CI](https://img.shields.io/github/actions/workflow/status/Mearman/cascade/ci.yml?branch=main&label=CI)](https://github.com/Mearman/cascade/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Mearman/cascade?sort=semver)](https://github.com/Mearman/cascade/releases/latest)
[![License](https://img.shields.io/github/license/Mearman/cascade)](LICENSE)
[![GitHub](https://img.shields.io/badge/GitHub-181717?logo=github&logoColor=white)](https://github.com/Mearman/cascade)
[![Homebrew](https://img.shields.io/badge/Homebrew-FBB040?logo=homebrew&logoColor=white)](https://github.com/Mearman/homebrew-cascade)
[![Scoop](https://img.shields.io/badge/Scoop-205081?logo=data:image/svg%2Bxml%3Bbase64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHZpZXdCb3g9IjAgMCAyNCAyNCI+PHBhdGggZD0iTTExIDJoMnY5aC0yek0xMiAyMmE3IDcgMCAwIDAgNy03SDVhNyA3IDAgMCAwIDcgN3oiIGZpbGw9IiNmZmYiLz48L3N2Zz4K&logoColor=white)](https://github.com/Mearman/scoop-cascade)

> Cross-platform cloud storage filesystem client built in Rust. On-demand file access, nested `.cascade` config with directory-walk precedence, offline pinning, policy-driven lifecycle management, P2P block sync, and multi-backend support. Uses native platform APIs (File Provider on macOS, ProjFS on Windows, FUSE on Linux) with NFS fallback — no kernel extensions required.

Rust (edition 2024) · Swift (macOS File Provider extension) · SQLite state · Tokio async runtime

## Why

Every cloud provider ships their own desktop client. They all share the same shape — pick a folder, sync it, hope your disk is bigger than the data. Google Drive for Desktop has a stream mode that loads files on demand, but it's macOS- and Windows-only, leaves you guessing what's actually local, and behaves badly offline. Dropbox, OneDrive, iCloud — same story, each with their own client, their own quirks, their own opinions about your filesystem layout.

[rclone](https://github.com/rclone/rclone) solves the cross-cloud unification well, but its mount story is per-OS fiddly: each platform needs its own filesystem driver, sometimes root, sometimes a separate install. The config is global; if you want different rules for `Work/` than `Personal/` you reach for a wrapper script.

Cascade is what happens when you want a single tool, with a single config language, that:

- Presents every backend — Google Drive, S3, the local filesystem — as one virtual tree under one mount point, mounted with the same shape on macOS, Linux, and Windows.
- Streams file content on demand. Directory listings work without downloading anything; only `open()` triggers a fetch.
- Uses the OS's native filesystem APIs — FSKit on macOS (15.4+), FUSE on Linux, WebDAV via the built-in `WebClient` on Windows — with no kernel extension, no admin escalation, no third-party driver.
- Takes its rules from `.cascade` files scattered through the tree (gitignore-style precedence), so behaviour can vary per directory without restarting anything.
- Mirrors the actual Drive web UI — `My Drive`, `Shared drives`, `Shared with me`, `Bin` — instead of pretending every account is one flat folder.

The bet is that one filesystem client that works the same on every laptop is more useful than five vendor clients plus a folder full of rclone scripts.

## Getting started

### Prerequisites

- Rust toolchain pinned in [`rust-toolchain.toml`](rust-toolchain.toml) (currently 1.96.0, edition 2024). `rustup` installs it automatically on first build.
    - **rustup (recommended)** — the project pins its Rust toolchain in `rust-toolchain.toml`. Install rustup from <https://rustup.rs/> so local builds, the pre-push hook, and CI all use the same compiler.
- macOS: Xcode Command Line Tools (for Swift File Provider and FSKit extensions). FSKit requires macOS 15.4+ (Sequoia).
- Linux: `libfuse3` runtime libraries for the FUSE presenter (`apt install libfuse3-dev` on Debian/Ubuntu, `yum install fuse3-devel` on RHEL/Fedora). NFS fallback additionally needs root to bind a privileged port.
- Windows: the built-in `WebClient` service for the WebDAV mount. It ships with Windows but is often set to manual start — `sc config WebClient start= auto` (in an elevated shell) makes it come up automatically. A native ProjFS presenter (`presenter-projfs`) is implemented and tried first by `cascade start` — it serves directory browsing and on-demand file reads through an engine-backed content provider — with WebDAV as the fallback if ProjFS cannot start (for example when the Client-ProjFS optional feature is disabled).

### Install

Pre-built binaries are published on each release from [GitHub Releases](https://github.com/Mearman/cascade/releases).

- **macOS and Linux (Homebrew)**:

  ```bash
  brew install Mearman/cascade/cascade
  ```

  The [`Formula/cascade.rb`](Formula/cascade.rb) bottle covers both `aarch64` and `x86_64` on macOS and Linux.

- **Linux (direct download)**: grab `cascade-aarch64-linux.tar.gz` or `cascade-x86_64-linux.tar.gz` from the release, extract, and place `cascade` on your `PATH`:

  ```bash
  curl -L https://github.com/Mearman/cascade/releases/latest/download/cascade-x86_64-linux.tar.gz | tar -xz
  install -m 0755 cascade ~/.local/bin/
  ```

- **Windows (Scoop)**:

  ```powershell
  scoop bucket add cascade https://github.com/Mearman/scoop-cascade
  scoop install cascade
  ```

- **Windows (direct download)**: grab `cascade-x86_64-windows.zip` from the release, extract, and place `cascade.exe` on your `PATH`.

- **From source (any platform)**: see [Build](#build) below.

### Build

```bash
cargo build --release

# Specific presenter
cargo build --release --features presenter-nfs
cargo build --release --features presenter-fileprovider  # macOS only

# Including Swift extensions (macOS)
cargo build --release
xcodebuild -project swift/CascadeFileProvider.xcodeproj -scheme CascadeFileProviderHost -configuration Release -destination "platform=macOS" build
cd swift/CascadeFSKit && xcodebuild
```

The `Makefile` wraps the common workflows:

```bash
make release   # cargo build --release
make build     # cargo build (debug)
make start     # build then run the daemon
make stop      # stop a running daemon
make dev       # cargo watch with debug logging
make debug     # run the release binary with RUST_LOG=debug
```

### Run

```bash
cascade backend add gdrive --name personal
cascade start
cascade status
cascade pin Documents/Accounts/
cascade stop
```

### Running as a background service

`cascade service` manages the daemon as an OS background service. The default scope is per-user and requires no administrator rights: a launchd `LaunchAgent` on macOS, a systemd `--user` unit on Linux, and a logon Scheduled Task on Windows.

```bash
cascade service install    # write the service definition and register it
cascade service start      # start the registered service
cascade service status     # show whether the service is registered and running
cascade service stop       # stop the service
cascade service uninstall  # deregister the service and remove its definition
```

The scope is selected in this order: an explicit `--user` or `--system` flag, then inference from the session (an interactive GUI desktop session picks the user scope; a headless host picks the system scope), then — only when there is both a GUI desktop and a terminal — a prompt that defaults to the user scope. The chosen scope and the reason for it are always printed. The `--system` scope installs a machine-wide service: on Linux it writes a systemd system unit (requires root); on macOS and Windows it errors clearly — a system-scoped service in session 0 cannot drive File Provider, FSKit, ProjFS, or WebDAV, which all require a user session, and this is a documented platform limitation rather than a missing feature.

If you installed via Homebrew, `brew services start cascade` works out of the box — the formula ships a `service` block that delegates to `cascade start`.

The daemon exits cleanly with a log message when no backends are configured, so a freshly-installed service does not crash-loop before `cascade backend add` has been run.

## Build, test, and lint

```bash
cargo test --workspace              # all unit tests
cargo test --test integration       # integration tests (require mock backend)
cargo test -p backend-gdrive        # single crate
cargo clippy --workspace            # lint
cargo fmt --check                   # format check
```

## Architecture

The design is documented in full at [`docs/design.md`](docs/design.md). This section covers the high-level structure.

```
┌──────────────────────────────────────────────────────────┐
│  Platform Layer (per-OS)                                 │
│  macOS:   File Provider · FSKit (15.4+) · WebDAV · NFS   │
│  Linux:   FUSE · NFS (root)                              │
│  Windows: ProjFS · WebDAV via WebClient                  │
│  Universal fallback: NFS server · WebDAV server          │
└────────────────────┬─────────────────────────────────────┘
                     │ VfsPresenter trait
┌────────────────────▼─────────────────────────────────────┐
│  Cascade Engine (Rust)                                   │
│  VFS Tree · .cascade config walk · Cache Manager         │
│  Backend trait · Expression Evaluator · P2P Engine (BEP) │
└──────────────────────────────────────────────────────────┘
```

`cascade start` tries the platform-preferred presenters in order and falls back as each one fails: on macOS that's FSKit → WebDAV → NFS; on Linux it's FUSE → NFS; on Windows it's ProjFS → WebDAV (mounted via `net use *` against the built-in `WebClient` service). The Windows ProjFS presenter implements the full callback table — directory enumeration, placeholder info, file-name queries, on-demand reads, notifications, and cancellation — and serves file contents through an engine-backed content provider. WebDAV remains the fallback for when ProjFS cannot start (for example when the Client-ProjFS optional feature is disabled on the machine).

Communication between the platform layer and the engine uses a Unix domain socket with a length-prefixed JSON protocol, shared by the CLI, the macOS File Provider and FSKit extensions, and any future GUI.

### Workspace structure

```
crates/
  engine/                 VFS tree, backend trait, cache manager, sync, state DB
  cascade-config/         .cascade parsing (4 formats), merge, directory walk
  expr/                   Conditional expression parser (PEG via pest) and evaluator
  p2p/                    BEP protocol, peer discovery (LAN, gossip, announce, Mainline DHT), block store
  cascade-announce-wire/  Announce-server wire contract: signed-candidate types, HMAC write auth, wasm-safe handler
  backend-gdrive/         Google Drive (Drive API v3, OAuth2 device code)
  backend-s3/             S3-compatible
  backend-local/          Local filesystem (adopt-and-sync)
  backend-p2p/            P2P-only content-addressed store (no cloud authority; blocks local, metadata in SQLite)
  presenter-nfs/          NFSv3 server
  presenter-fuse/         Linux FUSE presenter
  presenter-webdav/       WebDAV server presenter (cross-platform)
  presenter-fileprovider/ macOS File Provider bridge (Rust side)
  presenter-fskit/        macOS FSKit bridge (Rust side, macOS 15.4+)
  presenter-projfs/       Windows ProjFS presenter
  cascade/                Binary crate (CLI entry point and daemon)
  relay-server/           Opaque byte-pipe relay (binary): pairs two WebSocket clients by session ID for WAN NAT traversal, HMAC-gated, never inspects payload
swift/
  CascadeFileProvider/    macOS File Provider extension
  CascadeFSKit/           macOS FSKit extension (15.4+)
workers/
  announce/               Stateless announce-server Cloudflare Worker (workers-rs, KV soft state)
```

Integration tests live inside each crate's `tests/` directory rather than at workspace root.

### Key abstractions

- **`Backend` trait** — every cloud provider and the local filesystem implement this. The engine never sees provider-specific APIs. Each backend crate exposes `create_backend(config) -> Result<Box<dyn Backend>>`.
- **`VfsTree`** — composes multiple backends into a single tree, routed by longest-prefix match. Cross-backend moves trigger download + upload + delete.
- **`VfsPresenter` trait** — platform-agnostic interface for presenting the VFS to the OS. Compile-time selection: FSKit on macOS (with WebDAV and NFS fallbacks), FUSE on Linux (with NFS fallback), ProjFS on Windows with WebDAV via WebClient as fallback, NFS as universal fallback.
- **`.cascade` config walk** — like `.gitignore`: files in each directory layer with child-overrides-parent precedence. Four formats (gitignore-style, TOML, YAML, JSON) all deserialise to `CascadeConfig`.
- **Expression language** — PEG grammar evaluated against `EvalContext` (file, device, disk, network, power, time, peer). Used for conditional rules in `.cascade` files.
- **P2P engine** — based on Syncthing's BEP v1. Sits between VFS and cache as an optimisation layer, not as a backend. Cloud remains the authority for cloud-backed folders.
- **Node management plane** — a trusted device administers another over the authenticated peer connection. Authority is modelled as capability grants (a verb over a scope) held on the managed node; the `ManageRequest` / `ManageResponse` BEP frames carry the full command set — status read, pin/unpin, cache evict/warm, config push, policy set, backend add/remove, daemon restart/stop, and grant delegation/revocation — and dispatch into the same handlers the local CLI drives, gated by per-command authorisation and an append-only audit log. The dangerous verbs (backend, lifecycle, grant administration) are never satisfied by a node-wide grant and must be granted explicitly for a folder scope, and a delegated grant must be a subset of authority the caller can itself exercise. `cascade grant add|list|revoke|audit` administers the capabilities a node confers; `cascade remote <device-id>` drives a target reached over the discovery and connectivity stack. On top of the on-node grant list sits the signed capability-token model: `cascade token issue|revoke|list` mints and revokes tokens signed by the issuing node's real device key, which the bearer carries and presents with `--token <file>` on `cascade remote`. The node verifies the signature, expiry, and revocation list, then authorises the carried grant through the same path an on-node grant takes. Delegated tokens form bounded chains — each hop can only narrow authority, never widen it — and a token's expiry is clamped to its parent's, so a delegate never outlives the authority it derived from.

### State database

SQLite at `~/.config/cascade/state.db`. Tables: `files`, `backends`, `pin_rules`, `lifecycle_policies`, `config_cache`, `sync_cursors`, `p2p_peers`, `p2p_block_index`, `grants`, `manage_audit`. Full schema in [`docs/design.md`](docs/design.md).

## Conventions

- **Rust edition 2024** with `async_trait` for the Backend and VfsPresenter traits.
- **Error handling** with `anyhow` for applications and `thiserror` for library crate error types.
- **Serde** for all serialisation (JSON wire protocol, TOML/YAML/JSON config, SQLite bridge types).
- **Platform context** is injected via traits (`PlatformContext`), not pulled from global APIs. Each OS provides its own implementation behind the same contract.
- **Backend crates** are self-contained: each exposes exactly one `create_backend` function and implements the `Backend` trait.
- **Config merge semantics** differ by concern: ignore rules and pins accumulate, lifecycle policies are child-first first-match-wins, cache settings are nearest-wins, device config is root-only.
- **Strict workspace lints** in `Cargo.toml`: pedantic, nursery, and cargo Clippy groups are denied, plus `unwrap_used`, `expect_used`, `indexing_slicing`, `string_slice`, and `unsafe_code`. New code must satisfy these without `#[allow]` escapes.

## Gotchas and quirks

- **Linux FUSE runs without root.** The FUSE presenter mounts as the calling user via `fusermount3`. NFS fallback is different — binding a privileged port for NFS still needs `sudo`, so if FUSE is unavailable (missing `libfuse3`, no `/dev/fuse`) and you don't want to escalate, the daemon will fail rather than silently downgrade.
- **Windows mounts go via WebDAV and the WebClient service.** `cascade start` runs `net use * http://localhost:<port>/` which lets the OS pick the next free drive letter. If `WebClient` isn't running the mount fails immediately; start it once with `sc start WebClient` or set it to auto-start (see Prerequisites).
- **Google Drive auth tokens are stored per platform.** macOS and Linux: `$XDG_CONFIG_HOME/cascade/gdrive-tokens/<account>.json`, falling back to `~/.config/cascade/...`. Windows: `%APPDATA%\cascade\gdrive-tokens\<account>.json`, with the default NTFS ACLs restricting access to the current user.
- **NFS cache mode** controls write support, typed as the `NfsCacheMode` enum (`off`/`minimal`/`full`). `off` is read-only — writes are refused with `NFS3ERR_ROFS`/`NFS4ERR_ROFS`. `minimal` (default) is write-capable with minimal disk usage; `full` caches everything eagerly. Write-capable modes implement the full NFSv3 and NFSv4 write procedure set: `WRITE`, `CREATE`, `SETATTR` (size/mtime), `MKDIR`, `REMOVE`, `RMDIR`, `RENAME`, and `COMMIT`. All write procedures route through the same backend operations (`upload`, `update`, `create_dir`, `delete`, `rename`) as the WebDAV presenter.
- **P2P exposure is one posture, not a pile of flags.** A single `DiscoveryReach` (`lan-only`/`private`/`public`) governs how far a device reaches for peers, defaulting to `private` — LAN discovery, gossip, hole punch, and peer relay across a trusted mesh, with nothing published to any global directory. Each discovery source self-activates only when the posture permits its level *and* it has what it needs (a bound listener, a configured server). Global publication to the Mainline DHT and announce servers is opt-in via the `public` posture; the DHT's bootstrap set is always present (empty falls back to the built-in public nodes), so disabling the DHT is a posture choice, not missing config. **Rendezvous-by-presence** is a complementary live-pairing path: two peers register under the same rendezvous key at the same instant, and the `RendezvousBroker` — a distinct in-memory component hosted alongside the relay server's `SessionRegistry`, not the same registry — exchanges their candidate sets and hole-punch agreements so they can connect directly; no durable state remains once either side disconnects. It activates under the `public` posture only and requires a configured rendezvous endpoint.
- **Google Drive rate limits** — ~10,000 requests per 100 seconds per user. The backend uses a token-bucket rate limiter and batch requests where possible.
- **Cross-backend moves** are not atomic — they download from source, upload to destination, then delete the original. A failure partway through can leave duplicates.
- **P2P block sizes are adaptive**: 128KB for files under 250MB, 512KB for 250MB–1GB, 1MB for files over 1GB. This is not configurable per-file.
- **Conflict copies are never auto-deleted.** The losing version is renamed with the device name and date (e.g. `report (work-laptop 2026-05-27).conflict`). P2P-only folders use last-write-wins per-block.
- **Shadowing in nested mounts**: if a child mount point exists in the parent backend with files, the child takes over and the parent's files are hidden (not deleted). Removing the child reveals them.
- **Nested-mount listing is presenter-dependent (follow-up).** Reads and writes *route* into a nested mount correctly on every presenter (longest-prefix match). But injecting the child mount-point directory into its parent's listing — so the nested mount actually *appears* when you list the parent — is currently wired only through the NFS presenter (via `VfsTree::read_dir`). The WebDAV presenter (the practical default on macOS/Windows) and FUSE list by parent id and do not yet inject child mount-point directories, and the WebDAV root listing renders a multi-segment nested prefix (`work/projects`) as a single flat collection. Routing works; the listing/shadowing of a nested mount under WebDAV/FUSE is a tracked follow-up.
- **Pre-1.0 path-shape change (uniform-backend-mounts refactor).** Each configured backend now mounts at a named prefix under a neutral VFS root rather than at the bare root. A single backend named `gdrive` appears at `gdrive/Documents/…` instead of `Documents/…`. To keep the old layout, add `mount = "/"` to that backend's config — an at-root mount is a first-class configuration, not a special case. Pin rules, lifecycle policies, and any stored `files.path` values written before this change need updating to include the mount prefix (e.g. `Documents/**` → `gdrive/Documents/**`). The state database schema is unchanged; only the semantics of `files.path` changed from a backend-relative path to a full VFS-absolute path.
- **Multiple `.cascade` formats in one directory** is allowed — they merge in deterministic order: gitignore-style → TOML → YAML → JSON, with last-writer-wins for scalar settings.
- **Device identity** is derived from a self-generated TLS certificate (SHA-256 of cert, base32-encoded). All P2P connections are TLS-encrypted and authenticated by device ID.

## Roadmap

| Phase | Scope |
|-------|-------|
| v1 | NFS mount + `.cascade` (ignore only) + single backend (read-only, Google Drive) |
| v2 | Pinning + lifecycle + cache manager |
| v3 | Write-back + multi-backend + nested mounts + conflict resolution |
| v4 | Conditional rules (expressions + context providers) |
| v5 | macOS File Provider presenter (Swift extension), implemented |
| v6 | Adopt existing directories (local backend, adopt-and-sync, adopt-in-place) |
| v7 | P2P block sharing (LAN) |
| v8 | Windows native ProjFS presenter, implemented (Linux FUSE delivered earlier) |
| v9 | Full P2P (WAN discovery, NAT traversal), implemented |
| v10 | Node management plane (capability grants, remote administration over BEP), implemented |
| v11 | OS background service (`cascade service install|start|stop|status|uninstall`): per-user LaunchAgent on macOS, systemd `--user` unit on Linux, logon Scheduled Task on Windows, implemented |

Full timeline estimates, dependency list, and reference implementations in [`docs/design.md`](docs/design.md).

## References

- **Design specification** — [`docs/design.md`](docs/design.md): core types, Backend trait definition, VFS tree implementation, `.cascade` parser details, expression grammar, NFS/FUSE/File Provider presenter internals, state database schema, wire protocol, Google Drive backend details.
- **Deployment guide** — [`docs/deployment.md`](docs/deployment.md): deploying the announce Worker and relay server for WAN peer discovery and NAT traversal.
- **Reference implementations** — [rclone](https://github.com/rclone/rclone) (NFS, VFS caching, Google Drive), [Syncthing](https://github.com/syncthing/syncthing) (BEP, peer discovery, NAT traversal), [go-nfs](https://github.com/willscott/go-nfs) (NFSv3/XDR), [WinFSP](https://github.com/winfsp/winfsp) (Windows virtual filesystem).
