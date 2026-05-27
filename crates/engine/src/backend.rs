//! Backend trait — every cloud provider implements this.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;

use crate::types::{Change, Cursor, FileEntry, FileId, Quota};

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
        writer: &mut (dyn tokio::io::AsyncWrite + Unpin),
    ) -> anyhow::Result<()>;

    /// Upload file content, replacing the existing file or creating a new one.
    async fn upload(
        &self,
        path: &Path,
        reader: &mut (dyn tokio::io::AsyncRead + Unpin),
        parent_id: &FileId,
    ) -> anyhow::Result<FileEntry>;

    /// Create a directory.
    async fn create_dir(&self, path: &Path) -> anyhow::Result<FileEntry>;

    /// Delete a file or directory.
    async fn delete(&self, file: &FileEntry) -> anyhow::Result<()>;

    /// Move/rename a file or directory.
    async fn move_entry(&self, src: &Path, dst: &Path) -> anyhow::Result<FileEntry>;

    /// Recommended poll interval for this backend. Returns `None` if the
    /// backend doesn't support polling (use fixed interval from config).
    async fn poll_interval(&self) -> Option<Duration>;
}

/// A null backend used for P2P-only folders with no cloud storage.
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

    fn display_name(&self) -> &str {
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
        _writer: &mut (dyn tokio::io::AsyncWrite + Unpin),
    ) -> anyhow::Result<()> {
        anyhow::bail!("null backend has no files")
    }

    async fn upload(
        &self,
        _path: &Path,
        _reader: &mut (dyn tokio::io::AsyncRead + Unpin),
        _parent_id: &FileId,
    ) -> anyhow::Result<FileEntry> {
        anyhow::bail!("null backend cannot upload")
    }

    async fn create_dir(&self, _path: &Path) -> anyhow::Result<FileEntry> {
        anyhow::bail!("null backend cannot create directories")
    }

    async fn delete(&self, _file: &FileEntry) -> anyhow::Result<()> {
        anyhow::bail!("null backend cannot delete")
    }

    async fn move_entry(&self, _src: &Path, _dst: &Path) -> anyhow::Result<FileEntry> {
        anyhow::bail!("null backend cannot move")
    }

    async fn poll_interval(&self) -> Option<Duration> {
        None
    }
}
