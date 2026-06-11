//! Engine-side per-parent change index.
//!
//! Presenters that hand out a `(parent, since)` -> `Vec<Change>` style API to
//! the operating system — Apple's `enumerateChanges`, Linux FUSE's invalidate
//! channel, future Windows `ProjFS` callbacks — need a way to ask the engine
//! "what changed under parent X since I last looked?" The [`Backend::changes`]
//! trait method only exposes a *global* per-backend change stream; the change
//! feed is the engine's shared fan-out layer that partitions that stream into
//! per-parent ring buffers so each presenter can serve incremental deltas
//! without keeping its own snapshot table.
//!
//! [`Backend::changes`]: crate::backend::Backend::changes
//!
//! ## Feeding
//!
//! The feed does **not** poll backends. The [`SyncRunner`] already owns the
//! single canonical poll loop — it calls `Backend::changes(cursor)` per
//! backend, persists the cursor, applies changes to the state database, and
//! notifies the presenter. After it has applied a batch it hands the same
//! (post-ignore-filter) changes to [`ChangeFeed::record`], which files them
//! into the per-parent index. Running a second poll loop here would double
//! the backend API load (a real cost against the Google Drive quota) and
//! split the change stream across two independent cursors, so the feed is a
//! passive sink instead.
//!
//! [`SyncRunner`]: crate::sync::runner::SyncRunner
//!
//! ## Per-parent eviction
//!
//! Events are filed into a per-parent [`std::collections::VecDeque`] with a
//! hard cap of [`PER_PARENT_CAPACITY`] entries. When the buffer is full the
//! oldest entry is dropped — callers asking for a `since` sequence older
//! than the oldest retained entry get [`ChangeQueryResult::Evicted`] back
//! and are expected to fall back to a fresh enumeration. The cap is large
//! enough that a presenter polling once a minute should never miss events
//! short of a catastrophic burst, and small enough that an idle daemon
//! does not retain unbounded history.
//!
//! ## Fallback contract
//!
//! The feed surfaces three distinct outcomes for `parent_changes_since`:
//!
//! - [`ChangeQueryResult::Delta`] — events strictly after `since`, plus the
//!   new max sequence. The presenter translates these into added/modified
//!   `FileEntry`s and deleted IDs.
//! - [`ChangeQueryResult::Evicted`] — the caller's `since` predates the
//!   buffer's oldest retained entry. The presenter must fall back to a
//!   fresh enumeration this round; the next call carrying the updated
//!   cursor will resume the delta path.
//! - [`ChangeQueryResult::Unknown`] — the feed has never observed this
//!   parent. Either the parent contains no children yet, or the sync runner
//!   has not yet recorded its first batch. Same fallback as
//!   [`ChangeQueryResult::Evicted`].

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::types::{Change, FileEntry, ItemId};

/// Maximum number of events retained per parent directory.
///
/// 1024 is chosen so a presenter that polls every minute can absorb a
/// 17-event-per-second burst for a full minute without losing history.
/// At ~200 bytes per `Change` event (one or two `FileEntry`s) the
/// per-parent cap costs ~200KB — well within the memory budget for the
/// hundreds of parents a normal Finder window touches.
pub const PER_PARENT_CAPACITY: usize = 1024;

/// A change event tagged with the backend-monotonic sequence number used
/// to drive incremental delta queries.
///
/// The sequence is allocated by the [`ChangeFeed`] at the moment the
/// event is filed, so two events from the same backend always carry a
/// strict ordering even when the backend emits them in a single batch.
/// Sequences are *per backend*: a presenter's wire cursor therefore
/// includes the backend ID alongside the sequence to disambiguate.
#[derive(Debug, Clone)]
pub struct StampedChange {
    /// Backend-monotonic sequence allocated when the event was filed.
    pub seq: u64,
    /// The underlying change. Cloned out of the backend stream.
    pub change: Change,
}

/// Per-backend bookkeeping owned by a [`ChangeFeed`].
///
/// One instance per recorded backend. `next_seq` allocates the monotonic
/// stamp; `by_parent` partitions the events into ring buffers keyed by the
/// owning directory's [`ItemId`].
#[derive(Debug, Default)]
struct BackendChangeState {
    /// Next sequence number to allocate for this backend.
    next_seq: u64,
    /// Per-parent ring buffers, keyed by the parent's [`ItemId`].
    by_parent: HashMap<ItemId, VecDeque<StampedChange>>,
}

