//! Unified Cascade engine that owns and coordinates all components.
//!
//! The engine manages the lifecycle of:
//! - VFS tree (multi-backend routing)
//! - State database (file metadata, pin rules, P2P state)
//! - Sync runner (polls backends for changes)
//! - Cache manager (pin/evict/background worker)
//! - P2P bridge (optional, LAN block sharing)
//! - Config resolver (.cascade file filtering)

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::Result;
use tokio::sync::watch;
use tracing::info;

use crate::backend::Backend;
use crate::cache::manager::{CacheManager, CacheManagerConfig};
use crate::config::ConfigResolver;
use crate::db::{PinRuleRecord, StateDb};
use crate::p2p_bridge::P2pBridge;
use crate::presenter::VfsPresenter;
use crate::sync::runner::SyncRunner;
use crate::vfs::VfsTree;

/// Configuration for creating an Engine instance.
pub struct EngineConfig {
    /// Path to the `SQLite` state database file.
    pub db_path: PathBuf,
    /// Mount point for the VFS.
    pub mount_point: PathBuf,
    /// Backend instances. The first is the root; subsequent ones are mounted
    /// at their configured prefixes.
    pub backends: Vec<Arc<dyn Backend>>,
    /// Cache directory override. `None` uses the default.
    pub cache_dir: Option<PathBuf>,
    /// Whether to enable P2P block sharing.
    pub enable_p2p: bool,
    /// P2P data directory override. `None` uses `db_path` parent + `/p2p`.
    pub p2p_data_dir: Option<PathBuf>,
}

impl fmt::Debug for EngineConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EngineConfig")
            .field("db_path", &self.db_path)
            .field("mount_point", &self.mount_point)
            .field("backends", &format!("[{} backend(s)]", self.backends.len()))
            .field("cache_dir", &self.cache_dir)
            .field("enable_p2p", &self.enable_p2p)
            .field("p2p_data_dir", &self.p2p_data_dir)
            .finish()
    }
}

/// Unified Cascade engine that owns and coordinates all components.
pub struct Engine {
    db: Arc<StateDb>,
    vfs: Arc<RwLock<VfsTree>>,
    cache: CacheManager,
    config: Arc<ConfigResolver>,
    p2p: Option<P2pBridge>,
    cancel: watch::Sender<bool>,
    cancel_rx: watch::Receiver<bool>,
}

impl fmt::Debug for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Engine")
            .field("p2p_enabled", &self.p2p.is_some())
            .finish_non_exhaustive()
    }
}

impl Engine {
    /// Create and initialise a new engine.
    ///
    /// Opens the state database, creates the VFS tree, registers backends,
    /// and wires up the cache manager and optional P2P bridge.
    pub async fn new(config: EngineConfig) -> Result<Self> {
        let backends = config.backends;
        if backends.is_empty() {
            anyhow::bail!("at least one backend is required");
        }

        // Open state database.
        let db = Arc::new(StateDb::open(&config.db_path)?);
        info!(path = %config.db_path.display(), "state database opened");

        // Register all backends in the DB and build the VFS tree.
        // Safety: we checked `backends.is_empty()` above, so first() is always Some here.
        let root = backends
            .first()
            .ok_or_else(|| anyhow::anyhow!("backend list is empty"))?
            .clone();
        db.register_backend(root.id(), "unknown", root.display_name(), None, None)?;

        let mut vfs_tree = VfsTree::new(root);
        for backend in backends.get(1..).unwrap_or(&[]) {
            db.register_backend(backend.id(), "unknown", backend.display_name(), None, None)?;
            // Use the backend ID as the mount prefix for non-root backends.
            vfs_tree.mount(PathBuf::from(backend.id()), (*backend).clone());
        }
        let vfs = Arc::new(RwLock::new(vfs_tree));
        info!(backends = backends.len(), "VFS tree initialised");

        // Config resolver for .cascade file filtering.
        let config_resolver = Arc::new(ConfigResolver::new(config.mount_point.clone()));

        // Cache manager.
        let cache = CacheManager::new(db.clone(), CacheManagerConfig::default());

        // P2P bridge (optional).
        let p2p = if config.enable_p2p {
            let p2p_dir = config.p2p_data_dir.unwrap_or_else(|| {
                config
                    .db_path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join("p2p")
            });
            let p2p_engine = cascade_p2p::P2pEngine::new(&p2p_dir).await?;
            info!(device_id = %p2p_engine.device_id(), "P2P engine initialised");
            Some(P2pBridge::new(p2p_engine, db.clone()))
        } else {
            None
        };

        let (cancel, cancel_rx) = watch::channel(false);

        Ok(Self {
            db,
            vfs,
            cache,
            config: config_resolver,
            p2p,
            cancel,
            cancel_rx,
        })
    }

