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

#[cfg(all(test, feature = "native", feature = "p2p"))]
mod tests;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::Result;
use tracing::info;

use crate::backend::{Backend, BackendFactory, MountedBackend, NullBackend};
use crate::cache::manager::{CacheManager, CacheManagerConfig};
use crate::changefeed::ChangeFeed;
use crate::config::ConfigResolver;
use crate::db::PinRuleRecord;
#[cfg(feature = "native")]
use crate::db::StateDb;
use crate::manage::Grant;
#[cfg(feature = "p2p")]
use crate::manage::{DataAuthority, ManageDispatch};
#[cfg(feature = "p2p")]
use crate::p2p_bridge::P2pBridge;
#[cfg(feature = "native")]
use crate::portable::native::{NativeClock, SqliteStorage, StdFileSystem, TokioRuntimeHandle};
use crate::portable::{Clock, FileSystem, RuntimeHandle, StateStorage};
use crate::presenter::VfsPresenter;
use crate::sync::runner::{MountedRunnerBackend, SyncRunner};
use crate::vfs::{NEUTRAL_ROOT_ID, VfsTree};

/// Configuration for creating an Engine instance.
pub struct EngineConfig {
    /// Path to the `SQLite` state database file.
    pub db_path: PathBuf,
    /// Mount point for the VFS.
    pub mount_point: PathBuf,
    /// Backend instances paired with their configured mount paths. Each mounts
    /// under the neutral virtual root at its resolved prefix (the configured
    /// mount, or the backend id when unset; an explicit `"/"` mounts at the
    /// root). On restart the persisted `backends.mount_path` in the database
    /// overrides the configured mount, so a runtime-added backend reclaims its
    /// place rather than being lost.
    pub backends: Vec<MountedBackend>,
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
pub struct Engine<R: RuntimeHandle, C: Clock> {
    /// Native state database — used directly by operations, management plane,
    /// and external crates via [`Engine::db`]. Present only on native builds; a
    /// bare-portable host has no concrete `SQLite` database to expose.
    #[cfg(feature = "native")]
    db: Arc<StateDb>,
    /// Portable state storage — the backing-store-independent contract every
    /// portable code path reads and writes through.
    storage: Arc<dyn StateStorage>,
    /// Portable filesystem adapter.
    fs: Arc<dyn FileSystem>,
    /// Runtime handle for spawning and timers.
    runtime: R,
    /// Wall-clock port — injected so the core names no platform time API.
    clock: C,
    vfs: Arc<RwLock<VfsTree>>,
    cache: CacheManager<R>,
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
    /// The exec capability provider — `None` when the node was built without the
    /// `exec` feature or no provider was injected at composition. When `None`,
    /// every exec management verb fails loudly rather than silently doing
    /// nothing, mirroring the "no P2P backend" loud failure for token
    /// verification. Injected at the daemon composition edge, the same edge that
    /// wires p2p and the manage dispatch.
    #[cfg(feature = "exec")]
    exec: Option<std::sync::Arc<dyn cascade_exec::ExecProvider>>,
}

impl<R: RuntimeHandle, C: Clock> fmt::Debug for Engine<R, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut binding = f.debug_struct("Engine");
        #[cfg(feature = "p2p")]
        {
            binding.field("p2p_enabled", &self.p2p.is_some());
        }
        binding.finish_non_exhaustive()
    }
}

/// The native engine instantiation.
///
/// The concrete `Engine` every native consumer (the daemon, the presenters, the
/// web API) names. A bare-portable host parameterises `Engine` over its own
/// runtime and clock and so never refers to this alias.
#[cfg(feature = "native")]
pub type NativeEngine = Engine<TokioRuntimeHandle, NativeClock>;

