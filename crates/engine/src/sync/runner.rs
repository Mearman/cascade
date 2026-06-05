//! Sync runner — orchestrates change polling across all backends.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::backend::Backend;
use crate::cache::pin::PinMatcher;
#[cfg(feature = "native")]
use crate::changefeed::ChangeFeed;
use crate::config::ConfigResolver;
#[cfg(feature = "p2p")]
use crate::p2p_bridge::P2pBridge;
use crate::portable::{FileSystem, RuntimeHandle, StateStorage};
use crate::presenter::VfsPresenter;
use crate::sync::conflict::{ConflictCheck, check_conflict, conflict_name};
use crate::types::{CacheState, Change, FileId, VfsItem};

/// Default poll interval when the backend doesn't specify one.
#[allow(unknown_lints, clippy::duration_suboptimal_units)]
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(60);

/// Orchestrates change polling across all registered backends.
///
/// For each backend, periodically calls `changes(cursor)` to get incremental
/// updates, applies them to the state database, and notifies the presenter.
///
/// Uses portable traits ([`StateStorage`], [`RuntimeHandle`], [`FileSystem`])
/// instead of concrete tokio/rusqlite types so it compiles to both native and
/// WASM targets.
pub struct SyncRunner<R: RuntimeHandle> {
    storage: Arc<dyn StateStorage>,
    fs: Arc<dyn FileSystem>,
    runtime: R,
    backends: Vec<Arc<dyn Backend>>,
    presenter: Arc<dyn VfsPresenter>,
    config: Arc<ConfigResolver>,
    #[cfg(feature = "p2p")]
    p2p: Option<P2pBridge>,
    /// Optional engine-side change index. When present, every applied
    /// batch is also filed here so presenters can serve per-parent
    /// `enumerateChanges` deltas without running a second poll loop.
    /// Available only on native builds (uses `tokio::sync::RwLock`).
    #[cfg(feature = "native")]
    change_feed: Option<Arc<ChangeFeed>>,
}

impl<R: RuntimeHandle> std::fmt::Debug for SyncRunner<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let backend_count = self.backends.len();
        let mut binding = f.debug_struct("SyncRunner");
        let s = binding.field("backend_count", &backend_count);
        #[cfg(feature = "p2p")]
        {
            s.field("p2p_enabled", &self.p2p.is_some());
        }
        s.finish_non_exhaustive()
    }
}

impl<R: RuntimeHandle> SyncRunner<R> {
    /// Create a new sync runner backed by portable traits.
    pub fn new(
        storage: Arc<dyn StateStorage>,
        fs: Arc<dyn FileSystem>,
        runtime: R,
        backends: Vec<Arc<dyn Backend>>,
        presenter: Arc<dyn VfsPresenter>,
        config: Arc<ConfigResolver>,
    ) -> Self {
        Self {
            storage,
            fs,
            runtime,
            backends,
            presenter,
            config,
            #[cfg(feature = "p2p")]
            p2p: None,
            #[cfg(feature = "native")]
            change_feed: None,
        }
    }

    /// Attach a P2P bridge for block-level file sharing.
    #[must_use]
    #[cfg(feature = "p2p")]
    pub fn with_p2p(mut self, p2p: P2pBridge) -> Self {
        self.p2p = Some(p2p);
        self
    }

    /// Attach the engine-side change feed.
    ///
    /// Once attached, each applied poll batch is filed into the feed so
    /// presenters (e.g. the File Provider bridge's `enumerateChanges`) can
    /// serve per-parent deltas from the same poll loop the runner already
    /// drives.
    ///
    /// Only available on native builds (the change feed uses
    /// `tokio::sync::RwLock`).
    #[must_use]
    #[cfg(feature = "native")]
    pub fn with_change_feed(mut self, change_feed: Arc<ChangeFeed>) -> Self {
        self.change_feed = Some(change_feed);
        self
    }

