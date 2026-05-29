//! `WebDAV` presenter — serves files via `WebDAV` on loopback.
//!
//! Implements the engine's `VfsPresenter` trait, providing a `WebDAV` server
//! that macOS can mount using `/sbin/mount_webdav` without root privileges.
//!
//! # `WebDAV` methods supported
//!
//! - `PROPFIND` — directory listing and file metadata (`WebDAV` XML responses)
//! - `GET` — download file contents
//! - `PUT` — upload file contents
//! - `MKCOL` — create a directory
//! - `DELETE` — remove a file or directory
//! - `MOVE` — rename or move a resource
//! - `COPY` — copy a resource

pub mod server;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use cascade_engine::backend::Backend;
use cascade_engine::db::StateDb;
use cascade_engine::presenter::VfsPresenter;
use cascade_engine::types::{CacheState, ItemId, VfsItem};
use server::WebDavServer;

/// Directory used for cached file contents.
const CACHE_DIR_NAME: &str = "cascade/cache";

/// `WebDAV` presenter using an HTTP server on loopback.
pub struct WebDavPresenter {
    #[allow(dead_code)] // Used for mount/unmount commands
    mount_point: PathBuf,
    /// Base directory for cached files (defaults to ~/.config/cascade/cache).
    cache_dir: PathBuf,
    /// In-memory item store keyed by `ItemId`.
    items: Arc<RwLock<HashMap<String, VfsItem>>>,
    /// Running server handle (set after start).
    pub server: Arc<tokio::sync::Mutex<Option<WebDavServer>>>,
    /// Backends for on-demand directory expansion.
    backends: Arc<tokio::sync::RwLock<Vec<Arc<dyn Backend>>>>,
    /// State DB for persisting expanded items.
    db: Option<Arc<StateDb>>,
}

impl std::fmt::Debug for WebDavPresenter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebDavPresenter")
            .field("mount_point", &self.mount_point)
            .finish_non_exhaustive()
    }
}

impl WebDavPresenter {
    /// Create a new `WebDAV` presenter.
    #[must_use]
    pub fn new(mount_point: impl Into<PathBuf>) -> Self {
        Self {
            mount_point: mount_point.into(),
            cache_dir: dirs_cache_dir(),
            items: Arc::new(RwLock::new(HashMap::new())),
            server: Arc::new(tokio::sync::Mutex::new(None)),
            backends: Arc::new(tokio::sync::RwLock::new(Vec::new())),
            db: None,
        }
    }

    /// Create with default mount point.
    #[must_use]
    pub fn default_mount() -> Self {
        Self::new("/mnt/cascade")
    }

    /// Set the backends for on-demand directory expansion.
    pub fn with_backends(&mut self, backends: Vec<Arc<dyn Backend>>) {
        *self.backends.blocking_write() = backends;
    }

    /// Set the state DB for persisting expanded items.
    pub fn with_db(&mut self, db: Arc<StateDb>) {
        self.db = Some(db);
    }

    /// Get the items map (for server access).
    #[must_use]
    pub const fn items(&self) -> &Arc<RwLock<HashMap<String, VfsItem>>> {
        &self.items
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
impl VfsPresenter for WebDavPresenter {
    async fn upsert_item(&self, item: VfsItem) -> anyhow::Result<()> {
        tracing::debug!(id = %item.id, name = %item.name, "upsert_item");
        let key = item.id.0.clone();
        let mut items = self
            .items
            .write()
            .map_err(|e| anyhow::anyhow!("items RwLock poisoned: {e}"))?;
        items.insert(key, item);
        Ok(())
    }

    async fn delete_item(&self, id: &ItemId) -> anyhow::Result<()> {
        tracing::debug!(id = %id, "delete_item");
        {
            let mut items = self
                .items
                .write()
                .map_err(|e| anyhow::anyhow!("items RwLock poisoned: {e}"))?;
            items.remove(&id.0);
        }
        let cache_path = self.cache_path_for(id);
        if cache_path.exists() {
            tokio::fs::remove_file(&cache_path).await?;
        }
        Ok(())
    }

    async fn update_state(&self, id: &ItemId, state: CacheState) -> anyhow::Result<()> {
        tracing::debug!(id = %id, state = %state, "update_state");
        let mut items = self
            .items
            .write()
            .map_err(|e| anyhow::anyhow!("items RwLock poisoned: {e}"))?;
        if let Some(item) = items.get_mut(&id.0) {
            item.cache_state = state;
        }
        Ok(())
    }

    async fn fetch_contents(&self, id: &ItemId) -> anyhow::Result<PathBuf> {
        tracing::debug!(id = %id, "fetch_contents");
        let cache_path = self.cache_path_for(id);

        if cache_path.exists() {
            return Ok(cache_path);
        }

        // For the WebDAV presenter, fetch_contents signals that the file
        // needs to be downloaded. The actual download goes through the
        // engine's backend layer. We create a placeholder here — the sync
        // runner coordinates the real download.
        tokio::fs::create_dir_all(&self.cache_dir).await?;
        tokio::fs::write(&cache_path, b"").await?;
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
            "starting WebDAV presenter"
        );
        let server = WebDavServer::start(
            "127.0.0.1:0",
            self.items.clone(),
            self.cache_dir.clone(),
            self.backends.clone(),
            self.db.clone(),
        )
        .await?;
        let port = server.port();
        tracing::info!(port, "WebDAV server started");
        let mut guard = self.server.lock().await;
        *guard = Some(server);
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        tracing::info!("stopping WebDAV presenter");
        let mut guard = self.server.lock().await;
        if let Some(server) = guard.take() {
            server.stop()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cascade_engine::types::ItemId;

    #[test]
    fn safe_filename_sanitises() {
        assert_eq!(safe_filename("gdrive:abc123"), "gdrive_abc123");
        assert_eq!(safe_filename("path/to/file"), "path_to_file");
    }

    #[test]
    fn item_path_from_vfs_item() {
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
        assert_eq!(server::item_path(&item), "/gdrive/root");
    }

    #[tokio::test]
    async fn upsert_stores_item() {
        let presenter = WebDavPresenter::new("/tmp/test");
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
        let items = presenter.items.read().unwrap();
        assert!(items.contains_key("gdrive:root"));
    }

    #[tokio::test]
    async fn delete_removes_item() {
        let presenter = WebDavPresenter::new("/tmp/test");
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
        let items = presenter.items.read().unwrap();
        assert!(!items.contains_key("gdrive:root"));
    }

    #[tokio::test]
    async fn update_state_modifies_item() {
        let presenter = WebDavPresenter::new("/tmp/test");
        let id = ItemId::new("gdrive", "file1");
        let item = VfsItem {
            id: id.clone(),
            parent_id: ItemId::new("gdrive", ""),
            name: "file1".to_string(),
            is_dir: false,
            size: None,
            mod_time: None,
            cache_state: CacheState::Online,
            mime_type: None,
        };
        presenter.upsert_item(item).await.unwrap();
        presenter
            .update_state(&id, CacheState::Cached)
            .await
            .unwrap();
        let items = presenter.items.read().unwrap();
        assert_eq!(
            items.get("gdrive:file1").unwrap().cache_state,
            CacheState::Cached
        );
    }

    #[tokio::test]
    async fn evict_removes_cached_file() {
        let presenter = WebDavPresenter::new("/tmp/test");
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
}
