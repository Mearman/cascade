//! Backend trait — every cloud provider implements this.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

#[cfg(feature = "p2p")]
use crate::manage::{DataAuthority, ManageDispatch};
use crate::types::{Change, Cursor, FileEntry, FileId, Quota};

/// A backend-level error category that presenters can map to specific
/// HTTP status codes (or equivalent OS error codes for FUSE/File Provider).
///
/// Backends return `anyhow::Error` and wrap a `BackendError` inside it
/// when the error has a well-defined category; presenters can downcast
/// to recover the category. Returning a plain `anyhow::Error` (e.g.
/// `anyhow::anyhow!(...)`) means "generic failure" and maps to 500.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    /// The operation was rejected because the caller lacks permission
    /// (e.g. Drive 403, or a write to a read-only virtual directory).
    #[error("permission denied: {0}")]
    Forbidden(String),
    /// The target resource does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// The operation cannot be performed because the destination is a
    /// read-only view (e.g. Bin, Shared with me).
    #[error("read-only: {0}")]
    ReadOnly(String),
    /// The operation conflicts with the current state (e.g. name taken).
    #[error("conflict: {0}")]
    Conflict(String),
}

// Note: Box<dyn Backend> is the owned concrete return type from create_backend.
// Trait-object dyn Backend is used via Arc<dyn Backend> in VfsTree.

/// Constructs a [`Backend`] from a TOML config fragment, dispatching on the
/// backend type.
///
/// This is the composition port that lets the engine register a backend at
/// runtime — for example when an authorised manager pushes a
/// `BackendAdd` command — without the engine depending on any concrete backend
/// crate. The daemon implements it at the edge by routing to the per-type
/// `create_backend` factories (`cascade_backend_gdrive::create_backend`, …);
/// the engine holds only the contract. An engine with no factory injected
/// cannot add backends and fails loudly rather than silently dropping the
/// request.
pub trait BackendFactory: Send + Sync {
    /// Build a backend instance of `backend_type` named `name` from its TOML
    /// config document. Returns an error for an unsupported type or an invalid
    /// config.
    fn create(
        &self,
        name: &str,
        backend_type: &str,
        config_toml: &str,
    ) -> anyhow::Result<Arc<dyn Backend>>;
}

/// A backend providing file storage. Every cloud provider and the local
/// filesystem implement this trait. The engine never sees provider-specific
/// APIs — all operations go through here.
#[async_trait]
pub trait Backend: Send + Sync {
    /// Unique identifier for this backend instance (e.g. "gdrive-personal").
    fn id(&self) -> &str;

    /// Display name for the mount point (e.g. "Google Drive (Personal)").
    fn display_name(&self) -> &str;

    /// Total and available quota, if the backend reports it.
    async fn quota(&self) -> anyhow::Result<Option<Quota>>;

    /// Stream changes since the given cursor. Returns a new cursor.
    /// If cursor is `None`, returns a full snapshot of the tree.
    async fn changes(&self, cursor: Option<&Cursor>) -> anyhow::Result<(Vec<Change>, Cursor)>;

    /// Fetch metadata for a single file or directory by path.
    async fn metadata(&self, path: &Path) -> anyhow::Result<FileEntry>;

    /// Download file content. The backend writes to the provided writer.
    async fn download(
        &self,
        file: &FileEntry,
        writer: &mut (dyn tokio::io::AsyncWrite + Unpin + Send),
    ) -> anyhow::Result<()>;

    /// Read a byte range of a file's content.
    ///
    /// `offset` is the start byte; `length` is the maximum number of
    /// bytes to return. The returned `Vec` may be shorter than `length`
    /// at end-of-file, and is empty when `offset` is at or past the end.
    ///
    /// The default implementation materialises the whole file via
    /// [`Backend::download`] and slices it — correct, but not
    /// range-efficient. Backends with a native range read (HTTP `Range`,
    /// `seek`+`read`, a block store) should override this so presenters
    /// can serve arbitrary ranges without downloading and pinning the
    /// entire file, and without blocking on a large cold transfer.
    async fn read_range(
        &self,
        file: &FileEntry,
        offset: u64,
        length: u32,
    ) -> anyhow::Result<Vec<u8>> {
        let mut buf = Vec::new();
        self.download(file, &mut buf).await?;
        let start = usize::try_from(offset).unwrap_or(usize::MAX).min(buf.len());
        let len = usize::try_from(length).unwrap_or(usize::MAX);
        let end = start.saturating_add(len).min(buf.len());
        Ok(buf.get(start..end).unwrap_or_default().to_vec())
    }

    /// Upload a new file. Does not check for existing files — the caller
    /// should use `update()` when overwriting.
    async fn upload(
        &self,
        path: &Path,
        reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
        parent_id: &FileId,
    ) -> anyhow::Result<FileEntry>;

    /// Overwrite the content of an existing file.
    async fn update(
        &self,
        file_id: &FileId,
        reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
    ) -> anyhow::Result<FileEntry>;

