//! Engine-backed implementation of [`FileProviderHandlers`].
//!
//! Routes every inbound RPC through the Cascade engine: the `VfsTree` to
//! locate the owning backend, the `StateDb` to look up cached metadata, and
//! the [`Backend`] trait for the actual operation. The seven handlers below
//! map one-to-one onto the seven [`FileProviderHandlers`] methods.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use cascade_engine::backend::Backend;
use cascade_engine::db::StateDb;
use cascade_engine::types::{CacheState, FileEntry, FileId, ItemId, SyncCursor, VfsItem};
use cascade_engine::vfs::{VfsTree, derive_sync_cursor};
use chrono::{DateTime, Utc};
use data_encoding::BASE64URL_NOPAD;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::handlers::{
    EnumerateChangesOutput, EnumerateOutput, FileProviderHandlers, HandlerError, HandlerResult,
};
use crate::items::FileProviderItem;

/// Subdirectory inside the cache directory where File Provider materialised
/// contents live. Each item gets its own folder keyed by sanitised ID.
const CACHE_SUBDIR: &str = "file-provider";

/// Number of children returned per `enumerateItems` request before the
/// engine emits a `next_page` cursor.
///
/// The handler asks the backend for the full child list (the current
/// `Backend::list_children` contract has no native page parameter) and
/// slices it locally. A future round will push pagination down into the
/// backend trait so the slice happens at the source. The size is chosen
/// to match the default `NSFileProviderEnumerator` batch — 256 — which
/// keeps each round-trip response well under macOS's XPC payload limit.
const ENUMERATE_PAGE_SIZE: usize = 256;

/// Snapshot of the child set under a parent directory at the moment the
/// engine last emitted a cursor for that parent.
///
/// Keyed by item ID (`stub:file1`); the value carries the metadata tuple
/// the cursor is derived from. The `cursor` field is the cursor the
/// engine returned alongside this snapshot — a caller's `since_cursor`
/// must equal this for the engine to compute an incremental delta;
/// otherwise the handler treats the call as a fresh enumeration.
///
/// TODO(fileprovider): replace this snapshot-based diff with a real
/// event log driven by the backend's native change stream. The engine
/// already exposes `Backend::changes(cursor)`, but the File Provider
/// bridge has no per-parent fan-out for those events yet. Once the
/// engine gains a `(parent_id, since_event)` -> `Vec<Change>` query that
/// holds across the entire VFS tree, this in-memory snapshot table can
/// be retired in favour of the authoritative log.
#[derive(Debug, Clone)]
struct ParentSnapshot {
    cursor: SyncCursor,
    children: HashMap<String, SnapshotEntry>,
}

/// Metadata tuple captured for each child in a [`ParentSnapshot`].
///
/// Mirrors the fields hashed into the cursor by [`derive_sync_cursor`]
/// (`name`, `is_dir`, `size`, `mod_time`) so that "did this entry
/// change?" is a structural comparison rather than a hash check, while
/// remaining cheap enough to keep in memory for every observed parent.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SnapshotEntry {
    name: String,
    is_dir: bool,
    size: Option<u64>,
    mod_time: Option<DateTime<Utc>>,
}

impl SnapshotEntry {
    fn from_entry(entry: &FileEntry) -> Self {
        Self {
            name: entry.name.clone(),
            is_dir: entry.is_dir,
            size: entry.size,
            mod_time: entry.mod_time,
        }
    }
}

/// Production handler implementation.
///
/// Construction takes:
/// - a shared `VfsTree` so the handler can route operations to the correct
///   backend by `backend_id`;
/// - a shared `StateDb` so the handler can answer `getItem` from cached
///   metadata without round-tripping the cloud;
/// - a cache directory under which `fetchContents` materialises file
///   content.
pub struct EngineHandlers {
    vfs: Arc<RwLock<VfsTree>>,
    db: Arc<StateDb>,
    cache_dir: PathBuf,
    /// In-memory per-parent snapshot store used by `enumerateChanges`.
    ///
    /// Keyed by parent `ItemId` string. Bounded by the number of parent
    /// directories the File Provider extension has observed — typically
    /// one per visible Finder window — and rebuilt on demand whenever
    /// the caller's cursor goes stale.
    snapshots: Arc<Mutex<HashMap<String, ParentSnapshot>>>,
}

impl std::fmt::Debug for EngineHandlers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineHandlers")
            .field("cache_dir", &self.cache_dir)
            .finish_non_exhaustive()
    }
}

