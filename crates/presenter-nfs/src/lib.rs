#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::string_slice
    )
)]
//! NFS presenter — `NFSv3` server on loopback.
//!
//! Implements the engine's `VfsPresenter` trait, serving files via `NFSv3`
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

/// NFS presenter using an `NFSv3` server on loopback.
#[derive(Debug)]
pub struct NfsPresenter {
    #[allow(dead_code)] // Used for mount/unmount commands
    mount_point: PathBuf,
    nfs_port: u16,
    /// NFS context bridging procedures to the VFS tree.
    context: Arc<NfsContext>,
    /// Base directory for cached files (defaults to ~/.config/cascade/cache).
    cache_dir: PathBuf,
    /// State DB used to resolve an `ItemId` to its `FileEntry` for on-demand
    /// content fetch. Without it `fetch_contents` cannot recover the entry that
    /// a non-root mount needs to route to its owning backend.
    db: Option<Arc<cascade_engine::db::StateDb>>,
}

impl NfsPresenter {
    /// Create a new NFS presenter backed by the given VFS tree.
    #[must_use]
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
            db: None,
        }
    }

    /// Create with default mount point.
    #[must_use]
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
            db: None,
        }
    }

    #[must_use]
    pub const fn with_port(mut self, port: u16) -> Self {
        self.nfs_port = port;
        self
    }

    /// Attach the state DB used to resolve an `ItemId` to its `FileEntry`
    /// during on-demand content fetch.
    #[must_use]
    pub fn with_db(mut self, db: Arc<cascade_engine::db::StateDb>) -> Self {
        self.db = Some(db);
        self
    }

    /// Override the cache directory. Mainly used by tests so each test gets a
    /// unique tempdir and they don't race on the shared system cache path
    /// (which the running daemon also writes to).
    #[must_use]
    pub fn with_cache_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.cache_dir = path.into();
        self
    }

    /// Get a reference to the NFS context (for testing).
    #[must_use]
    pub const fn context(&self) -> &Arc<NfsContext> {
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

/// Sanitise an `ItemId` into a filesystem-safe filename.
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

        // `id` is an `ItemId` (`backend_id:native_id`), not a VFS path: route
        // by its backend id rather than feeding it to `vfs.resolve`, which does
        // longest-prefix matching on mount paths and would misroute any item
        // under a non-root mount to the neutral root. Look the entry up by
        // `ItemId` in the DB, then dispatch to the owning backend — mirroring
        // the File Provider handler.
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("NFS presenter has no state DB for content fetch"))?;
        let entry = db
            .get_file(id)?
            .ok_or_else(|| anyhow::anyhow!("item not found in state DB: {id}"))?;
        let backend: Arc<dyn cascade_engine::backend::Backend> = {
            let vfs = self
                .context
                .vfs()
                .read()
                .map_err(|e| anyhow::anyhow!("vfs RwLock poisoned: {e}"))?;
            let backend = vfs.backend_by_id(id.backend_id()).ok_or_else(|| {
                anyhow::anyhow!("no backend registered for id {}", id.backend_id())
            })?;
            Arc::clone(backend)
        };

        // Download to a temp file in the cache directory, then persist.
        tokio::fs::create_dir_all(&self.cache_dir).await?;
        let temp_path = cache_path.with_extension("tmp");
        let data = backend.download(&entry).await?;
        tokio::fs::write(&temp_path, &data).await?;

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

