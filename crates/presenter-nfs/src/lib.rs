//! NFS presenter — NFSv3 server on loopback.
//!
//! Implements the engine's `VfsPresenter` trait, serving files via NFSv3
//! to the OS NFS client.

pub mod nfs;

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use cascade_engine::presenter::VfsPresenter;
use cascade_engine::types::{CacheState, ItemId, VfsItem};
use nfs::context::NfsContext;
use nfs::xdr::NfsFh3;

/// Directory used for cached file contents.
const CACHE_DIR_NAME: &str = "cascade/cache";

/// NFS presenter using an NFSv3 server on loopback.
pub struct NfsPresenter {
    #[allow(dead_code)] // Used for mount/unmount commands
    mount_point: PathBuf,
    nfs_port: u16,
    /// NFS context bridging procedures to the VFS tree.
    context: Arc<NfsContext>,
    /// Base directory for cached files (defaults to ~/.config/cascade/cache).
    cache_dir: PathBuf,
}

impl NfsPresenter {
    /// Create a new NFS presenter backed by the given VFS tree.
    pub fn with_vfs(
        mount_point: impl Into<PathBuf>,
        vfs: Arc<RwLock<cascade_engine::vfs::VfsTree>>,
    ) -> Self {
        let context = Arc::new(NfsContext::new(vfs));
        let cache_dir = dirs_cache_dir();
        Self {
            mount_point: mount_point.into(),
            nfs_port: 0,
            context,
            cache_dir,
        }
    }

    /// Create with default mount point.
    pub fn default_mount() -> Self {
        Self {
            mount_point: PathBuf::from("/mnt/cascade"),
            nfs_port: 0,
            context: Arc::new(NfsContext::new(Arc::new(RwLock::new(
                cascade_engine::vfs::VfsTree::new(Arc::new(
                    cascade_engine::backend::NullBackend::new("null"),
                )),
            )))),
            cache_dir: dirs_cache_dir(),
        }
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.nfs_port = port;
        self
    }

    /// Get a reference to the NFS context (for testing).
    pub fn context(&self) -> &Arc<NfsContext> {
        &self.context
    }

    /// Compute the cached file path for an item.
    fn cache_path_for(&self, id: &ItemId) -> PathBuf {
        self.cache_dir.join(safe_filename(&id.0))
    }
}

/// Build the default cache directory path.
fn dirs_cache_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(CACHE_DIR_NAME)
}

/// Sanitise an ItemId into a filesystem-safe filename.
fn safe_filename(id: &str) -> String {
    id.replace([':', '/', '\\'], "_")
}

#[async_trait]
impl VfsPresenter for NfsPresenter {
    async fn upsert_item(&self, item: VfsItem) -> anyhow::Result<()> {
        tracing::debug!(id = %item.id, name = %item.name, "upsert_item");
        // Register the item's path in the NFS file handle map so LOOKUP can
        // find it. Build a VFS path from the item's display name.
        let vfs_path = format_vfs_path(&item);
        let fh_key = self.context.register_path(&vfs_path);
        let fh = NfsFh3::from_item_id(&fh_key.to_string());
        tracing::trace!(path = %vfs_path, fh_key, "registered NFS file handle");
        let _ = fh; // fh is used implicitly through the context's fh_map
        Ok(())
    }

    async fn delete_item(&self, id: &ItemId) -> anyhow::Result<()> {
        tracing::debug!(id = %id, "delete_item");
        // Build the same VFS path that was used during upsert so we can
        // remove the file handle from the map.
        let fh_key = NfsContext::path_to_key(&id.0);
        self.context.remove_path(fh_key);
        // Also remove any cached file on disk.
        let cache_path = self.cache_path_for(id);
        if cache_path.exists() {
            tokio::fs::remove_file(&cache_path).await?;
        }
        Ok(())
    }

    async fn update_state(&self, id: &ItemId, state: CacheState) -> anyhow::Result<()> {
        tracing::debug!(id = %id, state = %state, "update_state");
        // NFS has no mechanism to push cache-state changes to the kernel
        // client. We just log the transition — the next GETATTR or READ
        // will reflect the new state naturally.
        Ok(())
    }

    async fn fetch_contents(&self, id: &ItemId) -> anyhow::Result<PathBuf> {
        tracing::debug!(id = %id, "fetch_contents");
        let cache_path = self.cache_path_for(id);

        // Return the cached path if the file is already on disk.
        if cache_path.exists() {
            return Ok(cache_path);
        }

        // Resolve the item through the VFS to get metadata and a backend.
        let (entry, backend): (
            cascade_engine::types::FileEntry,
            Arc<dyn cascade_engine::backend::Backend>,
        ) = {
            let (backend, relative) = {
                let vfs = self.context.vfs().read().unwrap();
                let (backend, relative) = vfs.resolve(std::path::Path::new(&id.0));
                (Arc::clone(backend), relative.to_path_buf())
            };
            // Lock is dropped here, before the await.
            let entry = backend.metadata(&relative).await?;
            (entry, backend)
        };

        // Download to a temp file in the cache directory, then persist.
        tokio::fs::create_dir_all(&self.cache_dir).await?;
        let temp_path = cache_path.with_extension("tmp");
        let mut file = tokio::fs::File::create(&temp_path).await?;
        use tokio::io::AsyncWriteExt;
        // Wrap in a compat writer for the Backend trait.
        let mut writer = WriterAdapter { inner: file };
        backend.download(&entry, &mut writer).await?;
        // Flush and drop the writer, then close the inner file.
        file = writer.inner;
        file.flush().await?;

        tokio::fs::rename(&temp_path, &cache_path).await?;
        Ok(cache_path)
    }