    /// Mount a backend at a VFS prefix.
    pub fn mount_backend(&self, prefix: PathBuf, backend: Arc<dyn Backend>) {
        let mut tree = self
            .vfs
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tree.mount(prefix, backend);
    }

    /// Unmount a backend from a VFS prefix.
    pub fn unmount_backend(&self, prefix: &Path) {
        let mut tree = self
            .vfs
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tree.unmount(prefix);
    }

    /// Create a sync runner for polling all registered backends.
    ///
    /// The caller provides the presenter (typically the platform presenter —
    /// NFS, FUSE, etc.) and is responsible for spawning the runner as a
    /// background task.
    pub fn create_sync_runner(&self, presenter: Arc<dyn VfsPresenter>) -> SyncRunner {
        // Collect all backends from the VFS tree.
        let tree = self
            .vfs
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut backends = vec![tree.root().clone()];
        for (_, backend) in tree.children() {
            backends.push(backend.clone());
        }
        drop(tree);

        SyncRunner::new(self.db.clone(), backends, presenter, self.config.clone())
    }

    /// Start the engine's background tasks (cache manager).
    ///
    /// The sync runner is not started here — use [`Engine::create_sync_runner`]
    /// to build one with the real presenter, then spawn it yourself.
    pub fn start(&self) -> Result<EngineHandle> {
        // Start cache manager background worker.
        let cancel_rx = self.cancel.subscribe();
        let cache_db = self.db.clone();
        let cache_config = CacheManagerConfig::default();
        let cache_worker = CacheManager::new(cache_db, cache_config);
        let cache_handle = tokio::spawn(async move {
            cache_worker.run(cancel_rx).await;
        });

        info!("engine started");

        Ok(EngineHandle { cache_handle })
    }

    /// Graceful shutdown — signals all components to stop.
    pub fn shutdown(&self) {
        info!("engine shutting down");
        let _ = self.cancel.send(true);
        info!("engine shutdown complete");
    }

    /// Pin a path pattern. All files matching the glob will be kept offline.
    pub fn pin(&self, pattern: &str, recursive: bool) -> Result<()> {
        self.cache.pin(pattern, recursive)
    }

    /// Unpin a path pattern. Returns `true` if a rule was removed.
    pub fn unpin(&self, pattern: &str) -> Result<bool> {
        self.cache.unpin(pattern)
    }

    /// List active pin rules.
    pub fn list_pins(&self) -> Result<Vec<PinRuleRecord>> {
        self.cache.list_pins()
    }

    /// Get engine status — running state, mounted backends, cache stats.
    #[must_use]
    pub fn status(&self) -> EngineStatus {
        let backends = self
            .db
            .list_backends()
            .map(|records| {
                records
                    .into_iter()
                    .map(|r| format!("{} ({})", r.display_name, r.backend_type))
                    .collect()
            })
            .unwrap_or_default();

        let cache_stats = self
            .cache
            .stats()
            .map(|s| CacheStatsSnapshot {
                online_count: s.online_count,
                cached_count: s.cached_count,
                pinned_count: s.pinned_count,
                total_bytes: s.total_bytes,
            })
            .unwrap_or_default();

        let p2p_device_id = self.p2p.as_ref().map(|b| b.device_id().to_string());

        EngineStatus {
            running: !*self.cancel_rx.borrow(),
            backends,
            cache_stats,
            p2p_enabled: self.p2p.is_some(),
            p2p_device_id,
        }
    }

    /// Get the VFS tree (for presenters to use).
    #[must_use]
    pub const fn vfs(&self) -> &Arc<RwLock<VfsTree>> {
        &self.vfs
    }

    /// Get the state database (for presenters to use).
    #[must_use]
    pub const fn db(&self) -> &Arc<StateDb> {
        &self.db
    }
}

/// Handle returned by [`Engine::start()`] for monitoring background tasks.
#[derive(Debug)]
pub struct EngineHandle {
    /// Join handle for the cache manager background task.
    pub cache_handle: tokio::task::JoinHandle<()>,
}

