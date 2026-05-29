# Cascade

> Cross-platform cloud storage filesystem client built in Rust. On-demand file access, nested `.cascade` config with directory-walk precedence, offline pinning, policy-driven lifecycle management, P2P block sync, and multi-backend support. Uses native platform APIs (File Provider on macOS, ProjFS on Windows, FUSE on Linux) with NFS fallback — no kernel extensions required.

Rust (edition 2024) · Swift (macOS File Provider extension) · SQLite state · Tokio async runtime

## Getting started

### Prerequisites

- Rust toolchain pinned in [`rust-toolchain.toml`](rust-toolchain.toml) (currently 1.96.0, edition 2024). `rustup` installs it automatically on first build.
- macOS: Xcode Command Line Tools (for Swift File Provider and FSKit extensions). FSKit requires macOS 15.4+ (Sequoia).
- Linux: `libfuse3-dev` (for FUSE).
- Windows: not yet supported (WinFSP/ProjFS presenter is planned, not implemented).

### Build

```bash
cargo build --release

# Specific presenter
cargo build --release --features presenter-nfs
cargo build --release --features presenter-fileprovider  # macOS only

# Including Swift extensions (macOS)
cargo build --release && cd swift/CascadeFileProvider && xcodebuild
cargo build --release && cd swift/CascadeFSKit && xcodebuild
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
│  macOS: File Provider · FSKit (15.4+) · Linux: FUSE      │
│  Windows: WinFSP/ProjFS (planned)                        │
│  Universal fallback: NFS server · WebDAV server          │
└────────────────────┬─────────────────────────────────────┘
                     │ VfsPresenter trait
┌────────────────────▼─────────────────────────────────────┐
│  Cascade Engine (Rust)                                   │
│  VFS Tree · .cascade config walk · Cache Manager         │
│  Backend trait · Expression Evaluator · P2P Engine (BEP) │
└──────────────────────────────────────────────────────────┘
```

Communication between the platform layer and the engine uses a Unix domain socket with a length-prefixed JSON protocol, shared by the CLI, the macOS File Provider and FSKit extensions, and any future GUI.

### Workspace structure

```
crates/
  engine/                 VFS tree, backend trait, cache manager, sync, state DB
  cascade-config/         .cascade parsing (4 formats), merge, directory walk
  expr/                   Conditional expression parser (PEG via pest) and evaluator
  p2p/                    BEP protocol, peer discovery, block store
  backend-gdrive/         Google Drive (Drive API v3, OAuth2 device code)
  backend-s3/             S3-compatible
  backend-local/          Local filesystem (adopt-and-sync)
  presenter-nfs/          NFSv3 server
  presenter-fuse/         Linux FUSE presenter
  presenter-webdav/       WebDAV server presenter (cross-platform)
  presenter-fileprovider/ macOS File Provider bridge (Rust side)
  presenter-fskit/        macOS FSKit bridge (Rust side, macOS 15.4+)
  cascade/                Binary crate (CLI entry point and daemon)
swift/
  CascadeFileProvider/    macOS File Provider extension
  CascadeFSKit/           macOS FSKit extension (15.4+)
```

Integration tests live inside each crate's `tests/` directory rather than at workspace root.

### Key abstractions

- **`Backend` trait** — every cloud provider and the local filesystem implement this. The engine never sees provider-specific APIs. Each backend crate exposes `create_backend(config) -> Result<Box<dyn Backend>>`.
- **`VfsTree`** — composes multiple backends into a single tree, routed by longest-prefix match. Cross-backend moves trigger download + upload + delete.
- **`VfsPresenter` trait** — platform-agnostic interface for presenting the VFS to the OS. Compile-time selection: File Provider on macOS, FUSE on Linux, WinFSP on Windows, NFS as universal fallback.
- **`.cascade` config walk** — like `.gitignore`: files in each directory layer with child-overrides-parent precedence. Four formats (gitignore-style, TOML, YAML, JSON) all deserialise to `CascadeConfig`.
- **Expression language** — PEG grammar evaluated against `EvalContext` (file, device, disk, network, power, time, peer). Used for conditional rules in `.cascade` files.
- **P2P engine** — based on Syncthing's BEP v1. Sits between VFS and cache as an optimisation layer, not as a backend. Cloud remains the authority for cloud-backed folders.