impl BackendChangeState {
    /// File a single change into the per-parent ring buffer.
    ///
    /// A `Moved` event is partitioned into a synthetic delete on the old
    /// parent and a synthetic create on the new parent — that way each
    /// parent buffer carries a self-consistent timeline that a presenter
    /// can replay without cross-parent reconciliation.
    ///
    /// Events whose owning [`FileEntry`] carries an empty `parent_id`
    /// are dropped with a warn-level log: they cannot be filed without
    /// a parent and surfacing them to a presenter would be lying about
    /// which directory changed. This is rare in practice — the existing
    /// backends always populate `parent_id` — but the change feed is
    /// defensive about backend bugs rather than panicking on them.
    fn file_change(&mut self, change: Change) {
        match change {
            Change::Created(ref entry)
            | Change::Updated { new: ref entry, .. }
            | Change::Deleted(ref entry) => {
                let Some(parent) = parent_of(entry) else {
                    tracing::warn!(
                        item = %entry.id,
                        "change feed: dropping event for entry with no parent",
                    );
                    return;
                };
                let seq = self.allocate_seq();
                self.push_event(parent, StampedChange { seq, change });
            }
            Change::Moved { from, to } => {
                match parent_of(&from) {
                    Some(old) => {
                        let seq = self.allocate_seq();
                        self.push_event(
                            old,
                            StampedChange {
                                seq,
                                change: Change::Deleted(from),
                            },
                        );
                    }
                    None => {
                        tracing::warn!(
                            item = %from.id,
                            "change feed: dropping move-from event for entry with no parent",
                        );
                    }
                }
                match parent_of(&to) {
                    Some(new) => {
                        let seq = self.allocate_seq();
                        self.push_event(
                            new,
                            StampedChange {
                                seq,
                                change: Change::Created(to),
                            },
                        );
                    }
                    None => {
                        tracing::warn!(
                            item = %to.id,
                            "change feed: dropping move-to event for entry with no parent",
                        );
                    }
                }
            }
        }
    }

    const fn allocate_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        seq
    }

    fn push_event(&mut self, parent: ItemId, event: StampedChange) {
        let buffer = self.by_parent.entry(parent).or_default();
        if buffer.len() >= PER_PARENT_CAPACITY {
            buffer.pop_front();
        }
        buffer.push_back(event);
    }
}

/// Pull the parent [`ItemId`] off a [`FileEntry`], returning `None` when
/// it is empty (i.e. the entry has no recorded parent).
fn parent_of(entry: &FileEntry) -> Option<ItemId> {
    if entry.parent_id.0.is_empty() {
        None
    } else {
        Some(entry.parent_id.clone())
    }
}

/// Outcome of a [`ChangeFeed::parent_changes_since`] query.
///
/// See the module-level docs for the fallback contract: only `Delta`
/// carries usable events; `Evicted` and `Unknown` both signal "fall back
/// to a fresh enumeration".
#[derive(Debug)]
pub enum ChangeQueryResult {
    /// Returned events strictly after `since`, plus the new max
    /// sequence. The caller persists `new_seq` and passes it back on the
    /// next call to resume the delta.
    Delta { events: Vec<Change>, new_seq: u64 },
    /// The requested `since` predates this parent's buffer — caller
    /// should fall back to a fresh enumeration this round, then resume
    /// with the cursor returned by the fresh enumeration.
    Evicted,
    /// This parent has never been observed by the change feed (e.g. the
    /// parent contains no children yet, or the sync runner has not yet
    /// recorded a batch). Caller should fall back to a fresh enumeration.
    Unknown,
}

/// Shared per-backend index. Stored behind an `Arc<RwLock<...>>` so the
/// sync runner's [`ChangeFeed::record`] writes do not contend with reads
/// from [`ChangeFeed::parent_changes_since`] beyond the lock itself.
type FeedState = Arc<RwLock<HashMap<String, BackendChangeState>>>;

/// Engine-side per-parent change index.
///
/// A passive sink: the [`SyncRunner`](crate::sync::runner::SyncRunner)
/// feeds applied changes in via [`ChangeFeed::record`], and presenters read
/// per-parent deltas out via [`ChangeFeed::parent_changes_since`]. The feed
/// holds no background tasks and owns no poll loop of its own.
#[derive(Default)]
pub struct ChangeFeed {
    state: FeedState,
}

impl std::fmt::Debug for ChangeFeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChangeFeed").finish_non_exhaustive()
    }
}