/// Snapshot of current engine status.
#[derive(Debug)]
pub struct EngineStatus {
    /// Whether the engine's background tasks are running.
    pub running: bool,
    /// Display strings for all mounted backends.
    pub backends: Vec<String>,
    /// Cache statistics snapshot.
    pub cache_stats: CacheStatsSnapshot,
    /// Whether P2P block sharing is enabled.
    pub p2p_enabled: bool,
    /// This device's P2P ID, if P2P is enabled.
    pub p2p_device_id: Option<String>,
}

/// Cache statistics snapshot within engine status.
#[derive(Debug, Default, Clone, Copy)]
pub struct CacheStatsSnapshot {
    /// Files in Online state (metadata only).
    pub online_count: usize,
    /// Files in Cached state (on disk, evictable).
    pub cached_count: usize,
    /// Files in Pinned state (on disk, not evictable).
    pub pinned_count: usize,
    /// Total bytes used by cached and pinned files.
    pub total_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::NullBackend;

    async fn make_test_engine() -> Engine {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.db");
        Engine::new(EngineConfig {
            db_path,
            mount_point: PathBuf::from("/tmp/test-mount"),
            backends: vec![Arc::new(NullBackend::new("test"))],
            cache_dir: None,
            enable_p2p: false,
            p2p_data_dir: None,
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn engine_new_with_null_backend() {
        let engine = make_test_engine().await;

        let status = engine.status();
        // NullBackend's display_name is "P2P Only", registered with type "unknown".
        assert!(status.backends.iter().any(|b| b.contains("P2P Only")));
        assert!(!status.p2p_enabled);
        assert!(status.p2p_device_id.is_none());
    }

    #[tokio::test]
    async fn engine_mount_unmount_backend() {
        let engine = make_test_engine().await;

        engine.mount_backend(PathBuf::from("Work"), Arc::new(NullBackend::new("work")));

        let tree = engine.vfs().read().unwrap();
        assert_eq!(tree.children().len(), 1);
        drop(tree);

        engine.unmount_backend(Path::new("Work"));

        let tree = engine.vfs().read().unwrap();
        assert!(tree.children().is_empty());
    }

    #[tokio::test]
    async fn engine_pin_unpin_list() {
        let engine = make_test_engine().await;

        engine.pin("Documents/**", true).unwrap();

        let pins = engine.list_pins().unwrap();
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0].path_glob, "Documents/**");

        let removed = engine.unpin("Documents/**").unwrap();
        assert!(removed);

        let pins = engine.list_pins().unwrap();
        assert!(pins.is_empty());
    }

    #[tokio::test]
    async fn engine_status_reflects_state() {
        let engine = make_test_engine().await;

        let status = engine.status();
        assert!(status.running);
        assert_eq!(status.backends.len(), 1);
        assert_eq!(status.cache_stats.online_count, 0);
    }

    #[tokio::test]
    async fn engine_shutdown_signals_cancel() {
        let engine = make_test_engine().await;
        engine.shutdown();

        let status = engine.status();
        assert!(!status.running);
    }

    #[tokio::test]
    async fn engine_start_and_shutdown() {
        let engine = make_test_engine().await;
        let handle = engine.start().unwrap();

        // Give the task a moment to start.
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        engine.shutdown();
        handle.cache_handle.abort();
    }

    #[tokio::test]
    async fn engine_new_requires_at_least_one_backend() {
        let dir = tempfile::tempdir().unwrap();
        let result = Engine::new(EngineConfig {
            db_path: dir.path().join("state.db"),
            mount_point: PathBuf::from("/tmp/test-mount"),
            backends: vec![],
            cache_dir: None,
            enable_p2p: false,
            p2p_data_dir: None,
        })
        .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn engine_with_multiple_backends() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::new(EngineConfig {
            db_path: dir.path().join("state.db"),
            mount_point: PathBuf::from("/tmp/test-mount"),
            backends: vec![
                Arc::new(NullBackend::new("root")),
                Arc::new(NullBackend::new("work")),
            ],
            cache_dir: None,
            enable_p2p: false,
            p2p_data_dir: None,
        })
        .await
        .unwrap();

        let tree = engine.vfs().read().unwrap();
        assert_eq!(tree.children().len(), 1);
        drop(tree);

        let status = engine.status();
        assert_eq!(status.backends.len(), 2);
    }
}
