# Cascade

A cross-platform cloud storage filesystem client built in Rust. Combines on-demand file access, nested `.cascade` config files with directory-walk precedence, offline pinning, policy-driven lifecycle management, P2P block sync, and multi-backend support. Uses native platform APIs where available (File Provider on macOS, ProjFS on Windows, FUSE on Linux) with NFS as a fallback — no kernel extensions required.

## Table of contents

- [Architecture](#architecture)
- [Workspace layout](#workspace-layout)
- [Core types](#core-types)
- [Backend trait](#backend-trait)
- [VFS tree](#vfs-tree)
- [Platform presenter layer](#platform-presenter-layer)
- [`.cascade` config system](#cascade-config-system)
- [Expression language](#expression-language)
- [Cache manager](#cache-manager)
- [P2P engine](#p2p-engine)
- [State database](#state-database)
- [CLI interface](#cli-interface)
- [Adopting existing directories](#adopting-existing-directories)
- [Nested mounts](#nested-mounts)
- [macOS File Provider extension](#macos-file-provider-extension)
- [Google Drive backend](#google-drive-backend)
- [Conflict resolution](#conflict-resolution)
- [Build and run](#build-and-run)
- [Roadmap](#roadmap)

## Architecture

```
┌──────────────────────────────────────────────────────────┐
│  Platform Layer (per-OS)                                 │
│                                                          │
│  macOS: File Provider extension (Swift)                  │
│  Linux: FUSE (fuser)                                     │
│  Windows: WinFSP / ProjFS                                │
│  Fallback: NFS server (all platforms)                    │
└────────────────────┬─────────────────────────────────────┘
                     │ VfsPresenter trait
┌────────────────────▼─────────────────────────────────────┐
│                    Cascade Engine (Rust)                  │
│                                                          │
│  ┌─────────────┐  ┌──────────────┐  ┌────────────────┐  │
│  │  VFS Tree   │  │  .cascade    │  │  Cache Manager │  │
│  │             │  │  config walk │  │                │  │
│  │  nested     │  │              │  │  pinning       │  │
│  │  backends   │  │  merge +     │  │  lifecycle     │  │
│  │  routing    │  │  precedence  │  │  eviction      │  │
│  └──────┬──────┘  └──────────────┘  └────────────────┘  │
│         │                                                │
│  ┌──────▼──────────────────────────────────────────┐     │
│  │  Backend trait                                   │     │
│  │  gdrive │ s3 │ webdav │ dropbox │ onedrive │ local │  │
│  └─────────────────────────────────────────────────┘     │
│                                                          │
│  ┌──────────────┐  ┌───────────────┐                     │
│  │  Expression  │  │  P2P Engine   │                     │
│  │  Evaluator   │  │  (BEP)        │                     │
│  └──────────────┘  └───────────────┘                     │
└──────────────────────────────────────────────────────────┘
```

Communication between the platform layer and the engine uses a Unix domain socket with a length-prefixed JSON protocol. The same protocol is shared by the CLI, the macOS File Provider extension, and any future GUI.

## Workspace layout

```
cascade/
├── Cargo.toml                    # workspace root
├── crates/
│   ├── engine/                   # Core: VFS tree, backend trait, cache manager
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── vfs/
│   │       │   ├── mod.rs
│   │       │   ├── tree.rs       # VfsTree — routing, nested mounts
│   │       │   └── node.rs       # VfsNode — per-directory state
│   │       ├── cache/
│   │       │   ├── mod.rs
│   │       │   ├── manager.rs    # CacheManager — pinning, eviction, warming
│   │       │   ├── store.rs      # CacheStore — disk operations
│   │       │   └── lifecycle.rs  # LifecyclePolicy evaluation
│   │       ├── backend.rs        # Backend trait definition
│   │       ├── sync/
│   │       │   ├── mod.rs
│   │       │   ├── change.rs     # Change detection and reconciliation
│   │       │   └── conflict.rs   # Conflict detection and resolution
│   │       ├── platform/
│   │       │   ├── mod.rs        # PlatformContext trait
│   │       │   ├── macos.rs      # IOKit, SystemConfiguration
│   │       │   ├── linux.rs      # /sys/class, NetworkManager DBus
│   │       │   └── windows.rs    # WinRT APIs
│   │       ├── db.rs             # SQLite state database
│   │       └── protocol.rs       # Engine ↔ presenter wire protocol
│   ├── cascade/                  # .cascade parsing (4 formats), merge, directory walk
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── parse/
│   │       │   ├── mod.rs
│   │       │   ├── gitignore.rs  # gitignore-style .cascade parser
│   │       │   ├── toml.rs       # .cascade.toml parser
│   │       │   ├── yaml.rs       # .cascade.yaml parser
│   │       │   └── json.rs       # .cascade.json parser
│   │       ├── merge.rs          # ResolvedConfig builder, directory walk
│   │       └── types.rs          # CascadeConfig, IgnoreRule, LifecyclePolicy, PinRule
│   ├── expr/                     # Conditional expression parser and evaluator
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── grammar.pest      # PEG grammar for expression language
│   │       ├── ast.rs            # Expression AST types
│   │       ├── eval.rs           # Expression evaluator against EvalContext
│   │       └── context.rs        # EvalContext, FileContext, DiskContext, etc.
│   ├── p2p/                      # BEP protocol, peer discovery, block store
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── protocol/
│   │       │   ├── mod.rs
│   │       │   ├── bep.rs        # Block Exchange Protocol messages
│   │       │   ├── xdr.rs        # XDR encode/decode (BEP uses XDR)
│   │       │   └── hello.rs      # ClusterConfig exchange
│   │       ├── discovery/
│   │       │   ├── mod.rs
│   │       │   ├── local.rs      # UDP multicast (LAN)
│   │       │   ├── global.rs     # Global discovery server client
│   │       │   └── relay.rs      # Relay connection
│   │       ├── block/
│   │       │   ├── mod.rs
│   │       │   ├── store.rs      # BlockStore — split, hash, reassemble
│   │       │   └── index.rs      # Block index (file → block hashes)
│   │       ├── peer.rs           # Peer connection management
│   │       ├── device.rs         # Device ID (TLS cert fingerprint)
│   │       └── nat.rs            # STUN hole punching
│   ├── backend-gdrive/           # Google Drive (Drive API v3)
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── client.rs         # HTTP client, rate limiting, retry
│   │       ├── auth.rs           # OAuth2 device code + refresh flow
│   │       ├── changes.rs        # Changes API (cursor-based)
│   │       └── model.rs          # File, Folder, Shared Drive types
│   ├── backend-s3/               # S3-compatible (any provider)
│   ├── backend-webdav/           # Generic WebDAV
│   ├── backend-dropbox/          # Dropbox Files API
│   ├── backend-onedrive/         # OneDrive Graph API
│   ├── backend-local/            # Local filesystem (adopt-and-sync)
│   ├── presenter-nfs/            # NFS server + FUSE presenter
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── nfs/
│   │       │   ├── mod.rs
│   │       │   ├── server.rs     # NFSv3 server (RFC 1813)
│   │       │   ├── mount.rs      # Mount protocol (RFC 1814)
│   │       │   ├── xdr.rs        # XDR codec
│   │       │   └── procedures.rs # NFS procedure handlers
│   │       └── fuse/
│   │           └── presenter.rs  # fuser-based FUSE presenter (Linux)
│   ├── presenter-fileprovider/   # macOS File Provider bridge (Rust side)
│   │   └── src/
│   │       ├── lib.rs
│   │       └── bridge.rs         # Unix domain socket server for Swift extension
│   ├── presenter-winfsp/         # Windows WinFSP / ProjFS presenter
│   └── cascade/                  # Binary crate
│       └── src/
│           ├── main.rs           # CLI entry point, presenter selection
│           ├── cli/
│           │   ├── mod.rs
│           │   ├── mount.rs      # cascade start / stop
│           │   ├── pin.rs        # cascade pin / unpin
│           │   ├── cache.rs      # cascade cache status / evict / warm
│           │   ├── adopt.rs      # cascade adopt
│           │   ├── status.rs     # cascade status
│           │   └── config.rs     # cascade config
│           └── daemon.rs         # Background daemon management
├── swift/
│   └── CascadeFileProvider/
│       ├── CascadeFileProviderExtension.swift
│       ├── CascadeEngineClient.swift    # Unix socket client
│       ├── CascadeFileProviderItem.swift # NSFileProviderItem bridging
│       └── Info.plist
├── windows/
│   └── CascadeShellExt/                 # (optional, later)
└── tests/
    ├── integration/
    │   ├── backend_gdrive.rs
    │   ├── cascade_config.rs
    │   ├── vfs_tree.rs
    │   └── lifecycle.rs
    └── fixtures/
        ├── cascade_configs/             # Sample .cascade files
        └── backends/                    # Mock backend responses
```

## Core types

```rust
/// Unique identifier for a file or directory across all backends.
/// Format: "{backend_id}:{backend_native_id}"
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct ItemId(String);

/// Unique identifier for a file within a single backend.
/// Each backend defines its own concrete type.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct FileId(String);

/// Cursor for incremental change tracking.
/// Opaque to the engine — stored and passed through to backends.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Cursor(String);

/// A file or directory in the VFS.
#[derive(Debug, Clone)]
struct FileEntry {
    id: ItemId,
    parent_id: ItemId,
    name: String,
    is_dir: bool,
    size: Option<u64>,
    mod_time: Option<DateTime<Utc>>,
    mime_type: Option<String>,
    hash: Option<String>,
}

/// A change event from a backend.
#[derive(Debug)]
enum Change {
    Created(FileEntry),
    Updated { old: FileEntry, new: FileEntry },
    Deleted(FileEntry),
    Moved { from: FileEntry, to: FileEntry },
}

/// Cache state for a file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum CacheState {
    /// Metadata only — file exists in the backend but not on local disk.
    Online,
    /// Full file is on local disk. May be evicted by lifecycle policies.
    Cached,
    /// Full file on disk. Never evicted by lifecycle. Only removed by explicit unpin.
    Pinned,
    /// Currently downloading from backend.
    Downloading,
}

/// Provenance — where a file's content physically lives.
#[derive(Debug, Clone, PartialEq)]
enum Provenance {
    /// File exists in the cloud backend only, not yet downloaded.
    CloudOnly,
    /// File exists in the local cache (downloaded from cloud or adopted).
    Cached { local_path: PathBuf },
    /// File exists on the local filesystem, managed by a local backend.
    /// Read/write directly from disk, not from cache.
    Local { disk_path: PathBuf },
    /// File exists both locally and in the cloud — synced via adopt-and-sync.
    Synced { disk_path: PathBuf, cloud_id: FileId },
}

/// An item as presented to the platform layer.
#[derive(Debug, Clone)]
struct VfsItem {
    id: ItemId,
    parent_id: ItemId,
    name: String,
    is_dir: bool,
    size: Option<u64>,
    mod_time: Option<DateTime<Utc>>,
    cache_state: CacheState,
    mime_type: Option<String>,
}

/// Storage quota information.
#[derive(Debug, Clone)]
struct Quota {
    total: Option<u64>,
    used: Option<u64>,
    available: Option<u64>,
}
```

## Backend trait

Every cloud provider and the local filesystem implement this trait. The engine never sees provider-specific APIs.

```rust
use async_trait::async_trait;

#[async_trait]
trait Backend: Send + Sync {
    /// Unique identifier for this backend instance (e.g. "gdrive-personal", "s3-work").
    fn id(&self) -> &str;

    /// Display name for the mount point (e.g. "Google Drive (Personal)").
    fn display_name(&self) -> &str;

    /// Total and available quota, if the backend reports it.
    async fn quota(&self) -> Result<Option<Quota>>;

    /// Stream changes since the given cursor. Returns a new cursor.
    /// If cursor is None, returns a full snapshot of the tree.
    async fn changes(&self, cursor: Option<&Cursor>) -> Result<(Vec<Change>, Cursor)>;

    /// Fetch metadata for a single file or directory by path.
    async fn metadata(&self, path: &Path) -> Result<FileEntry>;

    /// Download file content. The backend writes to the provided writer.
    async fn download(&self, file: &FileEntry, writer: &mut dyn tokio::io::AsyncWrite) -> Result<()>;

    /// Upload file content, replacing the existing file or creating a new one.
    async fn upload(
        &self,
        path: &Path,
        reader: &mut dyn tokio::io::AsyncRead,
        parent_id: &FileId,
    ) -> Result<FileEntry>;

    /// Create a directory.
    async fn create_dir(&self, path: &Path) -> Result<FileEntry>;

    /// Delete a file or directory.
    async fn delete(&self, file: &FileEntry) -> Result<()>;

    /// Move/rename a file or directory.
    async fn move_entry(&self, src: &Path, dst: &Path) -> Result<FileEntry>;

    /// Recommended poll interval for this backend. None if the backend
    /// doesn't support polling (use fixed interval from config instead).
    async fn poll_interval(&self) -> Option<Duration>;
}
```

### Backend registration

Each backend crate exposes a single constructor function:

```rust
// In backend-gdrive:
pub fn create_backend(config: &toml::Value) -> Result<Box<dyn Backend>>;

// In backend-s3:
pub fn create_backend(config: &toml::Value) -> Result<Box<dyn Backend>>;
```

The binary crate registers backends by name at startup:

```rust
fn create_backend_by_name(name: &str, config: &toml::Value) -> Result<Box<dyn Backend>> {
    match name {
        "gdrive" => backend_gdrive::create_backend(config),
        "s3" => backend_s3::create_backend(config),
        "webdav" => backend_webdav::create_backend(config),
        "dropbox" => backend_dropbox::create_backend(config),
        "onedrive" => backend_onedrive::create_backend(config),
        "local" => backend_local::create_backend(config),
        "none" => Ok(Box::new(NullBackend::new())),  // P2P-only
        _ => bail!("unknown backend: {}", name),
    }
}
```

### Backend-specific details

| Backend | Auth | Change detection | Empty dirs | File IDs | Rate limits |
|---------|------|-----------------|------------|----------|-------------|
| Google Drive | OAuth2 (device code + refresh) | Changes API (cursor) | Supported | Stable string IDs | Generous (10k/100s per user) |
| Dropbox | OAuth2 (device code + refresh) | Longpoll cursor | Not natively | Rev-based | Strict (per-app) |
| S3 | Sigv4 / access key | List with prefix | Not supported | Key (path) | Per-account |
| WebDAV | Basic / OAuth2 | PROPFIND | Supported | URL | Varies |
| OneDrive | OAuth2 (device code + refresh) | Delta query | Supported | String IDs | Moderate |
| Local | N/A | FSEvents / inotify / ReadDirectoryChanges | Supported | Path | N/A |

## VFS tree

The VFS composes multiple backends into a single tree. Backends are bound to path prefixes. Operations are routed by longest-prefix match.

```rust
struct VfsTree {
    /// The root node — handles paths not covered by any child.
    root: Arc<dyn Backend>,

    /// Sorted list of (path_prefix, backend) bindings.
    /// Sorted longest-prefix-first for correct matching.
    children: Vec<(PathBuf, Arc<dyn Backend>)>,
}

impl VfsTree {
    /// Route a path to the correct backend.
    fn resolve(&self, path: &Path) -> (&Arc<dyn Backend>, PathBuf) {
        // Find the longest matching child prefix
        for (prefix, backend) in &self.children {
            if let Ok(rest) = path.strip_prefix(prefix) {
                return (backend, rest.to_path_buf());
            }
        }
        // No child matches — fall through to root
        (&self.root, path.to_path_buf())
    }

    /// List directory entries, merging backend content with child mount points.
    async fn read_dir(&self, path: &Path) -> Result<Vec<DirEntry>> {
        let mut entries = vec![];

        // 1. Get entries from the backend that owns this path
        let (backend, backend_path) = self.resolve(path);
        entries.extend(backend.read_dir(&backend_path).await?);

        // 2. Inject child mount point directories if this path is their parent
        for (child_prefix, _) in &self.children {
            if child_prefix.parent() == Some(path) {
                let mount_dir_name = child_prefix.file_name().unwrap();
                if !entries.iter().any(|e| e.name == mount_dir_name) {
                    entries.push(DirEntry::dir(mount_dir_name));
                }
            }
        }

        Ok(entries)
    }

    /// Move a file, handling cross-backend transfers.
    async fn rename(&self, src: &Path, dst: &Path) -> Result<()> {
        let (src_backend, src_path) = self.resolve(src);
        let (dst_backend, dst_path) = self.resolve(dst);

        if Arc::ptr_eq(src_backend, dst_backend) {
            // Same backend — simple rename
            src_backend.move_entry(&src_path, &dst_path).await
        } else {
            // Cross-backend — download, upload, delete original
            let entry = src_backend.metadata(&src_path).await?;
            let mut data = Vec::new();
            src_backend.download(&entry, &mut data).await?;
            dst_backend.upload(&dst_path, &mut &data[..], &entry.parent_id).await?;
            src_backend.delete(&entry).await?;
            Ok(())
        }
    }
}
```

### Shadowing rules

When a child mount shadows a directory from the parent backend:

| Situation | Behaviour |
|-----------|-----------|
| Child mount point doesn't exist in parent | Child appears as a directory. No conflict. |
| Child mount point exists in parent as empty dir | Child takes over. Parent's empty dir is invisible. |
| Child mount point exists in parent with files | Child takes over. Parent's files are hidden (not deleted). Removing the child reveals them. |
| Nested child shadows another child | Deepest (longest prefix) wins. |

## Platform presenter layer

The engine is platform-agnostic. Each OS provides a presenter that implements this trait:

```rust
#[async_trait]
trait VfsPresenter: Send + Sync {
    /// A file was added or updated in the VFS.
    async fn upsert_item(&self, item: VfsItem) -> Result<()>;

    /// A file or directory was deleted from the VFS.
    async fn delete_item(&self, id: &ItemId) -> Result<()>;

    /// A file's cache state changed (online → cached → pinned).
    async fn update_state(&self, id: &ItemId, state: CacheState) -> Result<()>;

    /// Download a file's contents (on-demand). Returns local path.
    async fn fetch_contents(&self, id: &ItemId) -> Result<PathBuf>;

    /// Evict a file (free up space).
    async fn evict_item(&self, id: &ItemId) -> Result<()>;

    /// Start presenting the VFS at the given mount point.
    async fn start(&self, mount_point: &Path) -> Result<()>;

    /// Stop presenting.
    async fn stop(&self) -> Result<()>;
}
```

### Compile-time selection

```rust
// cascade/src/main.rs

#[cfg(target_os = "macos")]
fn create_presenter(engine: EngineHandle) -> Box<dyn VfsPresenter> {
    if file_provider_available() {
        Box::new(FileProviderPresenter::new(engine))
    } else {
        Box::new(NfsPresenter::new(engine))
    }
}

#[cfg(target_os = "linux")]
fn create_presenter(engine: EngineHandle) -> Box<dyn VfsPresenter> {
    Box::new(NfsPresenter::new(engine))  // FUSE via fuser
}

#[cfg(target_os = "windows")]
fn create_presenter(engine: EngineHandle) -> Box<dyn VfsPresenter> {
    Box::new(WinFspPresenter::new(engine))  // ProjFS or FUSE via WinFSP
}
```

### NFS presenter

The NFS presenter spins up an NFSv3 server on loopback and mounts it using the OS's native NFS client. Works on all platforms without kernel extensions.

NFSv3 server implementation covers these procedures:

**Required for anything to work:**
- `GETATTR` — return file/directory metadata
- `LOOKUP` — resolve a name in a directory
- `READDIR` / `READDIR3` — list directory contents
- `READ` — read file data
- `FSSTAT` — return filesystem statistics

**For writes:**
- `CREATE` — create a file
- `WRITE` — write data to a file
- `REMOVE` — delete a file
- `RENAME` — rename/move a file
- `MKDIR` — create a directory
- `RMDIR` — remove a directory
- `SETATTR` — set file attributes (modification time, etc.)
- `COMMIT` — flush cached writes

The mount protocol (RFC 1814) has three procedures: `MOUNT`, `DUMP`, `UNMOUNT`.

All NFS data structures use XDR encoding (defined in RFC 1813). The XDR codec handles:
- `uint32`, `uint64`, `int32`, `int64`
- `bool`, `opaque<>`, `string<>`
- Fixed and variable-length arrays
- File handles (`nfs_fh3` — opaque 64-byte handles, internally the ItemId)

**VFS cache modes for NFS:**

| Mode | On-demand? | Writes? | Disk usage |
|------|-----------|---------|------------|
| `off` | Yes | Read-only (NFS limitation) | None |
| `minimal` | Yes | Yes | Minimal |
| `writes` | Yes | Yes | Moderate |
| `full` | No | Yes | High |

`minimal` is the default — on-demand reads, reliable writes, minimal disk usage.

### FUSE presenter (Linux)

Uses the [`fuser`](https://crates.io/crates/fuser) crate. Implements `fuser::Filesystem` trait with `lookup`, `read`, `write`, `readdir`, `getattr`, `setattr`, `mkdir`, `rmdir`, `rename`, `unlink` etc. Each callback routes to the engine's VFS.

No file badges or native placeholder support — CLI provides visibility instead.

### macOS File Provider presenter

See [macOS File Provider extension](#macos-file-provider-extension) below.

### Platform context providers

Conditional rules need platform-specific context. Common trait:

```rust
trait PlatformContext: Send + Sync {
    fn disk_info(&self) -> DiskInfo;
    fn network_info(&self) -> NetworkInfo;
    fn power_info(&self) -> PowerInfo;
    fn device_info(&self) -> DeviceInfo;
}

struct DiskInfo {
    total_bytes: u64,
    free_bytes: u64,
}

struct NetworkInfo {
    if_type: NetworkType,      // Wifi, Ethernet, Cellular, Unknown
    metered: bool,
    bandwidth_bps: Option<u64>,
}

struct PowerInfo {
    source: PowerSource,        // AC, Battery, Unknown
    battery_pct: Option<f64>,
}

struct DeviceInfo {
    id: String,
    name: String,
    tags: Vec<String>,
    arch: String,
    os: String,
}
```

Platform implementations:

| Context | macOS | Linux | Windows |
|---------|-------|-------|---------|
| Disk free/used | `statfs` | `statvfs` | `GetDiskFreeSpaceEx` |
| Network type | `SystemConfiguration.framework` | `/sys/class/net/`, NetworkManager DBus | `NetworkInformation` (WinRT) |
| Metered | `SCNetworkReachability` | NetworkManager metered property | `ConnectionProfile` |
| Power source | `IOPSGetPowerSourceDescription` | `/sys/class/power_supply/` | `GetSystemPowerStatus` |
| Device arch | `std::env::consts::ARCH` | Same | Same |

## `.cascade` config system

A single `.cascade` file in any directory controls ignore rules, pinning, lifecycle policies, cache settings, and P2P behaviour. The client walks from mount root to each directory, layering configs with child-overrides-parent precedence — identical mental model to `.gitignore`.

### Four formats, one type

| File | Parser |
|------|--------|
| `.cascade` (no extension) | Custom gitignore-style line parser |
| `.cascade.toml` | serde + `toml` crate |
| `.cascade.yaml` | serde + `serde_yaml` crate |
| `.cascade.json` | serde + `serde_json` crate |

All deserialise to:

```rust
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CascadeConfig {
    #[serde(default)]
    ignore: Vec<IgnoreRule>,

    #[serde(default)]
    lifecycle: Vec<LifecyclePolicy>,

    #[serde(default)]
    pin: Vec<PinRule>,

    #[serde(default)]
    unpin: Vec<PinRule>,

    #[serde(default)]
    cache: Option<CacheConfig>,

    #[serde(default)]
    p2p: Option<P2PConfig>,

    #[serde(default)]
    device: Option<DeviceConfig>,
}

#[derive(Debug, Deserialize)]
struct IgnoreRule {
    pattern: String,
    #[serde(default)]
    negated: bool,
    #[serde(default)]
    dir_only: bool,
    #[serde(default)]
    conditions: Vec<Expression>,
}

#[derive(Debug, Deserialize)]
struct LifecyclePolicy {
    path: String,
    max_age: Option<Duration>,
    max_file_size: Option<u64>,
    #[serde(default)]
    priority: i32,
    #[serde(default)]
    conditions: Vec<Expression>,
    /// Shorthand — equivalent to adding a condition.
    #[serde(default)]
    if_expr: Option<Expression>,
}

#[derive(Debug, Deserialize)]
struct PinRule {
    path: String,
    #[serde(default)]
    conditions: Vec<Expression>,
}

#[derive(Debug, Deserialize)]
struct CacheConfig {
    max_size: Option<u64>,
    max_age: Option<Duration>,
    #[serde(default)]
    default_state: Option<CacheState>,
}
```

### Gitignore-style parser

For `.cascade` files (no extension). Every line is one of:

| Line starts with | Meaning |
|-----------------|---------|
| `#` | Comment — skipped |
| `:[<expr>]` | Open conditional block |
| `:[end]` | Close conditional block |
| `:cache ...` | Cache directive |
| `:lifecycle ...` | Lifecycle directive |
| `:pin <pattern>` | Pin directive |
| `:unpin <pattern>` | Unpin directive |
| `:p2p ...` | P2P directive |
| `!pattern` | Negated ignore (un-ignore) |
| blank | Skipped |
| anything else | Ignore pattern (`.gitignore` semantics) |

Directive values are `key=value` pairs. Values support durations (`7d`, `1h`), sizes (`50GB`), strings (quoted or unquoted), booleans, and `if=<expression>`:

```rust
fn parse_gitignore_style(content: &str) -> CascadeConfig {
    let mut config = CascadeConfig::empty();
    let mut condition_stack: Vec<Expression> = vec![];

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if let Some(expr) = trimmed.strip_prefix(":[")
            .and_then(|s| s.strip_suffix("]"))
        {
            condition_stack.push(parse_expression(expr.trim()));
            continue;
        }

        if trimmed == ":[end]" {
            condition_stack.pop();
            continue;
        }

        if let Some(directive) = trimmed.strip_prefix(':') {
            parse_directive(directive.trim(), &condition_stack, &mut config);
            continue;
        }

        // Ignore pattern (standard .gitignore syntax)
        let mut rule = parse_ignore_pattern(trimmed);
        rule.conditions = condition_stack.clone();
        config.ignore.push(rule);
    }

    config
}
```

### Merge semantics — directory walk

The walk from mount root to any directory produces a `ResolvedConfig`:

```rust
struct ResolvedConfig {
    ignores: Vec<IgnoreRule>,
    lifecycle: Vec<LifecyclePolicy>,
    pins: Vec<PinRule>,
    cache: CacheConfig,
    p2p: P2PConfig,
}
```

```rust
impl ResolvedConfig {
    fn resolve(mount_root: &Path, target_dir: &Path, config_loader: &dyn ConfigLoader) -> Self {
        let mut builder = ResolvedConfigBuilder::new();

        for dir in ancestors_between(mount_root, target_dir) {
            if let Some(config) = config_loader.load(dir) {
                builder.apply(config);
            }
        }

        builder.build()
    }
}
```

| Concern | Merge strategy |
|---------|---------------|
| Ignore rules | **Accumulated** — every `.cascade` in the path contributes. `!` negation in a child un-ignores a parent's exclusion |
| Lifecycle policies | **Accumulated, child-first** — evaluated in order, first match wins; parent policies skipped |
| Pin rules | **Accumulated** — `[[pin]]` pins; `[[unpin]]` in a child removes |
| Cache settings | **Nearest-wins** — child overrides parent's scalar values |
| Device config | **Root only** — child configs cannot reidentify the machine |
| P2P settings | **Nearest-wins** for folder-level config; global settings from root |

The config walk crosses backend boundaries — a `.cascade` at the mount root applies to `Work/` even though that subtree is served by a different backend.

### Coexistence in one directory

If multiple `.cascade*` files exist in the same directory:

1. All are loaded — not an error.
2. Merged in deterministic order: gitignore-style → TOML → YAML → JSON.
3. Scalar settings (cache, P2P) use last-writer-wins (JSON overrides TOML).
4. Ignore/lifecycle/pin rules are accumulated from all sources.

## Expression language

Conditional rules use a small expression language evaluated against a context struct.

### Grammar

```
expression := or_expr
or_expr    := and_expr ("||" and_expr)*
and_expr   := primary ("&&" primary)*
primary    := comparison | "(" expression ")" | "!" primary
comparison := operand operator operand
operator   := "==" | "!=" | "<" | "<=" | ">" | ">=" | "matches" | "in" | "contains"
operand    := identifier | literal
literal    := string | integer | duration | size | boolean | percentage
```

Defined as a PEG grammar for `pest`:

```pest
// expr/src/grammar.pest

expression = { or_expr }
or_expr    = { and_expr ~ ("||" ~ and_expr)* }
and_expr   = { primary ~ ("&&" ~ primary)* }
primary    = { comparison | "(" ~ expression ~ ")" | "!" ~ primary }
comparison = { operand ~ operator ~ operand }

operator   = { "matches" | "contains" | "in" | "<=" | ">=" | "!=" | "==" | "<" | ">" }

operand    = { literal | identifier }
identifier = { ASCII_ALPHA_UPPER ~ (ASCII_ALPHANUMERIC | "." | "_")* }

literal    = { duration | percentage | size_bytes | boolean | string | integer }
duration   = { integer ~ ("ms" | "s" | "m" | "h" | "d" | "w" | "M" | "y") }
percentage = { integer ~ "%" }
size_bytes = { integer ~ ("B" | "KB" | "MB" | "GB" | "TB") }
boolean    = { "true" | "false" }
string     = { "\"" ~ (!"\"" ~ ANY)* ~ "\"" }
integer    = { ASCII_DIGIT+ }
```

### Evaluation context

```rust
struct EvalContext {
    file: FileContext,
    device: DeviceContext,
    disk: DiskContext,
    network: NetworkContext,
    power: PowerContext,
    time: TimeContext,
    peer: PeerContext,
}

struct FileContext {
    size: u64,
    mime: String,
    ext: String,
    name: String,
    modified: DateTime<Utc>,
    owner: String,
    shared: bool,
    starred: bool,
    dirty: bool,
    cached: bool,
    pinned: bool,
}

impl FileContext {
    fn age(&self) -> Duration { Utc::now() - self.modified }
    fn year(&self) -> i32 { self.modified.year() }
}

struct TimeContext {
    now: DateTime<Utc>,
}

impl TimeContext {
    fn hour(&self) -> u32 { self.now.hour() }
    fn is_weekday(&self) -> bool { self.now.weekday().number_from_monday() <= 5 }
}

struct PeerContext {
    online_count: usize,
    peers_with_file: usize,
}
```

### Predicate reference

| Category | Predicates |
|----------|-----------|
| File | `file.size`, `file.mime`, `file.ext`, `file.age`, `file.year`, `file.owner`, `file.shared`, `file.starred`, `file.dirty`, `file.cached`, `file.pinned` |
| Device | `device.id`, `device.name`, `device.tag`, `device.arch`, `device.os` |
| Environment | `disk.free`, `disk.used`, `network.type`, `network.metered`, `network.bandwidth`, `power.source`, `time.hour`, `time.day` |
| Peer | `peer.has_file`, `peer.online`, `peer.count` |

## Cache manager

### States

| State | On disk | Offline | On first open |
|-------|---------|---------|---------------|
| `Online` | Metadata only | No | Downloads then opens |
| `Cached` | Full file | Yes | Opens immediately |
| `Pinned` | Full file, never evicted | Yes | Opens immediately |

### Background worker

Runs on a configurable interval (default: 5 minutes). Each tick:

1. **Scan pinned paths** — ensure everything under a pinned directory is fully cached. Queue downloads for any that aren't.
2. **Check max size** — if total cache exceeds `cache.max_size`, evict the least-recently-accessed non-pinned files until under the limit.
3. **Check max age** — evict any non-pinned file where `last_access + max_age < now`, subject to matching lifecycle policy conditions.
4. **Upload dirty files** — push any locally-modified files back to the backend.
5. **Sync remote changes** — poll backend's `changes()` API and reconcile.

Eviction is always LRU among non-pinned files. Pinned files are only removed by explicit `unpin`.

### Lifecycle policy evaluation

```rust
impl CacheManager {
    fn eviction_candidates(&self, ctx: &EvalContext) -> Vec<FileEntry> {
        let policies = self.policies_sorted_by_priority();  // child-first
        let mut candidates = vec![];

        for file in self.cached_files() {
            if file.pinned { continue; }

            let file_ctx = EvalContext::for_file(&file, ctx);

            // Find the first matching policy (child-first evaluation)
            let policy = policies.iter().find(|p| {
                p.path.matches(&file.path)
                    && p.conditions.iter().all(|c| c.evaluate(&file_ctx))
            });

            if let Some(p) = policy {
                if self.should_evict(&file, p) {
                    candidates.push(file);
                }
            }
        }

        candidates.sort_by_key(|f| f.last_access);  // LRU first
        candidates
    }
}
```

## P2P engine

### Block Exchange Protocol (BEP)

Based on [Syncthing's BEP v1](https://docs.syncthing.net/specs/bep-v1.html). Messages:

| Message | Purpose |
|---------|---------|
| `ClusterConfig` | Exchange folder list and configuration on connect |
| `Index` | Announce which files and blocks you have |
| `IndexUpdate` | Incremental update when files change |
| `Request` | Ask a peer for a specific block |
| `Response` | Send the block data |
| `Ping` / `Close` | Keepalive and teardown |

All messages are XDR-encoded (re-use the XDR codec from the NFS presenter).

### Block store

```rust
struct FileBlocks {
    file_id: ItemId,
    size: u64,
    block_size: u32,        // typically 128KB, adaptive for large files
    blocks: Vec<BlockHash>,
}

struct BlockHash([u8; 32]);  // SHA-256

impl BlockStore {
    /// Split a file into blocks, hash each, store the index.
    fn index_file(&mut self, path: &Path) -> Result<FileBlocks>;

    /// Reassemble a file from blocks.
    fn reassemble(&self, blocks: &FileBlocks, output: &Path) -> Result<()>;

    /// Return the list of block hashes for a file.
    fn get_blocks(&self, file_id: &ItemId) -> Option<&FileBlocks>;

    /// Return the data for a single block.
    fn get_block(&self, hash: &BlockHash) -> Option<&[u8]>;
}
```

Block size is adaptive:
- Files under 250MB: 128KB blocks
- Files 250MB–1GB: 512KB blocks
- Files over 1GB: 1MB blocks

### P2P as layer, not backend

P2P sits between the VFS and the cache. The sync router decides:

- **Read a file**: check local cache → if miss, check P2P peers (LAN) → if miss, download from cloud backend.
- **Write a file**: write to local cache → upload to cloud backend → announce new blocks to P2P peers.
- **P2P-only folder** (no cloud backend): same flow, minus the cloud backend. All devices are equal.

Cloud remains the authority for cloud-backed folders. P2P is an optimisation for block transfer.

### Peer discovery

| Method | How | Scope |
|--------|-----|-------|
| Local | UDP multicast on port 21027 | LAN, zero config |
| Global | Announce to discovery server (self-hosted or default) | WAN |
| Configured | Static addresses in config | Any |
| Relay | Route through relay server when both peers behind NAT | WAN (fallback) |

### Device identity

Each device generates a TLS certificate on first run. The device ID is the SHA-256 of the certificate, encoded as a base32 string (same as Syncthing). All P2P connections are TLS-encrypted and authenticated by device ID.

### NAT traversal

v1 supports hole punching via STUN (works for most NAT types) and static addresses. Relay fallback for uncooperative NATs can come later.

The [`stun-rs`](https://crates.io/crates/stun-rs) crate handles STUN binding requests.

## State database

SQLite, stored at `~/.config/cascade/state.db`:

```sql
CREATE TABLE files (
    id            TEXT PRIMARY KEY,        -- ItemId: "{backend_id}:{native_id}"
    backend_id    TEXT NOT NULL,
    path          TEXT UNIQUE NOT NULL,     -- relative to mount root
    parent_id     TEXT,
    name          TEXT NOT NULL,
    is_dir        BOOLEAN NOT NULL,
    size          INTEGER,
    mime_type     TEXT,
    mod_time      INTEGER,                 -- Unix timestamp (epoch seconds)
    remote_hash   TEXT,                     -- backend-provided hash (MD5, ETag, etc.)
    local_hash    TEXT,                     -- computed SHA-256 after local changes

    cache_state   TEXT NOT NULL DEFAULT 'online',
    provenance    TEXT NOT NULL DEFAULT 'cloud',
    disk_path     TEXT,                     -- for local/synced provenance
    local_path    TEXT,                     -- path in cache directory
    cached_at     INTEGER,
    last_access   INTEGER,
    dirty         BOOLEAN NOT NULL DEFAULT FALSE,
    synced_at     INTEGER,

    FOREIGN KEY (backend_id) REFERENCES backends(id)
);

CREATE INDEX idx_files_path ON files(path);
CREATE INDEX idx_files_backend ON files(backend_id);
CREATE INDEX idx_files_cache_state ON files(cache_state);
CREATE INDEX idx_files_last_access ON files(last_access);

CREATE TABLE backends (
    id            TEXT PRIMARY KEY,
    backend_type  TEXT NOT NULL,            -- "gdrive", "s3", "webdav", etc.
    display_name  TEXT NOT NULL,
    mount_path    TEXT,                     -- relative path prefix in VFS
    config        TEXT                      -- JSON blob of backend-specific config
);

CREATE TABLE pin_rules (
    id            INTEGER PRIMARY KEY,
    path_glob     TEXT NOT NULL,
    recursive     BOOLEAN NOT NULL DEFAULT TRUE,
    conditions    TEXT                      -- serialised expression list
);

CREATE UNIQUE INDEX idx_pin_rules_path ON pin_rules(path_glob);

CREATE TABLE lifecycle_policies (
    id            INTEGER PRIMARY KEY,
    path_glob     TEXT NOT NULL,
    max_age       INTEGER,                 -- seconds, NULL = no limit
    max_file_size INTEGER,                 -- bytes, NULL = no limit
    priority      INTEGER NOT NULL DEFAULT 0,
    conditions    TEXT                      -- serialised expression list
);

CREATE INDEX idx_lifecycle_priority ON lifecycle_policies(priority DESC);

CREATE TABLE config_cache (
    dir_path      TEXT PRIMARY KEY,         -- directory containing the .cascade
    modified_at   INTEGER,                  -- mtime of the .cascade file
    config        TEXT NOT NULL             -- serialised CascadeConfig
);

CREATE TABLE sync_cursors (
    backend_id    TEXT PRIMARY KEY,
    cursor        TEXT NOT NULL
);

CREATE TABLE p2p_peers (
    device_id     TEXT PRIMARY KEY,
    name          TEXT,
    addresses     TEXT,                     -- JSON array of "tcp://host:port"
    last_seen     INTEGER,
    online        BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE TABLE p2p_block_index (
    file_id       TEXT NOT NULL,
    block_index   INTEGER NOT NULL,
    block_hash    BLOB NOT NULL,            -- SHA-256 (32 bytes)
    PRIMARY KEY (file_id, block_index)
);

CREATE INDEX idx_block_hash ON p2p_block_index(block_hash);
```

## CLI interface

```
cascade [command] [options]

Commands:
  start                         Start the daemon and mount all configured backends
  stop                          Stop the daemon and unmount
  restart                       Restart the daemon
  status                        Show mount status, cache usage, backend health

  pin <path>                    Pin a file or directory (always available offline)
  unpin <path>                  Unpin a file or directory
  pin list                      List all pinned paths

  cache status                  Show cache usage: pinned vs cached vs online
  cache evict [--all]           Manually run lifecycle eviction
  cache warm <path>             Pre-download a directory tree
  cache clear <path>            Evict specific files from cache

  adopt <path>                  Adopt a local directory
    --backend <name>            Cloud backend to sync with
    --remote-path <path>        Remote path in the backend
    --mode mirror|upload-only   Sync mode
    --in-place                  Replace existing cloud client at this path
    --reflink                   Use APFS copy-on-write (macOS)

  config validate               Validate all .cascade files in the tree
  config show <path>            Show the resolved config for a directory

  domain register               Register a File Provider domain (macOS)
    --name <name>
    --path <mount-point>
    --backend <name>

  backend list                  List configured backends
  backend test <name>           Test connection to a backend
  backend quota <name>          Show quota for a backend

  peer list                     List known P2P peers
  peer add <device-id>          Add a peer
  peer remove <device-id>       Remove a peer

Global options:
  --config <path>               Config file path (default: ~/.config/cascade/config.toml)
  --verbose, -v                 Increase verbosity
  --quiet, -q                   Suppress non-error output
```

## Adopting existing directories

Three modes:

### 1. Local backend — serve from disk

```toml
[[child]]
name = "local-projects"
path = "Projects"
backend = "local"
root = "~/Projects"
writable = true
```

Files stay at `~/Projects/`. Cascade reads them directly. `.cascade` rules still apply.

### 2. Adopt-and-sync — bidirectional sync in place

```bash
cascade adopt ~/Projects/ --backend gdrive-personal --remote-path Projects --mode mirror
```

Cascade indexes the directory, reconciles with the cloud backend (uploads new local files, downloads new cloud files), then watches for changes using [`notify`](https://crates.io/crates/notify) (FSEvents on macOS, inotify on Linux). `.cascade` rules filter what gets synced.

### 3. Adopt-in-place — replace an existing cloud client

```bash
# macOS — File Provider domain
cascade domain register --name personal --path "~/Google Drive" --backend gdrive-personal

# Linux / NFS fallback
cascade adopt ~/Google\ Drive/ --backend gdrive-personal --in-place --reflink
```

On macOS with File Provider, the existing directory becomes a File Provider domain. Files don't move. On Linux or NFS, files are moved (or APFS-reflinked on macOS) to Cascade's cache directory.

### File provenance

| Provenance | READ | WRITE | DELETE |
|------------|------|-------|--------|
| `CloudOnly` | Download, cache, serve | Write to cache, upload | Delete from backend |
| `Cached` | Serve from cache | Write to cache, upload | Delete from cache and backend |
| `Local` | Read from disk | Write to disk | Delete from disk |
| `Synced` | Read from disk | Write to disk, upload | Delete from disk and backend |

## Nested mounts

Multiple backends compose into a single VFS tree via the config file:

```toml
# ~/.config/cascade/config.toml

[global]
mount_point = "~/Cloud"
volname = "Cloud"

[root]
backend = "gdrive"
credentials_file = "~/.config/cascade/gdrive-personal.json"

[[child]]
name = "work"
path = "Work"
backend = "gdrive"
credentials_file = "~/.config/cascade/gdrive-work.json"
shared_drive = true
team_drive_id = "0AEd3EhGff9SaUk9PVA"

[[child]]
name = "company-assets"
path = "Work/Assets"           # nested inside the work child
backend = "s3"
bucket = "company-assets"
region = "eu-west-1"

[[child]]
name = "nas"
path = "NAS"
backend = "webdav"
url = "https://nas.example.com/dav"

[[child]]
name = "family-photos"
path = "Family Photos"
backend = "none"               # P2P only

[device]
name = "work-laptop"
tags = ["dev", "video-edit"]
```

The VFS resolves by longest-prefix match. Children can nest arbitrarily deep. Moving files between backends triggers a download + upload + delete.

## macOS File Provider extension

A Swift app extension (~1,000 lines) that bridges `NSFileProviderExtension` to the Rust engine over a Unix domain socket.

### Architecture

```
Finder ←→ NSFileProviderExtension (Swift) ←→ Unix socket ←→ Cascade Engine (Rust)
```

The extension lives in `swift/CascadeFileProvider/`. It contains:

- `CascadeFileProviderExtension.swift` — the `NSFileProviderExtension` subclass. Each method sends a request to the Rust engine and returns the result.
- `CascadeEngineClient.swift` — Unix domain socket client. Serialises requests as JSON, reads JSON responses.
- `CascadeFileProviderItem.swift` — bridges between the engine's `VfsItem` type and `NSFileProviderItem`.

### Wire protocol

The same protocol is used by the CLI, the File Provider extension, and any future GUI. JSON over Unix domain socket, length-prefixed:

```rust
// Each message: 4-byte big-endian length + JSON body
struct Request {
    id: u32,
    method: String,
    params: serde_json::Value,
}

struct Response {
    id: u32,
    result: Option<serde_json::Value>,
    error: Option<String>,
}
```

Methods:

| Method | Params | Returns |
|--------|--------|---------|
| `getItem` | `{ id }` | `VfsItem` |
| `fetchContents` | `{ id }` | `{ path }` |
| `enumerateItems` | `{ parent_id, page }` | `[VfsItem]` |
| `createDirectory` | `{ name, parent_id }` | `VfsItem` |
| `importDocument` | `{ source_url, parent_id }` | `VfsItem` |
| `moveItem` | `{ id, new_parent_id, new_name }` | `VfsItem` |
| `deleteItem` | `{ id }` | `{}` |
| `pinItem` | `{ id }` | `{}` |
| `evictItem` | `{ id }` | `{}` |
| `getThumbnail` | `{ id, size }` | `{ data }` |
| `getStatus` | `{}` | `StatusInfo` |

### What File Provider gives you

| Capability | NFS | File Provider |
|-----------|-----|---------------|
| File state badges | ✗ | ✓ |
| Placeholder files | Faked | Native |
| Right-click pin/evict | CLI only | Native context menus |
| Spotlight indexing | ✗ | ✓ |
| Quick Look previews | Triggers download | Extension provides thumbnails |
| Graceful offline | Mount dies | Cached files visible |
| Adopt existing dirs | ✗ | ✓ (domain registration) |
| Quarantine attrs | Bypassed | Applied automatically |

## Google Drive backend

### Authentication

OAuth2 device code flow (no browser redirect needed for headless/CLI usage):

1. `cascade backend add gdrive` — initiates device code flow
2. Prints a URL and code: "Visit https://google.com/device and enter ABC-DEF-GHI"
3. User authorises in browser
4. Backend polls for token, stores refresh token in keychain (`security` on macOS, `libsecret` on Linux, Credential Manager on Windows)

Refresh tokens are used automatically. The `google-drive3` crate from [google-apis-rs](https://github.com/Byron/google-apis-rs) handles the API surface.

### Shared Drives (Team Drives)

Configured during setup or in config.toml:

```toml
[root]
backend = "gdrive"
credentials_file = "~/.config/cascade/gdrive-work.json"
shared_drive = true
team_drive_id = "0AEd3EhGff9SaUk9PVA"
```

Multiple Google Drive remotes (personal + shared drives) each get their own backend instance.

### Change detection

Uses the [Drive API Changes stream](https://developers.google.com/drive/api/v3/reference/changes):

1. On startup, call `changes.list` with the stored cursor (or `changes.getStartPageToken` for initial sync).
2. Each `changes.list` call returns a page of changes and a page token.
3. Continue until no more pages. Store the final cursor.
4. Poll periodically (default: 1 minute) using the stored cursor.

### Rate limiting

Google Drive allows ~10,000 requests per 100 seconds per user. The backend implements a token-bucket rate limiter. Batch operations use batch requests where possible.

## Conflict resolution

When the same file is modified in two places simultaneously:

1. **Detect** — compare local hash vs remote hash on sync. If both changed, conflict.
2. **Resolve** — keep both versions. The losing version gets renamed: `report (work-laptop 2026-05-27).conflict`.
3. **Log** — record the conflict in the state database with both hashes and timestamps.

Conflict copies are never deleted automatically — the user resolves them manually or via the CLI.

For P2P-only folders (no cloud authority), last-write-wins on a per-block basis, with conflict copies for simultaneous full-file edits.

## Build and run

### Prerequisites

- Rust 1.85+ (edition 2024)
- On macOS: Xcode Command Line Tools (for Swift compilation of File Provider extension)
- On Linux: `libfuse3-dev` (for FUSE)
- On Windows: WinFSP installed

### Build

```bash
# Build the engine + CLI
cargo build --release

# Build with specific presenter
cargo build --release --features presenter-nfs
cargo build --release --features presenter-fileprovider  # macOS only

# Build everything including Swift extension (macOS)
cargo build --release
cd swift/CascadeFileProvider && xcodebuild
```

### Run

```bash
# Configure a backend
cascade backend add gdrive --name personal

# Start the daemon
cascade start

# Check status
cascade status

# Pin a directory
cascade pin Documents/Accounts/

# Stop
cascade stop
```

### Test

```bash
# Unit tests
cargo test --workspace

# Integration tests (require mock backend)
cargo test --test integration

# Test a specific backend
cargo test -p backend-gdrive
```

## Roadmap

| Phase | Scope | Time | Lines |
|-------|-------|------|-------|
| v1 | NFS mount + `.cascade` (ignore only) + single backend (read-only, Google Drive) | 8-10 weeks | ~6,000 |
| v2 | Pinning + lifecycle + cache manager | +8-12 weeks | +5,000 |
| v3 | Write-back + multi-backend + nested mounts + conflict resolution | +6-8 weeks | +3,000 |
| v4 | Conditional rules (expressions + context providers) | +7-10 weeks | +3,700 |
| v5 | macOS File Provider presenter (Swift extension) | +4-6 weeks | +1,500 |
| v6 | Adopt existing directories (local backend, adopt-and-sync, adopt-in-place) | +4-6 weeks | +2,000 |
| v7 | P2P block sharing (LAN) | +10-14 weeks | +6,000 |
| v8 | Linux FUSE presenter + Windows WinFSP presenter | +4-6 weeks | +2,000 |
| v9 | Full P2P (WAN discovery, NAT traversal) | +8-12 weeks | +4,000 |

## Dependencies

| Crate | Purpose | Phase |
|-------|---------|-------|
| `tokio` | Async runtime | v1 |
| `rusqlite` | SQLite state database | v1 |
| `serde` + `toml` + `serde_yaml` + `serde_json` | Config parsing | v1 |
| `ignore` | `.gitignore`-style glob matching (from ripgrep) | v1 |
| `pest` | PEG grammar for expression language | v4 |
| `notify` | Filesystem event watching (FSEvents / inotify) | v6 |
| `google-drive3` | Google Drive API client | v1 |
| `reqwest` | HTTP client (backends) | v1 |
| `rustls` | TLS (P2P connections, HTTPS backends) | v1 |
| `fuser` | FUSE presenter (Linux) | v8 |
| `winfsp` | WinFSP presenter (Windows) | v8 |
| `stun-rs` | STUN NAT traversal | v9 |
| `base32` | Device ID encoding | v7 |
| `sha2` | SHA-256 block hashing | v7 |
| `tracing` | Structured logging | v1 |
| `clap` | CLI argument parsing | v1 |
| `anyhow` + `thiserror` | Error handling | v1 |

## Reference implementations

- **rclone** — NFS server, VFS caching, Google Drive backend, mount management. Go. [GitHub](https://github.com/rclone/rclone)
- **Syncthing** — BEP protocol, peer discovery, NAT traversal, block-level delta sync, conflict resolution. Go. [GitHub](https://github.com/syncthing/syncthing)
- **go-nfs** — NFSv3 server in Go. Reference for XDR codec and procedure handlers. [GitHub](https://github.com/willscott/go-nfs)
- **WinFSP** — Windows virtual filesystem. Rust bindings available. [GitHub](https://github.com/winfsp/winfsp)
