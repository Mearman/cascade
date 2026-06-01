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
//! ## Polling cadence
//!
//! Each registered backend is polled every [`POLL_INTERVAL`] using
//! [`tokio::time::interval`], which coalesces missed ticks rather than
//! drifting under load. A small deterministic offset derived from the backend
//! ID hash (see the `BackendChangeState::initial_offset` private helper)
//! spreads concurrent
//! polls so a daemon with many backends does not stampede the network on
//! startup. The first tick fires immediately so a freshly-opened presenter
//! receives data within one poll round-trip.
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
//!   parent. Either the parent contains no children yet, or the change
//!   stream has not run its first poll cycle. Same fallback as
//!   [`ChangeQueryResult::Evicted`].

use std::collections::{HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;

use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};

use crate::backend::Backend;
use crate::types::{Change, Cursor, FileEntry, ItemId};

/// How often each backend is polled for new changes.
///
/// Picked to match the existing daemon refresh cadence for the read-only
/// VFS — 30s is short enough that Finder feels responsive when files
/// appear in another client, and long enough that we do not burn a token
/// bucket call per backend per minute.
pub const POLL_INTERVAL: Duration = Duration::from_secs(30);

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
/// One instance per registered backend. The cursor anchors the next
/// `Backend::changes` call; `next_seq` allocates the monotonic stamp; and
/// `by_parent` partitions the events into ring buffers keyed by the
/// owning directory's [`ItemId`].
#[derive(Debug, Default)]
struct BackendChangeState {
    /// Last seen cursor from `Backend::changes`. `None` until the first
    /// poll cycle completes.
    cursor: Option<Cursor>,
    /// Next sequence number to allocate for this backend.
    next_seq: u64,
    /// Per-parent ring buffers, keyed by the parent's [`ItemId`].
    by_parent: HashMap<ItemId, VecDeque<StampedChange>>,
}

impl BackendChangeState {
    /// Deterministic startup offset for this backend's polling task.
    ///
    /// Derived from the SHA-cheap [`DefaultHasher`] applied to the
    /// backend ID and rounded to a fraction of [`POLL_INTERVAL`]. Using
    /// a hash rather than randomness keeps the offset stable across
    /// restarts, which makes test runs deterministic and avoids needing
    /// a `rand` dependency in the engine.
    fn initial_offset(backend_id: &str) -> Duration {
        let mut hasher = DefaultHasher::new();
        backend_id.hash(&mut hasher);
        // Map the hash into [0, POLL_INTERVAL/4) so concurrent polls
        // never stampede but the first poll still completes well inside
        // one interval.
        let span_millis = POLL_INTERVAL.as_millis().saturating_div(4);
        // span_millis fits in u64 because POLL_INTERVAL is small; the
        // saturating cast keeps clippy happy without an `as` lint.
        let span_u64 = u64::try_from(span_millis).unwrap_or(u64::MAX);
        let offset_millis = if span_u64 == 0 {
            0
        } else {
            hasher.finish().rem_euclid(span_u64)
        };
        Duration::from_millis(offset_millis)
    }

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
    /// parent contains no children yet, or the change stream has not
    /// run a poll cycle yet). Caller should fall back to a fresh
    /// enumeration.
    Unknown,
}

/// Shared state owned by both the [`ChangeFeed`] handle and its polling
/// tasks. Stored behind an `Arc<RwLock<...>>` so the polling task can
/// update without contention with reads from `parent_changes_since`.
type FeedState = Arc<RwLock<HashMap<String, BackendChangeState>>>;

/// Engine-side per-parent change index.
///
/// Spawns one polling task per backend; each task drives the backend's
/// [`Backend::changes`] global stream into a shared per-parent index
/// available via [`ChangeFeed::parent_changes_since`]. Dropping the
/// feed aborts every polling task.
pub struct ChangeFeed {
    state: FeedState,
    tasks: Vec<JoinHandle<()>>,
}

impl std::fmt::Debug for ChangeFeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChangeFeed")
            .field("task_count", &self.tasks.len())
            .finish_non_exhaustive()
    }
}

