//! Core types shared across all Cascade crates.

use std::fmt;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use data_encoding::BASE64URL_NOPAD;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Unique identifier for a file or directory across all backends.
/// Format: `"{backend_id}:{backend_native_id}"`.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct ItemId(pub String);

impl ItemId {
    #[must_use]
    pub fn new(backend_id: &str, native_id: &str) -> Self {
        Self(format!("{backend_id}:{native_id}"))
    }

    #[must_use]
    pub fn backend_id(&self) -> &str {
        self.0.split_once(':').map_or(&self.0, |(b, _)| b)
    }

    #[must_use]
    pub fn native_id(&self) -> &str {
        self.0.split_once(':').map_or(&self.0, |(_, n)| n)
    }
}

impl fmt::Display for ItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Unique identifier for a file within a single backend.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileId(pub String);

impl FileId {
    /// Extract the native ID part after the colon.
    /// Format: `"{backend_id}:{native_id}"`. Returns the full string if
    /// no colon is present (bare native ID).
    #[must_use]
    pub fn native_id(&self) -> &str {
        self.0.split_once(':').map_or(&self.0, |(_, native)| native)
    }
}

impl fmt::Display for FileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Cursor for incremental change tracking.
/// Opaque to the engine — stored and passed through to backends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cursor(pub String);

/// Opaque cursor identifying a sync state.
///
/// The byte payload is engine-internal; consumers (the File Provider
/// extension, future presenters) treat it as a black box and pass it
/// back unchanged to resume enumeration or compare against the system's
/// last-known anchor.
///
/// Two cursors are equal if their bytes are equal; ordering is not
/// defined (the engine may emit cursors out of monotonic order if its
/// internal storage is updated by multiple writers).
///
/// On the wire the cursor is encoded as `base64url`-no-pad, which keeps
/// it URL- and JSON-string-safe without ever exposing the raw bytes to
/// consumers that have no business interpreting them.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct SyncCursor {
    bytes: Vec<u8>,
}

impl SyncCursor {
    /// Wrap the given bytes as a cursor.
    #[must_use]
    pub const fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// Empty cursor — used as the initial value when the consumer has
    /// no prior sync history. Always distinct from any cursor the engine
    /// has emitted (engine-emitted cursors always carry at least one
    /// non-zero byte; the empty cursor has length zero).
    #[must_use]
    pub const fn empty() -> Self {
        Self { bytes: Vec::new() }
    }

    /// Borrow the raw cursor bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// True when this is the empty cursor (no prior sync state).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl Serialize for SyncCursor {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&BASE64URL_NOPAD.encode(&self.bytes))
    }
}

impl<'de> Deserialize<'de> for SyncCursor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        let bytes = BASE64URL_NOPAD
            .decode(encoded.as_bytes())
            .map_err(serde::de::Error::custom)?;
        Ok(Self { bytes })
    }
}

/// A file or directory in the VFS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub id: ItemId,
    pub parent_id: ItemId,
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub mod_time: Option<DateTime<Utc>>,
    pub mime_type: Option<String>,
    pub hash: Option<String>,
}

impl FileEntry {
    /// Create a file entry.
    #[must_use]
    pub const fn file(id: ItemId, parent_id: ItemId, name: String) -> Self {
        Self {
            id,
            parent_id,
            name,
            is_dir: false,
            size: None,
            mod_time: None,
            mime_type: None,
            hash: None,
        }
    }

    /// Create a directory entry.
    #[must_use]
    pub const fn dir(id: ItemId, parent_id: ItemId, name: String) -> Self {
        Self {
            id,
            parent_id,
            name,
            is_dir: true,
            size: None,
            mod_time: None,
            mime_type: None,
            hash: None,
        }
    }

    /// Set the file size.
    #[must_use]
    pub const fn with_size(mut self, size: Option<u64>) -> Self {
        self.size = size;
        self
    }

    /// Set the file hash.
    #[must_use]
    pub fn with_hash(mut self, hash: Option<String>) -> Self {
        self.hash = hash;
        self
    }
}

/// A change event from a backend.
#[derive(Debug)]
pub enum Change {
    Created(FileEntry),
    Updated { old: FileEntry, new: FileEntry },
    Deleted(FileEntry),
    Moved { from: FileEntry, to: FileEntry },
}

/// Cache state for a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheState {
    /// Metadata only — file exists in the backend but not on local disk.
    Online,
    /// Full file is on local disk. May be evicted by lifecycle policies.
    Cached,
    /// Full file on disk. Never evicted by lifecycle. Only removed by explicit unpin.
    Pinned,
    /// Currently downloading from backend.
    Downloading,
}