#[cfg(feature = "native")]
impl Engine<TokioRuntimeHandle, NativeClock> {
    /// Create and initialise a new native engine.
    ///
    /// Opens the `SQLite` state database, builds the native runtime, storage,
    /// filesystem, and clock adapters, then performs the same construction
    /// sequence (VFS tree, backend registration, cache manager) that
    /// `with_ports` runs for non-native builds — over the concrete synchronous
    /// `StateDb` here rather than the async storage port. The optional P2P
    /// bridge, a native-and-`p2p` concern that needs the concrete database
    /// handle, is wired in afterwards.
    pub fn new(config: EngineConfig) -> Result<Self> {
        let backends = config.backends;
        let backend_factory = config.backend_factory;
        if backends.is_empty() {
            anyhow::bail!("at least one backend is required");
        }

        // Open state database.
        let db = Arc::new(StateDb::open(&config.db_path)?);
        info!(path = %config.db_path.display(), "state database opened");

        // Build the VFS tree, resolving and persisting each backend's mount
        // synchronously through the concrete database.
        let mut vfs_tree = VfsTree::new(Arc::new(NullBackend::new(NEUTRAL_ROOT_ID)));
        let mut claimed: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        for mounted in &backends {
            // On restart, the persisted mount in the database is the source of
            // truth — a runtime-added backend recorded its mount there, and a
            // fresh `EngineConfig` would otherwise default it back to its id and
            // lose the placement. Fall back to the configured mount when the
            // backend has no persisted row yet (first run for that backend).
            let effective_mount = db
                .get_backend_mount(mounted.backend.id())?
                .unwrap_or_else(|| mounted.mount.clone());
            let resolved = MountedBackend::new(effective_mount, mounted.backend.clone());
            let prefix = resolved.resolve_prefix();
            Self::claim_prefix(&mut claimed, &prefix, mounted)?;
            db.register_backend(
                mounted.backend.id(),
                "unknown",
                mounted.backend.display_name(),
                resolved.mount.as_deref(),
                None,
            )?;
            vfs_tree.mount(prefix, mounted.backend.clone());
        }
        let vfs = Arc::new(RwLock::new(vfs_tree));
        info!(backends = backends.len(), "VFS tree initialised");

        // Native ports: the tokio runtime, the SQLite-backed storage wrapping
        // the database just opened, the std filesystem, and the chrono clock.
        let runtime = TokioRuntimeHandle::current();
        let storage: Arc<dyn StateStorage> =
            Arc::new(SqliteStorage::new(db.clone(), runtime.clone()));
        let fs: Arc<dyn FileSystem> = Arc::new(StdFileSystem);
        let clock = NativeClock;

        let config_resolver = Arc::new(ConfigResolver::new(config.mount_point.clone()));
        let cache = CacheManager::new(
            storage.clone(),
            runtime.clone(),
            CacheManagerConfig::default(),
        );

        // P2P bridge (optional) — a native-and-`p2p` concern needing the db.
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
            clock,
            vfs,
            cache,
            config: config_resolver,
            #[cfg(feature = "p2p")]
            p2p,
            change_feed: Arc::new(ChangeFeed::new()),
            backend_factory,
            cancel,
            #[cfg(feature = "exec")]
            exec: None,
        })
    }

    /// Inject the exec capability provider, returning the engine.
    ///
    /// Composed at the daemon edge, the same place the p2p backend and manage
    /// dispatch are wired. Without it, every exec management verb fails loudly.
    #[cfg(feature = "exec")]
    #[must_use]
    pub fn with_exec_provider(
        mut self,
        exec: std::sync::Arc<dyn cascade_exec::ExecProvider>,
    ) -> Self {
        self.exec = Some(exec);
        self
    }
}

impl<R: RuntimeHandle, C: Clock> Engine<R, C> {
    /// Create and initialise an engine from injected ports.
    ///
    /// This is the portable construction path: it depends only on the
    /// [`StateStorage`], [`RuntimeHandle`], and [`Clock`] contracts, naming no
    /// host-only runtime or storage type, and it is asynchronous because the
    /// storage contract is. It builds the VFS tree, registers and mounts every
    /// configured backend (resolving each one's persisted mount through the
    /// storage contract), and wires the cache manager.
    ///
    /// A native host constructs the engine through [`Engine::new`] instead — that
    /// path owns the concrete `SQLite` database and resolves mounts
    /// synchronously, and also wires the optional P2P bridge, which is a
    /// native-and-`p2p` concern this portable path does not know about. The two
    /// constructors are mutually exclusive by feature: `new` exists only on
    /// native builds, `with_ports` only on bare-portable ones.
    #[cfg(not(feature = "native"))]
    pub async fn with_ports(
        config: EngineConfig,
        storage: Arc<dyn StateStorage>,
        fs: Arc<dyn FileSystem>,
        runtime: R,
        clock: C,
    ) -> Result<Self> {
        let backends = config.backends;
        let backend_factory = config.backend_factory;
        if backends.is_empty() {
            anyhow::bail!("at least one backend is required");
        }

        // The VFS root is a neutral container that owns no content of its own;
        // `VfsTree::read_dir` injects the mounted child directories. Every
        // configured backend mounts beneath it as a child — including a backend
        // at the empty prefix (the `"/"` case), which routes as the catch-all
        // fallback and so reproduces the single-backend-at-root path shape.
        let mut vfs_tree = VfsTree::new(Arc::new(NullBackend::new(NEUTRAL_ROOT_ID)));

        let mut claimed: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        for mounted in &backends {
            // On restart, the persisted mount in storage is the source of truth —
            // a runtime-added backend recorded its mount there, and a fresh
            // `EngineConfig` would otherwise default it back to its id and lose
            // the placement. Fall back to the configured mount when the backend
            // has no persisted row yet (first run for that backend).
            let effective_mount = storage
                .get_backend_mount(mounted.backend.id())
                .await?
                .unwrap_or_else(|| mounted.mount.clone());
            let resolved = MountedBackend::new(effective_mount, mounted.backend.clone());
            let prefix = resolved.resolve_prefix();
            Self::claim_prefix(&mut claimed, &prefix, mounted)?;
            storage
                .register_backend(
                    mounted.backend.id(),
                    "unknown",
                    mounted.backend.display_name(),
                    resolved.mount.as_deref(),
                    None,
                )
                .await?;
            vfs_tree.mount(prefix, mounted.backend.clone());
        }
        let vfs = Arc::new(RwLock::new(vfs_tree));
        info!(backends = backends.len(), "VFS tree initialised");

        let config_resolver = Arc::new(ConfigResolver::new(config.mount_point.clone()));
        let cache = CacheManager::new(
            storage.clone(),
            runtime.clone(),
            CacheManagerConfig::default(),
        );
        let cancel = Arc::new(AtomicBool::new(false));

        Ok(Self {
            storage,
            fs,
            runtime,
            clock,
            vfs,
            cache,
            config: config_resolver,
            #[cfg(feature = "p2p")]
            p2p: None,
            change_feed: Arc::new(ChangeFeed::new()),
            backend_factory,
            cancel,
            #[cfg(feature = "exec")]
            exec: None,
        })
    }