impl EngineHandlers {
    /// Create a new engine-backed handler.
    ///
    /// `cache_dir` is the directory under which fetched file contents are
    /// materialised. The handler creates `cache_dir/file-provider/` on
    /// demand.
    pub fn new(vfs: Arc<RwLock<VfsTree>>, db: Arc<StateDb>, cache_dir: PathBuf) -> Self {
        Self {
            vfs,
            db,
            cache_dir,
            snapshots: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Look up the backend that owns an `ItemId`.
    fn backend_for(&self, item_id: &ItemId) -> HandlerResult<Arc<dyn Backend>> {
        let vfs = self
            .vfs
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        vfs.backend_by_id(item_id.backend_id())
            .cloned()
            .ok_or_else(|| {
                HandlerError::not_found(format!(
                    "no backend registered for id {}",
                    item_id.backend_id()
                ))
            })
    }

    /// Compute the cache directory for a given item.
    ///
    /// Format: `<cache_dir>/file-provider/<sanitised-id>/`. The handler
    /// creates this on demand.
    fn cache_dir_for(&self, item_id: &ItemId) -> PathBuf {
        self.cache_dir
            .join(CACHE_SUBDIR)
            .join(safe_filename(&item_id.0))
    }

    /// Build the cache path for a specific version of an item's content.
    ///
    /// The version segment is derived from the engine's last-modified
    /// timestamp so a remote update invalidates the cached file. If no
    /// modification time is known, the version is `unknown` and the cached
    /// file is treated as if it could be stale on the next fetch.
    fn cache_path_for(&self, entry: &FileEntry) -> PathBuf {
        let version = entry
            .mod_time
            .map_or_else(|| "unknown".to_string(), |t| t.timestamp().to_string());
        self.cache_dir_for(&entry.id).join(version)
    }

    /// Execute a cross-backend move as a download / upload / delete dance.
    ///
    /// The destination upload is committed before the source delete runs.
    /// If the upload fails, the source is left untouched and the local
    /// staging file is removed. If the source delete fails after a
    /// successful upload, the move is reported as `Internal` with both
    /// `ItemId`s — the data is now in both places and the user must
    /// clean up the source manually. We deliberately do not roll back
    /// the destination in that case because that would destroy the only
    /// successful copy of the data.
    ///
    /// Directories are rejected up front with `NotSupported`; recursive
    /// cross-backend directory moves require a per-child walk and are
    /// out of scope for this round.
    ///
    /// Concurrency: the Apple File Provider framework serialises
    /// per-item operations on the system side, so this method does not
    /// take any additional locks. Two clients moving the same item
    /// concurrently is the system's problem to resolve, not the
    /// presenter's.
    async fn cross_backend_move(
        &self,
        item_id: &ItemId,
        new_parent: &ItemId,
        new_name: &str,
    ) -> HandlerResult<FileProviderItem> {
        let src_entry = self
            .db
            .get_file(item_id)?
            .ok_or_else(|| HandlerError::not_found(format!("item not found: {item_id}")))?;
        if src_entry.is_dir {
            // TODO(fileprovider): recursive cross-backend directory move.
            return Err(HandlerError::not_supported(
                "cross-backend directory moves require recursive copy; only files are supported"
                    .to_string(),
            ));
        }

        let src_backend = self.backend_for(item_id)?;
        let dst_backend = self.backend_for(new_parent)?;

        // Reject obvious name collisions before doing any I/O. The
        // destination backend may still reject the upload itself if a
        // collision sneaks in concurrently, in which case the existing
        // anyhow → HandlerError mapping will surface AlreadyExists if
        // the backend wraps it as `BackendError::Conflict`.
        let dst_children = dst_backend
            .list_children(new_parent.native_id())
            .await
            .map_err(|err| HandlerError::internal(format!("list destination children: {err}")))?;
        if dst_children.iter().any(|entry| entry.name == new_name) {
            return Err(HandlerError::already_exists(format!(
                "destination {new_parent} already contains an item named {new_name}"
            )));
        }

        let staging_path = self
            .stage_for_move(&src_entry, src_backend.as_ref())
            .await?;

        let mut reader = tokio::fs::File::open(&staging_path).await.map_err(|err| {
            HandlerError::internal(format!(
                "open staging file {}: {err}",
                staging_path.display()
            ))
        })?;
        let dst_parent_file_id = FileId(new_parent.0.clone());
        let upload_result = dst_backend
            .upload(Path::new(new_name), &mut reader, &dst_parent_file_id)
            .await;
        drop(reader);

        // The staging file is no longer needed after upload returns,
        // regardless of outcome. Best-effort cleanup — a failure here
        // does not change the move outcome.
        if let Err(err) = tokio::fs::remove_file(&staging_path).await {
            tracing::warn!(
                path = %staging_path.display(),
                error = %err,
                "failed to remove cross-backend move staging file",
            );
        }

        let dst_entry = match upload_result {
            Ok(entry) => entry,
            Err(err) => return Err(HandlerError::from(err)),
        };

        // Upload committed — try to delete the source. If this fails the
        // data is in BOTH places; we have to surface that loudly rather
        // than silently lose data by rolling back the destination.
        if let Err(err) = src_backend.delete(&src_entry).await {
            tracing::error!(
                source_id = %src_entry.id,
                destination_id = %dst_entry.id,
                error = %err,
                "cross-backend move: destination upload succeeded but source delete failed; manual cleanup required",
            );
            // The destination already exists in the DB once we upsert
            // below, so keep the bookkeeping consistent before
            // returning.
            self.db.upsert_file(&dst_entry)?;
            return Err(HandlerError::internal(format!(
                "cross-backend move partially completed: destination {} is the new copy; source {} still exists and could not be deleted ({err})",
                dst_entry.id, src_entry.id,
            )));
        }

        self.db.upsert_file(&dst_entry)?;
        self.db.delete_subtree(&src_entry.id)?;
        Ok(FileProviderItem::from(VfsItem::from(dst_entry)))
    }

    /// Materialise `src_entry` to a private staging file under the cache
    /// directory and return its path.
    ///
    /// This is similar to the staging step inside `fetch_contents`, but
    /// the resulting file is single-use: the cross-backend move uploads
    /// from it once and then unlinks it. We do not place it at the
    /// canonical cache path because a partially-uploaded cross-backend
    /// move should not poison the cache for a subsequent `fetch_contents`
    /// on the same source ID.
    async fn stage_for_move(
        &self,
        src_entry: &FileEntry,
        src_backend: &dyn Backend,
    ) -> HandlerResult<PathBuf> {
        let staging_dir = self.cache_dir_for(&src_entry.id);
        tokio::fs::create_dir_all(&staging_dir)
            .await
            .map_err(|err| HandlerError::internal(format!("create staging dir: {err}")))?;
        let staging_path = staging_dir.join("cross-backend-move.tmp");

        let file = tokio::fs::File::create(&staging_path)
            .await
            .map_err(|err| HandlerError::internal(format!("create staging file: {err}")))?;
        let mut writer = WriterAdapter { inner: file };
        src_backend.download(src_entry, &mut writer).await?;
        writer
            .inner
            .flush()
            .await
            .map_err(|err| HandlerError::internal(format!("flush staging file: {err}")))?;
        drop(writer);
        Ok(staging_path)
    }
}

#[async_trait]
impl FileProviderHandlers for EngineHandlers {
    async fn get_item(&self, id: &str) -> HandlerResult<FileProviderItem> {
        let item_id = ItemId(id.to_string());
        let entry = self
            .db
            .get_file(&item_id)?
            .ok_or_else(|| HandlerError::not_found(format!("item not found: {id}")))?;
        let cache_state = self
            .db
            .get_cache_state(&item_id)?
            .unwrap_or(CacheState::Online);

        let mut vfs_item: VfsItem = entry.into();
        vfs_item.cache_state = cache_state;
        Ok(FileProviderItem::from(vfs_item))
    }

    async fn enumerate_items(
        &self,
        parent_id: &str,
        page: Option<&str>,
    ) -> HandlerResult<EnumerateOutput> {
        let parent = ItemId(parent_id.to_string());
        let backend = self.backend_for(&parent)?;
        let mut entries = backend.list_children(parent.native_id()).await?;

        // Deterministic ordering — id is the only field guaranteed unique
        // and stable across calls.
        entries.sort_by(|a, b| a.id.0.cmp(&b.id.0));

        let after_id = decode_enumerate_page(page)?;
        let start = after_id.as_deref().map_or(0, |last| {
            entries
                .iter()
                .position(|entry| entry.id.0.as_str() > last)
                .unwrap_or(entries.len())
        });
        let end = start.saturating_add(ENUMERATE_PAGE_SIZE).min(entries.len());

        let next_page = if end < entries.len() {
            entries
                .get(end.saturating_sub(1))
                .map(|entry| encode_enumerate_page(&entry.id.0))
        } else {
            None
        };

        let items = entries
            .drain(start..end)
            .map(|entry| FileProviderItem::from(VfsItem::from(entry)))
            .collect();

        Ok(EnumerateOutput { items, next_page })
    }

    async fn fetch_contents(&self, id: &str) -> HandlerResult<PathBuf> {
        let item_id = ItemId(id.to_string());
        let entry = self
            .db
            .get_file(&item_id)?
            .ok_or_else(|| HandlerError::not_found(format!("item not found: {id}")))?;
        if entry.is_dir {
            return Err(HandlerError::permission_denied(format!(
                "cannot fetch contents of a directory: {id}"
            )));
        }

        let cache_path = self.cache_path_for(&entry);
        if cache_path.exists() {
            return Ok(cache_path);
        }

        let parent = cache_path
            .parent()
            .ok_or_else(|| HandlerError::internal("cache path has no parent".to_string()))?;
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| HandlerError::internal(format!("create cache dir: {error}")))?;

        let backend = self.backend_for(&entry.id)?;
        let temp_path = parent.join(format!(
            "{}.tmp",
            cache_path.file_name().map_or_else(
                || std::borrow::Cow::Borrowed("download"),
                std::ffi::OsStr::to_string_lossy
            )
        ));

        let file = tokio::fs::File::create(&temp_path)
            .await
            .map_err(|error| HandlerError::internal(format!("create temp file: {error}")))?;
        let mut writer = WriterAdapter { inner: file };
        backend.download(&entry, &mut writer).await?;
        writer
            .inner
            .flush()
            .await
            .map_err(|error| HandlerError::internal(format!("flush temp file: {error}")))?;
        if let Err(error) = tokio::fs::rename(&temp_path, &cache_path).await {
            // Best-effort cleanup of the staged temp file so a failing
            // rename (cross-device, permissions, disk full) doesn't
            // leave a `.tmp` sibling. Mirrors the cleanup pattern in
            // `cross_backend_move` below.
            if let Err(unlink_err) = tokio::fs::remove_file(&temp_path).await {
                tracing::warn!(
                    target: "cascade::presenter_fileprovider",
                    path = %temp_path.display(),
                    error = %unlink_err,
                    "failed to remove staged temp file after rename failure"
                );
            }
            return Err(HandlerError::internal(format!(
                "persist cache file: {error}"
            )));
        }

        Ok(cache_path)
    }

    async fn import_document(
        &self,
        source_url: &str,
        parent_id: &str,
        name: Option<&str>,
        existing_id: Option<&str>,
    ) -> HandlerResult<FileProviderItem> {
        let source_path = decode_source_url(source_url)?;
        let parent = ItemId(parent_id.to_string());
        let backend = self.backend_for(&parent)?;

        let mut reader = tokio::fs::File::open(&source_path).await.map_err(|error| {
            HandlerError::internal(format!("open source {}: {error}", source_path.display()))
        })?;

        let entry = if let Some(existing) = existing_id {
            let existing_item = ItemId(existing.to_string());
            let file_id = FileId(existing_item.0.clone());
            backend.update(&file_id, &mut reader).await?
        } else {
            let filename = match name {
                Some(value) => value.to_string(),
                None => source_path
                    .file_name()
                    .map(|os| os.to_string_lossy().into_owned())
                    .ok_or_else(|| {
                        HandlerError::internal(format!(
                            "cannot derive filename from source {}",
                            source_path.display()
                        ))
                    })?,
            };
            let parent_file_id = FileId(parent.0.clone());
            backend
                .upload(Path::new(&filename), &mut reader, &parent_file_id)
                .await?
        };

        self.db.upsert_file(&entry)?;
        Ok(FileProviderItem::from(VfsItem::from(entry)))
    }

    async fn create_directory(
        &self,
        name: &str,
        parent_id: &str,
    ) -> HandlerResult<FileProviderItem> {
        let parent = ItemId(parent_id.to_string());
        let backend = self.backend_for(&parent)?;
        let parent_file_id = FileId(parent.0.clone());
        let entry = backend
            .create_dir_with_parent(name, &parent_file_id)
            .await?;
        self.db.upsert_file(&entry)?;
        Ok(FileProviderItem::from(VfsItem::from(entry)))
    }

    async fn delete_item(&self, id: &str) -> HandlerResult<()> {
        let item_id = ItemId(id.to_string());
        let entry = self
            .db
            .get_file(&item_id)?
            .ok_or_else(|| HandlerError::not_found(format!("item not found: {id}")))?;
        let backend = self.backend_for(&item_id)?;
        backend.delete(&entry).await?;
        self.db.delete_subtree(&item_id)?;
        Ok(())
    }

    async fn move_item(
        &self,
        id: &str,
        new_parent_id: &str,
        new_name: &str,
    ) -> HandlerResult<FileProviderItem> {
        let item_id = ItemId(id.to_string());
        let new_parent = ItemId(new_parent_id.to_string());
        if item_id.backend_id() != new_parent.backend_id() {
            return self
                .cross_backend_move(&item_id, &new_parent, new_name)
                .await;
        }
        let backend = self.backend_for(&item_id)?;
        let src_file_id = FileId(item_id.0.clone());
        let dst_parent_file_id = FileId(new_parent.0.clone());
        let entry = backend
            .move_by_id(&src_file_id, &dst_parent_file_id, new_name)
            .await?;
        self.db.upsert_file(&entry)?;
        Ok(FileProviderItem::from(VfsItem::from(entry)))
    }

    async fn current_sync_cursor(&self, parent_id: &str) -> HandlerResult<SyncCursor> {
        let parent = ItemId(parent_id.to_string());
        // backend_for() releases the std::sync RwLock guard before
        // returning — it would not be Send across the await otherwise.
        let backend = self.backend_for(&parent)?;
        let cursor = derive_sync_cursor(backend.as_ref(), parent.native_id()).await?;
        Ok(cursor)
    }

    async fn enumerate_changes(
        &self,
        parent_id: &str,
        since_cursor: Option<&SyncCursor>,
    ) -> HandlerResult<EnumerateChangesOutput> {
        let parent = ItemId(parent_id.to_string());
        // Release the std::sync RwLock guard before awaiting the
        // backend; same pattern as `current_sync_cursor` above.
        let backend = self.backend_for(&parent)?;

        // Snapshot the parent's current children, ordered the same way
        // `derive_sync_cursor` orders them so the cursor we compute
        // matches what `currentSyncCursor` would emit for an unchanged
        // view.
        let mut entries = backend
            .list_children(parent.native_id())
            .await
            .map_err(HandlerError::from)?;
        entries.sort_by(|a, b| a.id.0.cmp(&b.id.0));

        let new_cursor = derive_cursor_from_sorted(&entries);
        let current: HashMap<String, SnapshotEntry> = entries
            .iter()
            .map(|entry| (entry.id.0.clone(), SnapshotEntry::from_entry(entry)))
            .collect();

        let mut snapshots = self.snapshots.lock().await;
        let previous = snapshots.get(parent_id);

        // Decide whether to compute an incremental delta or fall back to
        // the "treat everything as added" path. We fall back when:
        //   - the caller did not supply a cursor;
        //   - no snapshot exists for this parent (engine restart, first
        //     observation);
        //   - the supplied cursor does not match the stored snapshot's
        //     cursor (the client is out of date).
        let use_incremental = match (since_cursor, previous) {
            (Some(cursor), Some(snapshot)) => snapshot.cursor == *cursor,
            _ => false,
        };

        let output = if use_incremental {
            let previous_children = &previous
                .ok_or_else(|| {
                    // Unreachable: we only set use_incremental=true when
                    // previous is Some, but expressing this with the
                    // type system would require restructuring the match
                    // above. The Internal error keeps us within the
                    // strict-lints subset (no unwrap_used).
                    HandlerError::internal(
                        "snapshot disappeared between match and incremental branch".to_string(),
                    )
                })?
                .children;
            diff_snapshots(previous_children, &current, &entries, new_cursor.clone())
        } else {
            // First-call behaviour: every current child is "added", the
            // deleted list is empty, and the engine takes a fresh
            // snapshot so the next call carrying `new_cursor` can fall
            // through to the incremental path.
            EnumerateChangesOutput {
                added_or_modified: entries
                    .iter()
                    .cloned()
                    .map(|entry| FileProviderItem::from(VfsItem::from(entry)))
                    .collect(),
                deleted: Vec::new(),
                new_cursor: new_cursor.clone(),
            }
        };

        snapshots.insert(
            parent_id.to_string(),
            ParentSnapshot {
                cursor: new_cursor,
                children: current,
            },
        );
        Ok(output)
    }
}

/// Compute the per-parent cursor over an already-sorted child set.
///
/// Kept byte-for-byte identical to [`derive_sync_cursor`] so the cursor a
/// Swift client persists via `currentSyncCursor` is interchangeable with
/// the one we emit from `enumerateChanges`. The free function in
/// `cascade_engine::vfs` re-lists from the backend; we have the list in
/// hand already, so we reproduce the hash here rather than paying for a
/// second backend round-trip.
fn derive_cursor_from_sorted(entries: &[FileEntry]) -> SyncCursor {
    let mut hasher = Sha256::new();
    for entry in entries {
        hasher.update(entry.id.0.as_bytes());
        hasher.update([0u8]);
        hasher.update(entry.name.as_bytes());
        hasher.update([0u8]);
        hasher.update([u8::from(entry.is_dir)]);
        hasher.update(entry.size.unwrap_or(0).to_be_bytes());
        hasher.update(entry.mod_time.map_or(0i64, |t| t.timestamp()).to_be_bytes());
    }
    SyncCursor::new(hasher.finalize().to_vec())
}

/// Compute the added/modified/deleted sets between two snapshots.
///
/// `previous` and `current` are the two snapshot maps keyed by item ID;
/// `current_entries` is the same set as `current` but in the canonical
/// sorted order so the returned `added_or_modified` list is
/// deterministic. The cursor for the new snapshot is passed in rather
/// than recomputed here — the caller already has it.
fn diff_snapshots(
    previous: &HashMap<String, SnapshotEntry>,
    current: &HashMap<String, SnapshotEntry>,
    current_entries: &[FileEntry],
    new_cursor: SyncCursor,
) -> EnumerateChangesOutput {
    let mut added_or_modified: Vec<FileProviderItem> = Vec::new();
    for entry in current_entries {
        let key = &entry.id.0;
        let Some(current_metadata) = current.get(key) else {
            continue;
        };
        let unchanged = previous
            .get(key)
            .is_some_and(|prior| prior == current_metadata);
        if !unchanged {
            added_or_modified.push(FileProviderItem::from(VfsItem::from(entry.clone())));
        }
    }

    let mut deleted: Vec<String> = previous
        .keys()
        .filter(|key| !current.contains_key(key.as_str()))
        .cloned()
        .collect();
    // Stable order for callers that compare results across runs.
    deleted.sort();

    EnumerateChangesOutput {
        added_or_modified,
        deleted,
        new_cursor,
    }
}

/// Encode the last item ID of a returned page as the opaque wire cursor.
///
/// The wire cursor is base64url-no-pad of the raw ID bytes, which keeps
/// the cursor JSON-safe (no quoting, no padding) without exposing the
/// underlying ID format to the Swift side. Consumers must not interpret
/// the bytes.
fn encode_enumerate_page(last_id: &str) -> String {
    BASE64URL_NOPAD.encode(last_id.as_bytes())
}

/// Decode an opaque wire cursor back into the last item ID it carries.
///
/// `None` or an empty string mean "first page". Malformed cursors map to
/// `Internal` errors — the Swift side should always echo back what the
/// engine emitted, so a decode failure indicates a protocol bug.
fn decode_enumerate_page(page: Option<&str>) -> HandlerResult<Option<String>> {
    let Some(encoded) = page.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let bytes = BASE64URL_NOPAD
        .decode(encoded.as_bytes())
        .map_err(|error| HandlerError::internal(format!("invalid page cursor: {error}")))?;
    let id = String::from_utf8(bytes).map_err(|error| {
        HandlerError::internal(format!("page cursor is not valid UTF-8: {error}"))
    })?;
    Ok(Some(id))
}

/// Decode a `file://` URL or a bare path into a `PathBuf`.
///
/// The Swift side passes `URL.path` which is already a filesystem path with
/// no scheme prefix, but accept either form so the handler is robust to
/// implementation changes on either end.
fn decode_source_url(source: &str) -> HandlerResult<PathBuf> {
    let trimmed = source.strip_prefix("file://").unwrap_or(source);
    if trimmed.is_empty() {
        return Err(HandlerError::internal("empty source URL".to_string()));
    }
    Ok(PathBuf::from(trimmed))
}

/// Sanitise an `ItemId` string into a filesystem-safe filename segment.
fn safe_filename(id: &str) -> String {
    id.replace([':', '/', '\\'], "_")
}

/// Adapter that exposes a `tokio::fs::File` as the
/// `dyn AsyncWrite + Unpin + Send` the `Backend` trait expects.
struct WriterAdapter {
    inner: tokio::fs::File,
}

impl tokio::io::AsyncWrite for WriterAdapter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

impl Unpin for WriterAdapter {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::ErrorCode;
    use async_trait::async_trait;
    use cascade_engine::types::{Change, Cursor, Quota};
    use chrono::Utc;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::Duration;

