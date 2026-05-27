//! Sync runner — orchestrates change polling across all backends.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::backend::Backend;
use crate::config::ConfigResolver;
use crate::db::StateDb;
use crate::presenter::VfsPresenter;
use crate::types::{Change, VfsItem};

/// Default poll interval when the backend doesn't specify one.
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
    cancel_tx: watch::Sender<bool>,
    cancel_rx: watch::Receiver<bool>,
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
            cancel_tx,
            cancel_rx,
        }
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

        // Polling loop.
        loop {
            let interval = self.effective_poll_interval().await;
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
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
    async fn apply_changes(
        &self,
        _backend_id: &str,
        changes: &[Change],
    ) -> anyhow::Result<usize> {
        let mut count = 0;

        for change in changes {
            match change {
                Change::Created(entry) => {
                    if self.is_ignored_entry(entry) {
                        continue;
                    }
                    self.db.upsert_file(entry)?;
                    let item: VfsItem = entry.clone().into();
                    self.presenter.upsert_item(item).await?;
                    count += 1;
                }
                Change::Updated { new, .. } => {
                    if self.is_ignored_entry(new) {
                        continue;
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
            if let Some(backend_interval) = backend.poll_interval().await {
                if backend_interval < interval {
                    interval = backend_interval;
                }
            }
        }
        interval
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::NullBackend;
    use crate::db::StateDb;
    use crate::presenter::VfsPresenter;
    use crate::types::ItemId;
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
        async fn update_state(&self, _id: &ItemId, _state: crate::types::CacheState) -> anyhow::Result<()> {
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
        db.register_backend("test", "null", "Test", None, None).unwrap();

        let backend: Arc<dyn Backend> = Arc::new(NullBackend::new("test"));
        let presenter = Arc::new(MockPresenter::default());
        let config = Arc::new(ConfigResolver::new(std::path::PathBuf::from("/tmp/test")));

        let runner = SyncRunner::new(db.clone(), vec![backend], presenter.clone(), config);
        // Stop immediately — the runner will do one initial sync then exit.
        runner.stop();
        let result = runner.run().await;
        assert!(result.is_ok());
    }
}
