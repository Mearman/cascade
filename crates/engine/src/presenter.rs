use crate::types::*;
use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;

/// Platform-agnostic interface for presenting the VFS to the OS.
/// Compile-time selection: File Provider on macOS, FUSE on Linux,
/// WinFSP on Windows, NFS as universal fallback.
#[async_trait]
pub trait VfsPresenter: Send + Sync {
    /// A file was added or updated in the VFS.
    async fn upsert_item(&self, item: VfsItem) -> Result<()>;

    /// A file or directory was deleted from the VFS.
    async fn delete_item(&self, id: &ItemId) -> Result<()>;

    /// A file's cache state changed (online → cached → pinned).
    async fn update_state(&self, id: &ItemId, state: CacheState) -> Result<()>;

    /// Download a file's contents (on-demand). Returns local path.
    async fn fetch_contents(&self, id: &ItemId) -> Result<std::path::PathBuf>;

    /// Evict a file (free up space).
    async fn evict_item(&self, id: &ItemId) -> Result<()>;

    /// Start presenting the VFS at the given mount point.
    async fn start(&self, mount_point: &Path) -> Result<()>;

    /// Stop presenting.
    async fn stop(&self) -> Result<()>;
}