    /// Minimal in-memory backend used to exercise `EngineHandlers` without
    /// the real cloud transports. Implements just enough of the trait
    /// surface to cover the seven RPC paths.
    #[derive(Debug, Default)]
    struct InMemoryBackend {
        id: String,
        files: Mutex<HashMap<String, FileEntry>>,
        content: Mutex<HashMap<String, Vec<u8>>>,
        next_id: Mutex<u64>,
    }

    impl InMemoryBackend {
        fn new(id: &str) -> Self {
            Self {
                id: id.to_string(),
                files: Mutex::new(HashMap::new()),
                content: Mutex::new(HashMap::new()),
                next_id: Mutex::new(0),
            }
        }

        fn allocate_id(&self) -> String {
            let mut next = self
                .next_id
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *next += 1;
            format!("n{next}")
        }

        fn insert(&self, entry: FileEntry) {
            let mut files = self
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            files.insert(entry.id.0.clone(), entry);
        }
    }

    #[async_trait]
    impl Backend for InMemoryBackend {
        fn id(&self) -> &str {
            &self.id
        }

        fn display_name(&self) -> &str {
            &self.id
        }

        async fn quota(&self) -> anyhow::Result<Option<Quota>> {
            Ok(None)
        }

        async fn changes(&self, _cursor: Option<&Cursor>) -> anyhow::Result<(Vec<Change>, Cursor)> {
            Ok((vec![], Cursor("static".to_string())))
        }

