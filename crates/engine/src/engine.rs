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

use async_trait::async_trait;
use cascade_p2p::protocol::{ManageCommand, ManageResult, ManageScope as WireScope};
use chrono::{DateTime, Utc};

use crate::backend::Backend;
use crate::cache::manager::{CacheManager, CacheManagerConfig};
use crate::changefeed::ChangeFeed;
use crate::config::ConfigResolver;
use crate::db::{AuditEntry, PinRuleRecord, StateDb};
use crate::manage::{
    DeviceId, Grant, ManageCommandExecutor, ManageDispatch, ManageGrantStore, run_dispatch,
};
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
    /// Engine-side per-parent change index. Fed by the sync runner and
    /// read by presenters that serve `enumerateChanges`-style deltas.
    change_feed: Arc<ChangeFeed>,
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
    pub fn new(config: EngineConfig) -> Result<Self> {
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
            let p2p_engine = cascade_p2p::P2pEngine::new(&p2p_dir)?;
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
            change_feed: Arc::new(ChangeFeed::new()),
            cancel,
            cancel_rx,
        })
    }

    /// The engine's shared per-parent change index.
    ///
    /// Presenters that serve `enumerateChanges`-style deltas (the macOS
    /// File Provider bridge today) read per-parent change events from this
    /// feed. It is populated by the sync runner created via
    /// [`Engine::create_sync_runner`].
    #[must_use]
    pub fn change_feed(&self) -> Arc<ChangeFeed> {
        self.change_feed.clone()
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
            .with_change_feed(self.change_feed.clone())
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

/// The engine is the grant store and audit sink for the management plane,
/// reading and writing the two `state.db` tables the prior phase added.
impl ManageGrantStore for Engine {
    fn manage_grants(&self) -> Result<Vec<Grant>> {
        Ok(self
            .db
            .list_grants()?
            .into_iter()
            .map(|record| record.grant)
            .collect())
    }

    fn manage_append_audit(&self, entry: &AuditEntry) -> Result<()> {
        self.db.append_audit(entry).map(|_id| ())
    }
}

/// The engine is the command executor for the management plane. Each method is
/// the *same* operation the local CLI drives — `pin`, `unpin`, `status`, and a
/// cache eviction sweep — so a manager can never do more than the daemon could
/// do to itself, and no command logic is duplicated. The remote path reaches
/// these only after authorisation and auditing in [`run_dispatch`].
#[async_trait]
impl ManageCommandExecutor for Engine {
    async fn manage_status(&self) -> Result<String> {
        let status = self.status();
        Ok(format!(
            "running={} backends={} online={} cached={} pinned={} p2p_enabled={}",
            status.running,
            status.backends.len(),
            status.cache_stats.online_count,
            status.cache_stats.cached_count,
            status.cache_stats.pinned_count,
            status.p2p_enabled,
        ))
    }

    async fn manage_pin(&self, path_glob: &str, recursive: bool) -> Result<String> {
        self.pin(path_glob, recursive)?;
        Ok(format!("pinned {path_glob} (recursive={recursive})"))
    }

    async fn manage_unpin(&self, path_glob: &str) -> Result<String> {
        let removed = self.unpin(path_glob)?;
        Ok(if removed {
            format!("unpinned {path_glob}")
        } else {
            format!("no pin rule matched {path_glob}")
        })
    }

    async fn manage_cache_evict(&self) -> Result<String> {
        let report = self.cache.evict()?;
        Ok(format!(
            "evicted {} files ({} lifecycle, {} size), freed {} bytes",
            report.total_evicted(),
            report.lifecycle_evicted,
            report.size_evicted,
            report.bytes_freed,
        ))
    }
}

#[async_trait]
impl ManageDispatch for Engine {
    async fn dispatch(
        &self,
        caller: &DeviceId,
        command: ManageCommand,
        scope: WireScope,
        now: DateTime<Utc>,
    ) -> ManageResult {
        run_dispatch(self, self, caller, command, scope, now).await
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

    fn make_test_engine() -> Engine {
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
        .unwrap()
    }

    #[tokio::test]
    async fn engine_new_with_null_backend() {
        let engine = make_test_engine();

        let status = engine.status();
        // NullBackend's display_name is "P2P Only", registered with type "unknown".
        assert!(status.backends.iter().any(|b| b.contains("P2P Only")));
        assert!(!status.p2p_enabled);
        assert!(status.p2p_device_id.is_none());
    }

    #[tokio::test]
    async fn engine_mount_unmount_backend() {
        let engine = make_test_engine();

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
        let engine = make_test_engine();

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
        let engine = make_test_engine();

        let status = engine.status();
        assert!(status.running);
        assert_eq!(status.backends.len(), 1);
        assert_eq!(status.cache_stats.online_count, 0);
    }

    #[tokio::test]
    async fn engine_shutdown_signals_cancel() {
        let engine = make_test_engine();
        engine.shutdown();

        let status = engine.status();
        assert!(!status.running);
    }

    #[tokio::test]
    async fn engine_start_and_shutdown() {
        let engine = make_test_engine();
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
        });

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
        .unwrap();

        let tree = engine.vfs().read().unwrap();
        assert_eq!(tree.children().len(), 1);
        drop(tree);

        let status = engine.status();
        assert_eq!(status.backends.len(), 2);
    }

    // ── Management-plane dispatch against the real engine + DB ──

    use crate::manage::{Capability, Scope};
    use chrono::Utc;

    fn manager_id() -> DeviceId {
        DeviceId::new("MANAGER")
    }

    #[tokio::test]
    async fn dispatch_authorised_pin_mutates_state_and_audits() {
        let engine = make_test_engine();
        // Grant the manager pin:write over /work, persisted in the real DB.
        engine
            .db()
            .insert_grant(&Grant {
                grantee: manager_id(),
                capability: Capability::PinWrite,
                scope: Scope::folder("/work"),
                granted_by: DeviceId::new("OWNER"),
                expires: None,
            })
            .unwrap();

        let result = engine
            .dispatch(
                &manager_id(),
                ManageCommand::Pin {
                    path_glob: "/work/reports".to_owned(),
                    recursive: true,
                },
                WireScope::Folder {
                    path: "/work/reports".to_owned(),
                },
                Utc::now(),
            )
            .await;

        assert!(
            matches!(result, ManageResult::Ok { .. }),
            "authorised pin should succeed, got {result:?}",
        );
        // The side effect ran: the pin rule is now present.
        let pins = engine.list_pins().unwrap();
        assert!(
            pins.iter().any(|p| p.path_glob == "/work/reports"),
            "pin rule must have been recorded",
        );
        // The attempt was audited as allowed.
        let audit = engine.db().list_audit().unwrap();
        assert_eq!(audit.len(), 1);
        let row = audit.first().expect("one audit row");
        assert_eq!(row.entry.outcome, "allowed");
        assert_eq!(row.entry.actor_device, manager_id());
        assert_eq!(row.entry.capability, Capability::PinWrite);
    }

    #[tokio::test]
    async fn dispatch_pin_outside_granted_scope_is_refused_against_real_engine() {
        // Scope-escape regression against the live Engine + real DB: the
        // manager holds pin:write over /work and advertises a wire scope of
        // /work, but the command's path_glob targets /personal. The pin must be
        // refused and no rule may land in the database.
        let engine = make_test_engine();
        engine
            .db()
            .insert_grant(&Grant {
                grantee: manager_id(),
                capability: Capability::PinWrite,
                scope: Scope::folder("/work"),
                granted_by: DeviceId::new("OWNER"),
                expires: None,
            })
            .unwrap();

        let result = engine
            .dispatch(
                &manager_id(),
                ManageCommand::Pin {
                    path_glob: "/personal/secret".to_owned(),
                    recursive: false,
                },
                WireScope::Folder {
                    path: "/work".to_owned(),
                },
                Utc::now(),
            )
            .await;

        assert!(
            matches!(
                result,
                ManageResult::Err {
                    kind: cascade_p2p::protocol::ManageErrorKind::Unauthorised,
                    ..
                }
            ),
            "a pin escaping the granted scope must be refused, got {result:?}",
        );
        assert!(
            engine.list_pins().unwrap().is_empty(),
            "no pin rule may be created for a path outside the granted scope",
        );
        let audit = engine.db().list_audit().unwrap();
        assert_eq!(audit.len(), 1, "the denial is still audited");
        assert_eq!(
            audit.first().map(|r| r.entry.outcome.as_str()),
            Some("denied"),
        );
    }

    #[tokio::test]
    async fn dispatch_unauthorised_pin_makes_no_change_and_audits_denial() {
        let engine = make_test_engine();
        // Manager holds only status:read — a pin must be refused.
        engine
            .db()
            .insert_grant(&Grant {
                grantee: manager_id(),
                capability: Capability::StatusRead,
                scope: Scope::Node,
                granted_by: DeviceId::new("OWNER"),
                expires: None,
            })
            .unwrap();

        let result = engine
            .dispatch(
                &manager_id(),
                ManageCommand::Pin {
                    path_glob: "/work".to_owned(),
                    recursive: false,
                },
                WireScope::Folder {
                    path: "/work".to_owned(),
                },
                Utc::now(),
            )
            .await;

        assert!(
            matches!(
                result,
                ManageResult::Err {
                    kind: cascade_p2p::protocol::ManageErrorKind::Unauthorised,
                    ..
                }
            ),
            "unauthorised pin must be refused, got {result:?}",
        );
        // No pin rule was created.
        assert!(
            engine.list_pins().unwrap().is_empty(),
            "an unauthorised request must not mutate state",
        );
        // The denial was still audited.
        let audit = engine.db().list_audit().unwrap();
        assert_eq!(audit.len(), 1);
        assert_eq!(
            audit.first().map(|r| r.entry.outcome.as_str()),
            Some("denied"),
        );
    }

    #[tokio::test]
    async fn dispatch_status_read_is_authorised_by_node_scope() {
        let engine = make_test_engine();
        engine
            .db()
            .insert_grant(&Grant {
                grantee: manager_id(),
                capability: Capability::StatusRead,
                scope: Scope::Node,
                granted_by: DeviceId::new("OWNER"),
                expires: None,
            })
            .unwrap();

        let result = engine
            .dispatch(
                &manager_id(),
                ManageCommand::StatusRead,
                WireScope::Node,
                Utc::now(),
            )
            .await;
        match result {
            ManageResult::Ok { summary } => {
                assert!(summary.contains("running="), "status summary: {summary}");
            }
            err @ ManageResult::Err { .. } => {
                panic!("status read should be authorised, got {err:?}")
            }
        }
    }
}
