//! Cache manager — orchestrates pinning, eviction, and lifecycle evaluation.
//!
//! The cache manager is a background worker that periodically:
//! 1. Ensures pinned paths are cached (queues downloads)
//! 2. Evicts LRU non-pinned files when cache exceeds limits
//! 3. Applies lifecycle policies
//! 4. Reports cache statistics
//!
//! The manager uses portable traits ([`StateStorage`], [`RuntimeHandle`])
//! instead of concrete tokio/rusqlite types so it compiles to both native
//! and WASM targets. The cancel mechanism is runtime-specific: a tokio
//! `watch` channel on native, an `AtomicBool` flag on portable.

use crate::cache::lifecycle::{EvictionDecision, LifecycleEvaluator};
use crate::cache::pin::{self, PinMatcher};
#[cfg(feature = "p2p")]
use crate::p2p_bridge::P2pBridge;
use crate::portable::{RuntimeHandle, StateStorage};
use crate::types::CacheState;
use anyhow::Result;
use std::path::Path;
use std::sync::Arc;
use tracing::info;

/// Configuration for the cache manager.
#[derive(Debug, Clone, Copy)]
pub struct CacheManagerConfig {
    /// Maximum total cache size in bytes. None = unlimited.
    pub max_size: Option<u64>,
    /// Maximum age for cached files in seconds. None = no limit.
    pub max_age: Option<u64>,
    /// Interval between eviction sweeps in seconds.
    pub sweep_interval_secs: u64,
}

impl Default for CacheManagerConfig {
    fn default() -> Self {
        Self {
            max_size: None,
            max_age: None,
            sweep_interval_secs: 300, // 5 minutes
        }
    }
}

/// Manages file caching, pinning, and eviction.
pub struct CacheManager<R: RuntimeHandle> {
    storage: Arc<dyn StateStorage>,
    runtime: R,
    config: CacheManagerConfig,
    #[cfg(feature = "p2p")]
    p2p: Option<Arc<P2pBridge>>,
}

impl<R: RuntimeHandle> std::fmt::Debug for CacheManager<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut binding = f.debug_struct("CacheManager");
        let s = binding.field("config", &self.config);
        #[cfg(feature = "p2p")]
        {
            s.field("p2p_enabled", &self.p2p.is_some());
        }
        s.finish_non_exhaustive()
    }
}

impl<R: RuntimeHandle> CacheManager<R> {
    /// Create a new cache manager backed by portable traits.
    pub fn new(storage: Arc<dyn StateStorage>, runtime: R, config: CacheManagerConfig) -> Self {
        Self {
            storage,
            runtime,
            config,
            #[cfg(feature = "p2p")]
            p2p: None,
        }
    }

    /// Attach a P2P bridge for content fetching.
    #[must_use]
    #[cfg(feature = "p2p")]
    pub fn with_p2p(mut self, p2p: Arc<P2pBridge>) -> Self {
        self.p2p = Some(p2p);
        self
    }

    /// Try to fetch file contents from P2P peers before falling back to
    /// cloud. Returns the file data if P2P has it, None otherwise.
    #[cfg(feature = "p2p")]
    pub async fn fetch_from_p2p(&self, file: &crate::types::FileEntry) -> Result<Option<Vec<u8>>> {
        let Some(bridge) = &self.p2p else {
            return Ok(None);
        };
        bridge.try_fetch_from_peers(file).await
    }

    /// Index downloaded file data into the P2P block store for sharing.
    #[cfg(feature = "p2p")]
    pub async fn index_for_p2p(&self, file_id: &crate::types::ItemId, data: &[u8]) -> Result<()> {
        let Some(bridge) = &self.p2p else {
            return Ok(());
        };
        bridge.index_file_by_id(file_id, data).await?;
        Ok(())
    }

    /// Pin a path — all files matching the glob will be kept offline.
    pub async fn pin(&self, path_glob: &str, recursive: bool) -> Result<()> {
        let _matcher = pin::add_pin_rule(&*self.storage, path_glob, recursive).await?;

        // Transition matching files to Pinned state.
        // For now, we just record the rule. Actual file download is deferred
        // to the background worker which will detect Pinned-state files that
        // aren't yet cached and queue downloads.
        info!("pin rule added: {} (recursive={})", path_glob, recursive);
        Ok(())
    }

    /// Unpin a path — removes the pin rule.
    pub async fn unpin(&self, path_glob: &str) -> Result<bool> {
        let removed = pin::remove_pin_rule(&*self.storage, path_glob).await?;
        if removed {
            info!("pin rule removed: {}", path_glob);
        }
        Ok(removed)
    }