        async fn metadata(&self, _path: &Path) -> anyhow::Result<FileEntry> {
            anyhow::bail!("metadata not implemented for stub")
        }

        async fn download(
            &self,
            file: &FileEntry,
            writer: &mut (dyn tokio::io::AsyncWrite + Unpin + Send),
        ) -> anyhow::Result<()> {
            let bytes = {
                let content = self
                    .content
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                content
                    .get(&file.id.0)
                    .ok_or_else(|| anyhow::anyhow!("no content for {}", file.id))?
                    .clone()
            };
            writer.write_all(&bytes).await?;
            writer.flush().await?;
            Ok(())
        }

        async fn upload(
            &self,
            path: &Path,
            reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
            parent_id: &FileId,
        ) -> anyhow::Result<FileEntry> {
            let mut bytes = Vec::new();
            tokio::io::copy(reader, &mut bytes).await?;

            let new_id = self.allocate_id();
            let item_id = ItemId::new(&self.id, &new_id);
            let parent_item = ItemId(parent_id.0.clone());
            let entry = FileEntry {
                id: item_id.clone(),
                parent_id: parent_item,
                name: path
                    .file_name()
                    .map(|os| os.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "unnamed".to_string()),
                is_dir: false,
                size: Some(bytes.len() as u64),
                mod_time: Some(Utc::now()),
                mime_type: None,
                hash: None,
            };

            self.content
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(item_id.0.clone(), bytes);
            self.insert(entry.clone());
            Ok(entry)
        }

