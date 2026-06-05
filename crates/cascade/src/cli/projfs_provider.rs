//! Engine-backed [`ContentProvider`](cascade_presenter_projfs::ContentProvider)
//! for the Windows `ProjFS` presenter.
//!
//! `ProjFS`'s `GetFileData` callback runs on a kernel-owned worker thread
//! that is **not** part of any Tokio runtime, and the callback is
//! synchronous. This provider bridges that synchronous, runtime-free
//! context to the engine's async
//! [`Backend::read_range`](cascade_engine::backend::Backend::read_range) API.
//!
//! # Why this lives in the binary crate
//!
//! The provider needs the engine's backend list and the same
//! backend-resolution logic that `cli/mount.rs` already owns. Keeping it
//! here avoids leaking backend-resolution dependencies into the presenter
//! crate, which only knows the
//! [`ContentProvider`](cascade_presenter_projfs::ContentProvider) trait and
//! [`ItemId`](cascade_engine::types::ItemId).
//!
//! # Read strategy: bounded ranged fetch
//!
//! Each `GetFileData` range request resolves the file's metadata
//! synchronously through the state database, then asks the owning backend
//! for exactly the requested slice via
//! [`Backend::read_range`](cascade_engine::backend::Backend::read_range).
//! Only the bytes in `[offset, offset + length)` are fetched; the file is
//! never materialised in full, so a first touch of a large file no longer
//! downloads or pins the whole thing. Backends with a native range API
//! (HTTP `Range`, `seek` + `read`, a block store) serve the slice directly;
//! those without one fall back to the trait's download-and-slice default,
//! but that fallback is contained inside the backend rather than caching a
//! whole-file copy here.
//!
//! The async read runs via [`tokio::runtime::Handle::block_on`] on the
//! captured daemon runtime handle. `block_on` is valid here precisely
//! because the callback thread is outside any runtime — the same
//! precondition that lets the presenter's other callbacks call
//! `RwLock::blocking_read`. To stay panic-proof even if a future caller
//! invokes this from inside a runtime worker, the bridge checks
//! [`tokio::runtime::Handle::try_current`] and falls back to
//! [`tokio::task::block_in_place`] in that case, so it never hits the
//! "Cannot start a runtime from within a runtime" panic.
//!
//! # Known limitations (deliberate for v8)
//!
//! - A range read is not cancellable mid-flight: the presenter's
//!   `CancellationToken` is observed at checkpoints around `read_range`,
//!   not inside the backend fetch. Because each read is now a bounded
//!   slice rather than a whole-file transfer, the worst-case blocking
//!   duration is proportional to the requested range, not the file size —
//!   so a `GetFileData` for a large cold file no longer pins its callback
//!   thread (and delays shutdown) for the full download.

use std::io::{self, ErrorKind};
use std::sync::Arc;
use std::sync::RwLock;

use cascade_engine::backend::Backend;
use cascade_engine::db::StateDb;
use cascade_engine::types::ItemId;
use cascade_engine::vfs::VfsTree;
use cascade_presenter_projfs::ContentProvider;

/// Source of file bytes for the `ProjFS` `GetFileData` callback, backed
/// by the engine's state database and backend list.
///
/// All fields are cheap to clone (`Arc` bumps), so the provider can be
/// wrapped in an `Arc<dyn ContentProvider>` and shared with the
/// presenter's callback context.
///
/// `Debug` is implemented by hand because `dyn Backend` does not derive
/// `Debug`; the [`ContentProvider`] trait requires the bound, so the
/// impl summarises the backend list by count rather than printing it.
pub struct EngineContentProvider {
    /// Shared VFS tree. Retained for parity with the other engine-backed
    /// presenters and so future tree-based resolution has it to hand; the
    /// synchronous read path resolves metadata through `db` rather than
    /// the tree to avoid taking the VFS lock on a kernel callback thread.
    #[allow(dead_code)]
    vfs: Arc<RwLock<VfsTree>>,
    /// State database. `get_file` is synchronous `rusqlite`, so it is
    /// safe to call directly from the callback thread with no runtime.
    db: Arc<StateDb>,
    /// The backend list, used to resolve `id.backend_id()` back to the
    /// owning backend for the ranged read.
    backends: Vec<Arc<dyn Backend>>,
    /// Handle to the daemon's multi-thread Tokio runtime, captured while
    /// still on the runtime. Used to `block_on` the async ranged read.
    handle: tokio::runtime::Handle,
}

impl std::fmt::Debug for EngineContentProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineContentProvider")
            .field("backend_count", &self.backends.len())
            .finish_non_exhaustive()
    }
}