    /// List all pin rules.
    pub async fn list_pins(&self) -> Result<Vec<crate::db::PinRuleRecord>> {
        let matcher = PinMatcher::load(&*self.storage).await?;
        Ok(matcher.rules().to_vec())
    }

    /// Run one eviction sweep: evict files that exceed size or age limits,
    /// subject to lifecycle policies.
    ///
    /// Returns the number of files evicted.
    pub async fn evict(&self) -> Result<EvictionReport> {
        let mut report = EvictionReport::default();

        // 1. Evict files matching lifecycle policies.
        let lifecycle = LifecycleEvaluator::load(&*self.storage).await?;
        let cached_files = self
            .storage
            .list_files_by_cache_state(CacheState::Cached)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        for file in &cached_files {
            // Match against the full, mount-prefixed VFS path — the same string
            // stored in `files.path` and used by the sync runner and pin rules.
            // Using only the basename (`file.name`) would fail to match any policy
            // glob anchored to a mount prefix (e.g. `personal/Documents/**`).
            let path = Path::new(&file.path);
            if lifecycle.should_evict(file, path) != EvictionDecision::Keep {
                self.storage
                    .update_cache_state(&file.id, CacheState::Online)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                report.lifecycle_evicted += 1;
            }
        }

        // 2. Evict LRU files if cache exceeds max_size.
        if let Some(max_size) = self.config.max_size {
            let current_size = u64::try_from(
                self.storage
                    .cache_size()
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?,
            )
            .unwrap_or(0);
            if current_size > max_size {
                // Evict in LRU order until under limit.
                let candidates = self
                    .storage
                    .eviction_candidates(100)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                let mut freed: u64 = 0;
                for file in &candidates {
                    if current_size - freed <= max_size {
                        break;
                    }
                    freed += file.size.unwrap_or(0);
                    self.storage
                        .update_cache_state(&file.id, CacheState::Online)
                        .await
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    report.size_evicted += 1;
                }
                report.bytes_freed = freed;
            }
        }

        if report.total_evicted() > 0 {
            info!(
                "eviction sweep complete: {} lifecycle, {} size, {} bytes freed",
                report.lifecycle_evicted, report.size_evicted, report.bytes_freed,
            );
        }

        Ok(report)
    }

    /// Get cache statistics.
    pub async fn stats(&self) -> Result<CacheStats> {
        let online = self
            .storage
            .list_files_by_cache_state(CacheState::Online)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let cached = self
            .storage
            .list_files_by_cache_state(CacheState::Cached)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let pinned = self
            .storage
            .list_files_by_cache_state(CacheState::Pinned)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let total_size = self
            .storage
            .cache_size()
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        Ok(CacheStats {
            online_count: online.len(),
            cached_count: cached.len(),
            pinned_count: pinned.len(),
            total_bytes: u64::try_from(total_size).unwrap_or(0),
            max_bytes: self.config.max_size,
        })
    }

    /// Run the background eviction loop using a tokio watch channel for
    /// cancellation.
    #[cfg(feature = "native")]
    pub async fn run(&self, mut cancel: tokio::sync::watch::Receiver<bool>) {
        let interval = std::time::Duration::from_secs(self.config.sweep_interval_secs);
        let mut ticker = tokio::time::interval(interval);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Err(e) = self.evict().await {
                        tracing::error!("eviction sweep failed: {e}");
                    }
                }
                _ = cancel.changed() => {
                    info!("cache manager shutting down");
                    return;
                }
            }
        }
    }

    /// Run the background eviction loop using an atomic flag for cancellation.
    ///
    /// Polls the flag each sweep interval. Suitable for runtimes without tokio
    /// watch channels (e.g. WASM).
    pub async fn run_with_flag(&self, cancel: Arc<std::sync::atomic::AtomicBool>) {
        use std::sync::atomic::Ordering;

        let interval = std::time::Duration::from_secs(self.config.sweep_interval_secs);

        loop {
            if cancel.load(Ordering::Relaxed) {
                info!("cache manager shutting down");
                return;
            }
            let () = self.runtime.sleep(interval).await;
            if cancel.load(Ordering::Relaxed) {
                info!("cache manager shutting down");
                return;
            }
            if let Err(e) = self.evict().await {
                tracing::error!("eviction sweep failed: {e}");
            }
        }
    }
}

/// Cache statistics.
#[derive(Debug, Clone, Copy)]
pub struct CacheStats {
    pub online_count: usize,
    pub cached_count: usize,
    pub pinned_count: usize,
    pub total_bytes: u64,
    pub max_bytes: Option<u64>,
}