        async fn update(
            &self,
            file_id: &FileId,
            reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
        ) -> anyhow::Result<FileEntry> {
            let mut bytes = Vec::new();
            tokio::io::copy(reader, &mut bytes).await?;

            let mut files = self
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = files
                .get_mut(&file_id.0)
                .ok_or_else(|| anyhow::anyhow!("file not found"))?;
            entry.size = Some(bytes.len() as u64);
            entry.mod_time = Some(Utc::now());
            let updated = entry.clone();
            drop(files);

            self.content
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(file_id.0.clone(), bytes);
            Ok(updated)
        }

        async fn create_dir(&self, _path: &Path) -> anyhow::Result<FileEntry> {
            anyhow::bail!("create_dir is not used by the stub; use create_dir_with_parent")
        }

        async fn create_dir_with_parent(
            &self,
            name: &str,
            parent_id: &FileId,
        ) -> anyhow::Result<FileEntry> {
            let new_id = self.allocate_id();
            let item_id = ItemId::new(&self.id, &new_id);
            let parent_item = ItemId(parent_id.0.clone());
            let entry = FileEntry {
                id: item_id,
                parent_id: parent_item,
                name: name.to_string(),
                is_dir: true,
                size: None,
                mod_time: Some(Utc::now()),
                mime_type: None,
                hash: None,
            };
            self.insert(entry.clone());
            Ok(entry)
        }

        async fn delete(&self, file: &FileEntry) -> anyhow::Result<()> {
            let mut files = self
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            files.remove(&file.id.0);
            drop(files);
            self.content
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&file.id.0);
            Ok(())
        }

        async fn move_entry(&self, _src: &Path, _dst: &Path) -> anyhow::Result<FileEntry> {
            anyhow::bail!("move_entry is not used by the stub; use move_by_id")
        }

        async fn move_by_id(
            &self,
            src_id: &FileId,
            dst_parent_id: &FileId,
            new_name: &str,
        ) -> anyhow::Result<FileEntry> {
            let mut files = self
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = files
                .get_mut(&src_id.0)
                .ok_or_else(|| anyhow::anyhow!("file not found"))?;
            entry.parent_id = ItemId(dst_parent_id.0.clone());
            entry.name = new_name.to_string();
            entry.mod_time = Some(Utc::now());
            Ok(entry.clone())
        }

        async fn list_children(&self, parent_native_id: &str) -> anyhow::Result<Vec<FileEntry>> {
            let files = self
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let prefix = format!("{}:", self.id);
            let parent_full = format!("{prefix}{parent_native_id}");
            let entries = files
                .values()
                .filter(|entry| entry.parent_id.0 == parent_full)
                .cloned()
                .collect();
            Ok(entries)
        }

        async fn poll_interval(&self) -> Option<Duration> {
            None
        }
    }

    fn make_handlers() -> (EngineHandlers, Arc<InMemoryBackend>, tempfile::TempDir) {
        let backend = Arc::new(InMemoryBackend::new("stub"));
        let vfs = Arc::new(RwLock::new(VfsTree::new(backend.clone())));
        let cache_dir = tempfile::tempdir().unwrap();

        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("stub", "stub", "Stub", None, None)
            .unwrap();

        // Seed one parent directory and one child file.
        let parent_id = ItemId::new("stub", "root");
        let parent_entry = FileEntry {
            id: parent_id.clone(),
            parent_id: ItemId::new("stub", ""),
            name: "root".to_string(),
            is_dir: true,
            size: None,
            mod_time: Some(Utc::now()),
            mime_type: None,
            hash: None,
        };
        backend.insert(parent_entry.clone());
        db.upsert_file(&parent_entry).unwrap();

        let child_id = ItemId::new("stub", "file1");
        let child_entry = FileEntry {
            id: child_id.clone(),
            parent_id: parent_id.clone(),
            name: "hello.txt".to_string(),
            is_dir: false,
            size: Some(11),
            mod_time: Some(Utc::now()),
            mime_type: Some("text/plain".to_string()),
            hash: None,
        };
        backend.insert(child_entry.clone());
        db.upsert_file(&child_entry).unwrap();
        backend
            .content
            .lock()
            .unwrap()
            .insert(child_id.0.clone(), b"hello world".to_vec());

        let handlers = EngineHandlers::new(vfs, db, cache_dir.path().to_path_buf());
        (handlers, backend, cache_dir)
    }

