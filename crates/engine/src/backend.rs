//! Backend trait — every cloud provider implements this.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;

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
