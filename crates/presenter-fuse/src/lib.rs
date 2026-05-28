//! FUSE presenter — exposes the VFS tree as a mounted Linux FUSE filesystem.
//!
//! Implements the engine's `VfsPresenter` trait. On Linux, the FUSE mount is
//! served via the `fuser` crate. On other platforms, all operations return
//! errors — the crate compiles but does not mount.

pub mod inode;
pub mod ops;

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use cascade_engine::backend::Backend;
use cascade_engine::presenter::VfsPresenter;
use cascade_engine::types::{CacheState, ItemId, VfsItem};
use cascade_engine::vfs::VfsTree;

use crate::inode::InodeMap;

/// Directory used for cached file contents.
const CACHE_DIR_NAME: &str = "cascade/cache";

/// FUSE presenter wrapping an inode map and VFS tree reference.
pub struct FusePresenter {
    /// The root ItemId for the VFS tree.
    #[allow(dead_code)] // Used on Linux in start()
    root_id: ItemId,
    /// Mount point path.
    mount_point: PathBuf,
    /// Inode map for translating between FUSE inodes and VFS ItemIds.
    inode_map: Arc<std::sync::Mutex<InodeMap>>,
    /// VFS tree for resolving paths to backends.
    vfs: Arc<RwLock<VfsTree>>,
    /// Base directory for cached files.
    cache_dir: PathBuf,
}

impl FusePresenter {
    /// Create a new FUSE presenter for the given root ItemId and VFS tree.
    pub fn with_vfs(root_id: ItemId, vfs: Arc<RwLock<VfsTree>>) -> Self {
        let inode_map = Arc::new(std::sync::Mutex::new(InodeMap::new(root_id.clone())));
        let cache_dir = dirs_cache_dir();
        Self {
            root_id,
            mount_point: PathBuf::from("/mnt/cascade"),
            vfs,
            inode_map,
            cache_dir,
        }
    }

    /// Create a new FUSE presenter for the given root ItemId.
    pub fn new(root_id: ItemId) -> Self {
        let vfs = Arc::new(RwLock::new(VfsTree::new(Arc::new(
            cascade_engine::backend::NullBackend::new("null"),
        ))));
        Self::with_vfs(root_id, vfs)
    }

    /// Set a custom mount point.
    pub fn with_mount_point(mut self, path: impl Into<PathBuf>) -> Self {
        self.mount_point = path.into();
        self
    }