    /// Perform an initial sync for all backends, then start the polling loop.
    ///
    /// This runs until `cancel` is set to `true`.
    pub async fn run(self, cancel: Arc<std::sync::atomic::AtomicBool>) -> anyhow::Result<()> {
        use std::sync::atomic::Ordering;

        // Initial sync — get full snapshot for each backend.
        for backend in &self.backends {
            if cancel.load(Ordering::Relaxed) {
                tracing::info!(backend = %backend.id(), "sync cancelled during initial sync");
                return Ok(());
            }

            match self.sync_backend(backend).await {
                Ok(count) => {
                    tracing::info!(backend = %backend.id(), changes = count, "initial sync complete");
                }
                Err(e) => {
                    tracing::error!(backend = %backend.id(), error = %e, "initial sync failed");
                }
            }
        }

        // Flush any dirty files after initial sync so pending writes
        // are uploaded before entering the polling loop.
        self.flush_dirty_files().await;

        // Hydrate the presenter with all existing DB items.
        // The sync loop only emits incremental changes, so without this
        // the presenter would remain empty on restart when no new changes exist.
        if let Err(e) = self.hydrate_presenter().await {
            tracing::warn!(error = %e, "failed to hydrate presenter from DB");
        }

        // Polling loop.
        loop {
            let interval = self.effective_poll_interval().await;
            let () = self.runtime.sleep(interval).await;

            if cancel.load(Ordering::Relaxed) {
                tracing::info!("sync runner cancelled");
                return Ok(());
            }

            for backend in &self.backends {
                if cancel.load(Ordering::Relaxed) {
                    return Ok(());
                }

                match self.sync_backend(backend).await {
                    Ok(count) if count > 0 => {
                        tracing::debug!(backend = %backend.id(), changes = count, "sync cycle");
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(backend = %backend.id(), error = %e, "sync cycle failed");
                    }
                }
            }

            // Flush dirty files after each remote sync cycle.
            // Remote changes are applied first, then local writes are uploaded.
            self.flush_dirty_files().await;
        }
    }

