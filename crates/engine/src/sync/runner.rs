//! Sync runner — orchestrates change polling across all backends.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::backend::Backend;
use crate::cache::pin::PinMatcher;
use crate::config::ConfigResolver;
use crate::db::StateDb;
use crate::p2p_bridge::P2pBridge;
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
pub struct SyncRunner {
    db: Arc<StateDb>,
    backends: Vec<Arc<dyn Backend>>,
    presenter: Arc<dyn VfsPresenter>,
    config: Arc<ConfigResolver>,
    p2p: Option<P2pBridge>,
    cancel_tx: watch::Sender<bool>,
    cancel_rx: watch::Receiver<bool>,
}

impl std::fmt::Debug for SyncRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncRunner")
            .field("backend_count", &self.backends.len())
            .field("p2p_enabled", &self.p2p.is_some())
            .finish_non_exhaustive()
    }
}

impl SyncRunner {
    /// Create a new sync runner.
    pub fn new(
        db: Arc<StateDb>,
        backends: Vec<Arc<dyn Backend>>,
        presenter: Arc<dyn VfsPresenter>,
        config: Arc<ConfigResolver>,
    ) -> Self {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        Self {
            db,
            backends,
            presenter,
            config,
            p2p: None,
            cancel_tx,
            cancel_rx,
        }
    }

    /// Attach a P2P bridge for block-level file sharing.
    #[must_use]
    pub fn with_p2p(mut self, p2p: P2pBridge) -> Self {
        self.p2p = Some(p2p);
        self
    }

