//! Unified Cascade engine that owns and coordinates all components.
//!
//! The engine manages the lifecycle of:
//! - VFS tree (multi-backend routing)
//! - State database (file metadata, pin rules, P2P state)
//! - Sync runner (polls backends for changes)
//! - Cache manager (pin/evict/background worker)
//! - P2P bridge (optional, LAN block sharing)
//! - Config resolver (.cascade file filtering)

#[cfg(feature = "p2p")]
mod data_authority;
#[cfg(feature = "p2p")]
mod manage_dispatch;
mod operations;

#[cfg(test)]
mod tests;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::Result;
use tracing::info;

use crate::backend::{Backend, BackendFactory};
use crate::cache::manager::{CacheManager, CacheManagerConfig};
use crate::changefeed::ChangeFeed;
use crate::config::ConfigResolver;
use crate::db::{PinRuleRecord, StateDb};
use crate::manage::Grant;
#[cfg(feature = "p2p")]
use crate::manage::{DataAuthority, ManageDispatch};
#[cfg(feature = "p2p")]
use crate::p2p_bridge::P2pBridge;
use crate::portable::native::{SqliteStorage, StdFileSystem, TokioRuntimeHandle};
use crate::portable::{FileSystem, StateStorage};
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
    #[cfg(feature = "p2p")]
    pub enable_p2p: bool,
    /// P2P data directory override. `None` uses `db_path` parent + `/p2p`.
    #[cfg(feature = "p2p")]
    pub p2p_data_dir: Option<PathBuf>,
    /// Discovery reach for the engine's optimisation-layer P2P bridge.
    ///
    /// Controls how far the bridge reaches for peers when sharing blocks across
    /// cloud-backed backends. `None` uses the engine default (`private`). Only
    /// meaningful when `enable_p2p` is `true`. A pure-P2P backend (type `p2p`)
    /// carries its own posture in its per-backend TOML; this field is for the
    /// case where a cloud-backed node also wants block sharing with a specific
    /// reach.
    #[cfg(feature = "p2p")]
    pub p2p_posture: Option<cascade_p2p::DiscoveryReach>,
    /// Relay endpoint addresses for WAN NAT traversal of the optimisation-layer P2P.
    ///
    /// Each entry is a resolved `host:port` of a `cascade-relay` server. Empty
    /// means no relay strategy is provisioned for the optimisation layer. Only
    /// meaningful when `enable_p2p` is `true` and `p2p_posture` permits relay.
    #[cfg(feature = "p2p")]
    pub p2p_relay_endpoints: Vec<std::net::SocketAddr>,
    /// 32-byte HMAC shared secret for authenticating this node to the relay server.
    ///
    /// `None` means no authentication secret is configured; the relay strategy
    /// will be provisioned but the dial will be skipped. Only meaningful when
    /// `p2p_relay_endpoints` is non-empty.
    #[cfg(feature = "p2p")]
    pub p2p_relay_shared_secret: Option<[u8; 32]>,
    /// Factory used to construct backends at runtime — for example when an
    /// authorised manager pushes a `BackendAdd` command. `None` leaves the
    /// engine unable to add backends; such a request fails loudly rather than
    /// being silently dropped.
    pub backend_factory: Option<Arc<dyn BackendFactory>>,
}

impl fmt::Debug for EngineConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let backends_count = format!("[{} backend(s)]", self.backends.len());
        let mut binding = f.debug_struct("EngineConfig");
        let s = binding
            .field("db_path", &self.db_path)
            .field("mount_point", &self.mount_point)
            .field("backends", &backends_count)
            .field("cache_dir", &self.cache_dir);
        #[cfg(feature = "p2p")]
        {
            let relay_count = format!("[{} endpoint(s)]", self.p2p_relay_endpoints.len());
            s.field("enable_p2p", &self.enable_p2p)
                .field("p2p_data_dir", &self.p2p_data_dir)
                .field("p2p_posture", &self.p2p_posture)
                .field("p2p_relay_endpoints", &relay_count)
                .field(
                    "p2p_relay_shared_secret",
                    &self.p2p_relay_shared_secret.is_some(),
                );
        }
        s.field("backend_factory", &self.backend_factory.is_some())
            .finish()
    }
}