### State database

SQLite at `~/.config/cascade/state.db`. Tables: `files`, `backends`, `pin_rules`, `lifecycle_policies`, `config_cache`, `sync_cursors`, `p2p_peers`, `p2p_block_index`. Full schema in [`docs/design.md`](docs/design.md).

## Conventions

- **Rust edition 2024** with `async_trait` for the Backend and VfsPresenter traits.
- **Error handling** with `anyhow` for applications and `thiserror` for library crate error types.
- **Serde** for all serialisation (JSON wire protocol, TOML/YAML/JSON config, SQLite bridge types).
- **Platform context** is injected via traits (`PlatformContext`), not pulled from global APIs. Each OS provides its own implementation behind the same contract.
- **Backend crates** are self-contained: each exposes exactly one `create_backend` function and implements the `Backend` trait.
- **Config merge semantics** differ by concern: ignore rules and pins accumulate, lifecycle policies are child-first first-match-wins, cache settings are nearest-wins, device config is root-only.
- **Strict workspace lints** in `Cargo.toml`: pedantic, nursery, and cargo Clippy groups are denied, plus `unwrap_used`, `expect_used`, `indexing_slicing`, `string_slice`, and `unsafe_code`. New code must satisfy these without `#[allow]` escapes.

## Gotchas and quirks

- **NFS cache mode** controls write support. `off` is read-only. `minimal` (default) enables writes with minimal disk usage. `full` caches everything eagerly.
- **Google Drive rate limits** — ~10,000 requests per 100 seconds per user. The backend uses a token-bucket rate limiter and batch requests where possible.
- **Cross-backend moves** are not atomic — they download from source, upload to destination, then delete the original. A failure partway through can leave duplicates.
- **P2P block sizes are adaptive**: 128KB for files under 250MB, 512KB for 250MB–1GB, 1MB for files over 1GB. This is not configurable per-file.
- **Conflict copies are never auto-deleted.** The losing version is renamed with the device name and date (e.g. `report (work-laptop 2026-05-27).conflict`). P2P-only folders use last-write-wins per-block.
- **Shadowing in nested mounts**: if a child mount point exists in the parent backend with files, the child takes over and the parent's files are hidden (not deleted). Removing the child reveals them.
- **Multiple `.cascade` formats in one directory** is allowed — they merge in deterministic order: gitignore-style → TOML → YAML → JSON, with last-writer-wins for scalar settings.
- **Device identity** is derived from a self-generated TLS certificate (SHA-256 of cert, base32-encoded). All P2P connections are TLS-encrypted and authenticated by device ID.

## Roadmap

| Phase | Scope |
|-------|-------|
| v1 | NFS mount + `.cascade` (ignore only) + single backend (read-only, Google Drive) |
| v2 | Pinning + lifecycle + cache manager |
| v3 | Write-back + multi-backend + nested mounts + conflict resolution |
| v4 | Conditional rules (expressions + context providers) |
| v5 | macOS File Provider presenter (Swift extension) |
| v6 | Adopt existing directories (local backend, adopt-and-sync, adopt-in-place) |
| v7 | P2P block sharing (LAN) |
| v8 | Linux FUSE presenter + Windows WinFSP presenter |
| v9 | Full P2P (WAN discovery, NAT traversal) |

Full timeline estimates, dependency list, and reference implementations in [`docs/design.md`](docs/design.md).

## References

- **Design specification** — [`docs/design.md`](docs/design.md): core types, Backend trait definition, VFS tree implementation, `.cascade` parser details, expression grammar, NFS/FUSE/File Provider presenter internals, state database schema, wire protocol, Google Drive backend details.
- **Reference implementations** — [rclone](https://github.com/rclone/rclone) (NFS, VFS caching, Google Drive), [Syncthing](https://github.com/syncthing/syncthing) (BEP, peer discovery, NAT traversal), [go-nfs](https://github.com/willscott/go-nfs) (NFSv3/XDR), [WinFSP](https://github.com/winfsp/winfsp) (Windows virtual filesystem).