impl ChangeFeed {
    /// Start a feed over the given backends, spawning one polling task
    /// per backend. The tasks run for as long as the returned handle is
    /// alive; dropping the handle aborts them.
    #[must_use]
    pub fn start(backends: Vec<Arc<dyn Backend>>) -> Self {
        let state: FeedState = Arc::new(RwLock::new(HashMap::new()));
        let mut tasks = Vec::with_capacity(backends.len());
        for backend in backends {
            let id = backend.id().to_string();
            // Insert an empty state for this backend so reads before
            // the first poll cycle return Unknown rather than failing.
            {
                let state_for_init = state.clone();
                let id_for_init = id.clone();
                tasks.push(tokio::spawn(async move {
                    let mut guard = state_for_init.write().await;
                    guard.entry(id_for_init).or_default();
                }));
            }
            let task_state = Arc::downgrade(&state);
            let handle = tokio::spawn(poll_backend_loop(backend, id, task_state));
            tasks.push(handle);
        }
        Self { state, tasks }
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

    /// Abort every polling task and await their completion.
    ///
    /// Tests use this to deterministically drain pending work before
    /// asserting on feed state. Production callers can simply drop the
    /// feed.
    pub async fn shutdown(&mut self) {
        for handle in self.tasks.drain(..) {
            handle.abort();
            // Awaiting a JoinHandle after abort returns a JoinError, but
            // we only care that the task is no longer running.
            let _ = handle.await;
        }
    }
}

impl Drop for ChangeFeed {
    fn drop(&mut self) {
        for handle in &self.tasks {
            handle.abort();
        }
    }
}

/// Per-backend polling loop. Runs until the weak reference to the shared
/// state can no longer be upgraded (i.e. the owning [`ChangeFeed`] was
/// dropped).
async fn poll_backend_loop(
    backend: Arc<dyn Backend>,
    backend_id: String,
    state: Weak<RwLock<HashMap<String, BackendChangeState>>>,
) {
    let offset = BackendChangeState::initial_offset(&backend_id);
    let mut interval = tokio::time::interval_at(Instant::now() + offset, POLL_INTERVAL);
    // Coalesce missed ticks so a slow poll cycle does not produce a
    // burst of catch-up calls on the next backend round-trip.
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        interval.tick().await;
        let Some(state) = state.upgrade() else {
            // ChangeFeed handle was dropped — exit the polling loop so
            // the spawned task does not outlive its owner.
            return;
        };

        // Snapshot the current cursor, then drop the read lock before
        // the network round-trip so concurrent `parent_changes_since`
        // calls can still run while we wait for the backend.
        let cursor_before = {
            let guard = state.read().await;
            guard.get(&backend_id).and_then(|s| s.cursor.clone())
        };

        let result = backend.changes(cursor_before.as_ref()).await;
        match result {
            Ok((changes, new_cursor)) => {
                let mut guard = state.write().await;
                let entry = guard.entry(backend_id.clone()).or_default();
                for change in changes {
                    entry.file_change(change);
                }
                entry.cursor = Some(new_cursor);
            }
            Err(err) => {
                tracing::warn!(
                    backend_id = %backend_id,
                    error = %err,
                    "change feed: backend changes() poll failed; will retry on next tick",
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Cursor, FileEntry, FileId, ItemId, Quota};
    use async_trait::async_trait;
    use chrono::Utc;
    use std::path::Path;
    use std::sync::Mutex;
    use tokio::sync::Notify;

    /// Backend that hands out scripted change batches in order. Each
    /// `changes` call pops one batch off the script.
    #[derive(Debug)]
    struct ScriptedBackend {
        id: String,
        script: Mutex<Vec<Vec<Change>>>,
        notify: Arc<Notify>,
    }

    impl ScriptedBackend {
        fn new(id: &str, script: Vec<Vec<Change>>) -> (Arc<Self>, Arc<Notify>) {
            let notify = Arc::new(Notify::new());
            let backend = Arc::new(Self {
                id: id.to_string(),
                script: Mutex::new(script),
                notify: notify.clone(),
            });
            (backend, notify)
        }
    }

    #[async_trait]
    impl Backend for ScriptedBackend {
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
            let batch = {
                let mut script = self
                    .script
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if script.is_empty() {
                    Vec::new()
                } else {
                    script.remove(0)
                }
            };
            self.notify.notify_one();
            Ok((batch, Cursor("scripted".to_string())))
        }

        async fn metadata(&self, _path: &Path) -> anyhow::Result<FileEntry> {
            anyhow::bail!("not used in change feed tests")
        }

        async fn download(
            &self,
            _file: &FileEntry,
            _writer: &mut (dyn tokio::io::AsyncWrite + Unpin + Send),
        ) -> anyhow::Result<()> {
            anyhow::bail!("not used in change feed tests")
        }

        async fn upload(
            &self,
            _path: &Path,
            _reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
            _parent_id: &FileId,
        ) -> anyhow::Result<FileEntry> {
            anyhow::bail!("not used in change feed tests")
        }

        async fn update(
            &self,
            _file_id: &FileId,
            _reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
        ) -> anyhow::Result<FileEntry> {
            anyhow::bail!("not used in change feed tests")
        }

        async fn create_dir(&self, _path: &Path) -> anyhow::Result<FileEntry> {
            anyhow::bail!("not used in change feed tests")
        }

        async fn delete(&self, _file: &FileEntry) -> anyhow::Result<()> {
            anyhow::bail!("not used in change feed tests")
        }

        async fn move_entry(&self, _src: &Path, _dst: &Path) -> anyhow::Result<FileEntry> {
            anyhow::bail!("not used in change feed tests")
        }

        async fn poll_interval(&self) -> Option<Duration> {
            None
        }
    }

    /// Build a file entry under the given parent.
    fn make_entry(backend: &str, native_id: &str, parent_native: &str, name: &str) -> FileEntry {
        FileEntry {
            id: ItemId::new(backend, native_id),
            parent_id: ItemId::new(backend, parent_native),
            name: name.to_string(),
            is_dir: false,
            size: Some(1),
            mod_time: Some(Utc::now()),
            mime_type: None,
            hash: None,
        }
    }

    /// Drive the change feed forward by directly calling `file_change`
    /// on the per-backend state. Avoids the polling timer.
    async fn file_directly(feed: &ChangeFeed, backend_id: &str, change: Change) {
        let mut guard = feed.state.write().await;
        let entry = guard.entry(backend_id.to_string()).or_default();
        entry.file_change(change);
    }

    #[tokio::test]
    async fn delta_returns_events_strictly_after_since() {
        let (backend, _notify) = ScriptedBackend::new("scripted", vec![]);
        let mut feed = ChangeFeed::start(vec![backend.clone()]);
        let parent = ItemId::new("scripted", "root");

        file_directly(
            &feed,
            "scripted",
            Change::Created(make_entry("scripted", "a", "root", "a.txt")),
        )
        .await;
        file_directly(
            &feed,
            "scripted",
            Change::Created(make_entry("scripted", "b", "root", "b.txt")),
        )
        .await;
        file_directly(
            &feed,
            "scripted",
            Change::Created(make_entry("scripted", "c", "root", "c.txt")),
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

        feed.shutdown().await;
    }

    #[tokio::test]
    async fn evicted_fires_when_buffer_overflows() {
        let (backend, _notify) = ScriptedBackend::new("scripted", vec![]);
        let mut feed = ChangeFeed::start(vec![backend.clone()]);
        let parent = ItemId::new("scripted", "root");

        // Push two more than the cap so the head (seq=0 and seq=1) is
        // evicted; the oldest retained sequence is therefore 2. A
        // caller asking for events strictly after `since=0` would then
        // be missing seq=1 (dropped), which triggers the Evicted path.
        for index in 0..=PER_PARENT_CAPACITY.saturating_add(1) {
            let name = format!("file-{index}.txt");
            file_directly(
                &feed,
                "scripted",
                Change::Created(make_entry("scripted", &name, "root", &name)),
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

        feed.shutdown().await;
    }

    #[tokio::test]
    async fn unknown_fires_for_never_seen_parent() {
        let (backend, _notify) = ScriptedBackend::new("scripted", vec![]);
        let mut feed = ChangeFeed::start(vec![backend.clone()]);
        // Give the spawned init task a chance to populate the state map.
        tokio::task::yield_now().await;

        let ghost_parent = ItemId::new("scripted", "ghost");
        match feed
            .parent_changes_since("scripted", &ghost_parent, None)
            .await
        {
            ChangeQueryResult::Unknown => {}
            other => panic!("expected Unknown, got {other:?}"),
        }

        // Backend that the feed has not even heard about also returns Unknown.
        match feed
            .parent_changes_since("nonexistent", &ghost_parent, None)
            .await
        {
            ChangeQueryResult::Unknown => {}
            other => panic!("expected Unknown, got {other:?}"),
        }

        feed.shutdown().await;
    }

    #[tokio::test]
    async fn moved_partitions_into_old_and_new_parents() {
        let (backend, _notify) = ScriptedBackend::new("scripted", vec![]);
        let mut feed = ChangeFeed::start(vec![backend.clone()]);

        let from = make_entry("scripted", "x", "old", "x.txt");
        let to = FileEntry {
            id: from.id.clone(),
            parent_id: ItemId::new("scripted", "new"),
            name: "x.txt".to_string(),
            is_dir: false,
            size: Some(1),
            mod_time: Some(Utc::now()),
            mime_type: None,
            hash: None,
        };
        file_directly(&feed, "scripted", Change::Moved { from, to }).await;

        let old_parent = ItemId::new("scripted", "old");
        let new_parent = ItemId::new("scripted", "new");

        match feed
            .parent_changes_since("scripted", &old_parent, None)
            .await
        {
            ChangeQueryResult::Delta { events, .. } => {
                assert_eq!(events.len(), 1);
                assert!(matches!(events[0], Change::Deleted(_)));
            }
            other => panic!("expected Delta on old parent, got {other:?}"),
        }
        match feed
            .parent_changes_since("scripted", &new_parent, None)
            .await
        {
            ChangeQueryResult::Delta { events, .. } => {
                assert_eq!(events.len(), 1);
                assert!(matches!(events[0], Change::Created(_)));
            }
            other => panic!("expected Delta on new parent, got {other:?}"),
        }

        feed.shutdown().await;
    }

    #[tokio::test]
    async fn polling_loop_drains_backend_into_feed() {
        // A single batch is scripted; the poll task should pick it up
        // within one POLL_INTERVAL.
        let entry = make_entry("scripted", "f", "root", "f.txt");
        let (backend, notify) =
            ScriptedBackend::new("scripted", vec![vec![Change::Created(entry.clone())]]);
        let mut feed = ChangeFeed::start(vec![backend.clone()]);

        // Wait for the first poll to complete.
        tokio::time::timeout(Duration::from_secs(2), notify.notified())
            .await
            .expect("backend should be polled within the test timeout");
        // Give the feed a moment to file the event after the notify.
        tokio::task::yield_now().await;
        // Drain any remaining file work; the polling task writes after
        // the notify, so yield until the buffer is populated or we time
        // out the test.
        for _ in 0..32_u8 {
            let guard = feed.state.read().await;
            let observed = guard
                .get("scripted")
                .and_then(|s| s.by_parent.get(&ItemId::new("scripted", "root")))
                .map_or(0, VecDeque::len);
            drop(guard);
            if observed > 0 {
                break;
            }
            tokio::task::yield_now().await;
        }

        let parent = ItemId::new("scripted", "root");
        match feed.parent_changes_since("scripted", &parent, None).await {
            ChangeQueryResult::Delta { events, .. } => {
                assert_eq!(events.len(), 1);
            }
            other => panic!("expected Delta after poll, got {other:?}"),
        }

        feed.shutdown().await;
    }
}