impl EngineContentProvider {
    /// Build a new provider.
    #[must_use]
    pub const fn new(
        vfs: Arc<RwLock<VfsTree>>,
        db: Arc<StateDb>,
        backends: Vec<Arc<dyn Backend>>,
        handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            vfs,
            db,
            backends,
            handle,
        }
    }

    /// Resolve the backend that owns `id`, by matching `id.backend_id()`
    /// against each registered backend's identifier.
    fn backend_for(&self, id: &ItemId) -> Option<&Arc<dyn Backend>> {
        let backend_id = id.backend_id();
        self.backends.iter().find(|b| b.id() == backend_id)
    }
}

/// Map an [`anyhow::Error`] surfaced by the engine into an
/// [`io::Error`] so the `ProjFS` callback can translate it into a Win32
/// `HRESULT`. The kind is `Other`, which the presenter maps to
/// `ERROR_GEN_FAILURE` — a deliberate, visible failure rather than a
/// silent empty read.
fn to_io(err: anyhow::Error) -> io::Error {
    io::Error::other(format!("{err:#}"))
}

impl ContentProvider for EngineContentProvider {
    fn read_range(&self, id: &ItemId, offset: u64, length: u32) -> io::Result<Vec<u8>> {
        // 1. Resolve metadata synchronously. `get_file` is plain rusqlite,
        //    so no runtime is needed. Directories never reach here — the
        //    GetFileData callback returns ERROR_FILE_NOT_FOUND for them
        //    before invoking the provider.
        let entry = self
            .db
            .get_file(id)
            .map_err(to_io)?
            .ok_or_else(|| io::Error::from(ErrorKind::NotFound))?;

        // 2. Resolve the owning backend.
        let backend = self.backend_for(id).ok_or_else(|| {
            io::Error::new(
                ErrorKind::NotFound,
                format!("no backend registered for {}", id.backend_id()),
            )
        })?;
        let backend = Arc::clone(backend);

        // 3. Fetch exactly the requested range. `Backend::read_range`
        //    honours the same short-read-at-EOF contract the presenter
        //    expects: a `Vec` shorter than `length` near EOF, and an empty
        //    `Vec` when `offset` is at or past the end (which the presenter
        //    maps to S_OK). The whole file is never materialised.
        //
        //    The ProjFS GetFileData callback runs on a kernel-owned worker
        //    thread outside any Tokio runtime, so a bare `Handle::block_on`
        //    is the right bridge there. To be panic-proof in any caller
        //    (tests, or a future caller already on a runtime worker), detect
        //    that case with `Handle::try_current` and use `block_in_place`
        //    so we never hit the "Cannot start a runtime from within a
        //    runtime" panic path. On the real callback thread `try_current`
        //    returns `Err`, so we take the plain `block_on` branch.
        let read = async move { backend.read_range(&entry, offset, length).await };
        let bytes: anyhow::Result<Vec<u8>> = if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::block_in_place(|| self.handle.block_on(read))
        } else {
            self.handle.block_on(read)
        };

        bytes.map_err(to_io)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::Duration;

    use async_trait::async_trait;
    use cascade_engine::types::{Change, Cursor, FileEntry, FileId, ItemId, Quota};

    use super::*;

    /// Backend that records the `(offset, length)` of every `read_range`
    /// call and returns a fixed slice of its in-memory content, proving the
    /// provider asks for a bounded range and never falls back to a
    /// whole-file `download`. `download` panics so any accidental
    /// whole-file materialisation surfaces as a test failure.
    #[derive(Debug)]
    struct RecordingBackend {
        id: String,
        content: Vec<u8>,
        calls: Mutex<Vec<(u64, u32)>>,
    }