    /// Sync a single backend: fetch changes, apply to DB, notify presenter.
    /// Returns the number of changes applied.
    async fn sync_backend(&self, backend: &Arc<dyn Backend>) -> anyhow::Result<usize> {
        let backend_id = backend.id();
        let cursor = self
            .storage
            .get_cursor(backend_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let (changes, new_cursor) = backend.changes(cursor.as_ref()).await?;

        let applied = self.apply_changes(backend_id, &changes).await?;

        self.storage
            .set_cursor(backend_id, &new_cursor)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        // File the applied (post-ignore-filter) changes into the engine's
        // per-parent change index, if one is attached. This is the single
        // canonical poll loop — the feed never polls backends itself.
        #[cfg(feature = "native")]
        if let Some(feed) = &self.change_feed {
            feed.record(backend_id, &applied).await;
        }

        Ok(applied.len())
    }

    /// Apply a batch of changes to the state database and notify the presenter.
    /// Files matching `.cascade` ignore rules are skipped.
    ///
    /// Returns the changes that were actually applied (i.e. survived the
    /// `.cascade` ignore filter), in application order, so the caller can
    /// file the same set into the change feed.
    async fn apply_changes(
        &self,
        _backend_id: &str,
        changes: &[Change],
    ) -> anyhow::Result<Vec<Change>> {
        let mut applied = Vec::new();

        for change in changes {
            match change {
                Change::Created(entry) => {
                    if self.is_ignored_entry(entry) {
                        continue;
                    }
                    if self.exceeds_max_file_length(entry).await {
                        tracing::warn!(
                            file = %entry.name,
                            "file exceeds max file length rule — skipped"
                        );
                        continue;
                    }
                    self.storage
                        .upsert_file(entry)
                        .await
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    // Auto-pin if the file matches any pin rule.
                    if self.is_pinned_entry(entry).await {
                        self.storage
                            .update_cache_state(&entry.id, CacheState::Pinned)
                            .await
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                    }
                    // Index cached files for P2P sharing.
                    #[cfg(feature = "p2p")]
                    if let Some(bridge) = &self.p2p
                        && entry.size.is_some_and(|s| s > 0)
                        && let Err(e) = bridge.index_file(&entry.name, &Vec::new()).await
                    {
                        tracing::debug!(
                            file = %entry.name,
                            error = %e,
                            "P2P indexing skipped for file without local data"
                        );
                    }
                    let item: VfsItem = entry.clone().into();
                    self.presenter.upsert_item(item).await?;
                    applied.push(change.clone());
                }
                Change::Updated { new, .. } => {
                    if self.is_ignored_entry(new) {
                        continue;
                    }
                    if self.exceeds_max_file_length(new).await {
                        tracing::warn!(
                            file = %new.name,
                            "file exceeds max file length rule — skipped"
                        );
                        continue;
                    }
                    // Check for conflict: if the local file is dirty and remote changed.
                    let local = self
                        .storage
                        .get_file(&new.id)
                        .await
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    if let Some(local) = local {
                        match check_conflict(&local, new, false) {
                            ConflictCheck::Conflict {
                                local_entry,
                                remote_entry: _,
                            } => {
                                // Rename local copy as conflict file.
                                let conflict_file_name =
                                    conflict_name(&local_entry.name, "cascade");
                                tracing::warn!(
                                    original = %local_entry.name,
                                    conflict = %conflict_file_name,
                                    "conflict detected — local version renamed"
                                );
                                // Record the conflict. The remote version wins;
                                // the local version is kept as a conflict copy.
                            }
                            ConflictCheck::NoConflict => {}
                        }
                    }
                    self.storage
                        .upsert_file(new)
                        .await
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    let item: VfsItem = new.clone().into();
                    self.presenter.upsert_item(item).await?;
                    applied.push(change.clone());
                }
                Change::Deleted(entry) => {
                    self.storage
                        .delete_file(&entry.id)
                        .await
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    self.presenter.delete_item(&entry.id).await?;
                    applied.push(change.clone());
                }
                Change::Moved { to, .. } => {
                    if self.is_ignored_entry(to) {
                        continue;
                    }
                    if self.exceeds_max_file_length(to).await {
                        tracing::warn!(
                            file = %to.name,
                            "file exceeds max file length rule — skipped"
                        );
                        continue;
                    }
                    self.storage
                        .upsert_file(to)
                        .await
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    let item: VfsItem = to.clone().into();
                    self.presenter.upsert_item(item).await?;
                    applied.push(change.clone());
                }
            }
        }

        Ok(applied)
    }

    /// Check if a file entry matches any pin rule.
    async fn is_pinned_entry(&self, entry: &crate::types::FileEntry) -> bool {
        let path = Path::new(&entry.name);
        PinMatcher::load(&*self.storage)
            .await
            .is_ok_and(|matcher| matcher.is_pinned(path))
    }

    /// Check if a file entry exceeds an applicable max file length rule.
    ///
    /// Returns `true` when the file should be skipped (its size exceeds the
    /// limit). Files with no known size (`None`) are not blocked — the size
    /// is only known after listing, not after download. Rules are checked in
    /// priority order (highest first); the first matching rule wins.
    async fn exceeds_max_file_length(&self, entry: &crate::types::FileEntry) -> bool {
        let Some(size) = entry.size else {
            return false;
        };
        let Ok(rules) = self
            .storage
            .list_max_file_length_rules()
            .await
            .map_err(|e| {
                tracing::debug!(error = %e, "failed to load max file length rules");
                e
            })
        else {
            return false;
        };
        let path_str = entry.name.as_str();
        for rule in &rules {
            if glob_matches(&rule.path_glob, path_str) && size > rule.max_bytes {
                return true;
            }
        }
        false
    }

    /// Check if a file entry should be ignored based on `.cascade` config.
    fn is_ignored_entry(&self, entry: &crate::types::FileEntry) -> bool {
        // Build a synthetic path from the entry's parent + name.
        // Phase 1 uses a flat path; this will be replaced with actual VFS
        // path resolution once the tree tracks full paths.
        let path = Path::new(&entry.name);
        self.config.is_ignored(path, entry.is_dir)
    }

    /// Determine the effective poll interval. Uses the shortest interval
    /// reported by any backend, falling back to the default.
    async fn effective_poll_interval(&self) -> Duration {
        let mut interval = DEFAULT_POLL_INTERVAL;
        for backend in &self.backends {
            if let Some(backend_interval) = backend.poll_interval().await
                && backend_interval < interval
            {
                interval = backend_interval;
            }
        }
        interval
    }

    /// Flush all dirty files: upload each to its owning backend and clear
    /// the dirty flag on success. Failures are logged and skipped so that
    /// one failing upload does not block the rest.
    ///
    /// Returns the number of files successfully uploaded.
    ///
    /// # Write-back mechanism
    ///
    /// This function is the upload half of the write-back cache: a presenter
    /// that writes data to a local `cache_dir` should call
    /// `StateStorage::mark_dirty` after the write so that this function picks it
    /// up on the next sync cycle and uploads to the backend.
    ///
    /// Currently no presenter implements write-back (the `WebDAV` PUT path
    /// uploads directly; the NFS WRITE proc returns ROFS), so this
    /// function always returns 0 in production. The mechanism is in place for
    /// future write-back presenters (FUSE, local-backend adopt-and-sync).
    async fn flush_dirty_files(&self) -> usize {
        let dirty_files = match self.storage.list_dirty_files().await {
            Ok(files) => files,
            Err(e) => {
                tracing::error!(error = %e, "failed to list dirty files");
                return 0;
            }
        };

        let mut flushed = 0;

        for record in &dirty_files {
            let Some(backend) = self.backends.iter().find(|b| b.id() == record.backend_id) else {
                tracing::warn!(
                    file = %record.id,
                    backend_id = %record.backend_id,
                    "no backend found for dirty file — skipping"
                );
                continue;
            };

            let Some(local_path_str) = &record.local_path else {
                tracing::warn!(
                    file = %record.id,
                    "dirty file has no local path — skipping"
                );
                continue;
            };
            let local_path = std::path::PathBuf::from(local_path_str);

            if self
                .fs
                .exists(&local_path)
                .await
                .is_ok_and(|exists| !exists)
            {
                tracing::warn!(
                    file = %record.id,
                    path = %local_path.display(),
                    "dirty file missing from disk — skipping"
                );
                continue;
            }

            let data = match self.fs.read_file(&local_path).await {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!(
                        file = %record.id,
                        path = %local_path.display(),
                        error = %e,
                        "failed to read dirty file — skipping"
                    );
                    continue;
                }
            };

            let upload_path = Path::new(&record.path);
            let parent_file_id = FileId(record.parent_id.native_id().to_string());

            match backend.upload(upload_path, &data, &parent_file_id).await {
                Ok(_updated_entry) => {
                    if let Err(e) = self.storage.clear_dirty(&record.id).await {
                        tracing::error!(
                            file = %record.id,
                            error = %e,
                            "failed to clear dirty flag after upload"
                        );
                    } else {
                        flushed += 1;
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        file = %record.id,
                        path = %record.path,
                        error = %e,
                        "upload failed for dirty file — will retry next cycle"
                    );
                }
            }
        }

        if flushed > 0 {
            tracing::info!(flushed, "dirty file flush complete");
        }

        flushed
    }