impl ChangeFeed {
    /// Create an empty change feed.
    ///
    /// The feed starts knowing about no backends; every parent query
    /// returns [`ChangeQueryResult::Unknown`] until the sync runner records
    /// its first batch.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// File a batch of applied changes for `backend_id` into the index.
    ///
    /// Called by the sync runner after it has applied a poll batch to the
    /// state database and notified the presenter, with the same
    /// (post-ignore-filter) changes. Sequences are allocated in iteration
    /// order and persist across calls, so the cursor a presenter holds
    /// stays monotonic from one poll cycle to the next.
    pub async fn record(&self, backend_id: &str, changes: &[Change]) {
        if changes.is_empty() {
            // Still register the backend so a parent query can distinguish
            // "backend known, parent empty" from "backend never seen". A
            // backend with no changes yet should not block the delta path
            // once changes do arrive.
            let mut guard = self.state.write().await;
            guard.entry(backend_id.to_string()).or_default();
            return;
        }
        let mut guard = self.state.write().await;
        let entry = guard.entry(backend_id.to_string()).or_default();
        for change in changes {
            entry.file_change(change.clone());
        }
    }

    /// Query the events filed under `parent_id` strictly after `since`.
    ///
    /// `since == None` means "all events currently buffered for this
    /// parent". A successful [`ChangeQueryResult::Delta`] carries the
    /// new max sequence; the caller persists it into its cursor and
    /// passes it back on the next call.
    pub async fn parent_changes_since(
        &self,
        backend_id: &str,
        parent_id: &ItemId,
        since: Option<u64>,
    ) -> ChangeQueryResult {
        let guard = self.state.read().await;
        let Some(backend_state) = guard.get(backend_id) else {
            return ChangeQueryResult::Unknown;
        };
        let Some(buffer) = backend_state.by_parent.get(parent_id) else {
            return ChangeQueryResult::Unknown;
        };

        // Evicted detection: a non-empty `since` older than the oldest
        // retained sequence means the caller has fallen behind the ring
        // buffer's window. Empty buffers do not trigger eviction — the
        // caller simply observed a parent with no recorded events.
        if let (Some(since_seq), Some(oldest)) = (since, buffer.front())
            && since_seq < oldest.seq.saturating_sub(1)
        {
            return ChangeQueryResult::Evicted;
        }

        let events: Vec<Change> = buffer
            .iter()
            .filter(|stamped| since.is_none_or(|s| stamped.seq > s))
            .map(|stamped| stamped.change.clone())
            .collect();

        // The new sequence is the max sequence seen so far for this
        // parent, or the caller's `since` if the buffer is empty. The
        // saturating_sub keeps the cursor monotone when no events have
        // ever been filed for this parent.
        let new_seq = buffer
            .back()
            .map_or_else(|| since.unwrap_or(0), |stamped| stamped.seq);
        ChangeQueryResult::Delta { events, new_seq }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FileEntry, ItemId};
    use chrono::Utc;

    /// Build a file entry under the given parent.
    fn make_entry(backend: &str, native_id: &str, parent_native: &str, name: &str) -> FileEntry {
        FileEntry {
            id: ItemId::new(backend, native_id),
            parent_id: ItemId::new(backend, parent_native),
            path: name.to_string(),
            name: name.to_string(),
            is_dir: false,
            size: Some(1),
            mod_time: Some(Utc::now()),
            mime_type: None,
            hash: None,
        }
    }