    /// Create a directory.
    async fn create_dir(&self, path: &Path) -> anyhow::Result<FileEntry>;

    /// Create a directory given the parent's known `FileId` and the new name.
    ///
    /// Backends that can create a directory without re-walking the path (e.g.
    /// Google Drive, which addresses nodes by opaque ID) should override this
    /// to avoid a round-trip Drive API walk. The default falls back to
    /// `create_dir`.
    async fn create_dir_with_parent(
        &self,
        name: &str,
        parent_id: &FileId,
    ) -> anyhow::Result<FileEntry> {
        let _ = parent_id;
        self.create_dir(Path::new(name)).await
    }

    /// Delete a file or directory.
    async fn delete(&self, file: &FileEntry) -> anyhow::Result<()>;

    /// Move/rename a file or directory by path.
    async fn move_entry(&self, src: &Path, dst: &Path) -> anyhow::Result<FileEntry>;

    /// Move/rename a file or directory by ID. Takes the source file ID,
    /// destination parent directory ID, and new filename. Backends that
    /// can move by ID directly should override this to avoid a slow path
    /// walk. The default falls back to `move_entry`.
    async fn move_by_id(
        &self,
        src_id: &FileId,
        dst_parent_id: &FileId,
        new_name: &str,
    ) -> anyhow::Result<FileEntry> {
        let _ = (src_id, dst_parent_id, new_name);
        anyhow::bail!("move_by_id not implemented, use move_entry")
    }

    /// List immediate children of a directory by its native ID.
    /// Used for on-demand directory expansion in presenters.
    /// Returns an empty vec for backends that don't support this.
    async fn list_children(&self, _parent_native_id: &str) -> anyhow::Result<Vec<FileEntry>> {
        Ok(vec![])
    }

    /// Recommended poll interval for this backend. Returns `None` if the
    /// backend doesn't support polling (use fixed interval from config).
    async fn poll_interval(&self) -> Option<Duration>;

    /// Inject the management-plane dispatch port the daemon's engine implements.
    ///
    /// A backend that runs its own peer-to-peer transport (the P2P backend)
    /// receives inbound `ManageRequest` frames from remote managers; it needs a
    /// handle to the engine's [`ManageDispatch`] so an authorised remote command
    /// runs through the same authorise → audit → execute core the local CLI
    /// drives. The daemon calls this once, after constructing the engine, before
    /// the backend begins accepting connections.
    ///
    /// The default is a no-op: backends with no transport of their own (cloud
    /// and local-filesystem backends) never serve management requests, so they
    /// have nothing to wire. Overriding backends take `&self` and store the port
    /// behind their own interior mutability so the already-running listener and
    /// session loops observe it.
    #[cfg(feature = "p2p")]
    async fn set_manage_dispatch(&self, dispatch: Arc<dyn ManageDispatch>) {
        let _ = dispatch;
    }

    /// Inject the data-plane authority port the daemon's engine implements.
    ///
    /// A backend that runs its own peer-to-peer transport (the P2P backend)
    /// gates serving its index and blocks to a peer, and accepting a peer's
    /// index and blocks, on the engine's [`DataAuthority`] decision for that
    /// (peer, folder) pair. The daemon calls this once, after constructing the
    /// engine, before the backend begins serving sync frames.
    ///
    /// The default is a no-op: backends with no transport of their own never
    /// serve sync frames, so they have nothing to gate. The P2P backend
    /// overrides this and stores the port behind interior mutability so the
    /// already-running session loops observe it. When the port is never wired,
    /// the BEP path is default-open — every trusted peer keeps full
    /// bidirectional access — preserving the pre-feature behaviour.
    #[cfg(feature = "p2p")]
    async fn set_data_authority(&self, authority: Arc<dyn DataAuthority>) {
        let _ = authority;
    }
}

/// A null backend used for P2P-only folders with no cloud storage.
#[derive(Debug)]
pub struct NullBackend {
    id: String,
}

impl NullBackend {
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }
}

#[async_trait]
impl Backend for NullBackend {
    fn id(&self) -> &str {
        &self.id
    }