/// Report from an eviction sweep.
#[derive(Debug, Clone, Copy, Default)]
pub struct EvictionReport {
    /// Files evicted due to lifecycle policies.
    pub lifecycle_evicted: usize,
    /// Files evicted due to cache size limits.
    pub size_evicted: usize,
    /// Bytes freed by size-based eviction.
    pub bytes_freed: u64,
}

impl EvictionReport {
    /// Total files evicted.
    #[must_use]
    pub const fn total_evicted(&self) -> usize {
        self.lifecycle_evicted + self.size_evicted
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    #![allow(clippy::pedantic, clippy::nursery)]

    use super::*;
    use crate::db::StateDb;
    use crate::portable::native::{SqliteStorage, TokioRuntimeHandle};
    use crate::types::{FileEntry, ItemId};
    use std::sync::Arc;

    fn make_manager(db: Arc<StateDb>) -> CacheManager<TokioRuntimeHandle> {
        let runtime = TokioRuntimeHandle::current();
        let storage = SqliteStorage::new(db, runtime.clone());
        CacheManager::new(Arc::new(storage), runtime, CacheManagerConfig::default())
    }

    /// Insert a file record with the given VFS path and size, then mark it as
    /// `Cached` so the eviction sweep considers it.
    fn insert_cached(db: &StateDb, id: &ItemId, vfs_path: &str, name: &str, size: u64) {
        let entry = FileEntry::file(id.clone(), ItemId::new("test", "root"), name.to_string())
            .with_path(vfs_path.to_string())
            .with_size(Some(size));
        db.upsert_file(&entry).unwrap();
        db.update_cache_state(id, CacheState::Cached).unwrap();
    }

    /// Lifecycle eviction must match policies against the full, mount-prefixed
    /// VFS path (`file.path`), not just the basename (`file.name`).
    ///
    /// Here the policy is `personal/Documents/**` (a max-size rule). The file
    /// lives at `personal/Documents/large.bin` with size above the limit. If the
    /// eviction sweep incorrectly used the basename (`large.bin`) instead of the
    /// VFS path it would fail to match the prefix-anchored glob and the file
    /// would not be evicted.
    #[tokio::test]
    async fn evict_uses_vfs_path_for_lifecycle_matching() {
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("test", "local", "Test", Some("personal"), None)
            .unwrap();

        // Policy: evict anything under `personal/Documents/**` exceeding 1 KiB.
        let max_bytes: i64 = 1024;
        db.add_lifecycle_policy("personal/Documents/**", None, Some(max_bytes), 0, None)
            .unwrap();

        let big_id = ItemId::new("test", "big");
        let small_id = ItemId::new("test", "small");
        // Both files have the same *basename* `report.bin`; only their VFS path
        // distinguishes them from the policy's point of view.
        insert_cached(
            &db,
            &big_id,
            "personal/Documents/report.bin",
            "report.bin",
            2048,
        );
        insert_cached(&db, &small_id, "other/report.bin", "report.bin", 2048);

        let manager = make_manager(db.clone());
        let report = manager.evict().await.unwrap();

        assert_eq!(
            report.lifecycle_evicted, 1,
            "exactly the file under personal/Documents must be evicted"
        );

        // The file under personal/Documents was evicted (back to Online).
        assert_eq!(
            db.get_cache_state(&big_id).unwrap(),
            Some(CacheState::Online),
            "personal/Documents file must be evicted"
        );
        // The file whose basename matches but VFS path doesn't must be kept.
        assert_eq!(
            db.get_cache_state(&small_id).unwrap(),
            Some(CacheState::Cached),
            "other/report.bin must NOT be evicted (outside policy path)"
        );
    }

    /// A lifecycle policy using a bare directory name (no `**` glob) must
    /// still match files whose VFS path starts with `<policy>/`.
    #[tokio::test]
    async fn evict_bare_dir_policy_matches_prefix() {
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("test", "local", "Test", None, None)
            .unwrap();

        // Policy: anything under `Cache` (no glob, max-size 0 — evict everything).
        db.add_lifecycle_policy("Cache", None, Some(0), 0, None)
            .unwrap();

        let in_cache = ItemId::new("test", "c1");
        let not_cache = ItemId::new("test", "nc1");
        insert_cached(&db, &in_cache, "Cache/tmp.bin", "tmp.bin", 100);
        insert_cached(&db, &not_cache, "Documents/tmp.bin", "tmp.bin", 100);

        let manager = make_manager(db.clone());
        let report = manager.evict().await.unwrap();

        assert_eq!(report.lifecycle_evicted, 1);
        assert_eq!(
            db.get_cache_state(&in_cache).unwrap(),
            Some(CacheState::Online)
        );
        assert_eq!(
            db.get_cache_state(&not_cache).unwrap(),
            Some(CacheState::Cached)
        );
    }
}
