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
            let path = Path::new(&file.name);
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