impl CacheState {
    /// Return the string representation.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Cached => "cached",
            Self::Pinned => "pinned",
            Self::Downloading => "downloading",
        }
    }
}

impl fmt::Display for CacheState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::str::FromStr for CacheState {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "online" => Ok(Self::Online),
            "cached" => Ok(Self::Cached),
            "pinned" => Ok(Self::Pinned),
            "downloading" => Ok(Self::Downloading),
            _ => anyhow::bail!("unknown cache state: {s}"),
        }
    }
}

/// Provenance — where a file's content physically lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provenance {
    /// File exists in the cloud backend only, not yet downloaded.
    CloudOnly,
    /// File exists in the local cache (downloaded from cloud or adopted).
    Cached { local_path: PathBuf },
    /// File exists on the local filesystem, managed by a local backend.
    Local { disk_path: PathBuf },
    /// File exists both locally and in the cloud — synced via adopt-and-sync.
    Synced {
        disk_path: PathBuf,
        cloud_id: FileId,
    },
}

impl fmt::Display for Provenance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CloudOnly => write!(f, "cloud"),
            Self::Cached { .. } => write!(f, "cached"),
            Self::Local { .. } => write!(f, "local"),
            Self::Synced { .. } => write!(f, "synced"),
        }
    }
}

/// An item as presented to the platform layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VfsItem {
    pub id: ItemId,
    pub parent_id: ItemId,
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub mod_time: Option<DateTime<Utc>>,
    pub cache_state: CacheState,
    pub mime_type: Option<String>,
}

impl From<FileEntry> for VfsItem {
    fn from(entry: FileEntry) -> Self {
        Self {
            id: entry.id,
            parent_id: entry.parent_id,
            name: entry.name,
            is_dir: entry.is_dir,
            size: entry.size,
            mod_time: entry.mod_time,
            cache_state: CacheState::Online,
            mime_type: entry.mime_type,
        }
    }
}

impl From<&VfsItem> for FileEntry {
    fn from(item: &VfsItem) -> Self {
        Self {
            id: item.id.clone(),
            parent_id: item.parent_id.clone(),
            name: item.name.clone(),
            is_dir: item.is_dir,
            size: item.size,
            mod_time: item.mod_time,
            mime_type: item.mime_type.clone(),
            hash: None,
        }
    }
}

/// Storage quota information.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Quota {
    pub total: Option<u64>,
    pub used: Option<u64>,
    pub available: Option<u64>,
}

/// A directory entry returned by VFS listing.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
}

impl DirEntry {
    pub fn dir(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            is_dir: true,
        }
    }

    pub fn file(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            is_dir: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_id_parts() {
        let id = ItemId::new("gdrive-personal", "abc123");
        assert_eq!(id.backend_id(), "gdrive-personal");
        assert_eq!(id.native_id(), "abc123");
    }

    #[test]
    fn cache_state_display() {
        assert_eq!(CacheState::Online.to_string(), "online");
        assert_eq!(CacheState::Pinned.to_string(), "pinned");
    }

    #[test]
    fn provenance_display() {
        assert_eq!(Provenance::CloudOnly.to_string(), "cloud");
        assert_eq!(
            Provenance::Cached {
                local_path: PathBuf::from("/tmp/x")
            }
            .to_string(),
            "cached"
        );
    }

    #[test]
    fn sync_cursor_empty_round_trips() {
        let cursor = SyncCursor::empty();
        assert!(cursor.is_empty());
        let json = serde_json::to_string(&cursor).unwrap();
        assert_eq!(json, "\"\"");
        let decoded: SyncCursor = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, cursor);
    }

    #[test]
    fn sync_cursor_round_trips_through_json() {
        let cursor = SyncCursor::new(vec![1, 2, 3, 0xff, 0]);
        let json = serde_json::to_string(&cursor).unwrap();
        let decoded: SyncCursor = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, cursor);
        assert_eq!(decoded.as_bytes(), &[1, 2, 3, 0xff, 0]);
    }

    #[test]
    fn sync_cursor_rejects_invalid_base64() {
        let result: Result<SyncCursor, _> = serde_json::from_str("\"!!!\"");
        assert!(result.is_err());
    }

    #[test]
    fn file_entry_to_vfs_item() {
        let entry = FileEntry {
            id: ItemId::new("gdrive", "root"),
            parent_id: ItemId::new("gdrive", "parent"),
            name: "test.txt".to_string(),
            is_dir: false,
            size: Some(1024),
            mod_time: None,
            mime_type: Some("text/plain".to_string()),
            hash: None,
        };
        let item: VfsItem = entry.into();
        assert_eq!(item.name, "test.txt");
        assert_eq!(item.cache_state, CacheState::Online);
    }
}
