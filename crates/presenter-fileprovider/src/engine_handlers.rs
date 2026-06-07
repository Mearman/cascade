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
use cascade_engine::changefeed::{ChangeFeed, ChangeQueryResult};
use cascade_engine::db::StateDb;
use cascade_engine::types::{CacheState, Change, FileEntry, FileId, ItemId, SyncCursor, VfsItem};
use cascade_engine::vfs::{VfsTree, derive_sync_cursor};
use chrono::{DateTime, Utc};
use data_encoding::BASE64URL_NOPAD;
use sha2::{Digest, Sha256};
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

/// Snapshot of the child set under a parent directory.
///
/// The primary path through `enumerateChanges` reads from the engine's
/// [`ChangeFeed`], which serves real per-parent change events derived
/// from `Backend::changes`. The snapshot here is retained as a fallback
/// for the first observation of a parent and for callers that present
/// an evicted or version-mismatched cursor — the only times the feed
/// cannot answer a delta query. Keying the snapshot by item ID lets
/// the fallback diff the current child set against the prior one
/// without consulting the feed.
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

/// Magic prefix marking a [`SyncCursorV2`]-encoded `SyncCursor`.
///
/// V1 cursors are bare SHA-256 hashes (always 32 bytes, no prefix).
/// V2 cursors carry this three-byte ASCII tag so the handler can tell
/// them apart on the wire and fall back to a fresh enumeration when a
/// legacy V1 cursor arrives after a daemon upgrade.
const SYNC_CURSOR_V2_MAGIC: &[u8] = b"CF2";

/// Wire-stable cursor handed to the File Provider extension for
/// `enumerateChanges` resumption.
///
/// Carries the tuple `(backend_id, parent_id, feed_seq,
/// snapshot_hash)` that the `enumerateChanges` handler hands back to
/// resume an incremental delta. The bytes are not opaque to the
/// engine — they encode the exact resume key — but Apple's File
/// Provider framework treats them as a black box and round-trips them
/// unchanged.
///
/// `feed_seq` anchors the engine-side [`ChangeFeed`] query: events
/// strictly after `feed_seq` are translated into the
/// added-or-modified/deleted sets. `snapshot_hash` mirrors the V1
/// SHA-256 content hash so that callers see a fresh cursor whenever
/// the snapshot store advances, even when the change feed has
/// observed no events (e.g. a test backend that does not stream
/// changes through `Backend::changes`).
///
/// Encoding layout (little-endian throughout):
///
/// ```text
///   "CF2" (3 bytes) | backend_id_len: u16 | backend_id bytes |
///                   | parent_id_len: u16  | parent_id bytes  |
///                   | feed_seq: u64       | snapshot_hash_len: u16 |
///                   | snapshot_hash bytes
/// ```
///
/// A future round of the cursor format can lift the version byte
/// implied by the `CF2` magic and rev it to `CF3`; the V2 decoder
/// treats anything that does not start with `CF2` as a legacy cursor
/// and signals a fresh-enumeration fallback to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SyncCursorV2 {
    backend_id: String,
    parent_id: ItemId,
    feed_seq: u64,
    snapshot_hash: Vec<u8>,
}

impl SyncCursorV2 {
    /// Serialise the cursor into a [`SyncCursor`] for transport.
    fn encode(&self) -> SyncCursor {
        let backend_bytes = self.backend_id.as_bytes();
        let parent_bytes = self.parent_id.0.as_bytes();
        // Reject ID strings that overflow the u16 length prefix. In
        // practice every ItemId we mint is a short colon-prefixed
        // string, but the bound is part of the wire contract.
        let backend_len = u16::try_from(backend_bytes.len()).unwrap_or(u16::MAX);
        let parent_len = u16::try_from(parent_bytes.len()).unwrap_or(u16::MAX);
        let hash_len = u16::try_from(self.snapshot_hash.len()).unwrap_or(u16::MAX);

        let mut out = Vec::with_capacity(
            SYNC_CURSOR_V2_MAGIC
                .len()
                .saturating_add(2)
                .saturating_add(backend_bytes.len())
                .saturating_add(2)
                .saturating_add(parent_bytes.len())
                .saturating_add(8)
                .saturating_add(2)
                .saturating_add(self.snapshot_hash.len()),
        );
        out.extend_from_slice(SYNC_CURSOR_V2_MAGIC);
        out.extend_from_slice(&backend_len.to_le_bytes());
        out.extend_from_slice(
            backend_bytes
                .get(..usize::from(backend_len))
                .unwrap_or(backend_bytes),
        );
        out.extend_from_slice(&parent_len.to_le_bytes());
        out.extend_from_slice(
            parent_bytes
                .get(..usize::from(parent_len))
                .unwrap_or(parent_bytes),
        );
        out.extend_from_slice(&self.feed_seq.to_le_bytes());
        out.extend_from_slice(&hash_len.to_le_bytes());
        out.extend_from_slice(
            self.snapshot_hash
                .get(..usize::from(hash_len))
                .unwrap_or(&self.snapshot_hash),
        );
        SyncCursor::new(out)
    }