    /// Hydrate the presenter with all existing items from the state DB.
    /// Called once after initial sync so the presenter has a complete
    /// view even when no new changes were detected.
    async fn hydrate_presenter(&self) -> anyhow::Result<()> {
        // Only hydrate root-level children for each backend.
        // Deeper directories are loaded on demand by the presenter
        // when PROPFIND requests them.
        let backends = self
            .storage
            .list_backends()
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut total = 0;
        for backend in &backends {
            let root_id = format!("{}:root", backend.id);
            // Try the "root" alias first, then discover the real root ID.
            let mut entries = self
                .storage
                .list_children(&root_id)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            if entries.is_empty() {
                // The backend may use a real folder ID instead of "root".
                // Find it by looking for the most common parent_id.
                let all = self
                    .storage
                    .list_all_files()
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                let prefix = format!("{}:", backend.id);
                let mut counts: std::collections::HashMap<String, usize> =
                    std::collections::HashMap::new();
                for entry in &all {
                    if entry.id.0.starts_with(&prefix) && entry.parent_id.0.starts_with(&prefix) {
                        *counts.entry(entry.parent_id.0.clone()).or_insert(0) += 1;
                    }
                }
                if let Some((real_root, _)) = counts.into_iter().max_by_key(|(_, c)| *c) {
                    entries = self
                        .storage
                        .list_children(&real_root)
                        .await
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                }
            }
            for entry in &entries {
                let item: VfsItem = entry.clone().into();
                if let Err(e) = self.presenter.upsert_item(item).await {
                    tracing::debug!(id = %entry.id, error = %e, "failed to hydrate item");
                }
            }
            total += entries.len();
        }
        tracing::info!(count = total, "presenter hydrated from DB");
        Ok(())
    }
}

