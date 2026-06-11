//! Sync runner — orchestrates change polling across all backends.

use std::path::{Path, PathBuf};
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
use crate::sync::mount_path::{apply_mount_prefix, strip_mount_prefix};
use crate::types::{CacheState, Change, FileEntry, FileId, ItemId, VfsItem};

/// Default poll interval when the backend doesn't specify one.
#[allow(unknown_lints, clippy::duration_suboptimal_units)]
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(60);

/// A backend paired with the VFS mount prefix it is mounted at in the tree.
///
/// The runner sources both halves from the same `VfsTree` mount table the
/// router uses ([`VfsTree::children`](crate::vfs::VfsTree::children)), so the
/// prefix the runner stamps into each item's path cannot drift from the prefix
/// the router resolves on. A backend mounted at `/` carries the empty prefix,
/// for which [`apply_mount_prefix`]/[`strip_mount_prefix`] are no-ops and the
/// VFS path is byte-identical to the backend-relative path.
#[derive(Clone)]
pub struct MountedRunnerBackend {
    /// The VFS prefix the backend is mounted at (empty for a backend at `/`).
    pub mount_prefix: PathBuf,
    /// The backend instance.
    pub backend: Arc<dyn Backend>,
}

impl MountedRunnerBackend {
    /// Pair a backend with its VFS mount prefix.
    #[must_use]
    pub fn new(mount_prefix: PathBuf, backend: Arc<dyn Backend>) -> Self {
        Self {
            mount_prefix,
            backend,
        }
    }
}