/// Unified Cascade engine that owns and coordinates all components.
///
/// The engine holds both the native [`StateDb`] (for direct synchronous access
/// by external crates and the management plane) and a portable [`StateStorage`]
/// trait object (for use by the cache manager and sync runner, which are
/// portable). Cancellation uses an `AtomicBool` flag — runtime-agnostic and
/// observable by both native and portable workers.
pub struct Engine {
    /// Native state database — used directly by operations, management plane,
    /// and external crates via [`Engine::db`].
    db: Arc<StateDb>,
    /// Portable state storage — wraps the same `StateDb` behind the trait.
    storage: Arc<dyn StateStorage>,
    /// Portable filesystem adapter.
    fs: Arc<dyn FileSystem>,
    /// Runtime handle for spawning and timers.
    runtime: TokioRuntimeHandle,
    vfs: Arc<RwLock<VfsTree>>,
    cache: CacheManager<TokioRuntimeHandle>,
    config: Arc<ConfigResolver>,
    #[cfg(feature = "p2p")]
    p2p: Option<P2pBridge>,
    /// Engine-side per-parent change index. Fed by the sync runner and
    /// read by presenters that serve `enumerateChanges`-style deltas.
    change_feed: Arc<ChangeFeed>,
    /// Factory for constructing backends at runtime (the `BackendAdd` path).
    /// `None` when the host did not inject one.
    backend_factory: Option<Arc<dyn BackendFactory>>,
    /// Shared cancellation flag. Workers poll this; setting it to `true`
    /// quiesces the cache manager, sync runner, and any other subscriber.
    cancel: Arc<AtomicBool>,
}

impl fmt::Debug for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut binding = f.debug_struct("Engine");
        #[cfg(feature = "p2p")]
        {
            binding.field("p2p_enabled", &self.p2p.is_some());
        }
        binding.finish_non_exhaustive()
    }
}

impl Engine {
    /// Create and initialise a new engine.
    ///
    /// Opens the state database, creates the VFS tree, registers backends,
    /// and wires up the cache manager and optional P2P bridge.
    pub fn new(config: EngineConfig) -> Result<Self> {
        let backends = config.backends;
        let backend_factory = config.backend_factory;
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

        // Portable adapters wrapping the native StateDb.
        let runtime = TokioRuntimeHandle::current();
        let storage: Arc<dyn StateStorage> =
            Arc::new(SqliteStorage::new(db.clone(), runtime.clone()));
        let fs: Arc<dyn FileSystem> = Arc::new(StdFileSystem);

        // Cache manager backed by portable storage.
        let cache = CacheManager::new(
            storage.clone(),
            runtime.clone(),
            CacheManagerConfig::default(),
        );

        // P2P bridge (optional).
        #[cfg(feature = "p2p")]
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
            let bridge_config = crate::p2p_bridge::P2pBridgeConfig {
                posture: config.p2p_posture,
                relay_endpoints: config.p2p_relay_endpoints,
                relay_shared_secret: config.p2p_relay_shared_secret,
            };
            if let Some(posture) = bridge_config.posture {
                info!(?posture, "P2P bridge posture set");
            }
            Some(P2pBridge::with_config(
                p2p_engine,
                db.clone(),
                bridge_config,
            ))
        } else {
            None
        };

        let cancel = Arc::new(AtomicBool::new(false));