    #[tokio::test]
    async fn get_item_returns_metadata_for_known_id() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let item = handlers.get_item("stub:file1").await.unwrap();
        assert_eq!(item.id, "stub:file1");
        assert_eq!(item.filename, "hello.txt");
        assert!(!item.is_directory);
    }

    #[tokio::test]
    async fn get_item_returns_not_found_for_missing_id() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let err = handlers.get_item("stub:missing").await.unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn enumerate_items_lists_children_of_directory() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let output = handlers.enumerate_items("stub:root", None).await.unwrap();
        assert_eq!(output.items.len(), 1);
        assert_eq!(output.items[0].filename, "hello.txt");
        assert!(output.next_page.is_none());
    }

    #[tokio::test]
    async fn enumerate_items_fails_for_unknown_backend() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let err = handlers
            .enumerate_items("ghost:root", None)
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn fetch_contents_materialises_file_to_cache_dir() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let path = handlers.fetch_contents("stub:file1").await.unwrap();
        assert!(path.exists());
        let bytes = tokio::fs::read(&path).await.unwrap();
        assert_eq!(bytes, b"hello world");
    }

    #[tokio::test]
    async fn fetch_contents_is_idempotent() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let first = handlers.fetch_contents("stub:file1").await.unwrap();
        let second = handlers.fetch_contents("stub:file1").await.unwrap();
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn fetch_contents_refuses_directories() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let err = handlers.fetch_contents("stub:root").await.unwrap_err();
        assert_eq!(err.code, ErrorCode::PermissionDenied);
    }

    #[tokio::test]
    async fn import_document_uploads_a_new_file() {
        let (handlers, _backend, tempdir) = make_handlers();
        let source = tempdir.path().join("upload.txt");
        tokio::fs::write(&source, b"new bytes").await.unwrap();

        let item = handlers
            .import_document(
                source.to_str().unwrap(),
                "stub:root",
                Some("upload.txt"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(item.filename, "upload.txt");
        assert_eq!(item.parent_id, "stub:root");
        assert_eq!(item.size, Some(9));
    }

    #[tokio::test]
    async fn import_document_decodes_file_url_prefix() {
        let (handlers, _backend, tempdir) = make_handlers();
        let source = tempdir.path().join("u.txt");
        tokio::fs::write(&source, b"ok").await.unwrap();
        let url = format!("file://{}", source.display());

        let item = handlers
            .import_document(&url, "stub:root", Some("u.txt"), None)
            .await
            .unwrap();
        assert_eq!(item.filename, "u.txt");
    }

    #[tokio::test]
    async fn import_document_overwrites_existing_when_existing_id_set() {
        let (handlers, _backend, tempdir) = make_handlers();
        let source = tempdir.path().join("replace.txt");
        tokio::fs::write(&source, b"new contents long enough")
            .await
            .unwrap();

        let item = handlers
            .import_document(
                source.to_str().unwrap(),
                "stub:root",
                None,
                Some("stub:file1"),
            )
            .await
            .unwrap();
        assert_eq!(item.id, "stub:file1");
        assert_eq!(item.size, Some(24));
    }

    #[tokio::test]
    async fn create_directory_creates_under_parent() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let item = handlers
            .create_directory("Photos", "stub:root")
            .await
            .unwrap();
        assert!(item.is_directory);
        assert_eq!(item.filename, "Photos");
        assert_eq!(item.parent_id, "stub:root");
    }

    #[tokio::test]
    async fn delete_item_removes_known_id() {
        let (handlers, _backend, _tempdir) = make_handlers();
        handlers.delete_item("stub:file1").await.unwrap();
        let err = handlers.get_item("stub:file1").await.unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn delete_item_fails_for_missing_id() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let err = handlers.delete_item("stub:missing").await.unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn move_item_renames_and_reparents_within_one_backend() {
        let (handlers, _backend, _tempdir) = make_handlers();

        // Create a second directory to move into.
        let new_dir = handlers
            .create_directory("Archive", "stub:root")
            .await
            .unwrap();

        let moved = handlers
            .move_item("stub:file1", &new_dir.id, "renamed.txt")
            .await
            .unwrap();
        assert_eq!(moved.filename, "renamed.txt");
        assert_eq!(moved.parent_id, new_dir.id);
    }

    #[tokio::test]
    async fn move_item_cross_backend_unknown_destination_is_not_found() {
        // No `other` backend is registered, so the cross-backend branch
        // bails out at the destination lookup with NotFound.
        let (handlers, _backend, _tempdir) = make_handlers();
        let err = handlers
            .move_item("stub:file1", "other:root", "renamed.txt")
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }

    /// Stand up an `EngineHandlers` with two backends — `src` (mounted at
    /// the root) and `dst` (mounted at `/dst`) — each pre-seeded with a
    /// root directory entry. The source backend additionally holds a
    /// single file `hello.txt` under its root with known bytes.
    fn make_cross_backend_handlers() -> (
        EngineHandlers,
        Arc<InMemoryBackend>,
        Arc<InMemoryBackend>,
        tempfile::TempDir,
    ) {
        let src = Arc::new(InMemoryBackend::new("src"));
        let dst = Arc::new(InMemoryBackend::new("dst"));

        let mut tree = VfsTree::new(src.clone());
        tree.mount(PathBuf::from("/dst"), dst.clone());
        let vfs = Arc::new(RwLock::new(tree));
        let cache_dir = tempfile::tempdir().unwrap();

        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("src", "stub", "Src", None, None)
            .unwrap();
        db.register_backend("dst", "stub", "Dst", Some("/dst"), None)
            .unwrap();

        for backend in [&src, &dst] {
            let root_id = ItemId::new(backend.id(), "root");
            let root_entry = FileEntry {
                id: root_id,
                parent_id: ItemId::new(backend.id(), ""),
                name: "root".to_string(),
                is_dir: true,
                size: None,
                mod_time: Some(Utc::now()),
                mime_type: None,
                hash: None,
            };
            backend.insert(root_entry.clone());
            db.upsert_file(&root_entry).unwrap();
        }

        let src_file_id = ItemId::new("src", "file1");
        let src_file = FileEntry {
            id: src_file_id.clone(),
            parent_id: ItemId::new("src", "root"),
            name: "hello.txt".to_string(),
            is_dir: false,
            size: Some(11),
            mod_time: Some(Utc::now()),
            mime_type: Some("text/plain".to_string()),
            hash: None,
        };
        src.insert(src_file.clone());
        db.upsert_file(&src_file).unwrap();
        src.content
            .lock()
            .unwrap()
            .insert(src_file_id.0.clone(), b"hello world".to_vec());

        let handlers = EngineHandlers::new(vfs, db, cache_dir.path().to_path_buf());
        (handlers, src, dst, cache_dir)
    }

    #[tokio::test]
    async fn move_item_cross_backend_file_succeeds() {
        let (handlers, src, dst, _tempdir) = make_cross_backend_handlers();

        let moved = handlers
            .move_item("src:file1", "dst:root", "renamed.txt")
            .await
            .unwrap();

        assert_eq!(moved.parent_id, "dst:root");
        assert_eq!(moved.filename, "renamed.txt");
        assert!(!moved.is_directory);

        // Destination has the file with the moved bytes.
        let dst_files = dst.files.lock().unwrap();
        let dst_entry = dst_files
            .values()
            .find(|entry| entry.name == "renamed.txt")
            .expect("destination should now contain the moved file");
        let dst_content = dst
            .content
            .lock()
            .unwrap()
            .get(&dst_entry.id.0)
            .cloned()
            .expect("destination must have content for moved file");
        assert_eq!(dst_content, b"hello world");

        // Source is gone.
        assert!(
            !src.files.lock().unwrap().contains_key("src:file1"),
            "source backend must no longer hold the original entry"
        );
        assert!(
            !src.content.lock().unwrap().contains_key("src:file1"),
            "source backend must no longer hold the original bytes"
        );
    }

    #[tokio::test]
    async fn move_item_cross_backend_directory_returns_not_supported() {
        let (handlers, src, _dst, _tempdir) = make_cross_backend_handlers();

        // Seed a directory under the source root that we'll try to move.
        let dir_id = ItemId::new("src", "subdir");
        let dir_entry = FileEntry {
            id: dir_id,
            parent_id: ItemId::new("src", "root"),
            name: "subdir".to_string(),
            is_dir: true,
            size: None,
            mod_time: Some(Utc::now()),
            mime_type: None,
            hash: None,
        };
        src.insert(dir_entry.clone());
        handlers.db.upsert_file(&dir_entry).unwrap();

        let err = handlers
            .move_item("src:subdir", "dst:root", "subdir")
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::NotSupported);
        assert!(
            err.message.contains("recursive"),
            "message should explain why directories are rejected: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn move_item_cross_backend_name_collision_returns_already_exists() {
        let (handlers, _src, dst, _tempdir) = make_cross_backend_handlers();

        // Pre-seed the destination with a file of the target name.
        let collide_id = ItemId::new("dst", "existing");
        let collide_entry = FileEntry {
            id: collide_id,
            parent_id: ItemId::new("dst", "root"),
            name: "renamed.txt".to_string(),
            is_dir: false,
            size: Some(3),
            mod_time: Some(Utc::now()),
            mime_type: Some("text/plain".to_string()),
            hash: None,
        };
        dst.insert(collide_entry.clone());
        handlers.db.upsert_file(&collide_entry).unwrap();

        let err = handlers
            .move_item("src:file1", "dst:root", "renamed.txt")
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::AlreadyExists);
    }

    /// A backend whose `delete` always fails, used to simulate the
    /// partial-failure path of a cross-backend move where the upload
    /// committed but cleaning up the source did not.
    #[derive(Debug)]
    struct DeleteFailingBackend {
        inner: InMemoryBackend,
    }

    impl DeleteFailingBackend {
        fn new(id: &str) -> Self {
            Self {
                inner: InMemoryBackend::new(id),
            }
        }

        fn insert(&self, entry: FileEntry) {
            self.inner.insert(entry);
        }
    }

    #[async_trait]
    impl Backend for DeleteFailingBackend {
        fn id(&self) -> &str {
            self.inner.id()
        }

        fn display_name(&self) -> &str {
            self.inner.display_name()
        }

        async fn quota(&self) -> anyhow::Result<Option<Quota>> {
            self.inner.quota().await
        }

        async fn changes(&self, cursor: Option<&Cursor>) -> anyhow::Result<(Vec<Change>, Cursor)> {
            self.inner.changes(cursor).await
        }

        async fn metadata(&self, path: &Path) -> anyhow::Result<FileEntry> {
            self.inner.metadata(path).await
        }

        async fn download(
            &self,
            file: &FileEntry,
            writer: &mut (dyn tokio::io::AsyncWrite + Unpin + Send),
        ) -> anyhow::Result<()> {
            self.inner.download(file, writer).await
        }

        async fn upload(
            &self,
            path: &Path,
            reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
            parent_id: &FileId,
        ) -> anyhow::Result<FileEntry> {
            self.inner.upload(path, reader, parent_id).await
        }

        async fn update(
            &self,
            file_id: &FileId,
            reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
        ) -> anyhow::Result<FileEntry> {
            self.inner.update(file_id, reader).await
        }

        async fn create_dir(&self, path: &Path) -> anyhow::Result<FileEntry> {
            self.inner.create_dir(path).await
        }

        async fn create_dir_with_parent(
            &self,
            name: &str,
            parent_id: &FileId,
        ) -> anyhow::Result<FileEntry> {
            self.inner.create_dir_with_parent(name, parent_id).await
        }

        async fn delete(&self, _file: &FileEntry) -> anyhow::Result<()> {
            anyhow::bail!("simulated delete failure")
        }

        async fn move_entry(&self, src: &Path, dst: &Path) -> anyhow::Result<FileEntry> {
            self.inner.move_entry(src, dst).await
        }

        async fn move_by_id(
            &self,
            src_id: &FileId,
            dst_parent_id: &FileId,
            new_name: &str,
        ) -> anyhow::Result<FileEntry> {
            self.inner.move_by_id(src_id, dst_parent_id, new_name).await
        }

        async fn list_children(&self, parent_native_id: &str) -> anyhow::Result<Vec<FileEntry>> {
            self.inner.list_children(parent_native_id).await
        }

        async fn poll_interval(&self) -> Option<Duration> {
            self.inner.poll_interval().await
        }
    }

    #[tokio::test]
    async fn move_item_cross_backend_partial_failure_after_upload_returns_internal() {
        // Build a tree where the source backend's `delete` is rigged to
        // fail. The destination upload should still succeed and the
        // handler must report Internal with both IDs.
        let src = Arc::new(DeleteFailingBackend::new("src"));
        let dst = Arc::new(InMemoryBackend::new("dst"));

        let src_dyn: Arc<dyn Backend> = src.clone();
        let mut tree = VfsTree::new(src_dyn);
        tree.mount(PathBuf::from("/dst"), dst.clone());
        let vfs = Arc::new(RwLock::new(tree));
        let cache_dir = tempfile::tempdir().unwrap();

        let db = Arc::new(StateDb::open_in_memory().unwrap());
        db.register_backend("src", "stub", "Src", None, None)
            .unwrap();
        db.register_backend("dst", "stub", "Dst", Some("/dst"), None)
            .unwrap();

        // Seed the source root via the wrapper's insert (DeleteFailingBackend
        // does not own a public `insert` of its own — it delegates to its
        // inner `InMemoryBackend`).
        let src_root = FileEntry {
            id: ItemId::new("src", "root"),
            parent_id: ItemId::new("src", ""),
            name: "root".to_string(),
            is_dir: true,
            size: None,
            mod_time: Some(Utc::now()),
            mime_type: None,
            hash: None,
        };
        src.inner.insert(src_root.clone());
        db.upsert_file(&src_root).unwrap();

        let dst_root = FileEntry {
            id: ItemId::new("dst", "root"),
            parent_id: ItemId::new("dst", ""),
            name: "root".to_string(),
            is_dir: true,
            size: None,
            mod_time: Some(Utc::now()),
            mime_type: None,
            hash: None,
        };
        dst.insert(dst_root.clone());
        db.upsert_file(&dst_root).unwrap();

        let src_file_id = ItemId::new("src", "file1");
        let src_file = FileEntry {
            id: src_file_id.clone(),
            parent_id: ItemId::new("src", "root"),
            name: "hello.txt".to_string(),
            is_dir: false,
            size: Some(11),
            mod_time: Some(Utc::now()),
            mime_type: Some("text/plain".to_string()),
            hash: None,
        };
        src.insert(src_file.clone());
        db.upsert_file(&src_file).unwrap();
        src.inner
            .content
            .lock()
            .unwrap()
            .insert(src_file_id.0.clone(), b"hello world".to_vec());

        let handlers = EngineHandlers::new(vfs, db.clone(), cache_dir.path().to_path_buf());

        let err = handlers
            .move_item("src:file1", "dst:root", "renamed.txt")
            .await
            .unwrap_err();

        assert_eq!(err.code, ErrorCode::Internal);
        assert!(
            err.message.contains("src:file1"),
            "message must name the source item: {}",
            err.message
        );
        // Destination should have the new file, and a matching entry should
        // have been persisted in the DB before the error returned.
        let dst_entry = dst
            .files
            .lock()
            .unwrap()
            .values()
            .find(|entry| entry.name == "renamed.txt")
            .cloned()
            .expect("destination upload must have committed");
        assert!(
            err.message.contains(&dst_entry.id.0),
            "message must name the destination item: {}",
            err.message
        );
        let persisted = db.get_file(&dst_entry.id).unwrap();
        assert!(
            persisted.is_some(),
            "destination must be in the DB even on partial failure"
        );
    }

    #[tokio::test]
    async fn current_sync_cursor_returns_stable_value_when_no_changes() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let first = handlers.current_sync_cursor("stub:root").await.unwrap();
        let second = handlers.current_sync_cursor("stub:root").await.unwrap();
        assert_eq!(first, second);
        assert!(!first.is_empty());
    }

    #[tokio::test]
    async fn current_sync_cursor_changes_after_upsert() {
        let (handlers, backend, _tempdir) = make_handlers();
        let before = handlers.current_sync_cursor("stub:root").await.unwrap();

        backend.insert(FileEntry {
            id: ItemId::new("stub", "file2"),
            parent_id: ItemId::new("stub", "root"),
            name: "second.txt".to_string(),
            is_dir: false,
            size: Some(4),
            mod_time: Some(Utc::now()),
            mime_type: None,
            hash: None,
        });

        let after = handlers.current_sync_cursor("stub:root").await.unwrap();
        assert_ne!(before, after);
    }

    #[tokio::test]
    async fn current_sync_cursor_fails_for_unknown_backend() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let err = handlers
            .current_sync_cursor("ghost:root")
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }

    /// Populate the stub backend with `count` extra files under `stub:root`
    /// with deterministic names so the sort order is stable and easy to
    /// reason about across pages.
    fn seed_children(backend: &InMemoryBackend, count: usize) {
        let parent = ItemId::new("stub", "root");
        for index in 0..count {
            // Zero-pad to 4 digits so lexicographic sort matches numeric order.
            let native = format!("p{index:04}");
            let item_id = ItemId::new("stub", &native);
            let entry = FileEntry {
                id: item_id,
                parent_id: parent.clone(),
                name: format!("file-{index:04}.txt"),
                is_dir: false,
                size: Some(1),
                mod_time: Some(Utc::now()),
                mime_type: Some("text/plain".to_string()),
                hash: None,
            };
            backend.insert(entry);
        }
    }

    #[tokio::test]
    async fn enumerate_items_returns_null_next_page_when_done() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let output = handlers.enumerate_items("stub:root", None).await.unwrap();
        // Only one seeded child; page size 256, so we're done in one shot.
        assert_eq!(output.items.len(), 1);
        assert!(output.next_page.is_none());
    }

    #[tokio::test]
    async fn enumerate_items_paginates_when_more_than_one_page_of_children() {
        let (handlers, backend, _tempdir) = make_handlers();
        // Replace the seed child with a clean slate plus a known count.
        backend
            .files
            .lock()
            .unwrap()
            .retain(|_, entry| entry.is_dir);
        let total: usize = 600;
        seed_children(&backend, total);

        let mut seen: Vec<String> = Vec::new();
        let mut page_cursor: Option<String> = None;
        loop {
            let output = handlers
                .enumerate_items("stub:root", page_cursor.as_deref())
                .await
                .unwrap();
            for item in &output.items {
                seen.push(item.id.clone());
            }
            match output.next_page {
                Some(cursor) => page_cursor = Some(cursor),
                None => break,
            }
            assert!(
                seen.len() < total,
                "consumed all items but engine kept emitting next_page"
            );
        }

        assert_eq!(
            seen.len(),
            total,
            "all children must be returned exactly once"
        );
        let mut deduped = seen.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(deduped.len(), total, "no duplicates across pages");
    }

    #[tokio::test]
    async fn enumerate_items_rejects_malformed_page_cursor() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let err = handlers
            .enumerate_items("stub:root", Some("!!!not-base64!!!"))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Internal);
    }

    /// First call with no prior cursor must return every current child
    /// as added, an empty deleted set, and a non-empty new cursor.
    #[tokio::test]
    async fn enumerate_changes_first_call_returns_all_as_added() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let output = handlers.enumerate_changes("stub:root", None).await.unwrap();

        // Seeded child is `stub:file1` with name "hello.txt".
        assert_eq!(output.added_or_modified.len(), 1);
        assert_eq!(output.added_or_modified[0].id, "stub:file1");
        assert!(output.deleted.is_empty());
        assert!(!output.new_cursor.is_empty());

        // The cursor we just got back must match what
        // `currentSyncCursor` would emit right now — the two RPCs share
        // a derivation, so consumers can interchange them freely.
        let live_cursor = handlers.current_sync_cursor("stub:root").await.unwrap();
        assert_eq!(live_cursor, output.new_cursor);
    }

    /// After the snapshot is taken, an unchanged view must yield no
    /// deltas and the same cursor.
    #[tokio::test]
    async fn enumerate_changes_unchanged_view_returns_empty_delta() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let first = handlers.enumerate_changes("stub:root", None).await.unwrap();

        let second = handlers
            .enumerate_changes("stub:root", Some(&first.new_cursor))
            .await
            .unwrap();

        assert!(second.added_or_modified.is_empty());
        assert!(second.deleted.is_empty());
        assert_eq!(second.new_cursor, first.new_cursor);
    }

    /// Adding a child, deleting a child, and modifying a child between
    /// two snapshots must surface in the delta as added/deleted/modified.
    #[tokio::test]
    async fn enumerate_changes_reflects_add_delete_modify() {
        let (handlers, backend, _tempdir) = make_handlers();
        let first = handlers.enumerate_changes("stub:root", None).await.unwrap();

        // Add a new file.
        let new_file_id = ItemId::new("stub", "file2");
        backend.insert(FileEntry {
            id: new_file_id.clone(),
            parent_id: ItemId::new("stub", "root"),
            name: "second.txt".to_string(),
            is_dir: false,
            size: Some(4),
            mod_time: Some(Utc::now()),
            mime_type: None,
            hash: None,
        });

        // Modify the existing file (bump size).
        {
            let mut files = backend
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = files.get_mut("stub:file1").unwrap();
            entry.size = Some(42);
            entry.mod_time = Some(Utc::now());
        }

        // Note: no deletion yet — set up a second snapshot first so we
        // can split add / modify from delete cleanly.
        let second = handlers
            .enumerate_changes("stub:root", Some(&first.new_cursor))
            .await
            .unwrap();

        let ids: Vec<&str> = second
            .added_or_modified
            .iter()
            .map(|item| item.id.as_str())
            .collect();
        assert!(
            ids.contains(&"stub:file2"),
            "newly inserted file must appear as added"
        );
        assert!(
            ids.contains(&"stub:file1"),
            "metadata change must appear as modified"
        );
        assert!(second.deleted.is_empty(), "no deletes yet");
        assert_ne!(second.new_cursor, first.new_cursor);

        // Now delete the original file and check the delta.
        backend
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove("stub:file1");

        let third = handlers
            .enumerate_changes("stub:root", Some(&second.new_cursor))
            .await
            .unwrap();
        assert!(
            third.added_or_modified.is_empty(),
            "no further additions after delete"
        );
        assert_eq!(third.deleted, vec!["stub:file1".to_string()]);
        assert_ne!(third.new_cursor, second.new_cursor);
    }

    /// A cursor the engine does not recognise (either because the
    /// snapshot was dropped or because the client is on a stale cursor
    /// from a different run) must behave like a first call: every child
    /// reported as added, nothing reported as deleted.
    #[tokio::test]
    async fn enumerate_changes_cursor_mismatch_behaves_like_first_call() {
        let (handlers, _backend, _tempdir) = make_handlers();
        // Seed the snapshot first so we know mismatched cursors aren't
        // simply "no snapshot stored".
        let _ = handlers.enumerate_changes("stub:root", None).await.unwrap();

        let bogus = SyncCursor::new(vec![0xde, 0xad, 0xbe, 0xef]);
        let output = handlers
            .enumerate_changes("stub:root", Some(&bogus))
            .await
            .unwrap();

        // Everything seeded under the parent must come back as added.
        assert_eq!(output.added_or_modified.len(), 1);
        assert_eq!(output.added_or_modified[0].id, "stub:file1");
        assert!(output.deleted.is_empty());
        assert!(!output.new_cursor.is_empty());
    }

    /// Snapshots are keyed by parent ID; deltas for one parent must not
    /// bleed into another. Sanity check that two independent parents
    /// each carry their own snapshot.
    #[tokio::test]
    async fn enumerate_changes_per_parent_isolation() {
        let (handlers, backend, _tempdir) = make_handlers();

        // Add a second directory with its own child.
        let other_dir = ItemId::new("stub", "other");
        backend.insert(FileEntry {
            id: other_dir.clone(),
            parent_id: ItemId::new("stub", "root"),
            name: "other".to_string(),
            is_dir: true,
            size: None,
            mod_time: Some(Utc::now()),
            mime_type: None,
            hash: None,
        });
        backend.insert(FileEntry {
            id: ItemId::new("stub", "other-child"),
            parent_id: other_dir.clone(),
            name: "child.txt".to_string(),
            is_dir: false,
            size: Some(1),
            mod_time: Some(Utc::now()),
            mime_type: None,
            hash: None,
        });

        let root_first = handlers.enumerate_changes("stub:root", None).await.unwrap();
        let other_first = handlers
            .enumerate_changes("stub:other", None)
            .await
            .unwrap();

        // Modify only the `other` parent.
        backend
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove("stub:other-child");

        let root_second = handlers
            .enumerate_changes("stub:root", Some(&root_first.new_cursor))
            .await
            .unwrap();
        let other_second = handlers
            .enumerate_changes("stub:other", Some(&other_first.new_cursor))
            .await
            .unwrap();

        assert!(
            root_second.added_or_modified.is_empty() && root_second.deleted.is_empty(),
            "root parent must be unchanged"
        );
        assert_eq!(
            other_second.deleted,
            vec!["stub:other-child".to_string()],
            "only the touched parent should see the delete"
        );
    }

    #[tokio::test]
    async fn enumerate_changes_fails_for_unknown_backend() {
        let (handlers, _backend, _tempdir) = make_handlers();
        let err = handlers
            .enumerate_changes("ghost:root", None)
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }
}