    /// Reject a duplicate mount prefix loudly.
    ///
    /// Two backends at the same prefix would shadow each other and silently
    /// misroute. The empty prefix (a backend at `"/"`) is a single slot like any
    /// other. Returns the prefix as freshly claimed, or fails when it collides.
    fn claim_prefix(
        claimed: &mut std::collections::HashSet<PathBuf>,
        prefix: &Path,
        mounted: &MountedBackend,
    ) -> Result<()> {
        if claimed.insert(prefix.to_path_buf()) {
            return Ok(());
        }
        // An empty prefix is the at-root case; name it so the message is not a
        // bare, confusing empty path.
        let shown = if prefix.as_os_str().is_empty() {
            "/ (root)".to_owned()
        } else {
            prefix.display().to_string()
        };
        anyhow::bail!(
            "duplicate mount path {shown}: backend {} collides with another mount",
            mounted.backend.id(),
        )
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
        // The neutral root is synthetic and serves no transport, so only the
        // mounted child backends are wired.
        let backends: Vec<Arc<dyn Backend>> = {
            let tree = self
                .vfs
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            tree.children()
                .iter()
                .map(|(_, backend)| backend.clone())
                .collect()
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
    pub fn create_sync_runner(&self, presenter: Arc<dyn VfsPresenter>) -> SyncRunner<R> {
        // Collect the mounted child backends from the VFS tree, each paired with
        // the mount prefix it is mounted at. Sourcing both halves from the same
        // mount table the router resolves on means the prefix the runner stamps
        // into item paths cannot drift from the prefix the router walks. The
        // neutral root is synthetic and owns no content, so it is not polled for
        // changes.
        let tree = self
            .vfs
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let backends: Vec<MountedRunnerBackend> = tree
            .children()
            .iter()
            .map(|(prefix, backend)| MountedRunnerBackend::new(prefix.clone(), backend.clone()))
            .collect();
        drop(tree);

        let runner = SyncRunner::new(
            self.storage.clone(),
            self.fs.clone(),
            self.runtime.clone(),
            backends,
            presenter,
            self.config.clone(),
        );
        // The per-parent change feed the File Provider presenter consumes is a
        // native-only concern; a bare-portable build has no such presenter.
        #[cfg(feature = "native")]
        let runner = runner.with_change_feed(self.change_feed.clone());
        runner
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
        let cache_handle = self.runtime.spawn_joinable(Box::pin(async move {
            cache.run_with_flag(cancel).await;
        }));

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
    pub async fn add_max_file_length_rule(
        &self,
        path_glob: &str,
        max_bytes: u64,
        conditions: Option<&str>,
    ) -> Result<()> {
        operations::add_max_file_length_rule(self, path_glob, max_bytes, 0, conditions).await
    }

    /// List all max file length rules, ordered by priority descending.
    pub async fn list_max_file_length_rules(&self) -> Result<Vec<crate::db::MaxFileLengthRecord>> {
        operations::list_max_file_length_rules(self).await
    }

    /// Remove a max file length rule by id. Returns `true` if a row was removed.
    pub async fn remove_max_file_length_rule(&self, id: i64) -> Result<bool> {
        operations::remove_max_file_length_rule(self, id).await
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
    pub async fn config_push(
        &self,
        folder: &str,
        config: &cascade_config::CascadeConfig,
    ) -> Result<String> {
        operations::config_push(self, folder, config).await
    }

    /// Set a single lifecycle policy on the node.
    ///
    /// Delegates to the same `add_lifecycle_policy` state-database operation the
    /// local config-merge path uses, so a pushed policy behaves identically to
    /// one declared in a `.cascade` file.
    pub async fn policy_set(
        &self,
        path_glob: &str,
        max_age_secs: Option<i64>,
        max_file_size: Option<i64>,
        priority: i32,
    ) -> Result<String> {
        operations::policy_set(self, path_glob, max_age_secs, max_file_size, priority).await
    }

    /// Register and mount a backend at runtime.
    ///
    /// Builds the backend through the injected [`BackendFactory`] (the same
    /// per-type `create_backend` factories the local daemon uses), registers it
    /// in the state database, and mounts it into the live VFS tree at
    /// `mount_path`. Fails loudly when no factory was injected rather than
    /// silently dropping the request.
    pub async fn backend_add(
        &self,
        name: &str,
        backend_type: &str,
        mount_path: &str,
        config_toml: &str,
    ) -> Result<String> {
        operations::backend_add(self, name, backend_type, mount_path, config_toml).await
    }

    /// Unmount and deregister a backend by name.
    ///
    /// Unmounts it from the live VFS tree and removes its state-database row,
    /// the inverse of [`Engine::backend_add`].
    pub async fn backend_remove(&self, name: &str, mount_path: &str) -> Result<String> {
        operations::backend_remove(self, name, mount_path).await
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
    pub async fn grant_add(&self, grant: &Grant) -> Result<String> {
        let id = self.storage.insert_grant(grant).await?;
        Ok(format!(
            "grant {id} added: {} over {:?} for {}",
            grant.capability.as_wire(),
            grant.scope,
            grant.grantee,
        ))
    }

    /// Revoke a grant by its row id.
    pub async fn grant_revoke(&self, grant_id: i64) -> Result<String> {
        let removed = self.storage.revoke_grant(grant_id).await?;
        Ok(if removed {
            format!("grant {grant_id} revoked")
        } else {
            format!("no grant with id {grant_id}")
        })
    }

    /// Get engine status — running state, mounted backends, cache stats.
    ///
    /// Reads through the asynchronous storage contract, so it is itself async.
    pub async fn status(&self) -> EngineStatus {
        let backends = self
            .storage
            .list_backends()
            .await
            .map(|records| {
                records
                    .into_iter()
                    .map(|r| format!("{} ({})", r.display_name, r.backend_type))
                    .collect()
            })
            .unwrap_or_default();

        let online = self
            .storage
            .list_files_by_cache_state(crate::types::CacheState::Online)
            .await;
        let cached = self
            .storage
            .list_files_by_cache_state(crate::types::CacheState::Cached)
            .await;
        let pinned = self
            .storage
            .list_files_by_cache_state(crate::types::CacheState::Pinned)
            .await;
        let total_size = self.storage.cache_size().await;

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

    /// The injected wall-clock port.
    ///
    /// Code that timestamps engine-side state reads the current instant through
    /// this rather than a platform time API, so the same path holds on any
    /// target.
    #[must_use]
    pub const fn clock(&self) -> &C {
        &self.clock
    }

    /// Get the state database (for presenters to use).
    ///
    /// Native-only: a bare-portable host has no concrete `SQLite` handle to
    /// expose. Portable consumers read through the [`StateStorage`] contract.
    #[must_use]
    #[cfg(feature = "native")]
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
    pub async fn explicit_data_control_snapshot(
        &self,
    ) -> std::collections::HashMap<(String, String), (bool, bool)> {
        let Ok(rows) = self.storage.list_data_explicit_control().await else {
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
    /// Join handle for the cache manager background task. A portable handle so
    /// the same type is returned whichever runtime spawned the task.
    pub cache_handle: crate::portable::JoinHandle<()>,
}

impl EngineHandle {
    /// Cancel the cache-manager task immediately.
    ///
    /// [`Engine::shutdown`] only sets the cooperative cancel flag, which a task
    /// parked in its sweep sleep observes no sooner than the next interval.
    /// Aborting kills the task at once, matching the daemon's prompt-teardown
    /// expectation. A no-op on runtimes without a cancellation primitive.
    pub fn abort(&self) {
        self.cache_handle.abort();
    }
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