    /// Parse a cursor previously emitted by [`Self::encode`].
    ///
    /// Returns `Ok(None)` if the cursor is empty, missing the V2 magic,
    /// or otherwise refers to a different version — callers fall back
    /// to a fresh enumeration in that case. Returns `Err` only when
    /// the magic matches but the framing is corrupt; that is an engine
    /// bug rather than a legacy-cursor situation.
    fn decode(cursor: &SyncCursor) -> Result<Option<Self>, String> {
        let bytes = cursor.as_bytes();
        if bytes.len() < SYNC_CURSOR_V2_MAGIC.len() {
            return Ok(None);
        }
        let Some((magic, rest)) = bytes.split_at_checked(SYNC_CURSOR_V2_MAGIC.len()) else {
            return Ok(None);
        };
        if magic != SYNC_CURSOR_V2_MAGIC {
            return Ok(None);
        }

        let (backend_id, rest) = take_lp_string(rest, "backend_id")?;
        let (parent_id_raw, rest) = take_lp_string(rest, "parent_id")?;
        let Some((seq_bytes, rest)) = rest.split_at_checked(8) else {
            return Err("V2 cursor truncated before feed_seq".to_string());
        };
        let mut seq_arr = [0u8; 8];
        seq_arr.copy_from_slice(seq_bytes);

        let Some((hash_len_bytes, rest)) = rest.split_at_checked(2) else {
            return Err("V2 cursor truncated before snapshot_hash length".to_string());
        };
        let mut hash_len_arr = [0u8; 2];
        hash_len_arr.copy_from_slice(hash_len_bytes);
        let hash_len = usize::from(u16::from_le_bytes(hash_len_arr));
        let Some((hash_payload, tail)) = rest.split_at_checked(hash_len) else {
            return Err("V2 cursor truncated reading snapshot_hash payload".to_string());
        };
        if !tail.is_empty() {
            return Err("V2 cursor has trailing bytes after snapshot_hash".to_string());
        }
        Ok(Some(Self {
            backend_id,
            parent_id: ItemId(parent_id_raw),
            feed_seq: u64::from_le_bytes(seq_arr),
            snapshot_hash: hash_payload.to_vec(),
        }))
    }
}