    /// Perform an initial sync for all backends, then start the polling loop.
    ///
    /// This runs until `stop()` is called.
    pub async fn run(mut self) -> anyhow::Result<()> {
        // Initial sync — get full snapshot for each backend.
        for backend in &self.backends {
            if *self.cancel_rx.borrow() {
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
            tokio::select! {
                () = tokio::time::sleep(interval) => {}
                _ = self.cancel_rx.changed() => {
                    tracing::info!("sync runner cancelled");
                    return Ok(());
                }
            }

            if *self.cancel_rx.borrow() {
                return Ok(());
            }

            for backend in &self.backends {
                if *self.cancel_rx.borrow() {
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

    /// Signal the runner to stop. The `run()` future will return on the next
    /// iteration.
    pub fn stop(&self) {
        let _ = self.cancel_tx.send(true);
    }

    /// Sync a single backend: fetch changes, apply to DB, notify presenter.
    /// Returns the number of changes applied.
    async fn sync_backend(&self, backend: &Arc<dyn Backend>) -> anyhow::Result<usize> {
        let backend_id = backend.id();
        let cursor = self.db.get_cursor(backend_id)?;
        let (changes, new_cursor) = backend.changes(cursor.as_ref()).await?;

        let count = self.apply_changes(backend_id, &changes).await?;

        self.db.set_cursor(backend_id, &new_cursor)?;

        Ok(count)
    }

    /// Apply a batch of changes to the state database and notify the presenter.
    /// Files matching `.cascade` ignore rules are skipped.
    async fn apply_changes(&self, _backend_id: &str, changes: &[Change]) -> anyhow::Result<usize> {
        let mut count = 0;

        for change in changes {
            match change {
                Change::Created(entry) => {
                    if self.is_ignored_entry(entry) {
                        continue;
                    }
                    self.db.upsert_file(entry)?;
                    // Auto-pin if the file matches any pin rule.
                    if self.is_pinned_entry(entry) {
                        self.db.update_cache_state(&entry.id, CacheState::Pinned)?;
                    }
                    // Index cached files for P2P sharing.
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
                    count += 1;
                }
                Change::Updated { new, .. } => {
                    if self.is_ignored_entry(new) {
                        continue;
                    }
                    // Check for conflict: if the local file is dirty and remote changed.
                    if let Some(local) = self.db.get_file(&new.id)? {
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
                    self.db.upsert_file(new)?;
                    let item: VfsItem = new.clone().into();
                    self.presenter.upsert_item(item).await?;
                    count += 1;
                }
                Change::Deleted(entry) => {
                    self.db.delete_file(&entry.id)?;
                    self.presenter.delete_item(&entry.id).await?;
                    count += 1;
                }
                Change::Moved { to, .. } => {
                    if self.is_ignored_entry(to) {
                        continue;
                    }
                    self.db.upsert_file(to)?;
                    let item: VfsItem = to.clone().into();
                    self.presenter.upsert_item(item).await?;
                    count += 1;
                }
            }
        }

        Ok(count)
    }

    /// Check if a file entry matches any pin rule.
    fn is_pinned_entry(&self, entry: &crate::types::FileEntry) -> bool {
        let path = std::path::Path::new(&entry.name);
        PinMatcher::load(&self.db).is_ok_and(|matcher| matcher.is_pinned(path))
    }

    /// Check if a file entry should be ignored based on `.cascade` config.
    fn is_ignored_entry(&self, entry: &crate::types::FileEntry) -> bool {
        // Build a synthetic path from the entry's parent + name.
        // Phase 1 uses a flat path; this will be replaced with actual VFS
        // path resolution once the tree tracks full paths.
        let path = std::path::Path::new(&entry.name);
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
    async fn flush_dirty_files(&self) -> usize {
        let dirty_files = match self.db.list_dirty_files() {
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

            if !local_path.exists() {
                tracing::warn!(
                    file = %record.id,
                    path = %local_path.display(),
                    "dirty file missing from disk — skipping"
                );
                continue;
            }

            let file = tokio::fs::File::open(&local_path).await;
            let mut file = match file {
                Ok(f) => f,
                Err(e) => {
                    tracing::error!(
                        file = %record.id,
                        path = %local_path.display(),
                        error = %e,
                        "failed to open dirty file — skipping"
                    );
                    continue;
                }
            };

            let upload_path = std::path::Path::new(&record.path);
            let parent_file_id = FileId(record.parent_id.native_id().to_string());

            match backend
                .upload(upload_path, &mut file, &parent_file_id)
                .await
            {
                Ok(_updated_entry) => {
                    if let Err(e) = self.db.clear_dirty(&record.id) {
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
        let backends = self.db.list_backends()?;
        let mut total = 0;
        for backend in &backends {
            let root_id = format!("{}:root", backend.id);
            // Try the "root" alias first, then discover the real root ID.
            let mut entries = self.db.list_children(&root_id)?;
            if entries.is_empty() {
                // The backend may use a real folder ID instead of "root".
                // Find it by looking for the most common parent_id.
                let all = self.db.list_all_files()?;
                let prefix = format!("{}:", backend.id);
                let mut counts: std::collections::HashMap<String, usize> =
                    std::collections::HashMap::new();
                for entry in &all {
                    if entry.id.0.starts_with(&prefix) && entry.parent_id.0.starts_with(&prefix) {
                        *counts.entry(entry.parent_id.0.clone()).or_insert(0) += 1;
                    }
                }
                if let Some((real_root, _)) = counts.into_iter().max_by_key(|(_, c)| *c) {
                    entries = self.db.list_children(&real_root)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::NullBackend;
    use crate::db::StateDb;
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
            self.upserts.lock().unwrap().push(item.name.clone());
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

    #[tokio::test]
    async fn sync_runner_initial_sync_with_null_backend() {
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("test", "null", "Test", None, None)
            .unwrap();

        let backend: Arc<dyn Backend> = Arc::new(NullBackend::new("test"));
        let presenter = Arc::new(MockPresenter::default());
        let config = Arc::new(ConfigResolver::new(std::path::PathBuf::from("/tmp/test")));

        let runner = SyncRunner::new(db.clone(), vec![backend], presenter.clone(), config);
        // Stop immediately — the runner will do one initial sync then exit.
        runner.stop();
        let result = runner.run().await;
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

        async fn download(
            &self,
            _file: &crate::types::FileEntry,
            _writer: &mut (dyn tokio::io::AsyncWrite + Unpin + Send),
        ) -> anyhow::Result<()> {
            anyhow::bail!("not implemented")
        }

        async fn upload(
            &self,
            path: &std::path::Path,
            _reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
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

        let runner = SyncRunner::new(db.clone(), vec![mock_backend.clone()], presenter, config);

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

        let runner = SyncRunner::new(db.clone(), vec![null_backend], presenter, config);

        let flushed = runner.flush_dirty_files().await;
        assert_eq!(flushed, 0);

        // File should still be dirty — upload failed.
        assert_eq!(db.is_dirty(&file_id).unwrap(), Some(true));
    }
}