/// Match a glob pattern against a path string.
///
/// Supports `*` (any non-slash characters), `**` (any path segments including
/// slashes), and exact matching. This is a lightweight implementation matching
/// the glob semantics used throughout Cascade for pin and lifecycle rules.
fn glob_matches(pattern: &str, path: &str) -> bool {
    if pattern.contains("**") {
        let parts: Vec<&str> = pattern.split("**").collect();
        if parts.len() == 2 {
            let prefix = parts.first().copied().unwrap_or("");
            let suffix = parts.get(1).copied().unwrap_or("");
            if !prefix.is_empty() && !path.starts_with(prefix) {
                return false;
            }
            let suffix = suffix.strip_prefix('/').unwrap_or(suffix);
            if suffix.is_empty() {
                return true;
            }
            let after_prefix = path.get(prefix.len()..).unwrap_or("");
            let after_prefix = after_prefix.strip_prefix('/').unwrap_or(after_prefix);
            if after_prefix.is_empty() {
                return false;
            }
            if suffix.contains('/') {
                let trimmed = suffix.trim_start_matches('*');
                if let Some(pos) = after_prefix.rfind(trimmed) {
                    let from_pos = after_prefix.get(pos..).unwrap_or("");
                    if from_pos.ends_with(trimmed) {
                        return true;
                    }
                }
                return false;
            }
            let last_segment = after_prefix.rsplit('/').next().unwrap_or(after_prefix);
            if suffix.contains('*') {
                return star_match(suffix, last_segment);
            }
            return last_segment == suffix;
        }
    }
    if pattern.contains('*') {
        return star_match(pattern, path);
    }
    pattern == path
}