    #[tokio::test]
    async fn delta_returns_events_strictly_after_since() {
        let feed = ChangeFeed::new();
        let parent = ItemId::new("scripted", "root");

        feed.record(
            "scripted",
            &[
                Change::Created(make_entry("scripted", "a", "root", "a.txt")),
                Change::Created(make_entry("scripted", "b", "root", "b.txt")),
                Change::Created(make_entry("scripted", "c", "root", "c.txt")),
            ],
        )
        .await;

        // No since -> all three back.
        match feed.parent_changes_since("scripted", &parent, None).await {
            ChangeQueryResult::Delta { events, new_seq } => {
                assert_eq!(events.len(), 3);
                assert_eq!(new_seq, 2);
            }
            other => panic!("expected Delta, got {other:?}"),
        }

        // Since seq=0 -> events with seq>0 (i.e. seq 1 and 2).
        match feed
            .parent_changes_since("scripted", &parent, Some(0))
            .await
        {
            ChangeQueryResult::Delta { events, new_seq } => {
                assert_eq!(events.len(), 2);
                assert_eq!(new_seq, 2);
            }
            other => panic!("expected Delta, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn sequences_persist_across_record_calls() {
        let feed = ChangeFeed::new();
        let parent = ItemId::new("scripted", "root");

        feed.record(
            "scripted",
            &[Change::Created(make_entry(
                "scripted", "a", "root", "a.txt",
            ))],
        )
        .await;
        feed.record(
            "scripted",
            &[Change::Created(make_entry(
                "scripted", "b", "root", "b.txt",
            ))],
        )
        .await;

        // The second batch must continue the sequence (0 then 1), so a
        // caller that saw seq=0 only gets the second event back.
        match feed
            .parent_changes_since("scripted", &parent, Some(0))
            .await
        {
            ChangeQueryResult::Delta { events, new_seq } => {
                assert_eq!(events.len(), 1);
                assert_eq!(new_seq, 1);
            }
            other => panic!("expected Delta, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn evicted_fires_when_buffer_overflows() {
        let feed = ChangeFeed::new();
        let parent = ItemId::new("scripted", "root");

        // Push two more than the cap so the head (seq=0 and seq=1) is
        // evicted; the oldest retained sequence is therefore 2. A
        // caller asking for events strictly after `since=0` would then
        // be missing seq=1 (dropped), which triggers the Evicted path.
        for index in 0..=PER_PARENT_CAPACITY.saturating_add(1) {
            let name = format!("file-{index}.txt");
            feed.record(
                "scripted",
                &[Change::Created(make_entry(
                    "scripted", &name, "root", &name,
                ))],
            )
            .await;
        }

        // A `since` older than the new oldest must report Evicted.
        match feed
            .parent_changes_since("scripted", &parent, Some(0))
            .await
        {
            ChangeQueryResult::Evicted => {}
            other => panic!("expected Evicted, got {other:?}"),
        }

        // A `since` that still has a contiguous timeline with the
        // buffer's head must still get a Delta.
        match feed
            .parent_changes_since("scripted", &parent, Some(2))
            .await
        {
            ChangeQueryResult::Delta { .. } => {}
            other => panic!("expected Delta for in-range since, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_fires_for_never_seen_parent() {
        let feed = ChangeFeed::new();
        feed.record(
            "scripted",
            &[Change::Created(make_entry(
                "scripted", "a", "root", "a.txt",
            ))],
        )
        .await;

        let ghost_parent = ItemId::new("scripted", "ghost");
        match feed
            .parent_changes_since("scripted", &ghost_parent, None)
            .await
        {
            ChangeQueryResult::Unknown => {}
            other => panic!("expected Unknown, got {other:?}"),
        }

        // Backend that the feed has never recorded also returns Unknown.
        match feed
            .parent_changes_since("nonexistent", &ghost_parent, None)
            .await
        {
            ChangeQueryResult::Unknown => {}
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_batch_registers_backend_without_events() {
        let feed = ChangeFeed::new();
        // Recording an empty batch registers the backend but files no
        // events; a known parent therefore still reports Unknown because
        // it has no buffer, but the backend itself is recognised.
        feed.record("scripted", &[]).await;
        let parent = ItemId::new("scripted", "root");
        match feed.parent_changes_since("scripted", &parent, None).await {
            ChangeQueryResult::Unknown => {}
            other => panic!("expected Unknown for parent with no events, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn moved_partitions_into_old_and_new_parents() {
        let feed = ChangeFeed::new();

        let from = make_entry("scripted", "x", "old", "x.txt");
        let to = FileEntry {
            id: from.id.clone(),
            parent_id: ItemId::new("scripted", "new"),
            name: "x.txt".to_string(),
            path: "x.txt".to_string(),
            is_dir: false,
            size: Some(1),
            mod_time: Some(Utc::now()),
            mime_type: None,
            hash: None,
        };
        feed.record("scripted", &[Change::Moved { from, to }]).await;

        let old_parent = ItemId::new("scripted", "old");
        let new_parent = ItemId::new("scripted", "new");

        match feed
            .parent_changes_since("scripted", &old_parent, None)
            .await
        {
            ChangeQueryResult::Delta { events, .. } => match events.as_slice() {
                [Change::Deleted(_)] => {}
                other => panic!("expected exactly one Deleted event, got {other:?}"),
            },
            other => panic!("expected Delta on old parent, got {other:?}"),
        }
        match feed
            .parent_changes_since("scripted", &new_parent, None)
            .await
        {
            ChangeQueryResult::Delta { events, .. } => match events.as_slice() {
                [Change::Created(_)] => {}
                other => panic!("expected exactly one Created event, got {other:?}"),
            },
            other => panic!("expected Delta on new parent, got {other:?}"),
        }
    }
}