    fn display_name(&self) -> &'static str {
        "P2P Only"
    }

    async fn quota(&self) -> anyhow::Result<Option<Quota>> {
        Ok(None)
    }

    async fn changes(&self, _cursor: Option<&Cursor>) -> anyhow::Result<(Vec<Change>, Cursor)> {
        Ok((vec![], Cursor("null".to_string())))
    }

    async fn metadata(&self, _path: &Path) -> anyhow::Result<FileEntry> {
        anyhow::bail!("null backend has no files")
    }

    async fn download(
        &self,
        _file: &FileEntry,
        _writer: &mut (dyn tokio::io::AsyncWrite + Unpin + Send),
    ) -> anyhow::Result<()> {
        anyhow::bail!("null backend has no files")
    }

    async fn upload(
        &self,
        _path: &Path,
        _reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
        _parent_id: &FileId,
    ) -> anyhow::Result<FileEntry> {
        anyhow::bail!("null backend cannot upload")
    }

    async fn update(
        &self,
        _file_id: &FileId,
        _reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
    ) -> anyhow::Result<FileEntry> {
        anyhow::bail!("null backend cannot update")
    }

    async fn create_dir(&self, _path: &Path) -> anyhow::Result<FileEntry> {
        anyhow::bail!("null backend cannot create directories")
    }

    async fn create_dir_with_parent(
        &self,
        _name: &str,
        _parent_id: &FileId,
    ) -> anyhow::Result<FileEntry> {
        anyhow::bail!("null backend cannot create directories")
    }

    async fn delete(&self, _file: &FileEntry) -> anyhow::Result<()> {
        anyhow::bail!("null backend cannot delete")
    }

    async fn move_entry(&self, _src: &Path, _dst: &Path) -> anyhow::Result<FileEntry> {
        anyhow::bail!("null backend cannot move")
    }

    async fn move_by_id(
        &self,
        _src_id: &FileId,
        _dst_parent_id: &FileId,
        _new_name: &str,
    ) -> anyhow::Result<FileEntry> {
        anyhow::bail!("null backend cannot move")
    }

    async fn poll_interval(&self) -> Option<Duration> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ItemId;
    use tokio::io::AsyncWriteExt as _;

    /// Backend whose `download` writes a fixed buffer; every other method
    /// is unused. Exercises the default `read_range` (download-and-slice).
    #[derive(Debug)]
    struct FixedBackend {
        content: Vec<u8>,
    }

    #[async_trait]
    impl Backend for FixedBackend {
        fn id(&self) -> &'static str {
            "fixed"
        }
        fn display_name(&self) -> &'static str {
            "Fixed"
        }
        async fn quota(&self) -> anyhow::Result<Option<Quota>> {
            Ok(None)
        }
        async fn changes(&self, _cursor: Option<&Cursor>) -> anyhow::Result<(Vec<Change>, Cursor)> {
            Ok((vec![], Cursor("fixed".to_string())))
        }
        async fn metadata(&self, _path: &Path) -> anyhow::Result<FileEntry> {
            anyhow::bail!("unused")
        }
        async fn download(
            &self,
            _file: &FileEntry,
            writer: &mut (dyn tokio::io::AsyncWrite + Unpin + Send),
        ) -> anyhow::Result<()> {
            writer.write_all(&self.content).await?;
            writer.flush().await?;
            Ok(())
        }
        async fn upload(
            &self,
            _path: &Path,
            _reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
            _parent_id: &FileId,
        ) -> anyhow::Result<FileEntry> {
            anyhow::bail!("unused")
        }
        async fn update(
            &self,
            _file_id: &FileId,
            _reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
        ) -> anyhow::Result<FileEntry> {
            anyhow::bail!("unused")
        }
        async fn create_dir(&self, _path: &Path) -> anyhow::Result<FileEntry> {
            anyhow::bail!("unused")
        }
        async fn delete(&self, _file: &FileEntry) -> anyhow::Result<()> {
            anyhow::bail!("unused")
        }
        async fn move_entry(&self, _src: &Path, _dst: &Path) -> anyhow::Result<FileEntry> {
            anyhow::bail!("unused")
        }
        async fn move_by_id(
            &self,
            _src_id: &FileId,
            _dst_parent_id: &FileId,
            _new_name: &str,
        ) -> anyhow::Result<FileEntry> {
            anyhow::bail!("unused")
        }
        async fn poll_interval(&self) -> Option<Duration> {
            None
        }
    }

    fn entry() -> FileEntry {
        FileEntry {
            id: ItemId::new("fixed", "f"),
            parent_id: ItemId::new("fixed", "root"),
            name: "f.bin".to_string(),
            is_dir: false,
            size: Some(11),
            mod_time: None,
            mime_type: None,
            hash: None,
        }
    }

    #[tokio::test]
    async fn default_read_range_slices_the_downloaded_buffer() {
        let backend = FixedBackend {
            content: b"hello world".to_vec(),
        };
        let e = entry();

        // Mid-range read.
        assert_eq!(backend.read_range(&e, 6, 5).await.unwrap(), b"world");
        // Whole file when length covers it.
        assert_eq!(backend.read_range(&e, 0, 11).await.unwrap(), b"hello world");
        // Length past EOF clamps to what's available.
        assert_eq!(backend.read_range(&e, 6, 999).await.unwrap(), b"world");
    }

    #[tokio::test]
    async fn default_read_range_empty_at_or_past_eof() {
        let backend = FixedBackend {
            content: b"hello world".to_vec(),
        };
        let e = entry();
        // Offset exactly at EOF -> empty.
        assert!(backend.read_range(&e, 11, 10).await.unwrap().is_empty());
        // Offset past EOF -> empty (no panic).
        assert!(backend.read_range(&e, 1000, 10).await.unwrap().is_empty());
        // Zero length -> empty.
        assert!(backend.read_range(&e, 0, 0).await.unwrap().is_empty());
    }
}