    /// Get a reference to the inode map (for testing).
    pub fn inode_map(&self) -> &Arc<std::sync::Mutex<InodeMap>> {
        &self.inode_map
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
impl VfsPresenter for FusePresenter {
    async fn upsert_item(&self, item: VfsItem) -> anyhow::Result<()> {
        tracing::debug!(id = %item.id, name = %item.name, "upsert_item");
        let mut map = self.inode_map.lock().unwrap();
        map.allocate(item.id.clone());
        Ok(())
    }

    async fn delete_item(&self, id: &ItemId) -> anyhow::Result<()> {
        tracing::debug!(id = %id, "delete_item");
        // Remove from inode map.
        {
            let mut map = self.inode_map.lock().unwrap();
            map.remove(id);
        }
        // Remove any cached file on disk.
        let cache_path = self.cache_path_for(id);
        if cache_path.exists() {
            tokio::fs::remove_file(&cache_path).await?;
        }
        Ok(())
    }

    async fn update_state(&self, id: &ItemId, state: CacheState) -> anyhow::Result<()> {
        tracing::debug!(id = %id, state = %state, "update_state");
        // FUSE has no mechanism to push cache-state changes to the kernel.
        // Log the transition — the next getattr/read will reflect it.
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
        let (entry, backend): (cascade_engine::types::FileEntry, Arc<dyn Backend>) = {
            let (backend, relative) = {
                let vfs = self.vfs.read().unwrap();
                let (backend, relative) = vfs.resolve(Path::new(&id.0));
                (Arc::clone(backend), relative.to_path_buf())
            };
            // Lock is dropped before the await.
            let entry = backend.metadata(&relative).await?;
            (entry, backend)
        };

        // Download to a temp file in the cache directory, then persist.
        tokio::fs::create_dir_all(&self.cache_dir).await?;
        let temp_path = cache_path.with_extension("tmp");
        let file = tokio::fs::File::create(&temp_path).await?;
        let mut writer = WriterAdapter { inner: file };
        backend.download(&entry, &mut writer).await?;
        let mut file = writer.inner;
        use tokio::io::AsyncWriteExt;
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
        let mount_display = mount_point.display();

        #[cfg(target_os = "linux")]
        {
            tracing::info!(mount_point = %mount_display, "starting FUSE presenter");

            let ops =
                crate::ops::FuseOps::new_with_vfs(self.root_id.clone(), Arc::clone(&self.vfs));
            let mp = mount_point.to_path_buf();

            // Spawn the FUSE session in a background task.
            // fuser::spawn_mount2 blocks, so we run it in a dedicated thread.
            std::thread::spawn(move || {
                let mut config = fuser::Config::default();
                config.mount_options = vec![
                    fuser::MountOption::RO,
                    fuser::MountOption::FSName("cascade".to_string()),
                    fuser::MountOption::DefaultPermissions,
                ];
                match fuser::spawn_mount2(ops, &mp, &config) {
                    Ok(_session) => {
                        tracing::info!("FUSE session ended");
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "FUSE mount failed");
                    }
                }
            });

            Ok(())
        }

        #[cfg(not(target_os = "linux"))]
        {
            tracing::warn!(
                mount_point = %mount_display,
                "FUSE presenter is not available on this platform (Linux only)"
            );
            anyhow::bail!(
                "FUSE presenter requires Linux. Current platform does not support FUSE mounting."
            )
        }
    }

    async fn stop(&self) -> anyhow::Result<()> {
        tracing::info!("stopping FUSE presenter");

        #[cfg(target_os = "linux")]
        {
            // TODO: Signal the FUSE session to unmount and wait for the thread.
            // fuser sessions unmount when dropped, or via fuser::unmount().
        }

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

#[cfg(test)]
mod tests {
    use super::*;
    use cascade_engine::backend::NullBackend;

    #[allow(dead_code)]
    fn test_vfs() -> Arc<RwLock<VfsTree>> {
        Arc::new(RwLock::new(VfsTree::new(Arc::new(NullBackend::new(
            "test-backend",
        )))))
    }

    #[test]
    fn fuse_presenter_new() {
        let root = ItemId::new("gdrive", "root");
        let presenter = FusePresenter::new(root.clone());
        assert_eq!(presenter.root_id, root);
        assert_eq!(presenter.mount_point, PathBuf::from("/mnt/cascade"));
    }

    #[test]
    fn fuse_presenter_custom_mount() {
        let root = ItemId::new("gdrive", "root");
        let presenter = FusePresenter::new(root).with_mount_point("/media/cascade");
        assert_eq!(presenter.mount_point, PathBuf::from("/media/cascade"));
    }

    #[tokio::test]
    async fn start_fails_on_non_linux() {
        let root = ItemId::new("gdrive", "root");
        let presenter = FusePresenter::new(root);

        #[cfg(not(target_os = "linux"))]
        {
            let result = presenter.start(Path::new("/mnt/test")).await;
            assert!(result.is_err());
            let err = result.unwrap_err().to_string();
            assert!(
                err.contains("Linux"),
                "expected Linux-specific error, got: {err}"
            );
        }

        #[cfg(target_os = "linux")]
        {
            // On Linux, start would attempt a real mount — skip in unit tests.
        }
    }

    #[tokio::test]
    async fn upsert_allocates_inode() {
        let root = ItemId::new("gdrive", "root");
        let presenter = FusePresenter::new(root);
        let item = VfsItem {
            id: ItemId::new("gdrive", "file1"),
            parent_id: ItemId::new("gdrive", "root"),
            name: "test.txt".to_string(),
            is_dir: false,
            size: Some(100),
            mod_time: None,
            cache_state: CacheState::Online,
            mime_type: None,
        };

        presenter.upsert_item(item).await.unwrap();
        let map = presenter.inode_map().lock().unwrap();
        assert_eq!(map.get_inode(&ItemId::new("gdrive", "file1")), Some(2));
    }

    #[tokio::test]
    async fn delete_removes_inode() {
        let root = ItemId::new("gdrive", "root");
        let presenter = FusePresenter::new(root);
        let id = ItemId::new("gdrive", "file1");

        let item = VfsItem {
            id: id.clone(),
            parent_id: ItemId::new("gdrive", "root"),
            name: "test.txt".to_string(),
            is_dir: false,
            size: Some(100),
            mod_time: None,
            cache_state: CacheState::Online,
            mime_type: None,
        };
        presenter.upsert_item(item).await.unwrap();
        presenter.delete_item(&id).await.unwrap();
        let map = presenter.inode_map().lock().unwrap();
        assert_eq!(map.get_inode(&id), None);
    }

    #[tokio::test]
    async fn update_state_is_ok() {
        let root = ItemId::new("gdrive", "root");
        let presenter = FusePresenter::new(root);
        let id = ItemId::new("gdrive", "file1");
        presenter
            .update_state(&id, CacheState::Cached)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn evict_removes_cached_file() {
        let root = ItemId::new("gdrive", "root");
        let presenter = FusePresenter::new(root);
        let id = ItemId::new("test", "file1");
        let cache_path = presenter.cache_path_for(&id);

        tokio::fs::create_dir_all(cache_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&cache_path, b"data").await.unwrap();
        assert!(cache_path.exists());

        presenter.evict_item(&id).await.unwrap();
        assert!(!cache_path.exists());
    }

    #[tokio::test]
    async fn fetch_contents_caches_file() {
        // fetch_contents with a NullBackend will fail (no files),
        // but we can test the caching path by pre-placing a file.
        let root = ItemId::new("gdrive", "root");
        let presenter = FusePresenter::new(root);
        let id = ItemId::new("test", "cached");
        let cache_path = presenter.cache_path_for(&id);

        // Pre-place a cached file.
        tokio::fs::create_dir_all(cache_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&cache_path, b"cached data").await.unwrap();

        // fetch_contents should return the existing cached path.
        let result = presenter.fetch_contents(&id).await.unwrap();
        assert_eq!(result, cache_path);
    }
}