        Ok(Self {
            db,
            storage,
            fs,
            runtime,
            vfs,
            cache,
            config: config_resolver,
            #[cfg(feature = "p2p")]
            p2p,
            change_feed: Arc::new(ChangeFeed::new()),
            backend_factory,
            cancel,
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

    /// Wire the management-plane dispatch port and the data-plane authority port
    /// into every backend that serves a peer transport.
    ///
    /// The engine is the production [`ManageDispatch`] and [`DataAuthority`]
    /// implementation — it owns the grant store, the audit log, the command
    /// executor, and the data-grant ACL. A backend that runs its own peer
    /// transport (the P2P backend) receives inbound `ManageRequest` frames
    /// (authorised, audited, and executed through [`ManageDispatch`]) and gates
    /// serving/accepting index and blocks on the [`DataAuthority`] decision.
    /// Backends with no transport ignore both calls (the trait defaults are
    /// no-ops).
    ///
    /// Called once at daemon startup, after the engine is constructed and before
    /// its presenter begins accepting connections. Takes `self: &Arc<Self>` so
    /// the engine can hand a clone of itself, as an `Arc<dyn ManageDispatch>` and
    /// an `Arc<dyn DataAuthority>`, to each backend.
    #[cfg(feature = "p2p")]
    pub async fn wire_manage_dispatch(self: &Arc<Self>) {
        let dispatch: Arc<dyn ManageDispatch> = self.clone();
        let authority: Arc<dyn DataAuthority> = self.clone();
        let backends: Vec<Arc<dyn Backend>> = {
            let tree = self
                .vfs
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut backends = vec![tree.root().clone()];
            for (_, backend) in tree.children() {
                backends.push(backend.clone());
            }
            backends
        };
        for backend in backends {
            backend.set_manage_dispatch(dispatch.clone()).await;
            backend.set_data_authority(authority.clone()).await;
        }
    }

    /// Create a sync runner for polling all registered backends.
    ///
    /// The caller provides the presenter (typically the platform presenter —
    /// NFS, FUSE, etc.) and is responsible for spawning the runner as a
    /// background task.
    ///
    /// The runner receives a clone of the engine's cancellation flag, so
    /// [`Engine::shutdown`] / [`Engine::stop`] stop the sync loop along with the
    /// cache worker.
    pub fn create_sync_runner(
        &self,
        presenter: Arc<dyn VfsPresenter>,
    ) -> SyncRunner<TokioRuntimeHandle> {
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

        SyncRunner::new(
            self.storage.clone(),
            self.fs.clone(),
            self.runtime.clone(),
            backends,
            presenter,
            self.config.clone(),
        )
        .with_change_feed(self.change_feed.clone())
    }

    /// The engine's shared cancellation flag.
    ///
    /// Callers that spawn the sync runner (or any other background worker)
    /// should clone this flag and pass it to the worker's run method.
    #[must_use]
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    /// Start the engine's background tasks (cache manager).
    ///
    /// The sync runner is not started here — use [`Engine::create_sync_runner`]
    /// to build one with the real presenter, then spawn it yourself.
    pub fn start(&self) -> Result<EngineHandle> {
        let cancel = self.cancel.clone();
        let cache = CacheManager::new(
            self.storage.clone(),
            self.runtime.clone(),
            CacheManagerConfig::default(),
        );
        let cache_handle = tokio::spawn(async move {
            cache.run_with_flag(cancel).await;
        });

        info!("engine started");

        Ok(EngineHandle { cache_handle })
    }

    /// Graceful shutdown — signals all background workers to stop.
    ///
    /// Sets the shared cancellation flag that the cache-manager task spawned
    /// by [`Engine::start`] and the sync runner created by
    /// [`Engine::create_sync_runner`] both observe. Both quiesce on the next
    /// loop iteration.
    pub fn shutdown(&self) {
        info!("engine shutting down");
        self.cancel.store(true, Ordering::SeqCst);
        info!("engine shutdown complete");
    }

    /// Pin a path pattern. All files matching the glob will be kept offline.
    pub async fn pin(&self, pattern: &str, recursive: bool) -> Result<()> {
        self.cache.pin(pattern, recursive).await
    }

    /// Unpin a path pattern. Returns `true` if a rule was removed.
    pub async fn unpin(&self, pattern: &str) -> Result<bool> {
        self.cache.unpin(pattern).await
    }

    /// List active pin rules.
    pub async fn list_pins(&self) -> Result<Vec<PinRuleRecord>> {
        self.cache.list_pins().await
    }

    /// Pre-warm a path glob so matching files are fetched on the next sync.
    ///
    /// Warming is a recursive pin: the same operation the local `cascade cache
    /// warm` command performs. There is deliberately no separate warm path in
    /// the cache manager — warming *is* pinning, and the background worker
    /// materialises the pinned files.
    pub async fn warm(&self, pattern: &str) -> Result<()> {
        self.cache.pin(pattern, true).await
    }

    /// Add a max file length rule.
    ///
    /// Files matching `path_glob` that exceed `max_bytes` will be skipped during
    /// sync. Rules are ordered by `priority` (higher wins). An optional
    /// `conditions` expression is evaluated against the engine's `EvalContext`.
    pub fn add_max_file_length_rule(
        &self,
        path_glob: &str,
        max_bytes: u64,
        conditions: Option<&str>,
    ) -> Result<()> {
        operations::add_max_file_length_rule(self, path_glob, max_bytes, 0, conditions)
    }

    /// List all max file length rules, ordered by priority descending.
    pub fn list_max_file_length_rules(&self) -> Result<Vec<crate::db::MaxFileLengthRecord>> {
        operations::list_max_file_length_rules(self)
    }

    /// Remove a max file length rule by id. Returns `true` if a row was removed.
    pub fn remove_max_file_length_rule(&self, id: i64) -> Result<bool> {
        operations::remove_max_file_length_rule(self, id)
    }

    /// Merge a parsed `.cascade` fragment rooted at `folder` into the node's
    /// rule set.
    ///
    /// The fragment's pin and lifecycle rules are applied to the state database
    /// exactly as the equivalent local config-merge path would: each pin path
    /// becomes a recursive pin rule, and each lifecycle policy is inserted with
    /// its parsed age/size bounds.
    ///
    /// **Scope confinement is load-bearing for authorisation.** The dispatcher
    /// authorises a `ConfigPush` solely over its declared `folder`, so every
    /// rule the fragment carries must land inside that subtree — otherwise a
    /// manager authorised only over `/work` could push `[[pin]] path =
    /// "/personal"` and pin content it has no grant over. Each rule path is
    /// therefore rooted under `folder` (an absolute rule path is treated as
    /// relative to it, never as already node-absolute) and then re-checked:
    /// the resolved, normalised path must be covered by `Scope::folder(folder)`.
    /// A rule that escapes the authorised subtree — for example via a `..`
    /// traversal that climbs out — fails the whole push loudly, and **nothing**
    /// is applied. The escape check is performed up front so a later rule
    /// escaping cannot leave earlier rules already written to the database.
    pub fn config_push(
        &self,
        folder: &str,
        config: &cascade_config::CascadeConfig,
    ) -> Result<String> {
        operations::config_push(self, folder, config)
    }

    /// Set a single lifecycle policy on the node.
    ///
    /// Delegates to the same `add_lifecycle_policy` state-database operation the
    /// local config-merge path uses, so a pushed policy behaves identically to
    /// one declared in a `.cascade` file.
    pub fn policy_set(
        &self,
        path_glob: &str,
        max_age_secs: Option<i64>,
        max_file_size: Option<i64>,
        priority: i32,
    ) -> Result<String> {
        operations::policy_set(self, path_glob, max_age_secs, max_file_size, priority)
    }

    /// Register and mount a backend at runtime.
    ///
    /// Builds the backend through the injected [`BackendFactory`] (the same
    /// per-type `create_backend` factories the local daemon uses), registers it
    /// in the state database, and mounts it into the live VFS tree at
    /// `mount_path`. Fails loudly when no factory was injected rather than
    /// silently dropping the request.
    pub fn backend_add(
        &self,
        name: &str,
        backend_type: &str,
        mount_path: &str,
        config_toml: &str,
    ) -> Result<String> {
        operations::backend_add(self, name, backend_type, mount_path, config_toml)
    }

    /// Unmount and deregister a backend by name.
    ///
    /// Unmounts it from the live VFS tree and removes its state-database row,
    /// the inverse of [`Engine::backend_add`].
    pub fn backend_remove(&self, name: &str, mount_path: &str) -> Result<String> {
        operations::backend_remove(self, name, mount_path)
    }

    /// Restart the engine's cache-manager worker.
    ///
    /// Signals the current workers to stop, re-arms the cancellation flag,
    /// and spawns a fresh cache-manager task; the returned handle owns it.
    ///
    /// The sync runner is **not** revived here. It is created and spawned by the
    /// daemon (it needs the platform presenter, which the engine does not own),
    /// and once its `run()` future observes the stop flag it returns and cannot
    /// be respawned from inside the engine. Reviving the full worker set requires
    /// a daemon-level process restart. The re-arm therefore un-cancels the
    /// flag so the freshly spawned cache worker does not see a stale shutdown
    /// signal, but a sync runner that already returned stays stopped.
    pub fn restart(&self) -> Result<EngineHandle> {
        self.cancel.store(true, Ordering::SeqCst);
        // Re-arm: the freshly spawned worker reads `false` (running) so it
        // does not see a stale shutdown signal.
        self.cancel.store(false, Ordering::SeqCst);
        self.start()
    }

    /// Stop the engine's background workers — an alias of [`Engine::shutdown`]
    /// returning a summary for the management plane.
    ///
    /// This signals both the cache-manager task and the sync runner (both
    /// observe the engine's cancellation flag) to quiesce.
    #[must_use]
    pub fn stop(&self) -> String {
        self.shutdown();
        "daemon background workers (cache eviction and backend sync) signalled to stop".to_owned()
    }

    /// Insert a capability grant into the store.
    ///
    /// Delegates to the same `insert_grant` operation the local grant store
    /// uses. Authorisation and the subset/escalation check are the dispatcher's
    /// responsibility — this is the raw state mutation.
    pub fn grant_add(&self, grant: &Grant) -> Result<String> {
        let id = self.db.insert_grant(grant)?;
        Ok(format!(
            "grant {id} added: {} over {:?} for {}",
            grant.capability.as_wire(),
            grant.scope,
            grant.grantee,
        ))
    }

    /// Revoke a grant by its row id.
    pub fn grant_revoke(&self, grant_id: i64) -> Result<String> {
        let removed = self.db.revoke_grant(grant_id)?;
        Ok(if removed {
            format!("grant {grant_id} revoked")
        } else {
            format!("no grant with id {grant_id}")
        })
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

        // Cache stats are async on the portable path. For the synchronous
        // status snapshot, read directly from the native DB.
        let online = self
            .db
            .list_files_by_cache_state(crate::types::CacheState::Online);
        let cached = self
            .db
            .list_files_by_cache_state(crate::types::CacheState::Cached);
        let pinned = self
            .db
            .list_files_by_cache_state(crate::types::CacheState::Pinned);
        let total_size = self.db.cache_size();

        let cache_stats = match (online, cached, pinned, total_size) {
            (Ok(o), Ok(c), Ok(p), Ok(t)) => CacheStatsSnapshot {
                online_count: o.len(),
                cached_count: c.len(),
                pinned_count: p.len(),
                total_bytes: u64::try_from(t).unwrap_or(0),
            },
            _ => CacheStatsSnapshot::default(),
        };

        #[cfg(feature = "p2p")]
        let (p2p_enabled, p2p_device_id) = {
            let id = self.p2p.as_ref().map(|b| b.device_id().to_string());
            (id.is_some(), id)
        };
        #[cfg(not(feature = "p2p"))]
        let (p2p_enabled, p2p_device_id) = (false, None);

        EngineStatus {
            running: !self.cancel.load(Ordering::SeqCst),
            backends,
            cache_stats,
            p2p_enabled,
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

    /// This node's full device identity — certificate and private key — when a
    /// P2P backend is configured.
    ///
    /// The management plane (and the HTTP API that reuses its auth machinery)
    /// signs capability tokens with this identity, the same key a token's
    /// delegation chain roots in. `None` when no P2P backend is configured, in
    /// which case the node has no device identity to sign or verify against.
    #[must_use]
    #[cfg(feature = "p2p")]
    pub fn device_identity(&self) -> Option<&cascade_p2p::identity::DeviceIdentity> {
        self.p2p.as_ref().map(|bridge| bridge.engine().identity())
    }

    /// Snapshot the F2 explicit-control bit. Returns a map keyed by
    /// `(peer_device, folder_id)` with the per-direction state observed on
    /// the last successful token verify. Exists for the F2 integration
    /// test to assert the in-memory mirror reflects the durable state
    /// the engine just observed.
    #[must_use]
    pub fn explicit_data_control_snapshot(
        &self,
    ) -> std::collections::HashMap<(String, String), (bool, bool)> {
        let Ok(rows) = self.db.list_data_explicit_control() else {
            return std::collections::HashMap::new();
        };
        rows.into_iter()
            .map(|r| ((r.peer_device, r.folder_id), (r.data_read, r.data_write)))
            .collect()
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
