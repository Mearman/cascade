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
- [Background service](#background-service)
- [Build and run](#build-and-run)
- [Roadmap](#roadmap)

## Architecture

```
┌──────────────────────────────────────────────────────────┐
│  Platform Layer (per-OS)                                 │
│                                                          │
│  macOS: File Provider (Swift) · FSKit (15.4+) · WebDAV · NFS │
│  Linux: FUSE · NFS (root)                                │
│  Windows: ProjFS · WebDAV via WebClient                  │
│  Universal fallback: NFS server · WebDAV server          │
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
│   │       │   ├── mod.rs        # Discovery trait + DiscoveryService (concurrent fan-out, dedupe, priority order)
│   │       │   ├── lan.rs        # UDP multicast (LAN, zero config)
│   │       │   ├── gossip.rs     # Introducer-gossip source (reads the PeerBook)
│   │       │   ├── signing.rs    # SignedCandidates envelope — single sign/verify home, shared by announce + DHT
│   │       │   ├── announce.rs   # Announce-server source (behind the `announce` feature)
│   │       │   └── dht.rs        # Mainline DHT BEP44 source (behind the `dht` feature)
│   │       ├── relay.rs          # WebSocket relay client (blind byte-pipe, TLS through the tunnel)
│   │       ├── traversal.rs      # Connectivity strategy + hole-punch state machine
│   │       ├── nat.rs            # STUN binding requests, NAT-type detection, candidate gathering
│   │       ├── candidate.rs      # ICE-style candidates and RFC 8445 priority arithmetic
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
│   ├── presenter-projfs/         # Windows ProjFS presenter (native, via windows crate Win32_Storage_ProjectedFileSystem)
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

**S3 upload path.** Objects up to 5 GiB are uploaded via a single `PutObject` request. Objects larger than 5 GiB use the S3 multipart upload API: the object is split into parts, each uploaded via `UploadPart`, then finalised with `CompleteMultipartUpload`. Part size is the larger of the S3 minimum part size (5 MiB) and the value needed to keep the part count within S3's 10,000-part ceiling — i.e. `max(ceil(total_bytes / 10,000), 5 MiB)`. No object size limit applies above `PutObject`'s 5 GiB threshold; the multipart path handles arbitrarily large objects up to the S3 service maximum.

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
    Box::new(ProjFsPresenter::new(mount_point))  // native ProjFS; wired via try_projfs in crates/cascade/src/cli/mount.rs
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

The cache mode is the typed `NfsCacheMode` enum (`off`, `minimal`, `full`),
deserialised straight from the export config:

| Mode | On-demand? | Writes? | Disk usage |
|------|-----------|---------|------------|
| `off` | Yes | Read-only | None |
| `minimal` | Yes | Yes | Minimal |
| `full` | No | Yes | High |

`minimal` is the default — on-demand reads, write-capable, minimal disk usage.
Write support keys off the mode rather than a separate flag: an `off` export
refuses every write procedure with `NFS3ERR_ROFS` / `NFS4ERR_ROFS`. A
write-capable export (`minimal` or `full`) implements the full write procedure
set for both NFSv3 and NFSv4:

| Procedure | NFSv3 | NFSv4 op |
|-----------|-------|----------|
| Write data at offset | `WRITE` | `WRITE` |
| Create a regular file | `CREATE` | `OPEN` with `CREATE` flag |
| Create a directory | `MKDIR` | `CREATE` (type NF4DIR) |
| Set file attributes (size, mtime) | `SETATTR` | `SETATTR` |
| Remove a file | `REMOVE` | `REMOVE` |
| Remove a directory | `RMDIR` | `REMOVE` |
| Rename / move | `RENAME` | `RENAME` |
| Flush pending writes | `COMMIT` | `COMMIT` |

All write procedures route through the shared `write` helper module, which
translates them into the same backend operations (`upload`, `update`,
`create_dir`, `delete`, `rename`) as the WebDAV presenter uses. The mode
threads from `NfsServerConfig` through dispatch into both the NFSv3 and NFSv4
procedure handlers.

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
    /// Maximum on-disk cache size, parsed from a human-readable byte string
    /// (`5GB`, `512MiB`). Stored as a byte count; malformed values are
    /// rejected during deserialisation rather than at use.
    #[serde(default)]
    max_size: Option<MaxSize>,
    /// Maximum age a cached file may reach before it is eligible for eviction,
    /// parsed from a human-readable duration (`7d`, `1h30m`). Stored as a
    /// `Duration`; malformed values are rejected during deserialisation.
    #[serde(default)]
    max_age: Option<MaxAge>,
    /// Declared default cache-state posture for the subtree. Absent means the
    /// engine's own default applies.
    #[serde(default)]
    default_state: Option<CacheStatePosture>,
}

/// The default cache-state posture a `.cascade` file may declare. Distinct
/// from the engine's per-file runtime `CacheState`, which also models
/// transient states such as downloading.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum CacheStatePosture {
    Pinned,   // keep resident on disk; never evict automatically
    Online,   // keep metadata-only; fetch content on demand
    Auto,     // let lifecycle policies and cache limits decide residency
}
```

`max_size` and `max_age` are typed wrappers (`MaxSize`, `MaxAge`) with custom
serde implementations: they accept the same human-readable strings the
gitignore-style directives use and parse them at config load, so a malformed
`5gigs` is a load-time error rather than a value that misbehaves later. The
size accepts both decimal (`GB`) and binary (`GiB`) units.

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

Discovery turns a peer's device ID into a set of reachable [candidates](#nat-traversal). Each mechanism implements the `Discovery` trait and is composed behind `DiscoveryService`, which fans every source out concurrently, deduplicates the union by `(address, kind)`, and orders the survivors by descending RFC 8445 priority. The engine depends only on the trait, never on a concrete source. The sources live under `crates/p2p/src/discovery/`:

| Source | Module | How | Needs an operated server? |
|--------|--------|-----|---------------------------|
| LAN multicast | `lan.rs` | UDP multicast group on port 21027 | No — serverless, zero config |
| Introducer gossip | `gossip.rs` | Reads the `PeerBook` that introducer gossip fills, so peers learned transitively through trusted devices surface without any directory | No — serverless |
| Announce server | `announce.rs` | A rendezvous directory: a device registers its signed candidate set keyed by device ID (`POST <base>/announce/<id>`), any other device looks it up (`GET <base>/announce/<id>`) | Yes — an operated announce endpoint (behind the `announce` cargo feature) |
| DHT | `dht.rs` | Stores the same signed candidate set in the BitTorrent Mainline DHT as a BEP44 mutable item, keyed by an ed25519 keypair derived deterministically from the device ID | No — serverless, but uses the public DHT |

LAN multicast and gossip cover peers that share a network segment or a trust path; neither reaches two devices that have never met and sit on different networks. The announce server closes that gap. It stores and serves only candidate sets — it never carries payload traffic — so a looker-up connects directly (or via a relay) using the connectivity stack once it holds the candidates.

The DHT fills the same "find an unknown peer" role serverlessly, after Syncthing's global-discovery model. Rather than holding the candidate set on an operated directory, a device publishes it into the BitTorrent Mainline DHT as a BEP44 mutable item. BEP44 addresses a mutable item by `SHA-1(public_key || salt)`, so the rendezvous only works if both ends derive the same writer keypair: `DhtKey::from_device_id` seeds an ed25519 keypair from a domain-separated hash of the device ID, so the announcer and the looker-up independently compute the identical BEP44 target with no shared secret and no per-node persisted key. `DhtDiscovery` is generic over a `DhtNode` put/get contract — exercised against an in-memory node in tests — with the live `mainline`-backed node (`MainlineDht`) behind the `dht` cargo feature. The `mainline` crate pulls in the ed25519 BEP44 signing stack.

The live node is productionised. Out of the box it bootstraps against `DEFAULT_DHT_BOOTSTRAP_NODES` — the public Mainline-DHT routers the wider BitTorrent ecosystem joins against (the `mainline` crate's own default set) — so the DHT is usable without configuring anything. An operator who wants to pin their own swarm sets `dht_bootstrap_nodes` in the backend config; an explicit empty list falls back to the public default rather than starting isolated, and any other value is handed verbatim to the node's bootstrap as already-resolved addresses. `MainlineDht::open` is a blocking constructor — it resolves the bootstrap host:port set with synchronous `getaddrinfo` and binds the DHT UDP socket on the calling thread — so the backend opens it off the tokio runtime via `spawn_blocking`, keeping a slow or unreachable resolver from stalling a worker. Published candidate sets are soft state — a BEP44 mutable item is dropped if a storing node has not seen it refreshed within roughly the BEP44 expiry window (the de-facto two-hour figure BEP5 also uses) — so a device republishes on a cadence derived from that estimate rather than a hand-picked number: `DHT_REPUBLISH_INTERVAL` is half the expiry window, refreshing twice per window so one missed tick still leaves the previous value live. A lookup that overruns `DHT_RESOLVE_TIMEOUT` is reported as a clean absence (a typed `DhtGetOutcome::TimedOut` distinct from `NotFound`) — the same not-found every discovery source can return — so a slow DHT never wedges the management-plane dial waiting on it, and the timeout is logged distinctly from a real not-found. CI exercises the live path against an ephemeral in-process `mainline::Testnet` — a small swarm of DHT nodes on local UDP sockets — covering put/get round-trips and republish supersession without touching the public network; the same job re-lints the feature-gated live test code so a regression there fails before any swarm is bootstrapped.

#### Self-certifying candidate sets

The announce server and the DHT are both blind, untrusted directories — neither is asked to vouch for what it stores. An announced candidate set is signed by the announcing device and carries its own proof, so the carrier is a dumb pipe and the looking-up client does the verifying. A `SignedCandidates` envelope binds the candidate set, the claimed device ID, and an expiry, signed over a canonical field-by-field byte encoding rather than JSON, so the signature does not depend on serialiser whitespace or key ordering. The signing module (`discovery/signing.rs`) is the single signing-and-verification home shared by both sources, and the key is the same device-ID-derived ed25519 keypair the DHT BEP44 path already signs with — one seed-to-keypair site feeds both the announce path and the DHT live node's `MutableItem` signing, so the two transports cannot drift onto different keys. No new crypto, no second key.

Verification derives the verifying key from the device ID being resolved — the envelope carries no public key — so a set signed by device A only verifies when resolved as A. The claimed ID must match the resolved ID, the signature must verify against the canonical bytes, and the expiry must be unexpired; any failure is a hard, logged rejection, never a panic and never a silent acceptance. `AnnounceDiscovery` and `DhtDiscovery` both verify on read through that shared path. The DHT stores the same envelope and re-signs the set after the byte-budget trim so the signature always covers exactly the stored bytes; the relay-server announce directory stores and serves the opaque blob verbatim and never inspects it.

The guarantee is deliberately bounded. Because the verifying key is derivable from the public device ID (itself a hash of the TLS certificate exchanged on every handshake), so is the signing key — anyone who knows the ID can re-derive the keypair and mint a valid envelope. The construction therefore resists substitution, relabelling, single-ID tampering, and replay, but it is not forgery- or MITM-resistant against a party that already knows the ID. The authenticated TLS handshake (trusted fingerprints pinned at the rustls verifier) is the connect-time backstop: a forged candidate set at most points at the wrong address, and the handshake fails for any peer whose certificate does not match the pinned ID. Binding the signature to the TLS private key instead would break the ID-derivable-verifying-key property the rendezvous depends on, so the weaker guarantee is the documented limitation.

#### Rendezvous-by-presence

The lookup-style discovery sources (announce server, DHT) turn a device id into a candidate set that may have been published minutes earlier. Rendezvous-by-presence is a complementary *live-pairing* path for two peers that happen to be online at the same instant but have no prior relationship.

Both peers register under the same rendezvous key with the `RendezvousBroker` — a distinct in-memory component hosted by the relay server alongside, but separate from, the relay server's `SessionRegistry`. `SessionRegistry` pairs sockets for byte-pipe forwarding; `RendezvousBroker` exchanges `RendezvousOffer` structs (candidate sets plus a `SyncPunchAgreement`) so peers can attempt direct connections — no byte-pipe forwarding is involved. The broker holds **no persistent state** — a registration is an in-memory slot, alive only while the registering peer holds its handle, and swept after a TTL if no counterpart arrives. When the second peer registers, the broker exchanges each side's offer and both sides immediately drive `run_hole_punch` to open a direct or hole-punched connection, without the broker carrying any further traffic.

Activation requires both the `public` posture and a configured rendezvous endpoint. The broker itself is posture-agnostic; gating happens at the site that decides whether to register a presence at all, mirroring how other server-assisted discovery sources activate. Broker capacity is bounded by an absolute count (matching the relay session cap via `DEFAULT_MAX_PRESENCES`) to prevent a flood of half-open registrations exhausting memory.

#### Deployment

The relay server (`crates/relay-server/`) and the announce directory share the same announce wire contract but have different operational shapes. The relay carries live tunnelled traffic and must be an always-on host. The announce directory only stores and serves short-lived signed blobs and holds nothing between requests, so it can run as a stateless Cloudflare Worker (`workers/announce/`, workers-rs). Soft state lives in a Workers KV namespace with an `expiration_ttl` matching the republish cadence; announcers republish, so KV losing an entry costs nothing. The Worker authenticates writers with the shared HMAC bound to the device ID and the exact request body, and bounds request size and candidate count, rejecting oversized or malformed input loudly — storing a blob does not require trusting it, since the signature makes the envelope self-certifying for the looking-up client. The routing, HMAC auth, size-bound, and blob round-trip logic live in `cascade-announce-wire` and are unit-tested on the native target against an in-memory store; the Worker crate is thin wasm-only glue mapping `worker::Request` onto the handler and a KV namespace onto the blob-store contract.

### Exposure posture

How far a device reaches out for peers is governed by a single `DiscoveryReach`
posture rather than a scatter of independent `enable_*` flags. The posture names
an *intent* — the furthest exposure level the operator is comfortable with — and
each discovery and traversal source self-activates when the posture permits its
level **and** the source has what it needs to run. The server lists
(`stun_servers`, `announce_servers`, `relay_endpoints`) and the DHT
configuration say *where to point* a source; the posture decides *whether* it
runs.

| Posture | Reaches | LAN multicast | Gossip · hole punch · peer relay | Global directory (DHT, announce) |
|---------|---------|:---:|:---:|:---:|
| `lan-only` | Local segment only | yes | no | no |
| `private` (default) | Trusted private mesh | yes | yes | no |
| `public` | Open to the wider internet | yes | yes | yes |

The levels nest — `LanOnly` ⊂ `Private` ⊂ `Public` — so each permits everything
the level below does plus its own additions. `Private` is the default: a trusted
mesh with LAN discovery, introducer gossip, NAT hole punching, and peer relaying,
but no publication to any global directory. A device at `Private` is discoverable
only by peers it already shares a segment, an introducer, or a relay with.

Global publication is opt-in: only `Public` lets a device publish to and query
the Mainline DHT and any configured announce servers, so never-met peers can
resolve it by device id for zero-config WAN discovery. The default never opts a
node into that — moving to `Public` is a deliberate choice.

Self-activation means a permitted source still stays idle until it can actually
work. LAN multicast is permitted at every posture (LAN is the floor) but only
runs once a `listen_addr` is bound — without an inbound port a discovered peer
would have nothing to dial. The DHT carries its bootstrap set always (an empty
set falls back to the `mainline` crate's built-in public bootstrap nodes), so an
empty bootstrap list means "use the public set", never "DHT disabled" —
disabling the DHT is the `lan-only`/`private` posture, not the absence of
config. Announce servers and peer relay endpoints are likewise contacted only
when the posture permits and a server is configured.

### Device identity

Each device generates a TLS certificate on first run. The device ID is the SHA-256 of the certificate, encoded as a base32 string (same as Syncthing). All P2P connections are TLS-encrypted and authenticated by device ID — trusted fingerprints are pinned at the rustls verifier so the handshake fails for any unapproved peer before a byte is exchanged.

### NAT traversal

Once discovery yields candidates, the connectivity ladder climbs from the cheapest, fully serverless path to the most operationally heavy, stopping at the first rung that works:

1. **LAN multicast** — devices on the same segment find and dial each other directly. Serverless.
2. **Static / gossip candidates** — addresses configured statically or learned through introducer gossip, dialled directly. Serverless.
3. **Observed (server-reflexive) direct** — a STUN binding request (`nat.rs`, RFC 5389; full NAT-type classification via the RFC 5780 two-server probe) reveals the device's externally observed mapping, advertised as a server-reflexive candidate so a peer can dial the mapping directly. STUN is a query-only echo, not a relay — but it is an operated endpoint.
4. **Hole punch** — when both ends sit behind punchable NATs, `traversal.rs` runs a synchronised, deterministic probe burst (ICE-style pairing per RFC 8445, coordinated over the existing gossip channel à la libp2p DCUtR). Serverless apart from the STUN endpoint already used to gather candidates.
5. **Peer relay** — an `Open`- or `FullCone`-typed device whose volunteer policy permits it advertises itself as a relay (`BepMessage::RelayOffer`); a stuck pair bridges through it (`BepMessage::RelayConnect`, `RelayRoute::Peer`). Serverless — the relay is just another participating device.
6. **Operated relay** — last resort. Traffic tunnels through a configured relay server (`relay.rs`, `RelayRoute::Operated`) as a blind byte-pipe: the two ends negotiate TLS *through* the tunnel, so the operator sees only opaque ciphertext. This is the one rung that requires a dedicated operated server (`crates/relay-server/`).

`decide_connectivity` (`traversal.rs`) picks the strategy from the local and remote NAT types and the remote's advertised candidates, preferring a volunteering peer relay over an operated one whenever a punch is not viable. So the only rungs that depend on an operated server are the optional announce-server discovery source, the STUN endpoint used to observe reflexive mappings, and the operated-relay fallback; everything else — LAN multicast, static/gossip candidates, hole punching, and peer relays — is fully serverless.

The [`stun-rs`](https://crates.io/crates/stun-rs) crate handles STUN binding requests.

The connectivity ladder is exercised end to end on Linux network namespaces. The `nat-integration` CI job (`.github/workflows/ci.yml`, behind the `nat-integration` cargo feature) builds the test binaries without privilege, then runs them under `sudo` for `CAP_NET_ADMIN`. The `nat_integration` test drives the operated-relay rung; the `serverless_rungs` test proves the fully serverless rungs with no operated servers of any kind — a cone-NAT pair connecting via peer-as-STUN observed-address learning plus hole punch, and a symmetric-NAT pair bridging through a third open peer acting as a peer relay — each asserting a block transfer completes over the expected rung.

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

-- Receive-only conflict quarantine. A write-denied peer's proposed file rows
-- (a peer we will not accept writes from under a directional data:read-only or
-- no-share posture) are kept here as flagged local additions rather than merged
-- into the authoritative index or silently discarded. Surfaced to the operator
-- as "N rejected local additions from <peer>"; a newer proposal for a path
-- replaces the older one, keeping the table bounded.
CREATE TABLE data_receive_quarantine (
    folder_id     TEXT NOT NULL,
    peer_device   TEXT NOT NULL,
    path          TEXT NOT NULL,
    file_json     TEXT NOT NULL,            -- serialised proposed FileInfo
    observed_at   INTEGER NOT NULL,         -- Unix seconds when observed
    PRIMARY KEY (folder_id, peer_device, path)
);
```

The two data-plane capabilities `data:read` and `data:write` are ordinary
folder-scoped (never dangerous) capabilities in the same `grants` table and the
same signed/delegatable/revocable token machinery as the management-plane verbs;
they gate the BEP sync serve and accept directions per (peer, folder). See
[`docs/directional-data-sharing.md`](directional-data-sharing.md) for the
default-open ACL decision, the enforcement points in the sync path, and the
receive-only / write-only conflict semantics.

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

  grant add <device-id>         Grant capabilities to a device
    --cap <list>                Comma-separated capabilities (e.g. status:read,pin:write)
    --scope <path|*>            Path prefix, or * for node-wide
    --expires <rfc3339>         Optional expiry timestamp
  grant list                    List grants held on this node
  grant revoke <id>             Revoke a grant by ID
  grant audit                   Print the management audit log

  remote <device-id> status     Read a remote node's status
  remote <device-id> pin <path>   Pin a path on a remote node
  remote <device-id> unpin <path> Unpin a path on a remote node
  remote <device-id> cache evict  Run cache eviction on a remote node
  remote <device-id> cache warm <path>  Warm a path on a remote node

  service install               Write the service definition and register it
  service uninstall             Deregister the service and remove its definition
  service start                 Start the registered service
  service stop                  Stop the registered service
  service status                Report whether the service is registered and running
    --user                      Force the per-user scope (no elevation)
    --system                    Force the machine-wide scope (Linux: systemd system unit, requires root; macOS/Windows: unsupported — native mounts require a user session)

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

### Google Drive TLS deadlock workaround

The HTTP client is built fresh for every request with connection pooling disabled (`pool_max_idle_per_host(0)`) and HTTP/1.1 forced (`http1_only`), in both `DriveClient::http` and the token-refresh path. Both build their client through the shared `build_unpooled_http1_client` helper, which returns the builder error rather than falling back to a default client — a default client re-enables pooling and HTTP/2, the exact configuration the workaround forbids. This is a deliberate workaround, not an oversight, and it must not be reverted without a confirmed root cause and a passing reproduction.

The hang it works around was only ever observed through the WebDAV presenter, where an axum/hyper-1.x server and a reqwest/hyper-1.x client share one tokio runtime. A backend TLS handshake opened while the server was mid-response never completed, so the second use of a pooled connection (or an HTTP/2 stream reused across that boundary) wedged the task. The WebDAV presenter independently carries the matching half of the workaround — it forces `Connection: close` on every response and runs the backend write on an isolated thread with its own runtime.

The investigation ruled out everything internal to the backend. The one place a lock sits near an `.await` is the refresh slow path of `GdriveBackend::access_token`: it locks the token `Mutex`, clones the refresh token, drops the guard, awaits the refresh, then re-locks to store the result. The integration test `concurrent_refresh_through_shared_client_does_not_deadlock` exercises exactly that path — it seeds the backend with an already-expired token plus a refresh token, points the OAuth2 token endpoint at a deliberately delayed mock, and fires many concurrent callers so they genuinely contend on the mutex re-acquisition. All complete well inside the deadline; a guard held across the refresh await would leave callers unable to re-lock and trip the timeout (verified by temporarily holding the guard, which fails the test). An in-memory token store keeps the refresh's persistence off the host Keychain. Because the mock speaks plain HTTP, it still cannot exercise the TLS handshake that is the suspected trigger — that the test stays green is the evidence that the remaining cause sits at the hyper server+client boundary, outside this crate. Earlier work also tried and reverted native-tls, aws-lc-rs, manual `serve_connection`, and `block_in_place`; the per-request, unpooled, HTTP/1.1-only client is the combination that held.

## Conflict resolution

When the same file is modified in two places simultaneously:

1. **Detect** — compare local hash vs remote hash on sync. If both changed, conflict.
2. **Resolve** — keep both versions. The losing version gets renamed: `report (work-laptop 2026-05-27).conflict`.
3. **Log** — record the conflict in the state database with both hashes and timestamps.

Conflict copies are never deleted automatically — the user resolves them manually or via the CLI.

For P2P-only folders (no cloud authority), last-write-wins on a per-block basis, with conflict copies for simultaneous full-file edits.

## Background service

`cascade service` manages the daemon as an OS background service. Each platform uses the standard per-user mechanism — no administrator rights required.

| Platform | Mechanism | Definition written to |
|----------|-----------|----------------------|
| macOS | launchd `LaunchAgent` | `~/Library/LaunchAgents/io.cascade.daemon.plist` |
| Linux | systemd `--user` unit | `~/.config/systemd/user/io.cascade.daemon.service` |
| Windows | logon Scheduled Task | registered under the current user via `schtasks` |

### Architecture

Each platform module is split into two halves:

- **Pure generator** — a `generate` free function that turns a `ServiceSpec` into the platform's service-definition text (plist / unit / task XML). No OS calls, no `cfg(target_os)` gate — compiled and unit-tested on every host.
- **Platform adapter** — the `ServiceManager` trait implementation that writes the generated file and drives the OS register command (`launchctl bootstrap gui/<uid>` / `systemctl --user` / `schtasks /Create /XML`). Only this half is `cfg(target_os)`-gated.

### Scope selection

The install scope is resolved in this order:

1. An explicit `--user` or `--system` flag always wins.
2. Otherwise the session is inspected: an interactive GUI desktop session (X11/Wayland on Linux, a non-SSH login on macOS, any logon on Windows) picks the user scope; a headless host (SSH, no display, system boot) picks the system scope.
3. At the boundary where a person is at a real desktop terminal without a flag, the resolver shows a prompt that defaults to the user scope. A non-interactive invocation never blocks — it uses the deterministic inference.

The chosen scope and the reason for the choice are always printed before any action is taken, so the operator is never surprised by a silent escalation.

The `System` scope is part of the `ServiceScope` enum and the `ServiceManager` contract. On Linux, `--system` writes a systemd system unit to `/etc/systemd/system/` and manages it with `systemctl` (without `--user`), which requires root. On macOS and Windows the `System` scope errors clearly: File Provider, FSKit, ProjFS, and WebDAV all require a user session, so a session-0 system service cannot drive the native filesystem mount. This is a documented platform limitation, not a missing feature.

### Homebrew integration

The Homebrew formula ships a `service do` block so `brew services start cascade` works without any manual configuration: it delegates to `cascade start`, sets `keep_alive true`, and routes logs to `$(brew --prefix)/var/log/cascade.log`. This path is independent of `cascade service install`; both result in the daemon running as a launchd agent, but `brew services` manages the plist inside the Homebrew prefix while `cascade service install` writes to `~/Library/LaunchAgents`.

### Graceful empty start

The daemon exits cleanly with a log message and exit code 0 when no backends are configured. This means a freshly-installed service does not crash-loop before `cascade backend add` has been run.

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

## Node management plane

Data-plane trust is flat: `trusted_device_ids` gates whether a peer may sync the folders you share with it, and nothing more, while every administrative command (`pin`, `cache evict`, `stop`) travels a local Unix socket from the CLI to the local daemon. The management plane adds a second, authenticated front-end onto the *same* command handlers the local CLI drives, so a trusted device can administer another. The constraint that keeps it honest: a manager can never do anything to a node that the node could not already do to itself, and no command logic is duplicated.

The principal is the existing [device identity](#device-identity) (the TLS-certificate SHA-256), carried as a `DeviceId` newtype in `crates/engine/src/manage/`. Authority is modelled as **capabilities, not roles** — each a verb over a scope — held as grants *on the managed node*, mirroring the consent direction of the introducer relationship: you grant authority to a manager; a manager cannot assert it.

| Capability | Grants |
|------------|--------|
| `status:read` | read mount status, cache usage, backend health, peer list |
| `pin:write` | pin / unpin paths |
| `cache:manage` | evict / warm |
| `config:push` | merge `.cascade` / device config |
| `policy:set` | lifecycle and pin policies |
| `backend:manage` | add / remove backends (dangerous) |
| `lifecycle:control` | start / stop / restart the daemon (dangerous) |
| `grant:admin` | delegate a subset of held grants (dangerous) |
| `data:read` | serve our index and blocks to a peer for a folder (directional, not dangerous) |
| `data:write` | accept and merge a peer's index and blocks for a folder (directional, not dangerous) |

The two `data:*` capabilities are the data plane's directional sharing controls, carried by the same grant rows and token machinery as the management verbs but enforced in the BEP sync serve/accept path rather than the management command surface. Unlike the management verbs, they are evaluated *default-open*: a trusted peer with no data grant keeps full bidirectional sharing, and configuring a `data:read` or `data:write` grant only ever narrows that peer to one direction. The full model — the default-open ACL decision, the sync-path enforcement points, and the receive-only / write-only conflict semantics — is documented in [`docs/directional-data-sharing.md`](directional-data-sharing.md).

`Scope` is either node-wide (`Scope::Node`, written `*`) or a folder subtree identified by a path prefix. Coverage matches on normalised path *components*, never raw substrings, so `/work` covers `/work/reports` but not `/workspace`, and a crafted `/work/../personal` target fails to normalise into the granted subtree rather than slipping through. A `Grant` is `{ grantee, capability, scope, granted_by, expires }`. The three dangerous capabilities (`backend:manage`, `lifecycle:control`, `grant:admin`) are never satisfied implicitly by a node-wide grant — including a folder scope that normalises to the root, which is node-wide in everything but name — so each must be granted explicitly for the exact folder it is exercised over. Grants are persisted in two `state.db` tables: `grants` (decomposed into `scope_kind` / `scope_path` columns) and an append-only `manage_audit` log of every command the node processed. The audit table has no `UPDATE` or `DELETE` path in the typed API, so a compromised manager cannot erase its tracks. Grants are optionally declared in the root device config (root-only merge, matching the existing device-config rule), so a fleet provisions declaratively rather than imperatively.

On the wire it is two variants on the existing `BepMessage` enum — `ManageRequest` and `ManageResponse` — carried over the already-TLS-authenticated peer connection, so the caller's device ID is established before a command is read. `ManageRequest` carries a `request_id`, a `ManageCommand`, and the `ManageScope` it targets; `ManageResponse` echoes the `request_id` and a `ManageResult` (`Ok { summary }` or a typed `Err` with `ManageErrorKind::Unauthorised` or `Failed`). The wire command surface is now complete. `ManageCommand` carries every administrative verb, each mapped to exactly one required capability by `required_capability`:

| Command | Required capability |
|---------|--------------------|
| `StatusRead` | `status:read` |
| `Pin` / `Unpin` | `pin:write` |
| `CacheEvict` / `CacheWarm` | `cache:manage` |
| `ConfigPush` | `config:push` |
| `PolicySet` | `policy:set` |
| `BackendAdd` / `BackendRemove` | `backend:manage` |
| `Restart` / `Stop` | `lifecycle:control` |
| `GrantAdd` / `GrantRevoke` | `grant:admin` |

Because the dangerous capabilities are never satisfied by a node-wide or root-normalising grant, the commands that need them carry the scope they are authorised over explicitly: `BackendAdd` / `BackendRemove` carry the backend's `mount_path`, and `Restart` / `Stop` take a folder scope rather than a wildcard. A `ConfigPush` body is one of the four `.cascade` formats, wire-typed as `ManageConfigFormat` so the protocol crate stays free of the config crate's parser, and applies under the folder it targets.

`GrantAdd` carries a second, stricter gate on top of the `grant:admin` capability check: the grant being delegated must be a subset of authority the caller can *itself exercise*. `caller_can_delegate` collects the caller's own grants of the same capability whose scope covers the delegated scope and which `authorises` would permit the caller to exercise directly — reusing `authorises` folds in the dangerous + node-wide bar, so a node-wide dangerous grant the caller can never use cannot be laundered into a narrow folder-scoped delegation. Holding `grant:admin` alone is not enough; a manager can delegate only what it could already do. The delegated expiry is clamped to the latest expiry among the backing grants, so a delegate never outlives the authority it derived from, and `granted_by` is stamped with the authenticated caller rather than trusted off the wire.

The managed side enforces through a shared dispatch core (`manage::dispatch::run_dispatch`). It resolves the caller's grants, derives both the scope the command's payload actually mutates (the fixed directory prefix of a pin glob) and the wire scope the caller advertised, and authorises the required capability over *both* — closing the scope-escape where a caller pins `/personal` while advertising a `/work` scope it does hold. The audit row is written **before** any side effect, so a change always leaves a trace even if the write that follows fails; a denial is audited too. Only then does it dispatch into the same internal handlers the local CLI uses, via the `ManageCommandExecutor` contract that the engine implements over its existing `pin` / `unpin` / `status` / cache-evict methods — the very ones the CLI calls. The `Backend` trait carries a manage-dispatch seam wired from the engine; the daemon injects the dispatcher into its P2P backend at startup, and a session whose peer identity was not proven by an end-to-end TLS handshake (relayed or post-hole-punch sessions, whose device ID is merely asserted) is refused before reaching the dispatch port.

The CLI exists on both sides. On the managed node, `cascade grant add|list|revoke|audit` administers the capabilities this node confers and reads its audit log. On the manager, `cascade remote <device-id>` sends the full command set to a target addressed by device ID — `status`, `pin` / `unpin`, `cache evict` / `cache warm`, `config push`, `policy set`, `backend add` / `backend remove`, `restart` / `stop`, and `grant add` / `grant revoke`. The file-bearing variants (`config push`, `backend add`) read their body from disk and carry it as a literal TOML or `.cascade` document, never an interpolated path. The target is reached over the same `DiscoveryService` and [connectivity ladder](#nat-traversal) as any other connection, so management works across NAT through the existing rungs. "One or more nodes managing one or more others" is just a many-to-many grant relationship; each node owns its own grant list, so the model stays decentralised with no fleet registry.

Grants are kept as a local list with the connection's authenticated device ID as the principal. This on-node grant list is the base authority model. On top of it sits the signed capability-token model:

**Capability tokens** are portable, offline-issuable grants. A token is a JSON structure the issuing node signs with its real device-identity private key (the key behind its TLS certificate). It carries the issuer device id, the bearer device id, the capability, the scope, and an expiry — there is no never-expiring token. The token also carries the issuer's DER certificate, because a SHA-256 hash is not invertible and the public key cannot be recovered from the device id alone. The bearer presents the token alongside a `cascade remote` command using `--token <file>`; the receiving node verifies the signature by re-deriving the device id from the carried DER certificate (binding check: the derived id must equal the issuer id in the token), then extracting the public key from that certificate to verify the signature — checks that the bearer matches the authenticated connection, checks the expiry against its clock, checks the token id against its revocation list, and then feeds the token's grant through the same `authorises` path an on-node grant takes.

**CLI.** `cascade token issue <bearer> --cap <capability> --scope <path|*> --expires <RFC 3339>` mints and signs a token, records it in the node's state database, and prints the JSON. `cascade token revoke <token-id>` adds the id to the append-only revocation list. `cascade token list` shows all tokens this node has issued with their status.

**Bounded delegation.** The bearer of a capability token may mint a delegated sub-token — a child token that carries its parent inline. No `grant:admin` capability is required to delegate a token; the only guards are that the delegating device is the token's bearer and that the child's capability, scope, and expiry are each contained within the parent's. `verify` walks the chain to a root issued by the verifying node and enforces subset-containment at every hop: the child's capability must equal the parent's, its scope must be covered by the parent's, and its expiry must not exceed the parent's. A chain can only narrow authority, never widen it. The maximum delegation depth is eight hops.

**Revocation.** `cascade token revoke` writes to an append-only table (`token_revocations`) in the state database; every `verify` call checks every id in the presented chain against this list. Revoking a parent invalidates the whole chain below it.

## Roadmap

| Phase | Scope | Time | Lines |
|-------|-------|------|-------|
| v1 | NFS mount + `.cascade` (ignore only) + single backend (read-only, Google Drive) | 8-10 weeks | ~6,000 |
| v2 | Pinning + lifecycle + cache manager | +8-12 weeks | +5,000 |
| v3 | Write-back + multi-backend + nested mounts + conflict resolution | +6-8 weeks | +3,000 |
| v4 | Conditional rules (expressions + context providers) | +7-10 weeks | +3,700 |
| v5 | macOS File Provider presenter (Swift extension), implemented | +4-6 weeks | +1,500 |
| v6 | Adopt existing directories (local backend, adopt-and-sync, adopt-in-place) | +4-6 weeks | +2,000 |
| v7 | P2P block sharing (LAN) | +10-14 weeks | +6,000 |
| v8 | Linux FUSE presenter + Windows native ProjFS presenter (implemented) | +4-6 weeks | +2,000 |
| v9 | Full P2P (WAN discovery, NAT traversal), implemented — includes rendezvous-by-presence | +8-12 weeks | +4,000 |
| v10 | Node management plane (capability grants, remote administration over BEP), implemented — includes signed capability tokens, delegation chains, and revocation | +6-10 weeks | +3,000 |
| v11 | OS background service (`cascade service install|start|stop|status|uninstall`): per-user LaunchAgent on macOS, systemd `--user` unit on Linux, logon Scheduled Task on Windows, implemented | +1-2 weeks | +500 |

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
| `windows` (Win32_Storage_ProjectedFileSystem) | Native ProjFS presenter (Windows) | v8 |
| `stun-rs` | STUN NAT traversal | v9 |
| `base32` | Device ID encoding | v7 |
| `sha2` | SHA-256 block hashing | v7 |
| `tracing` | Structured logging | v1 |
| `clap` | CLI argument parsing | v1 |
| `anyhow` + `thiserror` | Error handling | v1 |

## Deployment

The announce Worker and relay server that enable WAN peer discovery and NAT traversal across the internet are documented separately: [`docs/deployment.md`](deployment.md).

## Reference implementations

- **rclone** — NFS server, VFS caching, Google Drive backend, mount management. Go. [GitHub](https://github.com/rclone/rclone)
- **Syncthing** — BEP protocol, peer discovery, NAT traversal, block-level delta sync, conflict resolution. Go. [GitHub](https://github.com/syncthing/syncthing)
- **go-nfs** — NFSv3 server in Go. Reference for XDR codec and procedure handlers. [GitHub](https://github.com/willscott/go-nfs)
- **Projected File System (ProjFS)** — Windows native virtualisation API, used directly through the `windows` crate's `Win32_Storage_ProjectedFileSystem` module. [Win32 docs](https://learn.microsoft.com/en-us/windows/win32/projfs/projected-file-system)