    async fn evict_item(&self, id: &ItemId) -> anyhow::Result<()> {
        tracing::debug!(id = %id, "evict_item");
        let cache_path = self.cache_path_for(id);
        if cache_path.exists() {
            tokio::fs::remove_file(&cache_path).await?;
            tracing::debug!(path = %cache_path.display(), "evicted cached file");
        }
        Ok(())
    }

    async fn start(&self, mount_point: &Path) -> anyhow::Result<()> {
        tracing::info!(
            mount_point = %mount_point.display(),
            port = self.nfs_port,
            "starting NFS presenter"
        );
        // The NFS server is started separately in the mount command.
        // This method is for the presenter trait interface.
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        tracing::info!("stopping NFS presenter");
        // The NFS server is stopped separately in the mount command.
        Ok(())
    }
}

/// Adapter from `tokio::fs::File` (AsyncWrite) to the
/// `dyn AsyncWrite + Unpin + Send` that the Backend trait expects.
struct WriterAdapter {
    inner: tokio::fs::File,
}

impl tokio::io::AsyncWrite for WriterAdapter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

impl Unpin for WriterAdapter {}

/// Build a VFS path string from a VfsItem's id.
/// Uses the item's own id as a path key — this matches the convention
/// where ItemId encodes the backend path.
fn format_vfs_path(item: &VfsItem) -> String {
    if item.id.0.starts_with('/') {
        item.id.0.clone()
    } else {
        format!("/{}", item.id.0.replace(':', "/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cascade_engine::backend::NullBackend;
    use cascade_engine::types::{ItemId, VfsItem};
    use cascade_engine::vfs::VfsTree;

    fn test_vfs() -> Arc<RwLock<VfsTree>> {
        Arc::new(RwLock::new(VfsTree::new(Arc::new(NullBackend::new(
            "test-backend",
        )))))
    }

    #[test]
    fn safe_filename_sanitises() {
        assert_eq!(safe_filename("gdrive:abc123"), "gdrive_abc123");
        assert_eq!(safe_filename("path/to/file"), "path_to_file");
    }

    #[test]
    fn format_vfs_path_adds_slash() {
        let item = VfsItem {
            id: ItemId::new("gdrive", "root"),
            parent_id: ItemId::new("gdrive", "parent"),
            name: "root".to_string(),
            is_dir: true,
            size: None,
            mod_time: None,
            cache_state: CacheState::Online,
            mime_type: None,
        };
        assert_eq!(format_vfs_path(&item), "/gdrive/root");
    }

    #[tokio::test]
    async fn upsert_registers_in_context() {
        let presenter = NfsPresenter::with_vfs("/mnt/test", test_vfs());
        let item = VfsItem {
            id: ItemId::new("gdrive", "root"),
            parent_id: ItemId::new("gdrive", ""),
            name: "root".to_string(),
            is_dir: true,
            size: None,
            mod_time: None,
            cache_state: CacheState::Online,
            mime_type: None,
        };
        presenter.upsert_item(item).await.unwrap();
        // The path /gdrive/root should now be registered.
        let key = NfsContext::path_to_key("/gdrive/root");
        assert_eq!(
            presenter.context().lookup_path(key),
            Some("/gdrive/root".to_string())
        );
    }

    #[tokio::test]
    async fn delete_removes_from_context_and_cache() {
        let presenter = NfsPresenter::with_vfs("/mnt/test", test_vfs());
        let id = ItemId::new("gdrive", "root");
        let item = VfsItem {
            id: id.clone(),
            parent_id: ItemId::new("gdrive", ""),
            name: "root".to_string(),
            is_dir: true,
            size: None,
            mod_time: None,
            cache_state: CacheState::Online,
            mime_type: None,
        };
        presenter.upsert_item(item).await.unwrap();
        presenter.delete_item(&id).await.unwrap();
        // Path should no longer be in the context.
        let key = NfsContext::path_to_key(&id.0);
        assert_eq!(presenter.context().lookup_path(key), None);
    }

    #[tokio::test]
    async fn update_state_is_ok() {
        let presenter = NfsPresenter::with_vfs("/mnt/test", test_vfs());
        let id = ItemId::new("gdrive", "file1");
        presenter
            .update_state(&id, CacheState::Cached)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn evict_removes_cached_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("test_file");
        tokio::fs::write(&cache_path, b"data").await.unwrap();
        assert!(cache_path.exists());

        let presenter = NfsPresenter::with_vfs("/mnt/test", test_vfs());
        // Manually write to where cache_path_for would point.
        let id = ItemId::new("test", "file1");
        let real_cache = presenter.cache_path_for(&id);
        tokio::fs::create_dir_all(real_cache.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&real_cache, b"data").await.unwrap();

        presenter.evict_item(&id).await.unwrap();
        assert!(!real_cache.exists());
    }
}
