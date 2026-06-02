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

    /// Wire the management-plane dispatch port into every backend that serves
    /// remote management.
    ///
    /// The engine is the production [`ManageDispatch`] implementation — it owns
    /// the grant store, the audit log, and the command executor. A backend that
    /// runs its own peer transport (the P2P backend) receives inbound
    /// `ManageRequest` frames and needs this port to authorise, audit, and
    /// execute them through the same core the local CLI drives. Backends with no
    /// transport ignore the call (the trait default is a no-op).
    ///
    /// Called once at daemon startup, after the engine is constructed and before
    /// its presenter begins accepting connections. Takes `self: &Arc<Self>` so
    /// the engine can hand a clone of itself, as an `Arc<dyn ManageDispatch>`, to
    /// each backend.
    pub async fn wire_manage_dispatch(self: &Arc<Self>) {
        let dispatch: Arc<dyn ManageDispatch> = self.clone();
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
        }
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
    /// its parsed age/size bounds. Rule paths declared relative to the fragment
    /// (not absolute) are rooted under `folder` so they land in the subtree the
    /// push is authorised over. Returns a short summary of what was applied.
    pub fn config_push(
        &self,
        folder: &str,
        config: &cascade_config::CascadeConfig,
    ) -> Result<String> {
        let mut pins_applied = 0usize;
        for pin in &config.pin {
            let path = root_under(folder, &pin.path);
            self.db.add_pin_rule(&path, true, None)?;
            pins_applied += 1;
        }

        let mut policies_applied = 0usize;
        for policy in &config.lifecycle {
            let path = root_under(folder, &policy.path);
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
            self.db
                .add_lifecycle_policy(&path, max_age, max_file_size, policy.priority, None)?;
            policies_applied += 1;
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

    /// Restart the engine's background workers.
    ///
    /// Signals the current workers to stop, re-arms the cancellation channel,
    /// and spawns a fresh worker set — the in-process equivalent of the daemon
    /// `cascade restart`. The returned handle owns the new background tasks.
    pub fn restart(&self) -> Result<EngineHandle> {
        let _ = self.cancel.send(true);
        // Re-arm: a freshly subscribed receiver reads `false` (running) so the
        // new worker set does not see a stale shutdown signal.
        let _ = self.cancel.send(false);
        self.start()
    }

    /// Stop the engine's background workers — an alias of [`Engine::shutdown`]
    /// returning a summary for the management plane.
    #[must_use]
    pub fn stop(&self) -> String {
        self.shutdown();
        "daemon workers signalled to stop".to_owned()
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
        // The returned handle owns freshly spawned background tasks. The daemon
        // keeps the engine alive past this call, so detaching the handle lets
        // the new workers run for the daemon's lifetime exactly as the initial
        // `start()` handle does.
        let _handle = self.restart()?;
        Ok("daemon workers restarted".to_owned())
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

/// Root a config rule's path under the fragment's target folder.
///
/// A rule whose path is absolute (begins with `/`) is taken as already
/// node-absolute and left untouched. A relative rule path is joined beneath
/// `folder` so a pushed fragment's rules land in the subtree the push is
/// authorised over, never leaking outside it.
fn root_under(folder: &str, rule_path: &str) -> String {
    if rule_path.starts_with('/') {
        return rule_path.to_owned();
    }
    let folder = folder.trim_end_matches('/');
    let rule = rule_path.trim_start_matches('/');
    if folder.is_empty() {
        format!("/{rule}")
    } else {
        format!("{folder}/{rule}")
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
    fn root_under_roots_relative_and_preserves_absolute() {
        assert_eq!(root_under("/work", "reports"), "/work/reports");
        assert_eq!(root_under("/work/", "reports"), "/work/reports");
        // An absolute rule path is left untouched.
        assert_eq!(root_under("/work", "/personal/secret"), "/personal/secret");
        // Empty folder roots at the filesystem root.
        assert_eq!(root_under("", "reports"), "/reports");
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
