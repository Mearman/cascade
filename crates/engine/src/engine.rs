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
use cascade_p2p::protocol::{
    ManageCommand, ManageConfigFormat, ManageResult, ManageScope as WireScope,
};
use chrono::{DateTime, Utc};

use crate::backend::{Backend, BackendFactory};
use crate::cache::manager::{CacheManager, CacheManagerConfig};
use crate::changefeed::ChangeFeed;
use crate::config::ConfigResolver;
use crate::db::{AuditEntry, PinRuleRecord, QuarantineRecord, StateDb};
use crate::manage::{
    DataAccess, DataAuthority, DeviceId, ExplicitControlState, Grant, ManageCommandExecutor,
    ManageDispatch, ManageGrantStore, Scope, data_access_with_explicit_control, run_dispatch,
    verify_data_token,
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
    /// Discovery reach for the engine's optimisation-layer P2P bridge.
    ///
    /// Controls how far the bridge reaches for peers when sharing blocks across
    /// cloud-backed backends. `None` uses the engine default (`private`). Only
    /// meaningful when `enable_p2p` is `true`. A pure-P2P backend (type `p2p`)
    /// carries its own posture in its per-backend TOML; this field is for the
    /// case where a cloud-backed node also wants block sharing with a specific
    /// reach.
    pub p2p_posture: Option<cascade_p2p::DiscoveryReach>,
    /// Relay endpoint addresses for WAN NAT traversal of the optimisation-layer P2P.
    ///
    /// Each entry is a resolved `host:port` of a `cascade-relay` server. Empty
    /// means no relay strategy is provisioned for the optimisation layer. Only
    /// meaningful when `enable_p2p` is `true` and `p2p_posture` permits relay.
    pub p2p_relay_endpoints: Vec<std::net::SocketAddr>,
    /// 32-byte HMAC shared secret for authenticating this node to the relay server.
    ///
    /// `None` means no authentication secret is configured; the relay strategy
    /// will be provisioned but the dial will be skipped. Only meaningful when
    /// `p2p_relay_endpoints` is non-empty.
    pub p2p_relay_shared_secret: Option<[u8; 32]>,
    /// Factory used to construct backends at runtime — for example when an
    /// authorised manager pushes a `BackendAdd` command. `None` leaves the
    /// engine unable to add backends; such a request fails loudly rather than
    /// being silently dropped.
    pub backend_factory: Option<Arc<dyn BackendFactory>>,
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
            .field("p2p_posture", &self.p2p_posture)
            .field(
                "p2p_relay_endpoints",
                &format!("[{} endpoint(s)]", self.p2p_relay_endpoints.len()),
            )
            .field(
                "p2p_relay_shared_secret",
                &self.p2p_relay_shared_secret.is_some(),
            )
            .field("backend_factory", &self.backend_factory.is_some())
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
    /// Factory for constructing backends at runtime (the `BackendAdd` path).
    /// `None` when the host did not inject one.
    backend_factory: Option<Arc<dyn BackendFactory>>,
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

        let (cancel, cancel_rx) = watch::channel(false);

        Ok(Self {
            db,
            vfs,
            cache,
            config: config_resolver,
            p2p,
            change_feed: Arc::new(ChangeFeed::new()),
            backend_factory,
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
    /// The runner subscribes to the engine's cancellation channel, so
    /// [`Engine::shutdown`] / [`Engine::stop`] stop the sync loop along with the
    /// cache worker. The runner observes a `false → true` edge on this channel,
    /// so it must be created while the channel reads `false` (running) — which it
    /// always does immediately after [`Engine::new`] and after a
    /// [`Engine::restart`] re-arm.
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

        SyncRunner::new(
            self.db.clone(),
            backends,
            presenter,
            self.config.clone(),
            self.cancel.subscribe(),
        )
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

    /// Graceful shutdown — signals all background workers to stop.
    ///
    /// Sends the cancellation edge every worker subscribed to the engine's
    /// channel observes: the cache-manager task spawned by [`Engine::start`] and
    /// the sync runner created by [`Engine::create_sync_runner`] (which
    /// subscribes this same channel). Both quiesce on the next loop iteration.
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

    /// Pre-warm a path glob so matching files are fetched on the next sync.
    ///
    /// Warming is a recursive pin: the same operation the local `cascade cache
    /// warm` command performs. There is deliberately no separate warm path in
    /// the cache manager — warming *is* pinning, and the background worker
    /// materialises the pinned files.
    pub fn warm(&self, pattern: &str) -> Result<()> {
        self.cache.pin(pattern, true)
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
        let folder_scope = Scope::folder(folder.to_owned());

        // Resolve and confine every rule path before applying any, so a single
        // escaping rule rejects the entire push without partial application.
        let pin_paths = config
            .pin
            .iter()
            .map(|pin| confine_rule_path(folder, &pin.path, &folder_scope))
            .collect::<Result<Vec<_>>>()?;

        let policy_inputs = config
            .lifecycle
            .iter()
            .map(|policy| {
                let path = confine_rule_path(folder, &policy.path, &folder_scope)?;
                let max_age = policy
                    .max_age
                    .as_deref()
                    .map(parse_duration_secs)
                    .transpose()?;
                let max_file_size = policy
                    .max_file_size
                    .as_deref()
                    .map(parse_size_bytes)
                    .transpose()?;
                Ok::<_, anyhow::Error>((path, max_age, max_file_size, policy.priority))
            })
            .collect::<Result<Vec<_>>>()?;

        let pins_applied = pin_paths.len();
        for path in pin_paths {
            self.db.add_pin_rule(&path, true, None)?;
        }

        let policies_applied = policy_inputs.len();
        for (path, max_age, max_file_size, priority) in policy_inputs {
            self.db
                .add_lifecycle_policy(&path, max_age, max_file_size, priority, None)?;
        }

        Ok(format!(
            "config push into {folder}: {pins_applied} pin rule(s), {policies_applied} lifecycle policy/policies applied",
        ))
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
        self.db
            .add_lifecycle_policy(path_glob, max_age_secs, max_file_size, priority, None)?;
        Ok(format!("lifecycle policy set for {path_glob}"))
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
        let factory = self.backend_factory.as_ref().ok_or_else(|| {
            anyhow::anyhow!("this node cannot add backends: no backend factory is configured")
        })?;
        let backend = factory.create(name, backend_type, config_toml)?;
        self.db.register_backend(
            name,
            backend_type,
            backend.display_name(),
            Some(mount_path),
            Some(config_toml),
        )?;
        self.mount_backend(PathBuf::from(mount_path), backend);
        Ok(format!(
            "backend {name} ({backend_type}) added at {mount_path}",
        ))
    }

    /// Unmount and deregister a backend by name.
    ///
    /// Unmounts it from the live VFS tree and removes its state-database row,
    /// the inverse of [`Engine::backend_add`].
    pub fn backend_remove(&self, name: &str, mount_path: &str) -> Result<String> {
        self.unmount_backend(Path::new(mount_path));
        let removed = self.db.remove_backend(name)?;
        Ok(if removed {
            format!("backend {name} removed from {mount_path}")
        } else {
            format!("no backend named {name} was registered")
        })
    }

    /// Restart the engine's cache-manager worker.
    ///
    /// Signals the current workers to stop, re-arms the cancellation channel,
    /// and spawns a fresh cache-manager task; the returned handle owns it.
    ///
    /// The sync runner is **not** revived here. It is created and spawned by the
    /// daemon (it needs the platform presenter, which the engine does not own),
    /// and once its `run()` future observes the stop edge it returns and cannot
    /// be respawned from inside the engine. Reviving the full worker set requires
    /// a daemon-level process restart. The re-arm therefore un-cancels the
    /// channel so the freshly spawned cache worker does not see a stale shutdown
    /// signal, but a sync runner that already returned stays stopped.
    pub fn restart(&self) -> Result<EngineHandle> {
        let _ = self.cancel.send(true);
        // Re-arm: a freshly subscribed receiver reads `false` (running) so the
        // new cache worker does not see a stale shutdown signal.
        let _ = self.cancel.send(false);
        self.start()
    }

    /// Stop the engine's background workers — an alias of [`Engine::shutdown`]
    /// returning a summary for the management plane.
    ///
    /// This signals both the cache-manager task and the sync runner (both
    /// subscribe the engine's cancellation channel) to quiesce.
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

    fn manage_grant_scope(&self, grant_id: i64) -> Result<Option<Scope>> {
        self.db.grant_scope(grant_id)
    }

    fn manage_append_audit(&self, entry: &AuditEntry) -> Result<()> {
        self.db.append_audit(entry).map(|_id| ())
    }

    fn manage_node_device_id(&self) -> Result<DeviceId> {
        // The management plane's identity is the P2P data-plane device identity —
        // the same key a capability token's chain roots in. Without a configured
        // P2P backend the node has no device identity, so token verification
        // fails loudly rather than rooting a chain in a placeholder.
        let id = self
            .p2p
            .as_ref()
            .map(|b| b.device_id().to_owned())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no P2P backend configured — the node has no device identity to verify a \
                 capability token against"
                )
            })?;
        Ok(DeviceId::new(id))
    }

    fn manage_revoked_token_ids(&self) -> Result<std::collections::HashSet<String>> {
        self.db.revoked_token_ids()
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

    async fn manage_cache_warm(&self, path_glob: &str) -> Result<String> {
        self.warm(path_glob)?;
        Ok(format!("warmed {path_glob}"))
    }

    async fn manage_config_push(
        &self,
        format: ManageConfigFormat,
        folder: &str,
        body: &str,
    ) -> Result<String> {
        let config = parse_config_fragment(format, body)?;
        self.config_push(folder, &config)
    }

    async fn manage_policy_set(
        &self,
        path_glob: &str,
        max_age_secs: Option<i64>,
        max_file_size: Option<i64>,
        priority: i32,
    ) -> Result<String> {
        self.policy_set(path_glob, max_age_secs, max_file_size, priority)
    }

    async fn manage_backend_add(
        &self,
        name: &str,
        backend_type: &str,
        mount_path: &str,
        config_toml: &str,
    ) -> Result<String> {
        self.backend_add(name, backend_type, mount_path, config_toml)
    }

    async fn manage_backend_remove(&self, name: &str, mount_path: &str) -> Result<String> {
        self.backend_remove(name, mount_path)
    }

    async fn manage_restart(&self) -> Result<String> {
        // The returned handle owns the freshly spawned cache-manager task. The
        // daemon keeps the engine alive past this call, so detaching the handle
        // lets the new worker run for the daemon's lifetime exactly as the
        // initial `start()` handle does. The sync runner is owned by the daemon
        // and not revived here — see `Engine::restart`.
        let _handle = self.restart()?;
        Ok("daemon cache-manager worker restarted (sync runner unaffected — full restart requires restarting the daemon)".to_owned())
    }

    async fn manage_stop(&self) -> Result<String> {
        Ok(self.stop())
    }

    async fn manage_grant_add(&self, grant: &Grant) -> Result<String> {
        self.grant_add(grant)
    }

    async fn manage_grant_revoke(&self, grant_id: i64) -> Result<String> {
        self.grant_revoke(grant_id)
    }
}