    impl RecordingBackend {
        fn new(id: &str, content: &[u8]) -> Self {
            Self {
                id: id.to_string(),
                content: content.to_vec(),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl Backend for RecordingBackend {
        fn id(&self) -> &str {
            &self.id
        }
        fn display_name(&self) -> &'static str {
            "Recording"
        }
        async fn quota(&self) -> anyhow::Result<Option<Quota>> {
            Ok(None)
        }
        async fn changes(&self, _cursor: Option<&Cursor>) -> anyhow::Result<(Vec<Change>, Cursor)> {
            Ok((vec![], Cursor("recording".to_string())))
        }
        async fn metadata(&self, _path: &Path) -> anyhow::Result<FileEntry> {
            anyhow::bail!("unused")
        }
        async fn download(&self, _file: &FileEntry) -> anyhow::Result<Vec<u8>> {
            panic!("download must not be called: read_range should fetch a bounded slice");
        }
        async fn read_range(
            &self,
            _file: &FileEntry,
            offset: u64,
            length: u32,
        ) -> anyhow::Result<Vec<u8>> {
            self.calls.lock().map_or_else(
                |_| panic!("recording mutex poisoned"),
                |mut calls| calls.push((offset, length)),
            );
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(self.content.len());
            let len = usize::try_from(length).unwrap_or(usize::MAX);
            let end = start.saturating_add(len).min(self.content.len());
            Ok(self.content.get(start..end).unwrap_or_default().to_vec())
        }
        async fn upload(
            &self,
            _path: &Path,
            _data: &[u8],
            _parent_id: &FileId,
        ) -> anyhow::Result<FileEntry> {
            anyhow::bail!("unused")
        }
        async fn update(&self, _file_id: &FileId, _data: &[u8]) -> anyhow::Result<FileEntry> {
            anyhow::bail!("unused")
        }
        async fn create_dir(&self, _path: &Path) -> anyhow::Result<FileEntry> {
            anyhow::bail!("unused")
        }
        async fn delete(&self, _file: &FileEntry) -> anyhow::Result<()> {
            anyhow::bail!("unused")
        }
        async fn move_entry(&self, _src: &Path, _dst: &Path) -> anyhow::Result<FileEntry> {
            anyhow::bail!("unused")
        }
        async fn poll_interval(&self) -> Option<Duration> {
            None
        }
    }

    /// Build a provider whose state DB knows about one file owned by the
    /// given backend, plus a handle to the current multi-thread runtime.
    fn provider_with(backend: Arc<RecordingBackend>, entry: &FileEntry) -> EngineContentProvider {
        let db = Arc::new(StateDb::open_in_memory().expect("open in-memory state db"));
        db.register_backend(backend.id(), "recording", "Recording", None, None)
            .expect("register backend row");
        db.upsert_file(entry).expect("seed file row");

        let root: Arc<dyn Backend> = backend.clone();
        let vfs = Arc::new(RwLock::new(VfsTree::new(root)));
        let backends: Vec<Arc<dyn Backend>> = vec![backend];

        EngineContentProvider::new(vfs, db, backends, tokio::runtime::Handle::current())
    }

    fn file_entry(backend_id: &str) -> FileEntry {
        FileEntry {
            id: ItemId::new(backend_id, "f"),
            parent_id: ItemId::new(backend_id, "root"),
            name: "f.bin".to_string(),
            is_dir: false,
            size: Some(11),
            mod_time: None,
            mime_type: None,
            hash: None,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn read_range_delegates_to_backend_read_range() {
        let backend = Arc::new(RecordingBackend::new("rec", b"hello world"));
        let entry = file_entry("rec");
        let provider = provider_with(backend.clone(), &entry);

        let bytes = provider.read_range(&entry.id, 6, 5).expect("read range");
        assert_eq!(bytes, b"world");

        let calls = backend.calls.lock().expect("calls lock");
        assert_eq!(
            *calls,
            vec![(6, 5)],
            "provider must pass the exact range through"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn read_range_clamps_length_past_eof() {
        let backend = Arc::new(RecordingBackend::new("rec", b"hello world"));
        let entry = file_entry("rec");
        let provider = provider_with(backend, &entry);

        // Length runs past EOF: the backend clamps to what's available.
        let bytes = provider.read_range(&entry.id, 6, 999).expect("read range");
        assert_eq!(bytes, b"world");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn read_range_empty_at_or_past_eof() {
        let backend = Arc::new(RecordingBackend::new("rec", b"hello world"));
        let entry = file_entry("rec");
        let provider = provider_with(backend, &entry);

        // Offset exactly at EOF and zero-length both yield empty buffers.
        assert!(
            provider
                .read_range(&entry.id, 11, 10)
                .expect("at eof")
                .is_empty()
        );
        assert!(
            provider
                .read_range(&entry.id, 0, 0)
                .expect("zero length")
                .is_empty()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn read_range_missing_file_is_not_found() {
        let backend = Arc::new(RecordingBackend::new("rec", b"hello world"));
        let entry = file_entry("rec");
        let provider = provider_with(backend, &entry);

        let unknown = ItemId::new("rec", "absent");
        let err = provider
            .read_range(&unknown, 0, 4)
            .expect_err("missing file errors");
        assert_eq!(err.kind(), ErrorKind::NotFound);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn read_range_unknown_backend_is_not_found() {
        let backend = Arc::new(RecordingBackend::new("rec", b"hello world"));
        // Seed a file whose backend id has no registered backend.
        let entry = file_entry("missing-backend");
        let db = Arc::new(StateDb::open_in_memory().expect("open in-memory state db"));
        db.register_backend("missing-backend", "recording", "Missing", None, None)
            .expect("register backend row");
        db.upsert_file(&entry).expect("seed file row");
        let root: Arc<dyn Backend> = backend.clone();
        let vfs = Arc::new(RwLock::new(VfsTree::new(root)));
        let backends: Vec<Arc<dyn Backend>> = vec![backend];
        let provider =
            EngineContentProvider::new(vfs, db, backends, tokio::runtime::Handle::current());

        let err = provider
            .read_range(&entry.id, 0, 4)
            .expect_err("unknown backend errors");
        assert_eq!(err.kind(), ErrorKind::NotFound);
    }
}