/// Pull a length-prefixed UTF-8 string off the front of a slice.
///
/// Returns the decoded string and the remaining bytes. Used by
/// [`SyncCursorV2::decode`] to parse the backend ID and parent ID
/// fields in turn.
fn take_lp_string<'a>(bytes: &'a [u8], field: &str) -> Result<(String, &'a [u8]), String> {
    let Some((len_bytes, rest)) = bytes.split_at_checked(2) else {
        return Err(format!("V2 cursor truncated before {field} length"));
    };
    let mut len_arr = [0u8; 2];
    len_arr.copy_from_slice(len_bytes);
    let len = usize::from(u16::from_le_bytes(len_arr));
    let Some((payload, tail)) = rest.split_at_checked(len) else {
        return Err(format!("V2 cursor truncated reading {field} payload"));
    };
    let decoded = std::str::from_utf8(payload)
        .map_err(|err| format!("V2 cursor {field} is not valid UTF-8: {err}"))?
        .to_string();
    Ok((decoded, tail))
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
    /// Engine-side per-parent change index.
    ///
    /// Primary source for `enumerateChanges` deltas. When the feed can
    /// answer a `(backend_id, parent_id, seq)` query the handler
    /// translates its events directly into added-or-modified entries
    /// plus deleted IDs. When it cannot (`Unknown` or `Evicted`, or
    /// when the caller's cursor is a legacy V1) the handler falls back
    /// to the snapshot diff below.
    change_feed: Arc<ChangeFeed>,
    /// In-memory per-parent snapshot store used as the fallback path
    /// for `enumerateChanges` when the change feed cannot resume.
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
    /// demand. `change_feed` is the engine's shared per-parent change
    /// index — see the `cascade_engine::changefeed` module for the
    /// polling contract and eviction semantics.
    pub fn new(
        vfs: Arc<RwLock<VfsTree>>,
        db: Arc<StateDb>,
        cache_dir: PathBuf,
        change_feed: Arc<ChangeFeed>,
    ) -> Self {
        Self {
            vfs,
            db,
            cache_dir,
            change_feed,
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
    /// Files take the simple path: stage to disk, upload to the
    /// destination, then delete the source. If the upload fails the
    /// source is left untouched. If the source delete fails after a
    /// successful upload, the move is reported as `Internal` with both
    /// `ItemId`s — the data is now in both places and the user must
    /// clean up the source manually. We deliberately do not roll back
    /// the destination in that case because that would destroy the only
    /// successful copy of the data.
    ///
    /// Directories descend recursively: the destination tree is built
    /// up file-by-file via [`Self::copy_directory_tree`], and only once every
    /// child has been copied successfully is the source subtree
    /// deleted. If any child copy fails partway through, the partially
    /// built destination tree is left in place and the source subtree
    /// is not touched at all. The caller sees an `Internal` error
    /// pointing at the failed child; the user can either retry the move
    /// (which will collide at the top-level name and be rejected) or
    /// delete the partial destination and try again. We choose this
    /// over rolling back because partial state on the destination is
    /// recoverable, whereas data loss on the source is not.
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

        if src_entry.is_dir {
            self.cross_backend_move_directory(
                &src_entry,
                src_backend.as_ref(),
                dst_backend.as_ref(),
                new_parent,
                new_name,
            )
            .await
        } else {
            self.cross_backend_move_file(
                &src_entry,
                src_backend.as_ref(),
                dst_backend.as_ref(),
                new_parent,
                new_name,
            )
            .await
        }
    }

    /// Top-level cross-backend file move: stage, upload, then delete
    /// the source. Updates the state DB with the new destination entry
    /// and removes the source subtree on success.
    async fn cross_backend_move_file(
        &self,
        src_entry: &FileEntry,
        src_backend: &dyn Backend,
        dst_backend: &dyn Backend,
        new_parent: &ItemId,
        new_name: &str,
    ) -> HandlerResult<FileProviderItem> {
        let dst_entry = self
            .copy_file_across_backends(src_entry, src_backend, dst_backend, new_parent, new_name)
            .await?;

        // Upload committed — try to delete the source. If this fails the
        // data is in BOTH places; we have to surface that loudly rather
        // than silently lose data by rolling back the destination.
        if let Err(err) = src_backend.delete(src_entry).await {
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

    /// Top-level cross-backend directory move: recursively copy the
    /// source tree to the destination, and only delete the source
    /// subtree once every child has been copied successfully.
    ///
    /// Partial-failure semantics: if any child copy fails, the
    /// partially-built destination tree is left in place and the
    /// source is untouched. We do not roll back the destination
    /// because the source subtree is still authoritative — the user
    /// can delete the partial destination tree and retry. Rolling
    /// back automatically risks deleting data the user has already
    /// observed on the destination, which is strictly worse than
    /// leaving a partial tree behind.
    async fn cross_backend_move_directory(
        &self,
        src_entry: &FileEntry,
        src_backend: &dyn Backend,
        dst_backend: &dyn Backend,
        new_parent: &ItemId,
        new_name: &str,
    ) -> HandlerResult<FileProviderItem> {
        let dst_root = self
            .copy_directory_tree(src_entry, src_backend, dst_backend, new_parent, new_name)
            .await?;

        // Entire tree copied — now reclaim the source. We walk
        // leaves-first so backends that do not cascade delete (the
        // in-memory test backend and some primitive providers) still
        // see an empty directory by the time we delete the root.
        if let Err(err) = Self::delete_source_subtree(src_entry, src_backend).await {
            tracing::error!(
                source_id = %src_entry.id,
                destination_id = %dst_root.id,
                error = %err,
                "cross-backend directory move: destination tree built but source subtree delete failed; manual cleanup required",
            );
            self.db.upsert_file(&dst_root)?;
            return Err(HandlerError::internal(format!(
                "cross-backend directory move partially completed: destination {} is the new copy; source {} still exists and could not be deleted ({err})",
                dst_root.id, src_entry.id,
            )));
        }

        self.db.upsert_file(&dst_root)?;
        self.db.delete_subtree(&src_entry.id)?;
        Ok(FileProviderItem::from(VfsItem::from(dst_root)))
    }

    /// Stage `src_entry` to a temp file, upload it to `dst_backend`
    /// under `dst_parent`, then unlink the staging file.
    ///
    /// Does not touch the source backend or the state DB — callers
    /// compose this into the larger move flow. Returns the new
    /// destination `FileEntry`.
    async fn copy_file_across_backends(
        &self,
        src_entry: &FileEntry,
        src_backend: &dyn Backend,
        dst_backend: &dyn Backend,
        dst_parent: &ItemId,
        new_name: &str,
    ) -> HandlerResult<FileEntry> {
        let staging_path = self.stage_for_move(src_entry, src_backend).await?;

        let data = tokio::fs::read(&staging_path).await.map_err(|err| {
            HandlerError::internal(format!(
                "read staging file {}: {err}",
                staging_path.display()
            ))
        })?;
        let dst_parent_file_id = FileId(dst_parent.0.clone());
        let upload_result = dst_backend
            .upload(Path::new(new_name), &data, &dst_parent_file_id)
            .await;

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

        upload_result.map_err(HandlerError::from)
    }

    /// Recursively copy `src_dir` (a directory `FileEntry`) from
    /// `src_backend` into `dst_backend` under `dst_parent`, naming the
    /// new top-level directory `new_name`.
    ///
    /// The destination directory is created first; then every child
    /// is copied via `copy_file_across_backends` (for files) or this
    /// method recursively (for subdirectories). The source is left
    /// untouched — the caller is responsible for deleting it after
    /// the entire tree has been copied successfully.
    ///
    /// Returns the new top-level destination directory entry.
    fn copy_directory_tree<'a>(
        &'a self,
        src_dir: &'a FileEntry,
        src_backend: &'a dyn Backend,
        dst_backend: &'a dyn Backend,
        dst_parent: &'a ItemId,
        new_name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = HandlerResult<FileEntry>> + Send + 'a>>
    {
        Box::pin(async move {
            // Create the destination directory before walking children
            // so subsequent uploads have a parent ID to point at.
            let dst_parent_file_id = FileId(dst_parent.0.clone());
            let dst_dir = dst_backend
                .create_dir_with_parent(new_name, &dst_parent_file_id)
                .await
                .map_err(HandlerError::from)?;

            let children = src_backend
                .list_children(src_dir.id.native_id())
                .await
                .map_err(|err| {
                    HandlerError::internal(format!("list source children of {}: {err}", src_dir.id))
                })?;

            for child in children {
                if child.is_dir {
                    self.copy_directory_tree(
                        &child,
                        src_backend,
                        dst_backend,
                        &dst_dir.id,
                        &child.name,
                    )
                    .await?;
                } else {
                    self.copy_file_across_backends(
                        &child,
                        src_backend,
                        dst_backend,
                        &dst_dir.id,
                        &child.name,
                    )
                    .await?;
                }
            }

            Ok(dst_dir)
        })
    }

    /// Recursively delete the source subtree rooted at `entry` from
    /// `backend`, leaves first. Used after a successful directory
    /// copy to reclaim space on the source side.
    ///
    /// Walks leaves-first because some backends (the in-memory test
    /// backend, and primitive providers that lack server-side cascade
    /// delete) only remove the directly-named entry. Backends that do
    /// cascade delete (Google Drive trashes the whole subtree when you
    /// trash a parent) see an already-empty directory by the time we
    /// reach the root and the redundant per-child deletes succeed
    /// because the children are already gone.
    fn delete_source_subtree<'a>(
        entry: &'a FileEntry,
        backend: &'a dyn Backend,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            if entry.is_dir {
                let children = backend.list_children(entry.id.native_id()).await?;
                for child in children {
                    Self::delete_source_subtree(&child, backend).await?;
                }
            }
            backend.delete(entry).await
        })
    }

    /// Snapshot-based fallback for `enumerate_changes`.
    ///
    /// Used when the change feed cannot resume — either the caller's
    /// cursor is a legacy V1 SHA-256 hash, it names a different
    /// `(backend, parent)` from the requested one, or the feed reports
    /// `Unknown`/`Evicted` for the requested key. In every case the
    /// fallback computes a fresh per-parent diff against the most
    /// recent stored snapshot and emits a V2 cursor anchored to the
    /// feed's current high-water mark for this parent plus the hash
    /// of the new snapshot. The next call carrying that cursor will
    /// resume the feed path; if the feed still has no events, the
    /// snapshot hash on the cursor lets callers detect content-only
    /// changes without consulting the feed at all.
    async fn enumerate_changes_via_snapshot(
        &self,
        parent: &ItemId,
        since_cursor: Option<&SyncCursor>,
        decoded_v2: Option<&SyncCursorV2>,
    ) -> HandlerResult<EnumerateChangesOutput> {
        let backend = self.backend_for(parent)?;
        let mut entries = backend
            .list_children(parent.native_id())
            .await
            .map_err(HandlerError::from)?;
        entries.sort_by(|a, b| a.id.0.cmp(&b.id.0));

        let legacy_v1_cursor = derive_cursor_from_sorted(&entries);
        let snapshot_hash = legacy_v1_cursor.as_bytes().to_vec();
        let current: HashMap<String, SnapshotEntry> = entries
            .iter()
            .map(|entry| (entry.id.0.clone(), SnapshotEntry::from_entry(entry)))
            .collect();

        // Pull the feed's current head so the cursor we emit can resume
        // the delta path immediately on the next call. Querying with
        // `since=None` returns the max seq the feed has observed for
        // this parent so far; if the feed is unaware of the parent the
        // cursor starts at 0 and the first real event will land at seq=0.
        let backend_id = parent.backend_id().to_string();
        let feed_head_seq = match self
            .change_feed
            .parent_changes_since(&backend_id, parent, None)
            .await
        {
            ChangeQueryResult::Delta { new_seq, .. } => new_seq,
            ChangeQueryResult::Evicted | ChangeQueryResult::Unknown => 0,
        };
        let new_cursor = SyncCursorV2 {
            backend_id,
            parent_id: parent.clone(),
            feed_seq: feed_head_seq,
            snapshot_hash,
        }
        .encode();

        let mut snapshots = self.snapshots.lock().await;
        let parent_key = parent.0.clone();
        let previous = snapshots.get(&parent_key);

        // Decide whether to diff against the stored snapshot. We diff
        // when the caller's cursor matches either the prior V1 hash or
        // the V2 cursor's snapshot_hash; otherwise the call is a first
        // observation and every child is "added".
        let use_incremental = match (since_cursor, decoded_v2, previous) {
            (Some(cursor), None, Some(snapshot)) => snapshot.cursor == *cursor,
            (_, Some(v2), Some(snapshot)) => snapshot.cursor.as_bytes() == v2.snapshot_hash,
            _ => false,
        };

        let output = if use_incremental {
            let previous_children = &previous
                .ok_or_else(|| {
                    HandlerError::internal(
                        "snapshot disappeared between lookup and diff".to_string(),
                    )
                })?
                .children;
            diff_snapshots(previous_children, &current, &entries, new_cursor.clone())
        } else {
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
            parent_key,
            ParentSnapshot {
                cursor: legacy_v1_cursor,
                children: current,
            },
        );
        Ok(output)
    }

    /// Refresh the stored snapshot for a parent and return its V1
    /// SHA-256 hash for embedding in a V2 cursor.
    ///
    /// Called from the change-feed path after a successful `Delta` so
    /// later snapshot-fallback calls (e.g. after an eviction) can still
    /// produce a coherent diff against the most recent observed state.
    async fn refresh_snapshot(
        &self,
        parent: &ItemId,
        backend: &dyn Backend,
    ) -> HandlerResult<Vec<u8>> {
        let mut entries = backend
            .list_children(parent.native_id())
            .await
            .map_err(HandlerError::from)?;
        entries.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        let legacy_v1_cursor = derive_cursor_from_sorted(&entries);
        let snapshot_hash = legacy_v1_cursor.as_bytes().to_vec();
        let current: HashMap<String, SnapshotEntry> = entries
            .iter()
            .map(|entry| (entry.id.0.clone(), SnapshotEntry::from_entry(entry)))
            .collect();
        let mut snapshots = self.snapshots.lock().await;
        snapshots.insert(
            parent.0.clone(),
            ParentSnapshot {
                cursor: legacy_v1_cursor,
                children: current,
            },
        );
        Ok(snapshot_hash)
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

        let data = src_backend.download(src_entry).await?;
        tokio::fs::write(&staging_path, &data)
            .await
            .map_err(|err| HandlerError::internal(format!("write staging file: {err}")))?;
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

        let data = backend.download(&entry).await?;
        tokio::fs::write(&temp_path, &data)
            .await
            .map_err(|error| HandlerError::internal(format!("write temp file: {error}")))?;
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

        let data = tokio::fs::read(&source_path).await.map_err(|error| {
            HandlerError::internal(format!("read source {}: {error}", source_path.display()))
        })?;

        let entry = if let Some(existing) = existing_id {
            let existing_item = ItemId(existing.to_string());
            let file_id = FileId(existing_item.0.clone());
            backend.update(&file_id, &data).await?
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
                .upload(Path::new(&filename), &data, &parent_file_id)
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
        let backend_id_owned = parent.backend_id().to_string();

        // Decode the caller's cursor. A V2 cursor that names this
        // backend and parent feeds the change-feed path; anything else
        // (legacy V1, malformed, mismatched parent, absent) drops to
        // the snapshot fallback for this call only.
        let decoded_v2 = match since_cursor {
            Some(cursor) => SyncCursorV2::decode(cursor).map_err(HandlerError::internal)?,
            None => None,
        };
        let feed_query = decoded_v2.as_ref().and_then(|cursor| {
            if cursor.backend_id == backend_id_owned && cursor.parent_id == parent {
                Some(cursor.feed_seq)
            } else {
                None
            }
        });

        if let Some(since_seq) = feed_query {
            let result = self
                .change_feed
                .parent_changes_since(&backend_id_owned, &parent, Some(since_seq))
                .await;
            if let ChangeQueryResult::Delta { events, new_seq } = result
                && !events.is_empty()
            {
                // The feed has real per-parent events. Translate them
                // and refresh the snapshot store so a future fallback
                // call still produces a coherent diff against the
                // post-event state.
                let backend = backend_for_self(&self.vfs, &parent)?;
                let snapshot_hash = self.refresh_snapshot(&parent, backend.as_ref()).await?;
                let cursor_v2 = SyncCursorV2 {
                    backend_id: backend_id_owned,
                    parent_id: parent.clone(),
                    feed_seq: new_seq,
                    snapshot_hash,
                };
                let (added_or_modified, deleted) = translate_change_events(events);
                return Ok(EnumerateChangesOutput {
                    added_or_modified,
                    deleted,
                    new_cursor: cursor_v2.encode(),
                });
            }
            // Empty Delta, Evicted, or Unknown — drop to the snapshot
            // fallback so a stale-feed presenter still sees backend
            // changes that arrived outside the change stream (e.g.
            // tests using in-memory backends that mutate state
            // directly).
        }

        self.enumerate_changes_via_snapshot(&parent, since_cursor, decoded_v2.as_ref())
            .await
    }
}