/// Parse a pushed `.cascade` config fragment in `format` into a
/// [`CascadeConfig`](cascade_config::CascadeConfig).
///
/// Routes to the matching `cascade-config` parser for the wire format. The
/// gitignore parser is infallible; the structured parsers surface a parse error
/// loudly rather than yielding an empty config.
fn parse_config_fragment(
    format: ManageConfigFormat,
    body: &str,
) -> Result<cascade_config::CascadeConfig> {
    match format {
        ManageConfigFormat::Gitignore => Ok(cascade_config::parse::gitignore::parse(body)),
        ManageConfigFormat::Toml => cascade_config::parse::toml::parse(body),
        ManageConfigFormat::Yaml => cascade_config::parse::yaml::parse(body),
        ManageConfigFormat::Json => cascade_config::parse::json::parse(body),
    }
}

#[async_trait]
impl ManageDispatch for Engine {
    async fn dispatch(
        &self,
        caller: &DeviceId,
        command: ManageCommand,
        scope: WireScope,
        token: Option<String>,
        now: DateTime<Utc>,
    ) -> ManageResult {
        run_dispatch(self, self, caller, command, scope, token, now).await
    }
}

/// The engine is the data-plane authority for the BEP sync path: it resolves a
/// peer's directional read/write access to a folder from the on-node data
/// grants, the token revocation list, and any signed data-verb token the peer
/// presented on its sync `ClusterConfig`.
///
/// The decision is **default-open** (see [`data_access_with_explicit_control`]):
/// a trusted peer with no data grant configured keeps full bidirectional
/// access, and the feature only ever narrows. Both the grant rows and the
/// revocation list are read on every call, so revoking or expiring a grant
/// takes effect at the next frame rather than at restart. The F2
/// explicit-control bit is consulted on every call too, so a verified-token
/// restriction survives the token revocation or expiry that prompted it.
#[async_trait]
impl DataAuthority for Engine {
    async fn data_access(
        &self,
        peer: &DeviceId,
        folder: &str,
        presented_token: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<DataAccess> {
        // On-node data grants. Read every call so a freshly added or revoked
        // grant is honoured promptly. A grant row carries no token, so the
        // revocation list does not touch these — only a presented token below.
        let mut grants: Vec<Grant> = self
            .db
            .list_data_grants()?
            .into_iter()
            .map(|record| record.grant)
            .collect();

        // Fold in the peer's presented data-verb token, if it verifies against
        // this node (signed by us or a chain rooting in us, unexpired, not
        // revoked, bearer == this peer). A token that does not verify, or that
        // carries a non-data verb, confers nothing — it can never widen access.
        if let Some(token_json) = presented_token
            && let Some(token_grant) = verify_data_token(self, peer, token_json, now)
        {
            // The F2 invariant: a successful verify pins the peer into
            // explicit-control mode for the folder. Record the bit so the
            // absent direction stays denied even if the token is later
            // revoked or allowed to expire. The data-plane gate keys on
            // `folder`, the runtime value the BEP session is bound to, not
            // the token's carried scope — the verify path's scope-cover
            // check has already confirmed the two agree.
            self.db.record_data_explicit_control(
                peer.as_str(),
                folder,
                matches!(token_grant.capability, crate::manage::Capability::DataRead),
                matches!(token_grant.capability, crate::manage::Capability::DataWrite),
                now,
            )?;
            grants.push(token_grant);
        }

        let explicit_control: Vec<ExplicitControlState> = self
            .db
            .list_data_explicit_control()?
            .into_iter()
            .map(|record| ExplicitControlState {
                peer: record.peer_device,
                folder: record.folder_id,
                data_read: record.data_read,
                data_write: record.data_write,
            })
            .collect();

        Ok(data_access_with_explicit_control(
            &grants,
            peer,
            folder,
            now,
            &explicit_control,
        ))
    }

    async fn quarantine_received(
        &self,
        peer: &DeviceId,
        folder: &str,
        path: &str,
        file_json: &str,
        observed_at: DateTime<Utc>,
    ) -> Result<()> {
        self.db.upsert_quarantine(&QuarantineRecord {
            folder_id: folder.to_string(),
            peer_device: peer.as_str().to_string(),
            path: path.to_string(),
            file_json: file_json.to_string(),
            observed_at,
        })
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

/// Root a config rule's path under the fragment's target folder, joining
/// unconditionally.
///
/// A rule path is always interpreted relative to `folder`: a leading `/` is
/// stripped so an absolute-looking rule path (`/personal`) is rooted *under*
/// the pushed folder (`/work/personal`) rather than escaping to node-absolute
/// `/personal`. This is the documented intent — a pushed fragment's rules live
/// in the subtree the push is authorised over. A `..` segment is left in the
/// joined string for the caller's containment check to fold and reject; this
/// function only performs the join.
fn root_under(folder: &str, rule_path: &str) -> String {
    let folder = folder.trim_end_matches('/');
    let rule = rule_path.trim_start_matches('/');
    if folder.is_empty() {
        format!("/{rule}")
    } else {
        format!("{folder}/{rule}")
    }
}

/// Root a config rule's path under `folder` and confine it to that subtree.
///
/// Returns the rooted path when it normalises to a location covered by
/// `folder_scope`. Fails loudly when the rule escapes the authorised subtree
/// (for example a `..` traversal that climbs above `folder`), so a `ConfigPush`
/// authorised only over `folder` can never plant a rule outside it. The
/// containment test reuses [`Scope::covers`], which normalises `.`/`..`/empty
/// segments and matches on path components, so the same defence the authz layer
/// applies to scopes is applied to every rule path.
fn confine_rule_path(folder: &str, rule_path: &str, folder_scope: &Scope) -> Result<String> {
    let rooted = root_under(folder, rule_path);
    if folder_scope.covers(&Scope::folder(rooted.clone())) {
        Ok(rooted)
    } else {
        anyhow::bail!(
            "config push rule path {rule_path:?} escapes the authorised folder {folder:?} \
             (resolved to {rooted:?}); the whole push is refused",
        )
    }
}

/// Seconds in one minute.
const SECS_PER_MINUTE: i64 = 60;
/// Seconds in one hour.
const SECS_PER_HOUR: i64 = SECS_PER_MINUTE * 60;
/// Seconds in one day.
const SECS_PER_DAY: i64 = SECS_PER_HOUR * 24;
/// Seconds in one week.
const SECS_PER_WEEK: i64 = SECS_PER_DAY * 7;

/// Parse a human duration in the `.cascade` lifecycle form into whole seconds.
///
/// Accepts an integer count followed by a single unit suffix: `s` (seconds),
/// `m` (minutes), `h` (hours), `d` (days), or `w` (weeks). A bare integer is
/// taken as seconds. Fails loudly on an empty, non-numeric, or unknown-unit
/// value rather than guessing.
fn parse_duration_secs(raw: &str) -> Result<i64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("empty duration");
    }
    let (digits, unit_secs) = match trimmed.strip_suffix(['s', 'm', 'h', 'd', 'w']) {
        Some(stripped) => {
            let unit = trimmed
                .as_bytes()
                .last()
                .copied()
                .ok_or_else(|| anyhow::anyhow!("duration unit missing: {raw}"))?;
            let multiplier = match unit {
                b's' => 1,
                b'm' => SECS_PER_MINUTE,
                b'h' => SECS_PER_HOUR,
                b'd' => SECS_PER_DAY,
                b'w' => SECS_PER_WEEK,
                _ => anyhow::bail!("unknown duration unit in {raw}"),
            };
            (stripped, multiplier)
        }
        None => (trimmed, 1),
    };
    let count: i64 = digits
        .trim()
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid duration count in {raw}: {e}"))?;
    count
        .checked_mul(unit_secs)
        .ok_or_else(|| anyhow::anyhow!("duration overflow in {raw}"))
}

/// Bytes in one kibibyte.
const BYTES_PER_KIB: i64 = 1024;
/// Bytes in one mebibyte.
const BYTES_PER_MIB: i64 = BYTES_PER_KIB * 1024;
/// Bytes in one gibibyte.
const BYTES_PER_GIB: i64 = BYTES_PER_MIB * 1024;
/// Bytes in one tebibyte.
const BYTES_PER_TIB: i64 = BYTES_PER_GIB * 1024;

/// Parse a human byte size in the `.cascade` cache/lifecycle form into bytes.
///
/// Accepts an integer count followed by an optional binary unit suffix: `KB`,
/// `MB`, `GB`, or `TB` (interpreted as binary multiples, matching the rest of
/// the cache sizing in this codebase). A bare integer is taken as bytes.
/// Case-insensitive. Fails loudly on an empty, non-numeric, or unknown-unit
/// value.
fn parse_size_bytes(raw: &str) -> Result<i64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("empty size");
    }
    let upper = trimmed.to_ascii_uppercase();
    // Two-letter binary units first (longest suffix wins), then the bare-byte
    // suffix, then a plain integer.
    let binary_units: [(&str, i64); 4] = [
        ("TB", BYTES_PER_TIB),
        ("GB", BYTES_PER_GIB),
        ("MB", BYTES_PER_MIB),
        ("KB", BYTES_PER_KIB),
    ];
    let (digits, multiplier) = binary_units
        .iter()
        .find_map(|(suffix, mult)| upper.strip_suffix(suffix).map(|d| (d, *mult)))
        .or_else(|| upper.strip_suffix('B').map(|d| (d, 1)))
        .unwrap_or((upper.as_str(), 1));
    let count: i64 = digits
        .trim()
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid size count in {raw}: {e}"))?;
    count
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow::anyhow!("size overflow in {raw}"))
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
            p2p_posture: None,
            p2p_relay_endpoints: Vec::new(),
            p2p_relay_shared_secret: None,
            backend_factory: None,
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
            p2p_posture: None,
            p2p_relay_endpoints: Vec::new(),
            p2p_relay_shared_secret: None,
            backend_factory: None,
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
            p2p_posture: None,
            p2p_relay_endpoints: Vec::new(),
            p2p_relay_shared_secret: None,
            backend_factory: None,
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
                None,
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
                None,
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
                None,
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
                None,
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

    // ── Duration / size parsing ──

    #[test]
    fn parse_duration_secs_units() {
        assert_eq!(parse_duration_secs("30").unwrap(), 30);
        assert_eq!(parse_duration_secs("45s").unwrap(), 45);
        assert_eq!(parse_duration_secs("2m").unwrap(), 120);
        assert_eq!(parse_duration_secs("3h").unwrap(), 3 * 3600);
        assert_eq!(parse_duration_secs("7d").unwrap(), 7 * 86_400);
        assert_eq!(parse_duration_secs("1w").unwrap(), 7 * 86_400);
    }

    #[test]
    fn parse_duration_secs_rejects_bad_input() {
        assert!(parse_duration_secs("").is_err());
        assert!(parse_duration_secs("abc").is_err());
        assert!(parse_duration_secs("10y").is_err());
    }

    #[test]
    fn parse_size_bytes_units() {
        assert_eq!(parse_size_bytes("512").unwrap(), 512);
        assert_eq!(parse_size_bytes("512B").unwrap(), 512);
        assert_eq!(parse_size_bytes("1KB").unwrap(), 1024);
        assert_eq!(parse_size_bytes("2MB").unwrap(), 2 * 1024 * 1024);
        assert_eq!(parse_size_bytes("1gb").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(
            parse_size_bytes("1TB").unwrap(),
            1024_i64 * 1024 * 1024 * 1024
        );
    }

    #[test]
    fn parse_size_bytes_rejects_bad_input() {
        assert!(parse_size_bytes("").is_err());
        assert!(parse_size_bytes("big").is_err());
    }

    #[test]
    fn root_under_always_roots_relative_to_folder() {
        assert_eq!(root_under("/work", "reports"), "/work/reports");
        assert_eq!(root_under("/work/", "reports"), "/work/reports");
        // An absolute-looking rule path is rooted UNDER the folder, never
        // treated as node-absolute — this is what stops a fragment escaping.
        assert_eq!(
            root_under("/work", "/personal/secret"),
            "/work/personal/secret"
        );
        // Empty folder roots at the filesystem root.
        assert_eq!(root_under("", "reports"), "/reports");
    }

    #[test]
    fn confine_rule_path_roots_absolute_paths_under_the_folder() {
        // The scope-escape blocker: an absolute rule path inside an authorised
        // `/work` push is confined to `/work/personal`, not leaked to a
        // node-absolute `/personal`. Confinement succeeds and returns the
        // rooted-under path.
        let scope = Scope::folder("/work".to_owned());
        let confined = confine_rule_path("/work", "/personal/secret", &scope).unwrap();
        assert_eq!(confined, "/work/personal/secret");
        assert!(scope.covers(&Scope::folder(confined)));
    }

    #[test]
    fn confine_rule_path_rejects_parent_traversal_escape() {
        // A `..` traversal that climbs out of the authorised folder must be
        // refused loudly rather than silently clamped or applied.
        let scope = Scope::folder("/work".to_owned());
        let err = confine_rule_path("/work", "../personal", &scope)
            .expect_err("a traversal escaping the folder must be refused");
        assert!(
            err.to_string().contains("escapes the authorised folder"),
            "error should name the escape, got {err}",
        );
        // A deeper climb that lands above the root is equally refused.
        assert!(confine_rule_path("/work", "../../etc", &scope).is_err());
    }

    // ── Engine-level command entry points against the real state DB ──

    #[test]
    fn config_push_applies_pins_and_policies_into_db() {
        let engine = make_test_engine();
        let body = r#"
            [[pin]]
            path = "reports"

            [[lifecycle]]
            path = "tmp"
            max_age = "7d"
            max_file_size = "1MB"
            priority = 3
        "#;
        let config = cascade_config::parse::toml::parse(body).unwrap();
        engine.config_push("/work", &config).unwrap();

        let pins = engine.db().list_pin_rules().unwrap();
        assert!(
            pins.iter().any(|p| p.path_glob == "/work/reports"),
            "relative pin path must be rooted under the pushed folder, got {pins:?}",
        );

        let policies = engine.db().list_lifecycle_policies().unwrap();
        let policy = policies
            .iter()
            .find(|p| p.path_glob == "/work/tmp")
            .expect("lifecycle policy rooted under the folder");
        assert_eq!(policy.max_age, Some(7 * 86_400));
        assert_eq!(policy.max_file_size, Some(1024 * 1024));
        assert_eq!(policy.priority, 3);
    }

    #[test]
    fn config_push_roots_absolute_rule_paths_under_the_folder() {
        // Scope-escape blocker, end to end: a fragment authorised over /work
        // carries an absolute pin path `/personal` and an absolute lifecycle
        // path `/`. Both must be rooted UNDER /work — no `/personal` or bare
        // `/` row may land in the DB.
        let engine = make_test_engine();
        let body = r#"
            [[pin]]
            path = "/personal"

            [[lifecycle]]
            path = "/"
            max_age = "1d"
            priority = 1
        "#;
        let config = cascade_config::parse::toml::parse(body).unwrap();
        engine.config_push("/work", &config).unwrap();

        let pins = engine.db().list_pin_rules().unwrap();
        assert!(
            pins.iter().all(|p| p.path_glob.starts_with("/work")),
            "no pin rule may escape the authorised /work subtree, got {pins:?}",
        );
        assert!(
            pins.iter().any(|p| p.path_glob == "/work/personal"),
            "the absolute /personal path must be rooted under /work, got {pins:?}",
        );

        let policies = engine.db().list_lifecycle_policies().unwrap();
        assert!(
            policies.iter().all(|p| p.path_glob.starts_with("/work")),
            "no lifecycle policy may escape the authorised /work subtree, got {policies:?}",
        );
    }

    #[test]
    fn config_push_with_traversal_escape_applies_nothing() {
        // A fragment whose rule path climbs out of the authorised folder via
        // `..` must reject the whole push and apply nothing — not even the
        // earlier, well-behaved rules in the same fragment.
        let engine = make_test_engine();
        let body = r#"
            [[pin]]
            path = "reports"

            [[pin]]
            path = "../personal"
        "#;
        let config = cascade_config::parse::toml::parse(body).unwrap();
        let err = engine
            .config_push("/work", &config)
            .expect_err("a traversal escape must reject the push");
        assert!(
            err.to_string().contains("escapes the authorised folder"),
            "error should name the escape, got {err}",
        );
        assert!(
            engine.db().list_pin_rules().unwrap().is_empty(),
            "no rule may be applied when any rule in the fragment escapes",
        );
    }

    #[test]
    fn policy_set_inserts_a_lifecycle_policy() {
        let engine = make_test_engine();
        engine
            .policy_set("/work/*.tmp", Some(3600), None, 1)
            .unwrap();
        let policies = engine.db().list_lifecycle_policies().unwrap();
        let policy = policies
            .iter()
            .find(|p| p.path_glob == "/work/*.tmp")
            .expect("policy inserted");
        assert_eq!(policy.max_age, Some(3600));
        assert_eq!(policy.max_file_size, None);
    }

    #[test]
    fn grant_add_and_revoke_round_trip_through_db() {
        use crate::manage::{Capability, Scope};
        let engine = make_test_engine();
        let g = Grant {
            grantee: DeviceId::new("SUBORDINATE"),
            capability: Capability::PinWrite,
            scope: Scope::folder("/work"),
            granted_by: manager_id(),
            expires: None,
        };
        engine.grant_add(&g).unwrap();
        let grants = engine.db().list_grants().unwrap();
        assert_eq!(grants.len(), 1);
        let record = grants.first().expect("one grant");
        assert_eq!(record.grant.grantee, DeviceId::new("SUBORDINATE"));
        assert_eq!(record.grant.granted_by, manager_id());

        let summary = engine.grant_revoke(record.id).unwrap();
        assert!(summary.contains("revoked"), "summary: {summary}");
        assert!(engine.db().list_grants().unwrap().is_empty());
    }

    #[tokio::test]
    async fn backend_add_without_factory_fails_loudly() {
        // make_test_engine injects no factory, so a BackendAdd must error rather
        // than silently no-op.
        let engine = make_test_engine();
        let err = engine
            .backend_add("x", "gdrive", "/drive", "type = \"gdrive\"\n")
            .expect_err("backend_add must fail with no factory");
        assert!(
            err.to_string().contains("no backend factory"),
            "error should name the missing factory, got {err}",
        );
    }

    #[tokio::test]
    async fn restart_rearms_running_state() {
        let engine = make_test_engine();
        engine.shutdown();
        assert!(!engine.status().running, "shutdown should stop the engine");
        let _handle = engine.restart().unwrap();
        assert!(
            engine.status().running,
            "restart must re-arm the running state",
        );
    }

    #[tokio::test]
    async fn dispatch_grant_add_escalation_is_refused_end_to_end() {
        use crate::manage::{Capability, Scope};
        let engine = make_test_engine();
        // The manager may delegate (grant:admin over /work) but does NOT hold
        // pin:write — delegating it is an escalation and must be refused, with
        // no grant inserted and the denial audited.
        engine
            .db()
            .insert_grant(&Grant {
                grantee: manager_id(),
                capability: Capability::GrantAdmin,
                scope: Scope::folder("/work"),
                granted_by: DeviceId::new("OWNER"),
                expires: None,
            })
            .unwrap();

        let result = engine
            .dispatch(
                &manager_id(),
                ManageCommand::GrantAdd {
                    grant: cascade_p2p::protocol::ManageGrant {
                        grantee: "SUBORDINATE".to_owned(),
                        capability: "pin:write".to_owned(),
                        scope: WireScope::Folder {
                            path: "/work".to_owned(),
                        },
                        expires: None,
                    },
                },
                WireScope::Folder {
                    path: "/work".to_owned(),
                },
                None,
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
            "escalating delegation must be refused, got {result:?}",
        );
        // Only the manager's own grant exists — no delegated grant was inserted.
        assert_eq!(engine.db().list_grants().unwrap().len(), 1);
        let audit = engine.db().list_audit().unwrap();
        assert_eq!(audit.len(), 1);
        assert_eq!(
            audit.first().map(|r| r.entry.outcome.as_str()),
            Some("denied"),
        );
    }
}