/// Build a VFS path string from a `VfsItem`'s id.
/// Produce the server-absolute VFS path for an item.
///
/// Returns `item.path` prefixed with `/`. The `path` field carries the full
/// mount-prefixed VFS path (no leading slash) written by the sync runner, so
/// no derivation from the `ItemId` is needed and the NFS presenter renders
/// by mount PATH rather than by backend ID.
fn format_vfs_path(item: &VfsItem) -> String {
    if item.path.starts_with('/') {
        item.path.clone()
    } else {
        format!("/{}", item.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cascade_engine::backend::{Backend, NullBackend};
    use cascade_engine::types::{Change, Cursor, FileEntry, FileId, ItemId, Quota, VfsItem};
    use cascade_engine::vfs::VfsTree;
    use std::path::Path as StdPath;
    use std::time::Duration;

    fn test_vfs() -> Arc<RwLock<VfsTree>> {
        Arc::new(RwLock::new(VfsTree::new(Arc::new(NullBackend::new(
            "test-backend",
        )))))
    }

    /// Backend whose `download` returns a fixed body tagged with its own id, so
    /// a test can prove `fetch_contents` routed to the owning backend.
    #[derive(Debug)]
    struct FixedBackend {
        id: String,
    }

    #[async_trait]
    impl Backend for FixedBackend {
        fn id(&self) -> &str {
            &self.id
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
        async fn metadata(&self, _path: &StdPath) -> anyhow::Result<FileEntry> {
            anyhow::bail!("metadata must not be called: fetch_contents routes by ItemId")
        }
        async fn download(&self, _file: &FileEntry) -> anyhow::Result<Vec<u8>> {
            Ok(format!("content-from-{}", self.id).into_bytes())
        }
        async fn upload(
            &self,
            _path: &StdPath,
            _data: &[u8],
            _parent_id: &FileId,
        ) -> anyhow::Result<FileEntry> {
            anyhow::bail!("no upload")
        }
        async fn update(&self, _file_id: &FileId, _data: &[u8]) -> anyhow::Result<FileEntry> {
            anyhow::bail!("no update")
        }
        async fn create_dir(&self, _path: &StdPath) -> anyhow::Result<FileEntry> {
            anyhow::bail!("no create_dir")
        }
        async fn delete(&self, _file: &FileEntry) -> anyhow::Result<()> {
            anyhow::bail!("no delete")
        }
        async fn move_entry(&self, _src: &StdPath, _dst: &StdPath) -> anyhow::Result<FileEntry> {
            anyhow::bail!("no move")
        }
        async fn poll_interval(&self) -> Option<Duration> {
            None
        }
    }

    #[tokio::test]
    async fn fetch_contents_routes_by_item_id_under_non_root_mount() {
        // A backend mounted at a non-root prefix. The item id `personalb:abc`
        // is NOT a VFS path: feeding it to vfs.resolve would misroute to the
        // neutral root. fetch_contents must look the entry up by ItemId and
        // dispatch via backend_by_id, reaching the owning backend.
        let backend: Arc<dyn Backend> = Arc::new(FixedBackend {
            id: "personalb".to_string(),
        });
        let mut tree = VfsTree::new(Arc::new(NullBackend::new("__neutral_root")));
        tree.mount(std::path::PathBuf::from("personal"), backend);
        let vfs = Arc::new(RwLock::new(tree));

        let db = Arc::new(cascade_engine::db::StateDb::open_in_memory().unwrap());
        db.register_backend("personalb", "fixed", "Fixed", Some("personal"), None)
            .unwrap();
        let id = ItemId::new("personalb", "abc");
        let entry = FileEntry::file(
            id.clone(),
            ItemId::new("personalb", "root"),
            "report.txt".to_string(),
        )
        .with_path("personal/report.txt".to_string());
        db.upsert_file(&entry).unwrap();

        let cache = tempfile::tempdir().unwrap();
        let presenter = NfsPresenter::with_vfs("/mnt/test", vfs)
            .with_db(db)
            .with_cache_dir(cache.path());

        let path = presenter.fetch_contents(&id).await.unwrap();
        let body = tokio::fs::read(&path).await.unwrap();
        assert_eq!(body, b"content-from-personalb");
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
            // The sync runner writes the mount-prefixed path; for a backend
            // mounted at "gdrive" the path is "gdrive/root".
            path: "gdrive/root".to_string(),
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
            path: "gdrive/root".to_string(),
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
            path: "gdrive/root".to_string(),
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