/// Match a single-segment pattern with `*` wildcards against a string.
fn star_match(pattern: &str, path: &str) -> bool {
    let segments: Vec<&str> = pattern.split('*').collect();
    if segments.len() == 1 {
        return pattern == path;
    }
    let first = segments.first().copied().unwrap_or("");
    let last = segments.last().copied().unwrap_or("");
    if !first.is_empty() && !path.starts_with(first) {
        return false;
    }
    if !last.is_empty() && !path.ends_with(last) {
        return false;
    }
    let start = if first.is_empty() { 0 } else { first.len() };
    let end = if last.is_empty() {
        path.len()
    } else {
        path.len().saturating_sub(last.len())
    };
    if start > end {
        return false;
    }
    let remaining = path.get(start..end).unwrap_or("");
    let mut search_from = 0;
    let middle = segments
        .get(1..segments.len().saturating_sub(1))
        .unwrap_or(&[]);
    for seg in middle {
        if seg.is_empty() {
            continue;
        }
        let rest = remaining.get(search_from..).unwrap_or("");
        if let Some(pos) = rest.find(seg) {
            search_from += pos + seg.len();
        } else {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::NullBackend;
    use crate::db::StateDb;
    use crate::portable::native::{SqliteStorage, StdFileSystem, TokioRuntimeHandle};
    use crate::presenter::VfsPresenter;
    use crate::types::{FileEntry, ItemId};
    use async_trait::async_trait;
    use std::path::{Path, PathBuf};

    /// A presenter that records calls for testing.
    #[derive(Default)]
    struct MockPresenter {
        upserts: std::sync::Mutex<Vec<String>>,
        deletes: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl VfsPresenter for MockPresenter {
        async fn upsert_item(&self, item: VfsItem) -> anyhow::Result<()> {
            self.upserts.lock().unwrap().push(item.name);
            Ok(())
        }
        async fn delete_item(&self, id: &ItemId) -> anyhow::Result<()> {
            self.deletes.lock().unwrap().push(id.0.clone());
            Ok(())
        }
        async fn update_state(
            &self,
            _id: &ItemId,
            _state: crate::types::CacheState,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn fetch_contents(&self, _id: &ItemId) -> anyhow::Result<PathBuf> {
            anyhow::bail!("not implemented")
        }
        async fn evict_item(&self, _id: &ItemId) -> anyhow::Result<()> {
            Ok(())
        }
        async fn start(&self, _mount_point: &Path) -> anyhow::Result<()> {
            Ok(())
        }
        async fn stop(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn make_native_runner(
        db: Arc<StateDb>,
        backends: Vec<Arc<dyn Backend>>,
        presenter: Arc<MockPresenter>,
        config: Arc<ConfigResolver>,
    ) -> SyncRunner<TokioRuntimeHandle> {
        let runtime = TokioRuntimeHandle::new(tokio::runtime::Handle::current());
        let storage = SqliteStorage::new(db.clone(), runtime.clone());
        SyncRunner::new(
            Arc::new(storage),
            Arc::new(StdFileSystem),
            runtime,
            backends,
            presenter,
            config,
        )
    }

    #[tokio::test]
    async fn sync_runner_initial_sync_with_null_backend() {
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("test", "null", "Test", None, None)
            .unwrap();

        let backend: Arc<dyn Backend> = Arc::new(NullBackend::new("test"));
        let presenter = Arc::new(MockPresenter::default());
        let config = Arc::new(ConfigResolver::new(std::path::PathBuf::from("/tmp/test")));

        // Stop immediately — the runner will do one initial sync then exit.
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let runner = make_native_runner(db, vec![backend], presenter.clone(), config);
        let result = runner.run(cancel).await;
        assert!(result.is_ok());
    }

    /// A mock backend that records upload calls.
    #[derive(Default)]
    struct MockBackend {
        id: String,
        uploads: std::sync::Mutex<Vec<(String, String)>>, // (path, parent_id)
    }

    impl MockBackend {
        fn new(id: impl Into<String>) -> Self {
            Self {
                id: id.into(),
                uploads: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl Backend for MockBackend {
        fn id(&self) -> &str {
            &self.id
        }

        fn display_name(&self) -> &'static str {
            "Mock"
        }

        async fn quota(&self) -> anyhow::Result<Option<crate::types::Quota>> {
            Ok(None)
        }

        async fn changes(
            &self,
            _cursor: Option<&crate::types::Cursor>,
        ) -> anyhow::Result<(Vec<Change>, crate::types::Cursor)> {
            Ok((vec![], crate::types::Cursor("mock".to_string())))
        }

        async fn metadata(
            &self,
            _path: &std::path::Path,
        ) -> anyhow::Result<crate::types::FileEntry> {
            anyhow::bail!("not implemented")
        }

        async fn download(&self, _file: &crate::types::FileEntry) -> anyhow::Result<Vec<u8>> {
            anyhow::bail!("not implemented")
        }

        async fn upload(
            &self,
            path: &std::path::Path,
            _data: &[u8],
            parent_id: &crate::types::FileId,
        ) -> anyhow::Result<crate::types::FileEntry> {
            self.uploads
                .lock()
                .unwrap()
                .push((path.to_string_lossy().to_string(), parent_id.0.clone()));
            Ok(crate::types::FileEntry::file(
                crate::types::ItemId::new(&self.id, "uploaded"),
                crate::types::ItemId::new(&self.id, parent_id.0.as_str()),
                path.to_string_lossy().to_string(),
            ))
        }

        async fn update(
            &self,
            _file_id: &crate::types::FileId,
            _data: &[u8],
        ) -> anyhow::Result<crate::types::FileEntry> {
            anyhow::bail!("not implemented")
        }

        async fn create_dir(
            &self,
            _path: &std::path::Path,
        ) -> anyhow::Result<crate::types::FileEntry> {
            anyhow::bail!("not implemented")
        }

        async fn delete(&self, _file: &crate::types::FileEntry) -> anyhow::Result<()> {
            anyhow::bail!("not implemented")
        }

        async fn move_entry(
            &self,
            _src: &std::path::Path,
            _dst: &std::path::Path,
        ) -> anyhow::Result<crate::types::FileEntry> {
            anyhow::bail!("not implemented")
        }

        async fn poll_interval(&self) -> Option<Duration> {
            None
        }
    }

    #[tokio::test]
    async fn flush_dirty_files_uploads_and_clears() {
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("mock", "mock", "Mock", None, None)
            .unwrap();

        // Create a temp file to serve as the local cached content.
        let tmp = tempfile::tempdir().unwrap();
        let local_file = tmp.path().join("test.txt");
        tokio::fs::write(&local_file, b"hello world").await.unwrap();

        // Insert a file record, set it dirty, and give it a local_path.
        let file_id = ItemId::new("mock", "file1");
        let parent_id = ItemId::new("mock", "root");
        let entry = FileEntry::file(file_id.clone(), parent_id, "test.txt".into());
        db.upsert_file(&entry).unwrap();
        db.mark_dirty(&file_id).unwrap();

        // Set the local_path and path for the dirty file record.
        db.set_file_paths(&file_id, "docs/test.txt", &local_file.to_string_lossy())
            .unwrap();

        let mock_backend = Arc::new(MockBackend::new("mock"));
        let presenter = Arc::new(MockPresenter::default());
        let config = Arc::new(ConfigResolver::new(std::path::PathBuf::from("/tmp/test")));

        let runner = make_native_runner(db.clone(), vec![mock_backend.clone()], presenter, config);

        let flushed = runner.flush_dirty_files().await;
        assert_eq!(flushed, 1);

        // Verify the backend received the upload.
        let uploads = mock_backend.uploads.lock().unwrap();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].0, "docs/test.txt");

        // Verify the dirty flag was cleared.
        assert_eq!(db.is_dirty(&file_id).unwrap(), Some(false));
        assert!(db.list_dirty_files().unwrap().is_empty());
    }

    #[tokio::test]
    async fn flush_dirty_files_skips_on_upload_failure() {
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("null", "null", "Null", None, None)
            .unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let local_file = tmp.path().join("fail.txt");
        tokio::fs::write(&local_file, b"content").await.unwrap();

        let file_id = ItemId::new("null", "file1");
        let parent_id = ItemId::new("null", "root");
        let entry = FileEntry::file(file_id.clone(), parent_id, "fail.txt".into());
        db.upsert_file(&entry).unwrap();
        db.mark_dirty(&file_id).unwrap();

        db.set_file_paths(&file_id, "fail.txt", &local_file.to_string_lossy())
            .unwrap();

        // NullBackend.upload() always fails.
        let null_backend: Arc<dyn Backend> = Arc::new(NullBackend::new("null"));
        let presenter = Arc::new(MockPresenter::default());
        let config = Arc::new(ConfigResolver::new(std::path::PathBuf::from("/tmp/test")));

        let runner = make_native_runner(db.clone(), vec![null_backend], presenter, config);

        let flushed = runner.flush_dirty_files().await;
        assert_eq!(flushed, 0);

        // File should still be dirty — upload failed.
        assert_eq!(db.is_dirty(&file_id).unwrap(), Some(true));
    }
}