impl std::fmt::Debug for MountedRunnerBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MountedRunnerBackend")
            .field("mount_prefix", &self.mount_prefix)
            .field("backend_id", &self.backend.id())
            .finish()
    }
}

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
    backends: Vec<MountedRunnerBackend>,
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
        backends: Vec<MountedRunnerBackend>,
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
        for mounted in &self.backends {
            if cancel.load(Ordering::Relaxed) {
                tracing::info!(backend = %mounted.backend.id(), "sync cancelled during initial sync");
                return Ok(());
            }

            match self.sync_backend(mounted).await {
                Ok(count) => {
                    tracing::info!(backend = %mounted.backend.id(), changes = count, "initial sync complete");
                }
                Err(e) => {
                    tracing::error!(backend = %mounted.backend.id(), error = %e, "initial sync failed");
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

            for mounted in &self.backends {
                if cancel.load(Ordering::Relaxed) {
                    return Ok(());
                }

                match self.sync_backend(mounted).await {
                    Ok(count) if count > 0 => {
                        tracing::debug!(backend = %mounted.backend.id(), changes = count, "sync cycle");
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(backend = %mounted.backend.id(), error = %e, "sync cycle failed");
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
    async fn sync_backend(&self, mounted: &MountedRunnerBackend) -> anyhow::Result<usize> {
        let backend = &mounted.backend;
        let backend_id = backend.id();
        let cursor = self
            .storage
            .get_cursor(backend_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let (changes, new_cursor) = backend.changes(cursor.as_ref()).await?;

        let applied = self.apply_changes(mounted, &changes).await?;

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
    ///
    /// # Path assembly
    ///
    /// For every `Created`, `Updated`, and `Moved` entry the runner assembles
    /// the item's full, mount-prefixed VFS path *once, on the way in*, before
    /// any of the four downstream consumers see it (`upsert_file`,
    /// `presenter.upsert_item`, and the `is_ignored_entry` / `is_pinned_entry`
    /// / `exceeds_max_file_length` rule checks). The assembled path is the
    /// mount prefix joined with the parent's already-stored VFS path joined
    /// with the entry's basename — see [`Self::repath_entry`]. A backend
    /// mounted at `/` carries the empty prefix, so its assembled path is
    /// byte-identical to the pre-refactor mount-relative path.
    ///
    /// `Deleted` carries no surviving entry to re-path: deletion is keyed on
    /// the stable [`ItemId`], which the mount prefix never
    /// touches.
    async fn apply_changes(
        &self,
        mounted: &MountedRunnerBackend,
        changes: &[Change],
    ) -> anyhow::Result<Vec<Change>> {
        let prefix = &mounted.mount_prefix;
        let backend = &mounted.backend;
        let mut applied = Vec::new();

        for change in changes {
            // Re-assemble the surviving entry of each variant with its full,
            // mount-prefixed VFS path stamped into `entry.path`. The applied
            // change pushed onto the result carries the re-pathed entry so the
            // change feed and any later consumer observe the same VFS path.
            let repathed: Option<Change> = match change {
                Change::Created(entry) => {
                    let entry = self.repath_entry(prefix, backend.as_ref(), entry).await?;
                    Some(Change::Created(entry))
                }
                Change::Updated { old, new } => {
                    let new = self.repath_entry(prefix, backend.as_ref(), new).await?;
                    Some(Change::Updated {
                        old: old.clone(),
                        new,
                    })
                }
                Change::Moved { from, to } => {
                    let to = self.repath_entry(prefix, backend.as_ref(), to).await?;
                    Some(Change::Moved {
                        from: from.clone(),
                        to,
                    })
                }
                // Deletion is keyed on the stable ItemId; there is no path to
                // assemble.
                Change::Deleted(_) => None,
            };

            match repathed.as_ref().unwrap_or(change) {
                Change::Created(entry) => {
                    if self.is_ignored_entry(entry) {
                        continue;
                    }
                    if self.exceeds_max_file_length(entry).await {
                        tracing::warn!(
                            path = %entry.path,
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
                        && let Err(e) = bridge.index_file(&entry.path, &Vec::new()).await
                    {
                        tracing::debug!(
                            path = %entry.path,
                            error = %e,
                            "P2P indexing skipped for file without local data"
                        );
                    }
                    let item: VfsItem = entry.clone().into();
                    self.presenter.upsert_item(item).await?;
                }
                Change::Updated { new, .. } => {
                    if self.is_ignored_entry(new) {
                        continue;
                    }
                    if self.exceeds_max_file_length(new).await {
                        tracing::warn!(
                            path = %new.path,
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
                }
                Change::Deleted(entry) => {
                    self.storage
                        .delete_file(&entry.id)
                        .await
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    self.presenter.delete_item(&entry.id).await?;
                }
                Change::Moved { to, .. } => {
                    if self.is_ignored_entry(to) {
                        continue;
                    }
                    if self.exceeds_max_file_length(to).await {
                        tracing::warn!(
                            path = %to.path,
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
                }
            }

            applied.push(repathed.unwrap_or_else(|| change.clone()));
        }

        Ok(applied)
    }

    /// Return a clone of `entry` with its full, mount-prefixed VFS path stamped
    /// into [`FileEntry::path`].
    ///
    /// The parent's already-stored VFS path (the full, mount-prefixed
    /// `files.path` value) is looked up via [`StateStorage::get_file_path`] and
    /// the entry's basename is joined onto it. Parents precede their children in
    /// a backend's change stream, so the parent path is present by the time the
    /// child is processed.
    ///
    /// When the parent has no stored path, the parent is either a recognised
    /// backend root container — in which case the entry sits directly under the
    /// mount prefix and its path is `apply_mount_prefix(prefix, name)` — or a
    /// genuine ordering bug (a child arrived before its parent), in which case
    /// this fails loudly rather than silently falling back to the basename.
    async fn repath_entry(
        &self,
        prefix: &Path,
        backend: &dyn Backend,
        entry: &FileEntry,
    ) -> anyhow::Result<FileEntry> {
        let vfs_path = match self
            .storage
            .get_file_path(&entry.parent_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?
        {
            // The parent's stored path is already the full, mount-prefixed VFS
            // path; the child sits directly beneath it.
            Some(parent_path) => format!("{parent_path}/{}", entry.name),
            // No stored parent: a recognised backend root container places the
            // entry directly under the mount prefix.
            None if backend.is_root_native_id(entry.parent_id.native_id()) => {
                apply_mount_prefix(prefix, &entry.name)
            }
            None => anyhow::bail!(
                "cannot assemble VFS path: parent {:?} has no stored path and is \
                 not a backend root — child applied before its parent",
                entry.parent_id,
            ),
        };
        Ok(entry.clone().with_path(vfs_path))
    }

    /// Check if a file entry matches any pin rule.
    ///
    /// Pin globs are matched against the entry's full, mount-prefixed VFS path,
    /// the same path the entry is stored under. For a backend mounted at `/`
    /// the prefix is empty, so the matched string is byte-identical to the
    /// pre-refactor mount-relative path and existing pin rules behave
    /// unchanged.
    async fn is_pinned_entry(&self, entry: &crate::types::FileEntry) -> bool {
        let path = Path::new(&entry.path);
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
        // Match against the full, mount-prefixed VFS path. At-root backends
        // carry an empty prefix, so this is byte-identical to the pre-refactor
        // mount-relative path.
        let path_str = entry.path.as_str();
        for rule in &rules {
            if glob_matches(&rule.path_glob, path_str) && size > rule.max_bytes {
                return true;
            }
        }
        false
    }

    /// Check if a file entry should be ignored based on `.cascade` config.
    ///
    /// Ignore rules are matched against the entry's full, mount-prefixed VFS
    /// path — the same anchoring as the pin and max-file-length checks. For a
    /// backend mounted at `/` the prefix is empty, so the matched string is
    /// byte-identical to the pre-refactor mount-relative path.
    fn is_ignored_entry(&self, entry: &crate::types::FileEntry) -> bool {
        let path = Path::new(&entry.path);
        self.config.is_ignored(path, entry.is_dir)
    }

    /// Determine the effective poll interval. Uses the shortest interval
    /// reported by any backend, falling back to the default.
    async fn effective_poll_interval(&self) -> Duration {
        let mut interval = DEFAULT_POLL_INTERVAL;
        for mounted in &self.backends {
            if let Some(backend_interval) = mounted.backend.poll_interval().await
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
            let Some(mounted) = self
                .backends
                .iter()
                .find(|m| m.backend.id() == record.backend_id)
            else {
                tracing::warn!(
                    file = %record.id,
                    backend_id = %record.backend_id,
                    "no backend found for dirty file — skipping"
                );
                continue;
            };
            let backend = &mounted.backend;

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

            // The stored `record.path` is the full, mount-prefixed VFS path.
            // The backend understands only its own native, mount-relative paths,
            // so strip the mount prefix on the way out — the exact inverse of
            // the assembly the runner performed on the way in. A backend mounted
            // at `/` carries the empty prefix, so this is a no-op and the path
            // is byte-identical to the pre-refactor value.
            let Some(native_path) = strip_mount_prefix(&mounted.mount_prefix, &record.path) else {
                tracing::error!(
                    file = %record.id,
                    path = %record.path,
                    mount_prefix = %mounted.mount_prefix.display(),
                    "dirty file path does not lie under its backend's mount prefix \
                     — routing bug, skipping"
                );
                continue;
            };
            let upload_path = PathBuf::from(native_path);
            let parent_file_id = FileId(record.parent_id.native_id().to_string());

            match backend.upload(&upload_path, &data, &parent_file_id).await {
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

    /// Hydrate the presenter with the existing mount layout from the state DB.
    ///
    /// Called once after the initial sync so a cold restart shows the same view
    /// the lazy `read_dir`/`PROPFIND` path would build, without waiting for a new
    /// change to arrive. Two things are surfaced for each configured mount:
    ///
    /// 1. The mount's top-level entries — the immediate children of the backend's
    ///    declared root container ([`Backend::root_native_id`]), each carrying its
    ///    already-stored, mount-prefixed [`FileEntry::path`]. There is no
    ///    `{backend}:root` literal or most-common-parent heuristic: the backend
    ///    names its own root container.
    /// 2. The neutral root's own child — the synthetic mount-point directory for
    ///    a backend mounted directly beneath the neutral root (a single-segment
    ///    mount prefix), mirroring the directory [`VfsTree::read_dir`](crate::vfs::VfsTree::read_dir) injects
    ///    lazily so the mount point is visible from a cold start even when the
    ///    backend holds no files yet. A backend mounted at `/` (the empty prefix)
    ///    is the root itself and injects no mount-point directory; a nested mount
    ///    (a multi-segment prefix) is the child of another backend's subtree, not
    ///    of the neutral root, so `read_dir` injects it lazily and hydration
    ///    leaves it alone.
    ///
    /// Deeper directories stay lazy — presenters expand them on demand.
    ///
    /// The mount table ([`Self::backends`]) is the single source of truth for the
    /// prefix and root id, the same table the router resolves on, so hydration
    /// cannot disagree with routing about where a backend lives.
    async fn hydrate_presenter(&self) -> anyhow::Result<()> {
        let mut total = 0;
        for mounted in &self.backends {
            let backend = mounted.backend.as_ref();
            let root_id = ItemId::new(backend.id(), backend.root_native_id());

            // The mount's top-level entries: direct children of the backend's
            // root container, already stored at their full mount-prefixed paths.
            let entries = self
                .storage
                .list_children(&root_id.0)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            for entry in &entries {
                let item: VfsItem = entry.clone().into();
                if let Err(e) = self.presenter.upsert_item(item).await {
                    tracing::debug!(id = %entry.id, error = %e, "failed to hydrate item");
                }
            }
            total += entries.len();

            // The neutral root's child: the synthetic mount-point directory.
            // Inject it exactly once per top-level mount so the mount point shows
            // on a cold start regardless of content.
            if let Some(mount_dir) = mount_point_item(&mounted.mount_prefix, &root_id) {
                if let Err(e) = self.presenter.upsert_item(mount_dir).await {
                    tracing::debug!(prefix = %mounted.mount_prefix.display(), error = %e, "failed to hydrate mount point");
                } else {
                    total += 1;
                }
            }
        }
        tracing::info!(count = total, "presenter hydrated from DB");
        Ok(())
    }
}

/// Build the synthetic mount-point directory a top-level mount contributes to
/// the neutral root, or `None` when the mount contributes no such directory.
///
/// A mount-point directory exists only for a backend mounted directly beneath
/// the neutral root — a single-segment prefix such as `personal`. Its
/// `parent_id` is the neutral root's container so presenters list it under the
/// absolute root; its `id` is the backend's root container so a request that
/// descends into it routes straight to the owning backend; and its `path` is the
/// mount prefix itself.
///
/// Returns `None` for:
/// - the empty prefix (a backend mounted at `/`), which *is* the root rather
///   than a directory under it, so it injects nothing; and
/// - a multi-segment prefix (a nested mount), whose mount-point directory is a
///   child of another backend's subtree — [`VfsTree::read_dir`](crate::vfs::VfsTree::read_dir) injects that one
///   lazily into the parent backend's listing, so eager hydration must not also
///   surface it under the neutral root.
fn mount_point_item(prefix: &Path, root_id: &ItemId) -> Option<VfsItem> {
    let mut components = prefix.components();
    let first = components.next()?;
    // A nested mount has a further component beyond the first; its mount-point
    // directory belongs to the parent backend's subtree, not the neutral root.
    if components.next().is_some() {
        return None;
    }
    let name = first.as_os_str().to_string_lossy().into_owned();
    Some(VfsItem {
        id: root_id.clone(),
        parent_id: crate::vfs::neutral_root_item_id(),
        name,
        path: prefix.to_string_lossy().into_owned(),
        is_dir: true,
        size: None,
        mod_time: None,
        cache_state: CacheState::Online,
        mime_type: None,
    })
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
    ///
    /// `upserts` keeps only the item names for the many existing assertions that
    /// match on name; `upsert_items` keeps the whole [`VfsItem`] so tests that
    /// need the assembled path, parent, or id can inspect it.
    #[derive(Default)]
    struct MockPresenter {
        upserts: std::sync::Mutex<Vec<String>>,
        upsert_items: std::sync::Mutex<Vec<VfsItem>>,
        deletes: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl VfsPresenter for MockPresenter {
        async fn upsert_item(&self, item: VfsItem) -> anyhow::Result<()> {
            self.upserts.lock().unwrap().push(item.name.clone());
            self.upsert_items.lock().unwrap().push(item);
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

    /// Wrap a plain backend at the empty (at-root) mount prefix. Existing
    /// behaviour-preserving tests mount their single backend at `/`, so the
    /// assembled VFS path equals the mount-relative path and remains
    /// byte-identical to the pre-refactor model.
    fn at_root(backend: Arc<dyn Backend>) -> MountedRunnerBackend {
        MountedRunnerBackend::new(PathBuf::new(), backend)
    }

    /// Wrap a plain backend at an explicit, non-empty mount prefix.
    fn at(prefix: &str, backend: Arc<dyn Backend>) -> MountedRunnerBackend {
        MountedRunnerBackend::new(PathBuf::from(prefix), backend)
    }

    /// Build a runner from plain backends, each mounted at the root (empty)
    /// prefix. The behaviour-preserving default for tests that do not exercise
    /// nested mount paths.
    fn make_native_runner(
        db: Arc<StateDb>,
        backends: Vec<Arc<dyn Backend>>,
        presenter: Arc<dyn VfsPresenter>,
        config: Arc<ConfigResolver>,
    ) -> SyncRunner<TokioRuntimeHandle> {
        make_native_runner_mounted(
            db,
            backends.into_iter().map(at_root).collect(),
            presenter,
            config,
        )
    }

    /// A root-mounted `MountedRunnerBackend` for backend id `scr`, used by the
    /// `apply_changes` tests that drive the method directly (the runner itself
    /// holds no backends in those tests). The backend's default
    /// `is_root_native_id` recognises the `root` sentinel the test entries
    /// parent to.
    fn scr_mount() -> MountedRunnerBackend {
        at_root(Arc::new(ScriptedBackend::new("scr")))
    }

    /// Like [`scr_mount`] but at an explicit, non-empty mount prefix.
    fn scr_mount_at(prefix: &str) -> MountedRunnerBackend {
        at(prefix, Arc::new(ScriptedBackend::new("scr")))
    }

    /// Build a runner from backends already paired with their mount prefixes.
    fn make_native_runner_mounted(
        db: Arc<StateDb>,
        backends: Vec<MountedRunnerBackend>,
        presenter: Arc<dyn VfsPresenter>,
        config: Arc<ConfigResolver>,
    ) -> SyncRunner<TokioRuntimeHandle> {
        let runtime = TokioRuntimeHandle::new(tokio::runtime::Handle::current());
        let storage = SqliteStorage::new(db, runtime.clone());
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

    // ── Scripted backend for orchestration tests ──

    /// A backend whose `changes`, `poll_interval`, and error behaviour are
    /// scripted by the test. Records how many times `changes` was called so a
    /// test can assert that an empty change set still drove exactly one poll.
    struct ScriptedBackend {
        id: String,
        /// Batches handed out on successive `changes` calls. The last batch is
        /// reused once the script is exhausted so the polling loop keeps a
        /// stable steady state.
        batches: std::sync::Mutex<std::collections::VecDeque<Vec<Change>>>,
        /// When `true`, `changes` returns an error instead of a batch.
        fail_changes: bool,
        /// Reported poll interval, if any.
        poll_interval: Option<Duration>,
        /// Number of times `changes` has been invoked.
        changes_calls: std::sync::atomic::AtomicUsize,
        /// Cursor seen on the most recent `changes` call.
        last_cursor: std::sync::Mutex<Option<crate::types::Cursor>>,
    }

    impl ScriptedBackend {
        fn new(id: impl Into<String>) -> Self {
            Self {
                id: id.into(),
                batches: std::sync::Mutex::new(std::collections::VecDeque::new()),
                fail_changes: false,
                poll_interval: None,
                changes_calls: std::sync::atomic::AtomicUsize::new(0),
                last_cursor: std::sync::Mutex::new(None),
            }
        }

        fn with_batch(self, batch: Vec<Change>) -> Self {
            self.batches.lock().unwrap().push_back(batch);
            self
        }

        const fn failing(mut self) -> Self {
            self.fail_changes = true;
            self
        }

        const fn with_poll_interval(mut self, interval: Duration) -> Self {
            self.poll_interval = Some(interval);
            self
        }

        fn changes_call_count(&self) -> usize {
            self.changes_calls
                .load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    #[async_trait]
    impl Backend for ScriptedBackend {
        fn id(&self) -> &str {
            &self.id
        }

        fn display_name(&self) -> &'static str {
            "Scripted"
        }

        async fn quota(&self) -> anyhow::Result<Option<crate::types::Quota>> {
            Ok(None)
        }

        async fn changes(
            &self,
            cursor: Option<&crate::types::Cursor>,
        ) -> anyhow::Result<(Vec<Change>, crate::types::Cursor)> {
            self.changes_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            *self.last_cursor.lock().unwrap() = cursor.cloned();
            if self.fail_changes {
                anyhow::bail!("scripted changes failure");
            }
            let mut batches = self.batches.lock().unwrap();
            // Hand out the next batch; keep the last one for the steady state.
            let batch = if batches.len() > 1 {
                batches.pop_front().unwrap_or_default()
            } else {
                batches.front().cloned().unwrap_or_default()
            };
            Ok((batch, crate::types::Cursor(format!("{}-cursor", self.id))))
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
            self.poll_interval
        }
    }

    /// A presenter whose `upsert_item` always fails, used to confirm that a
    /// presenter error propagates out of `apply_changes` rather than being
    /// swallowed.
    #[derive(Default)]
    struct FailingPresenter;

    #[async_trait]
    impl VfsPresenter for FailingPresenter {
        async fn upsert_item(&self, _item: VfsItem) -> anyhow::Result<()> {
            anyhow::bail!("presenter upsert failed")
        }
        async fn delete_item(&self, _id: &ItemId) -> anyhow::Result<()> {
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

    /// Assert that an optional `Cursor` carries the expected inner payload.
    /// `Cursor` deliberately does not implement `PartialEq` (it is an opaque
    /// wire type), so tests compare the inner string instead.
    fn assert_cursor(actual: Option<crate::types::Cursor>, expected: Option<&str>) {
        assert_eq!(actual.map(|c| c.0).as_deref(), expected);
    }

    /// Build a `Created` change for a backend-scoped file under `<backend>:root`.
    fn created(backend: &str, native_id: &str, name: &str) -> Change {
        let entry = FileEntry::file(
            ItemId::new(backend, native_id),
            ItemId::new(backend, "root"),
            name.to_string(),
        );
        Change::Created(entry)
    }

    #[tokio::test]
    async fn apply_changes_handles_each_variant() {
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("scr", "scripted", "Scripted", None, None)
            .unwrap();
        let presenter = Arc::new(MockPresenter::default());
        let config = Arc::new(ConfigResolver::new(PathBuf::from("/tmp/apply")));
        let runner = make_native_runner(db.clone(), vec![], presenter.clone(), config);

        // Pre-existing entry so the Updated/Deleted/Moved variants act on a row.
        let updated_old = FileEntry::file(
            ItemId::new("scr", "u1"),
            ItemId::new("scr", "root"),
            "old-name.txt".into(),
        );
        let removed = FileEntry::file(
            ItemId::new("scr", "d1"),
            ItemId::new("scr", "root"),
            "gone.txt".into(),
        );
        // The Moved target lands under a `sub` directory, so that directory must
        // already exist (parents precede children) for the runner to resolve
        // the moved file's full VFS path.
        let sub_dir = FileEntry::dir(
            ItemId::new("scr", "sub"),
            ItemId::new("scr", "root"),
            "sub".into(),
        );
        db.upsert_file(&updated_old).unwrap();
        db.upsert_file(&removed).unwrap();
        db.upsert_file(&sub_dir).unwrap();

        let updated_new = FileEntry::file(
            ItemId::new("scr", "u1"),
            ItemId::new("scr", "root"),
            "new-name.txt".into(),
        );
        let moved_from = FileEntry::file(
            ItemId::new("scr", "m1"),
            ItemId::new("scr", "root"),
            "before.txt".into(),
        );
        let moved_to = FileEntry::file(
            ItemId::new("scr", "m1"),
            ItemId::new("scr", "sub"),
            "after.txt".into(),
        );

        let changes = vec![
            created("scr", "c1", "fresh.txt"),
            Change::Updated {
                old: updated_old,
                new: updated_new.clone(),
            },
            Change::Deleted(removed.clone()),
            Change::Moved {
                from: moved_from,
                to: moved_to.clone(),
            },
        ];

        let applied = runner.apply_changes(&scr_mount(), &changes).await.unwrap();

        // Every variant survived (none ignored, none oversized).
        assert_eq!(applied.len(), changes.len());

        // Created and Moved/Updated targets are in the DB; the deleted one is gone.
        assert!(db.get_file(&ItemId::new("scr", "c1")).unwrap().is_some());
        assert_eq!(
            db.get_file(&ItemId::new("scr", "u1"))
                .unwrap()
                .unwrap()
                .name,
            "new-name.txt"
        );
        assert!(db.get_file(&ItemId::new("scr", "d1")).unwrap().is_none());
        assert_eq!(
            db.get_file(&ItemId::new("scr", "m1"))
                .unwrap()
                .unwrap()
                .name,
            "after.txt"
        );
        // At the root prefix the moved file's full VFS path is its parent's
        // path joined with its basename: `sub/after.txt`.
        assert_eq!(
            db.get_file_path(&ItemId::new("scr", "m1"))
                .unwrap()
                .as_deref(),
            Some("sub/after.txt")
        );

        // Presenter saw three upserts (created, updated, moved) and one delete.
        let upserts = presenter.upserts.lock().unwrap();
        assert_eq!(upserts.len(), 3);
        assert!(upserts.contains(&"fresh.txt".to_string()));
        assert!(upserts.contains(&"new-name.txt".to_string()));
        assert!(upserts.contains(&"after.txt".to_string()));
        let deletes = presenter.deletes.lock().unwrap();
        assert_eq!(deletes.as_slice(), &["scr:d1".to_string()]);
    }

    #[tokio::test]
    async fn apply_changes_empty_set_is_noop() {
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("scr", "scripted", "Scripted", None, None)
            .unwrap();
        let presenter = Arc::new(MockPresenter::default());
        let config = Arc::new(ConfigResolver::new(PathBuf::from("/tmp/empty")));
        let runner = make_native_runner(db.clone(), vec![], presenter.clone(), config);

        let applied = runner.apply_changes(&scr_mount(), &[]).await.unwrap();

        assert!(applied.is_empty());
        assert!(presenter.upserts.lock().unwrap().is_empty());
        assert!(presenter.deletes.lock().unwrap().is_empty());
        assert!(db.list_all_files().unwrap().is_empty());
    }

    #[tokio::test]
    async fn sync_backend_empty_changes_polls_once_and_persists_cursor() {
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("scr", "scripted", "Scripted", None, None)
            .unwrap();
        let presenter = Arc::new(MockPresenter::default());
        let config = Arc::new(ConfigResolver::new(PathBuf::from("/tmp/sb-empty")));
        let backend = Arc::new(ScriptedBackend::new("scr").with_batch(vec![]));
        let backend_dyn: Arc<dyn Backend> = backend.clone();
        let runner = make_native_runner(db.clone(), vec![], presenter.clone(), config);

        let count = runner
            .sync_backend(&at_root(backend_dyn.clone()))
            .await
            .unwrap();

        assert_eq!(count, 0);
        assert_eq!(backend.changes_call_count(), 1);
        assert!(presenter.upserts.lock().unwrap().is_empty());
        // The new cursor was persisted even though no changes were applied.
        assert_cursor(db.get_cursor("scr").unwrap(), Some("scr-cursor"));
    }

    #[tokio::test]
    async fn sync_backend_passes_stored_cursor_and_advances_it() {
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("scr", "scripted", "Scripted", None, None)
            .unwrap();
        // Seed an existing cursor so we can confirm the backend receives it.
        db.set_cursor("scr", &crate::types::Cursor("start".into()))
            .unwrap();
        let presenter = Arc::new(MockPresenter::default());
        let config = Arc::new(ConfigResolver::new(PathBuf::from("/tmp/sb-cursor")));
        let backend =
            Arc::new(ScriptedBackend::new("scr").with_batch(vec![created("scr", "c1", "x.txt")]));
        let backend_dyn: Arc<dyn Backend> = backend.clone();
        let runner = make_native_runner(db.clone(), vec![], presenter, config);

        let count = runner
            .sync_backend(&at_root(backend_dyn.clone()))
            .await
            .unwrap();

        assert_eq!(count, 1);
        // Backend was handed the previously-stored cursor.
        assert_cursor(backend.last_cursor.lock().unwrap().clone(), Some("start"));
        // And the cursor advanced to the backend's returned value.
        assert_cursor(db.get_cursor("scr").unwrap(), Some("scr-cursor"));
    }

    #[tokio::test]
    async fn sync_backend_surfaces_changes_error() {
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("scr", "scripted", "Scripted", None, None)
            .unwrap();
        let presenter = Arc::new(MockPresenter::default());
        let config = Arc::new(ConfigResolver::new(PathBuf::from("/tmp/sb-err")));
        let backend = Arc::new(ScriptedBackend::new("scr").failing());
        let backend_dyn: Arc<dyn Backend> = backend;
        let runner = make_native_runner(db.clone(), vec![], presenter, config);

        let result = runner.sync_backend(&at_root(backend_dyn.clone())).await;

        // The backend error is surfaced, not swallowed.
        let err = result.unwrap_err();
        assert!(err.to_string().contains("scripted changes failure"));
        // No cursor was persisted because the error short-circuited before set_cursor.
        assert!(db.get_cursor("scr").unwrap().is_none());
    }

    #[tokio::test]
    async fn sync_backend_propagates_presenter_failure() {
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("scr", "scripted", "Scripted", None, None)
            .unwrap();
        let presenter = Arc::new(FailingPresenter);
        let config = Arc::new(ConfigResolver::new(PathBuf::from("/tmp/sb-presenter")));
        let backend =
            Arc::new(ScriptedBackend::new("scr").with_batch(vec![created("scr", "c1", "x.txt")]));
        let backend_dyn: Arc<dyn Backend> = backend;
        let runner = make_native_runner(db.clone(), vec![], presenter, config);

        let result = runner.sync_backend(&at_root(backend_dyn.clone())).await;

        let err = result.unwrap_err();
        assert!(err.to_string().contains("presenter upsert failed"));
        // The cursor must not advance when applying the batch failed mid-way.
        assert!(db.get_cursor("scr").unwrap().is_none());
    }

    #[tokio::test]
    async fn run_continues_past_failing_backend_in_initial_sync() {
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("bad", "scripted", "Bad", None, None)
            .unwrap();
        db.register_backend("good", "scripted", "Good", None, None)
            .unwrap();
        let presenter = Arc::new(MockPresenter::default());
        let config = Arc::new(ConfigResolver::new(PathBuf::from("/tmp/run-mixed")));

        let bad: Arc<dyn Backend> = Arc::new(ScriptedBackend::new("bad").failing());
        // A short poll interval keeps the polling-loop sleep tiny so the test
        // does not wait the 60s default before observing the cancel flag.
        let good_backend = Arc::new(
            ScriptedBackend::new("good")
                .with_poll_interval(Duration::from_millis(5))
                .with_batch(vec![created("good", "g1", "ok.txt")]),
        );
        let good: Arc<dyn Backend> = good_backend.clone();

        // cancel=false so run() does the initial sync, hydrate, then enters the
        // polling loop. We trip cancel after the good backend has polled once.
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel_watch = cancel.clone();
        let good_watch = good_backend.clone();
        let watcher = tokio::spawn(async move {
            // Wait until the good backend has been polled at least once (initial
            // sync), then cancel so run() returns from the polling loop.
            loop {
                if good_watch.changes_call_count() >= 1 {
                    cancel_watch.store(true, std::sync::atomic::Ordering::Relaxed);
                    return;
                }
                tokio::task::yield_now().await;
            }
        });

        let runner = make_native_runner(db.clone(), vec![bad, good], presenter, config);
        let result = runner.run(cancel).await;
        watcher.await.unwrap();

        // The bad backend's error did not abort the run; the good backend's
        // change still landed in the DB.
        assert!(result.is_ok());
        assert!(db.get_file(&ItemId::new("good", "g1")).unwrap().is_some());
        // Cursor advanced only for the good backend; the failing one never set one.
        assert!(db.get_cursor("bad").unwrap().is_none());
        assert_cursor(db.get_cursor("good").unwrap(), Some("good-cursor"));
    }

    #[tokio::test]
    async fn apply_changes_skips_ignored_entry() {
        // A real .cascade file in the mount root marks *.tmp as ignored.
        // `is_ignored_entry` resolves config for the entry name's parent dir,
        // so the entry names are full paths under the mount root and the walk
        // loads the root `.cascade`.
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join(".cascade"), b"*.tmp\n")
            .await
            .unwrap();
        let keep_name = tmp.path().join("keep.txt").to_string_lossy().into_owned();
        let skip_name = tmp
            .path()
            .join("scratch.tmp")
            .to_string_lossy()
            .into_owned();

        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("scr", "scripted", "Scripted", None, None)
            .unwrap();
        let presenter = Arc::new(MockPresenter::default());
        let config = Arc::new(ConfigResolver::new(tmp.path().to_path_buf()));
        let runner = make_native_runner(db.clone(), vec![], presenter.clone(), config);

        let changes = vec![
            Change::Created(FileEntry::file(
                ItemId::new("scr", "keep"),
                ItemId::new("scr", "root"),
                keep_name.clone(),
            )),
            Change::Created(FileEntry::file(
                ItemId::new("scr", "skip"),
                ItemId::new("scr", "root"),
                skip_name,
            )),
        ];
        let applied = runner.apply_changes(&scr_mount(), &changes).await.unwrap();

        // Only the non-ignored entry was applied.
        assert_eq!(applied.len(), 1);
        assert!(db.get_file(&ItemId::new("scr", "keep")).unwrap().is_some());
        assert!(db.get_file(&ItemId::new("scr", "skip")).unwrap().is_none());
        let upserts = presenter.upserts.lock().unwrap();
        assert_eq!(upserts.as_slice(), &[keep_name]);
    }

    #[tokio::test]
    async fn apply_changes_skips_entry_exceeding_max_file_length() {
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("scr", "scripted", "Scripted", None, None)
            .unwrap();
        // Rule: anything matching *.bin must not exceed `max_bytes`.
        let max_bytes = 10u64;
        db.add_max_file_length_rule("*.bin", max_bytes, 0, None)
            .unwrap();
        let presenter = Arc::new(MockPresenter::default());
        let config = Arc::new(ConfigResolver::new(PathBuf::from("/tmp/maxlen")));
        let runner = make_native_runner(db.clone(), vec![], presenter.clone(), config);

        let too_big = Change::Created(
            FileEntry::file(
                ItemId::new("scr", "big"),
                ItemId::new("scr", "root"),
                "blob.bin".into(),
            )
            .with_size(Some(max_bytes + 1)),
        );
        let within = Change::Created(
            FileEntry::file(
                ItemId::new("scr", "small"),
                ItemId::new("scr", "root"),
                "tiny.bin".into(),
            )
            .with_size(Some(max_bytes)),
        );

        let applied = runner
            .apply_changes(&scr_mount(), &[too_big, within])
            .await
            .unwrap();

        // The oversized file is skipped; the one at the limit is applied.
        assert_eq!(applied.len(), 1);
        assert!(db.get_file(&ItemId::new("scr", "big")).unwrap().is_none());
        assert!(db.get_file(&ItemId::new("scr", "small")).unwrap().is_some());
    }

    #[tokio::test]
    async fn apply_changes_auto_pins_matching_created_entry() {
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("scr", "scripted", "Scripted", None, None)
            .unwrap();
        // Pin rule matching the created file's name.
        db.add_pin_rule("pinme.txt", false, None).unwrap();
        let presenter = Arc::new(MockPresenter::default());
        let config = Arc::new(ConfigResolver::new(PathBuf::from("/tmp/pin")));
        let runner = make_native_runner(db.clone(), vec![], presenter, config);

        let changes = vec![
            created("scr", "p1", "pinme.txt"),
            created("scr", "p2", "ordinary.txt"),
        ];
        let applied = runner.apply_changes(&scr_mount(), &changes).await.unwrap();
        assert_eq!(applied.len(), 2);

        // The matching entry was auto-pinned; the other stays Online.
        assert_eq!(
            db.get_cache_state(&ItemId::new("scr", "p1")).unwrap(),
            Some(CacheState::Pinned)
        );
        assert_eq!(
            db.get_cache_state(&ItemId::new("scr", "p2")).unwrap(),
            Some(CacheState::Online)
        );
    }

    #[tokio::test]
    async fn hydrate_presenter_loads_root_children() {
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("scr", "scripted", "Scripted", None, None)
            .unwrap();
        // Two root-level children under "scr:root" plus a grandchild that must
        // NOT be hydrated (only root children are loaded eagerly).
        let child_a = FileEntry::dir(
            ItemId::new("scr", "a"),
            ItemId::new("scr", "root"),
            "folder-a".into(),
        );
        let child_b = FileEntry::file(
            ItemId::new("scr", "b"),
            ItemId::new("scr", "root"),
            "file-b.txt".into(),
        );
        let grandchild = FileEntry::file(
            ItemId::new("scr", "c"),
            ItemId::new("scr", "a"),
            "nested.txt".into(),
        );
        db.upsert_file(&child_a).unwrap();
        db.upsert_file(&child_b).unwrap();
        db.upsert_file(&grandchild).unwrap();

        let presenter = Arc::new(MockPresenter::default());
        let config = Arc::new(ConfigResolver::new(PathBuf::from("/tmp/hydrate")));
        // The backend is mounted at `/` (empty prefix), so no mount-point
        // directory is injected — the at-root backend *is* the root.
        let runner =
            make_native_runner_mounted(db.clone(), vec![scr_mount()], presenter.clone(), config);

        runner.hydrate_presenter().await.unwrap();

        let upserts = presenter.upserts.lock().unwrap();
        assert_eq!(upserts.len(), 2);
        assert!(upserts.contains(&"folder-a".to_string()));
        assert!(upserts.contains(&"file-b.txt".to_string()));
        assert!(!upserts.contains(&"nested.txt".to_string()));
    }

    /// A backend mounted at a single-segment prefix surfaces its root children at
    /// their stored mount-prefixed paths AND a synthetic mount-point directory
    /// for the neutral root — injected exactly once, parented to the neutral
    /// root, with the backend's root container as its id so a descent routes
    /// straight to the backend.
    #[tokio::test]
    async fn hydrate_presenter_injects_top_level_mount_directory() {
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("scr", "scripted", "Scripted", Some("personal"), None)
            .unwrap();
        // A root child stored at its full mount-prefixed path, exactly as the
        // sync runner's path assembly would have written it.
        let child = FileEntry::file(
            ItemId::new("scr", "f"),
            ItemId::new("scr", "root"),
            "report.txt".into(),
        )
        .with_path("personal/report.txt".into());
        db.upsert_file(&child).unwrap();

        let presenter = Arc::new(MockPresenter::default());
        let config = Arc::new(ConfigResolver::new(PathBuf::from("/tmp/hydrate-mount")));
        let runner = make_native_runner_mounted(
            db.clone(),
            vec![scr_mount_at("personal")],
            presenter.clone(),
            config,
        );

        runner.hydrate_presenter().await.unwrap();

        let items = presenter.upsert_items.lock().unwrap();
        // The root child carries its stored mount-prefixed path verbatim.
        let report = items
            .iter()
            .find(|i| i.name == "report.txt")
            .expect("root child hydrated");
        assert_eq!(report.path, "personal/report.txt");

        // Exactly one synthetic mount-point directory, parented to the neutral
        // root, named and pathed by the mount prefix, ided by the backend root.
        let mount_dirs: Vec<&VfsItem> = items
            .iter()
            .filter(|i| i.parent_id == crate::vfs::neutral_root_item_id())
            .collect();
        assert_eq!(mount_dirs.len(), 1, "exactly one mount-point directory");
        let mount_dir = mount_dirs[0];
        assert_eq!(mount_dir.name, "personal");
        assert_eq!(mount_dir.path, "personal");
        assert!(mount_dir.is_dir);
        assert_eq!(mount_dir.id, ItemId::new("scr", "root"));
    }

    /// A nested mount (multi-segment prefix) is a child of another backend's
    /// subtree, so `VfsTree::read_dir` injects its mount-point directory lazily.
    /// Hydration must not also surface it under the neutral root.
    #[tokio::test]
    async fn hydrate_presenter_skips_nested_mount_directory() {
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("scr", "scripted", "Scripted", Some("personal/Nested"), None)
            .unwrap();
        let presenter = Arc::new(MockPresenter::default());
        let config = Arc::new(ConfigResolver::new(PathBuf::from("/tmp/hydrate-nested")));
        let runner = make_native_runner_mounted(
            db.clone(),
            vec![scr_mount_at("personal/Nested")],
            presenter.clone(),
            config,
        );

        runner.hydrate_presenter().await.unwrap();

        let items = presenter.upsert_items.lock().unwrap();
        assert!(
            !items
                .iter()
                .any(|i| i.parent_id == crate::vfs::neutral_root_item_id()),
            "nested mount must not inject a neutral-root directory"
        );
    }

    /// End-to-end cold restart over a shared state DB: a first runner applies a
    /// backend's changes (stamping mount-prefixed paths into the DB), then a
    /// fresh runner over the same DB hydrates a brand-new presenter. The cold
    /// presenter must show the mount-point directory under the neutral root and
    /// the backend's root children at their stored mount-prefixed paths — no new
    /// change had to arrive for the view to reappear.
    #[tokio::test]
    async fn restart_hydrates_mount_dir_and_root_children_at_stored_paths() {
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("scr", "scripted", "Scripted", Some("personal"), None)
            .unwrap();

        // First boot: a runner applies the backend's initial changes. The path
        // assembly writes the full mount-prefixed paths into the DB.
        let first_presenter = Arc::new(MockPresenter::default());
        let first_config = Arc::new(ConfigResolver::new(PathBuf::from("/tmp/restart")));
        let first = make_native_runner_mounted(
            db.clone(),
            vec![scr_mount_at("personal")],
            first_presenter,
            first_config,
        );
        let dir = FileEntry::dir(
            ItemId::new("scr", "d"),
            ItemId::new("scr", "root"),
            "Documents".into(),
        );
        let file = FileEntry::file(
            ItemId::new("scr", "f"),
            ItemId::new("scr", "d"),
            "report.txt".into(),
        );
        first
            .apply_changes(
                &scr_mount_at("personal"),
                &[Change::Created(dir), Change::Created(file)],
            )
            .await
            .unwrap();
        drop(first);

        // Cold restart: a fresh runner over the same DB hydrates a fresh
        // presenter from the stored paths alone.
        let cold_presenter = Arc::new(MockPresenter::default());
        let cold_config = Arc::new(ConfigResolver::new(PathBuf::from("/tmp/restart")));
        let cold = make_native_runner_mounted(
            db.clone(),
            vec![scr_mount_at("personal")],
            cold_presenter.clone(),
            cold_config,
        );
        cold.hydrate_presenter().await.unwrap();

        let items = cold_presenter.upsert_items.lock().unwrap();

        // The neutral-root mount-point directory is present.
        let mount_dir = items
            .iter()
            .find(|i| i.parent_id == crate::vfs::neutral_root_item_id())
            .expect("mount-point directory hydrated on restart");
        assert_eq!(mount_dir.name, "personal");
        assert_eq!(mount_dir.path, "personal");

        // The backend's root child (the `Documents` directory) is present at its
        // stored mount-prefixed path. The grandchild `report.txt` stays lazy.
        let documents = items
            .iter()
            .find(|i| i.name == "Documents")
            .expect("root child hydrated on restart");
        assert_eq!(documents.path, "personal/Documents");
        assert!(
            !items.iter().any(|i| i.name == "report.txt"),
            "grandchild must not be eagerly hydrated"
        );
    }

    #[tokio::test]
    async fn effective_poll_interval_picks_shortest() {
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        let presenter = Arc::new(MockPresenter::default());
        let config = Arc::new(ConfigResolver::new(PathBuf::from("/tmp/poll")));

        let short = Duration::from_secs(5);
        let long = Duration::from_secs(30);
        let fast: Arc<dyn Backend> =
            Arc::new(ScriptedBackend::new("fast").with_poll_interval(short));
        let slow: Arc<dyn Backend> =
            Arc::new(ScriptedBackend::new("slow").with_poll_interval(long));
        let runner = make_native_runner(db, vec![slow, fast], presenter, config);

        // The shortest reported interval wins.
        assert_eq!(runner.effective_poll_interval().await, short);
    }

    #[tokio::test]
    async fn effective_poll_interval_defaults_when_none_reported() {
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        let presenter = Arc::new(MockPresenter::default());
        let config = Arc::new(ConfigResolver::new(PathBuf::from("/tmp/poll-default")));

        // ScriptedBackend with no interval reports None; NullBackend also None.
        let a: Arc<dyn Backend> = Arc::new(ScriptedBackend::new("a"));
        let b: Arc<dyn Backend> = Arc::new(NullBackend::new("b"));
        let runner = make_native_runner(db, vec![a, b], presenter, config);

        assert_eq!(
            runner.effective_poll_interval().await,
            DEFAULT_POLL_INTERVAL
        );
    }

    #[tokio::test]
    async fn flush_dirty_files_skips_unknown_backend() {
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("mock", "mock", "Mock", None, None)
            .unwrap();
        // The dirty file's backend is registered in the DB (so the file row's
        // FK holds) but is NOT given to the runner, modelling a backend that
        // was removed from the live set while a dirty file still references it.
        db.register_backend("ghost", "mock", "Ghost", None, None)
            .unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let local_file = tmp.path().join("orphan.txt");
        tokio::fs::write(&local_file, b"data").await.unwrap();

        // Dirty file owned by a backend the runner does not have in its set.
        let file_id = ItemId::new("ghost", "f1");
        let entry = FileEntry::file(
            file_id.clone(),
            ItemId::new("ghost", "root"),
            "orphan.txt".into(),
        );
        db.upsert_file(&entry).unwrap();
        db.mark_dirty(&file_id).unwrap();
        db.set_file_paths(&file_id, "orphan.txt", &local_file.to_string_lossy())
            .unwrap();

        // Runner only knows about the "mock" backend, not "ghost".
        let mock_backend = Arc::new(MockBackend::new("mock"));
        let presenter = Arc::new(MockPresenter::default());
        let config = Arc::new(ConfigResolver::new(PathBuf::from("/tmp/orphan")));
        let runner = make_native_runner(db.clone(), vec![mock_backend.clone()], presenter, config);

        let flushed = runner.flush_dirty_files().await;

        assert_eq!(flushed, 0);
        assert!(mock_backend.uploads.lock().unwrap().is_empty());
        // Still dirty — nothing could flush it.
        assert_eq!(db.is_dirty(&file_id).unwrap(), Some(true));
    }

    #[tokio::test]
    async fn flush_dirty_files_skips_missing_disk_file() {
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("mock", "mock", "Mock", None, None)
            .unwrap();

        // Point local_path at a file that does not exist.
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("never-written.txt");

        let file_id = ItemId::new("mock", "f1");
        let entry = FileEntry::file(file_id.clone(), ItemId::new("mock", "root"), "x.txt".into());
        db.upsert_file(&entry).unwrap();
        db.mark_dirty(&file_id).unwrap();
        db.set_file_paths(&file_id, "x.txt", &missing.to_string_lossy())
            .unwrap();

        let mock_backend = Arc::new(MockBackend::new("mock"));
        let presenter = Arc::new(MockPresenter::default());
        let config = Arc::new(ConfigResolver::new(PathBuf::from("/tmp/missing")));
        let runner = make_native_runner(db.clone(), vec![mock_backend.clone()], presenter, config);

        let flushed = runner.flush_dirty_files().await;

        assert_eq!(flushed, 0);
        assert!(mock_backend.uploads.lock().unwrap().is_empty());
        assert_eq!(db.is_dirty(&file_id).unwrap(), Some(true));
    }

    #[tokio::test]
    async fn flush_dirty_files_flushes_multiple_and_isolates_failures() {
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("mock", "mock", "Mock", None, None)
            .unwrap();

        let tmp = tempfile::tempdir().unwrap();

        // Two flushable files plus one whose disk content is missing. The
        // missing one must not stop the other two from flushing.
        let good1 = tmp.path().join("g1.txt");
        let good2 = tmp.path().join("g2.txt");
        tokio::fs::write(&good1, b"one").await.unwrap();
        tokio::fs::write(&good2, b"two").await.unwrap();
        let missing = tmp.path().join("absent.txt");

        let id1 = ItemId::new("mock", "g1");
        let id2 = ItemId::new("mock", "g2");
        let id3 = ItemId::new("mock", "absent");
        for (id, name) in [(&id1, "g1.txt"), (&id2, "g2.txt"), (&id3, "absent.txt")] {
            let entry = FileEntry::file(id.clone(), ItemId::new("mock", "root"), name.to_string());
            db.upsert_file(&entry).unwrap();
            db.mark_dirty(id).unwrap();
        }
        db.set_file_paths(&id1, "g1.txt", &good1.to_string_lossy())
            .unwrap();
        db.set_file_paths(&id2, "g2.txt", &good2.to_string_lossy())
            .unwrap();
        db.set_file_paths(&id3, "absent.txt", &missing.to_string_lossy())
            .unwrap();

        let mock_backend = Arc::new(MockBackend::new("mock"));
        let presenter = Arc::new(MockPresenter::default());
        let config = Arc::new(ConfigResolver::new(PathBuf::from("/tmp/multi")));
        let runner = make_native_runner(db.clone(), vec![mock_backend.clone()], presenter, config);

        let flushed = runner.flush_dirty_files().await;

        // Both readable files flushed; the missing one did not.
        assert_eq!(flushed, 2);
        assert_eq!(mock_backend.uploads.lock().unwrap().len(), 2);
        assert_eq!(db.is_dirty(&id1).unwrap(), Some(false));
        assert_eq!(db.is_dirty(&id2).unwrap(), Some(false));
        assert_eq!(db.is_dirty(&id3).unwrap(), Some(true));
    }

    #[cfg(feature = "native")]
    #[tokio::test]
    async fn sync_backend_records_applied_changes_into_change_feed() {
        use crate::changefeed::{ChangeFeed, ChangeQueryResult};

        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("scr", "scripted", "Scripted", None, None)
            .unwrap();
        let presenter = Arc::new(MockPresenter::default());
        let config = Arc::new(ConfigResolver::new(PathBuf::from("/tmp/feed")));
        let backend = Arc::new(
            ScriptedBackend::new("scr").with_batch(vec![created("scr", "c1", "feed.txt")]),
        );
        let backend_dyn: Arc<dyn Backend> = backend;

        let feed = Arc::new(ChangeFeed::new());
        let runner = make_native_runner(db.clone(), vec![], presenter, config)
            .with_change_feed(feed.clone());

        let count = runner
            .sync_backend(&at_root(backend_dyn.clone()))
            .await
            .unwrap();
        assert_eq!(count, 1);

        // The applied change is queryable from the feed under its parent.
        let result = feed
            .parent_changes_since("scr", &ItemId::new("scr", "root"), None)
            .await;
        match result {
            ChangeQueryResult::Delta { events, .. } => {
                assert_eq!(events.len(), 1);
                match &events[0] {
                    Change::Created(entry) => assert_eq!(entry.name, "feed.txt"),
                    other => panic!("unexpected change variant: {other:?}"),
                }
            }
            other => panic!("expected Delta, got {other:?}"),
        }
    }

    // ── mount-prefix path assembly ──

    /// A backend mounted at `/` (empty prefix) produces VFS paths
    /// byte-identical to the pre-refactor mount-relative paths: a root child is
    /// stored under its bare name, and a nested child under `parent/child`.
    #[tokio::test]
    async fn apply_changes_at_root_paths_are_byte_identical() {
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("scr", "scripted", "Scripted", None, None)
            .unwrap();
        let presenter = Arc::new(MockPresenter::default());
        let config = Arc::new(ConfigResolver::new(PathBuf::from("/tmp/at-root")));
        let runner = make_native_runner(db.clone(), vec![], presenter, config);

        // A root-level directory, then a file nested inside it.
        let dir = FileEntry::dir(
            ItemId::new("scr", "d"),
            ItemId::new("scr", "root"),
            "Documents".into(),
        );
        let nested = FileEntry::file(
            ItemId::new("scr", "f"),
            ItemId::new("scr", "d"),
            "report.txt".into(),
        );
        let changes = vec![Change::Created(dir), Change::Created(nested)];

        runner.apply_changes(&scr_mount(), &changes).await.unwrap();

        // Root child: bare name. Nested child: parent/child, no mount prefix.
        assert_eq!(
            db.get_file_path(&ItemId::new("scr", "d"))
                .unwrap()
                .as_deref(),
            Some("Documents")
        );
        assert_eq!(
            db.get_file_path(&ItemId::new("scr", "f"))
                .unwrap()
                .as_deref(),
            Some("Documents/report.txt")
        );
    }

    /// A backend mounted at a non-empty prefix stamps that prefix onto every
    /// VFS path: a root child becomes `mount/child`, a nested child becomes
    /// `mount/parent/child`.
    #[tokio::test]
    async fn apply_changes_nested_mount_prefixes_paths() {
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("scr", "scripted", "Scripted", Some("personal"), None)
            .unwrap();
        let presenter = Arc::new(MockPresenter::default());
        let config = Arc::new(ConfigResolver::new(PathBuf::from("/tmp/nested")));
        let runner = make_native_runner(db.clone(), vec![], presenter, config);

        let dir = FileEntry::dir(
            ItemId::new("scr", "d"),
            ItemId::new("scr", "root"),
            "Documents".into(),
        );
        let nested = FileEntry::file(
            ItemId::new("scr", "f"),
            ItemId::new("scr", "d"),
            "report.txt".into(),
        );
        let changes = vec![Change::Created(dir), Change::Created(nested)];

        runner
            .apply_changes(&scr_mount_at("personal"), &changes)
            .await
            .unwrap();

        assert_eq!(
            db.get_file_path(&ItemId::new("scr", "d"))
                .unwrap()
                .as_deref(),
            Some("personal/Documents")
        );
        assert_eq!(
            db.get_file_path(&ItemId::new("scr", "f"))
                .unwrap()
                .as_deref(),
            Some("personal/Documents/report.txt")
        );
    }

    /// A child whose parent is neither stored nor a recognised backend root is
    /// a changefeed-ordering bug; the runner fails that change loudly rather
    /// than silently falling back to the basename.
    #[tokio::test]
    async fn apply_changes_missing_non_root_parent_fails_loudly() {
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("scr", "scripted", "Scripted", None, None)
            .unwrap();
        let presenter = Arc::new(MockPresenter::default());
        let config = Arc::new(ConfigResolver::new(PathBuf::from("/tmp/orphan-parent")));
        let runner = make_native_runner(db.clone(), vec![], presenter, config);

        // Parent `scr:ghost` is neither stored nor a root sentinel.
        let orphan = FileEntry::file(
            ItemId::new("scr", "f"),
            ItemId::new("scr", "ghost"),
            "lost.txt".into(),
        );
        let result = runner
            .apply_changes(&scr_mount(), &[Change::Created(orphan)])
            .await;

        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("no stored path"),
            "unexpected error: {err}"
        );
        // Nothing was written for the orphan.
        assert!(db.get_file(&ItemId::new("scr", "f")).unwrap().is_none());
    }

    /// `flush_dirty_files` strips the mount prefix from the stored VFS path
    /// before handing the path to the backend, so the backend receives a
    /// native, mount-relative path even for a nested mount.
    #[tokio::test]
    async fn flush_dirty_files_strips_mount_prefix_before_upload() {
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("mock", "mock", "Mock", Some("personal"), None)
            .unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let local_file = tmp.path().join("test.txt");
        tokio::fs::write(&local_file, b"hello").await.unwrap();

        let file_id = ItemId::new("mock", "file1");
        let entry = FileEntry::file(
            file_id.clone(),
            ItemId::new("mock", "root"),
            "test.txt".into(),
        );
        db.upsert_file(&entry).unwrap();
        db.mark_dirty(&file_id).unwrap();
        // The stored path is the full, mount-prefixed VFS path.
        db.set_file_paths(
            &file_id,
            "personal/docs/test.txt",
            &local_file.to_string_lossy(),
        )
        .unwrap();

        let mock_backend = Arc::new(MockBackend::new("mock"));
        let presenter = Arc::new(MockPresenter::default());
        let config = Arc::new(ConfigResolver::new(PathBuf::from("/tmp/strip")));
        let runner = make_native_runner_mounted(
            db.clone(),
            vec![at("personal", mock_backend.clone())],
            presenter,
            config,
        );

        let flushed = runner.flush_dirty_files().await;
        assert_eq!(flushed, 1);

        // The backend received the mount-relative path, prefix stripped.
        let uploads = mock_backend.uploads.lock().unwrap();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].0, "docs/test.txt");
    }
}