/// Look up the backend that owns a parent ID without going through
/// `&self` — useful inside `enumerate_changes` where we need to call
/// `refresh_snapshot(&self, ..., backend: &dyn Backend)` after we
/// already released the `std::sync::RwLock` guard once.
fn backend_for_self(
    vfs: &Arc<RwLock<VfsTree>>,
    parent: &ItemId,
) -> HandlerResult<Arc<dyn Backend>> {
    let guard = vfs
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard
        .backend_by_id(parent.backend_id())
        .cloned()
        .ok_or_else(|| {
            HandlerError::not_found(format!(
                "no backend registered for id {}",
                parent.backend_id()
            ))
        })
}

/// Convert a vector of `Change` events into the `(added_or_modified,
/// deleted)` pair the File Provider extension expects.
///
/// `Created` and `Updated` events surface as added-or-modified items;
/// `Deleted` events surface as deleted IDs. The change feed has already
/// split `Moved` events into a delete on the old parent and a create
/// on the new parent, so this function does not need to handle them
/// explicitly.
fn translate_change_events(events: Vec<Change>) -> (Vec<FileProviderItem>, Vec<String>) {
    let mut added_or_modified: Vec<FileProviderItem> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();
    for event in events {
        match event {
            Change::Created(entry) | Change::Updated { new: entry, .. } => {
                added_or_modified.push(FileProviderItem::from(VfsItem::from(entry)));
            }
            Change::Deleted(entry) => {
                deleted.push(entry.id.0);
            }
            Change::Moved { from, to } => {
                // The change feed partitions Moved events, so by the
                // time they hit a presenter they should already be
                // pairs of Deleted/Created. Defensively handle them
                // anyway so a future feed change cannot break the
                // presenter silently.
                deleted.push(from.id.0);
                added_or_modified.push(FileProviderItem::from(VfsItem::from(to)));
            }
        }
    }
    (added_or_modified, deleted)
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

#[cfg(test)]
#[path = "engine_handlers_tests.rs"]
mod tests;
