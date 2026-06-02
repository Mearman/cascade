//! Engine-backed [`ContentProvider`] for the Windows `ProjFS` presenter.
//!
//! `ProjFS`'s `GetFileData` callback runs on a kernel-owned worker thread
//! that is **not** part of any Tokio runtime, and the callback is
//! synchronous. This provider bridges that synchronous, runtime-free
//! context to the engine's async [`Backend::download`] API.
//!
//! # Why this lives in the binary crate
//!
//! The provider needs the engine's backend list and the same
//! backend-resolution logic that `cli/mount.rs` already owns. Keeping it
//! here avoids leaking backend-resolution dependencies into the presenter
//! crate, which only knows the [`ContentProvider`] trait and [`ItemId`].
//!
//! # Read strategy: materialise once, then seek
//!
//! [`Backend::download`] streams an entire file; it has no range API. So
//! the first time any byte of a file is requested, the provider downloads
//! the whole file to a versioned cache path and atomically renames it into
//! place. Every subsequent range read — including every other range of the
//! same file — is served by a plain synchronous `std::fs` seek + read of
//! the materialised cache file, touching no runtime at all.
//!
//! The async download on the cold path runs via
//! [`tokio::runtime::Handle::block_on`] on the captured daemon runtime
//! handle. `block_on` is valid here precisely because the callback thread
//! is outside any runtime — the same precondition that lets the presenter's
//! other callbacks call `RwLock::blocking_read`. It would panic only if
//! entered from inside an active runtime worker, which a `ProjFS` callback
//! thread never is.
//!
//! # Known limitations (deliberate for v8)
//!
//! - The cold download is not cancellable mid-stream: the presenter's
//!   `CancellationToken` is observed at checkpoints around `read_range`,
//!   not inside the download. Warm range reads are cheap.
//! - When `mod_time` is `None`, the cache version is `"unknown"`, which
//!   inherits the same staleness edge as the File Provider cache scheme.
//!   A content-hash version is the proper fix but is a cross-presenter
//!   change out of scope here.
//! - True range-aware fetching needs a `Backend` trait extension (HTTP
//!   Range); materialise-then-seek is the correct interim.

use std::io::{self, ErrorKind, Read as _, Seek as _, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;

use cascade_engine::backend::Backend;
use cascade_engine::db::StateDb;
use cascade_engine::types::ItemId;
use cascade_engine::vfs::VfsTree;
use cascade_presenter_projfs::ContentProvider;
use tokio::io::AsyncWriteExt as _;

/// Source of file bytes for the `ProjFS` `GetFileData` callback, backed
/// by the engine's state database and backend list.
///
/// All fields are cheap to clone (`Arc` bumps and a `PathBuf`), so the
/// provider can be wrapped in an `Arc<dyn ContentProvider>` and shared
/// with the presenter's callback context.
///
/// `Debug` is implemented by hand because `dyn Backend` does not derive
/// `Debug`; the [`ContentProvider`] trait requires the bound, so the
/// impl summarises the backend list by count rather than printing it.
pub struct EngineContentProvider {
    /// Shared VFS tree. Retained for parity with the other engine-backed
    /// presenters and so future range-aware resolution has the tree to
    /// hand; the synchronous read path resolves metadata through `db`
    /// rather than the tree to avoid taking the VFS lock on a kernel
    /// callback thread.
    #[allow(dead_code)]
    vfs: Arc<RwLock<VfsTree>>,
    /// State database. `get_file` is synchronous `rusqlite`, so it is
    /// safe to call directly from the callback thread with no runtime.
    db: Arc<StateDb>,
    /// The backend list, used to resolve `id.backend_id()` back to the
    /// owning backend for the cold-path download.
    backends: Vec<Arc<dyn Backend>>,
    /// Root cache directory (a sibling of the state DB, matching the File
    /// Provider layout). Materialised files live under `<cache_dir>/projfs`.
    cache_dir: PathBuf,
    /// Handle to the daemon's multi-thread Tokio runtime, captured while
    /// still on the runtime. Used to `block_on` the async download on the
    /// cold path.
    handle: tokio::runtime::Handle,
}

impl std::fmt::Debug for EngineContentProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineContentProvider")
            .field("backend_count", &self.backends.len())
            .field("cache_dir", &self.cache_dir)
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
        cache_dir: PathBuf,
        handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            vfs,
            db,
            backends,
            cache_dir,
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

/// Sanitise an [`ItemId`] string into a single filesystem-safe path
/// component. Reproduced locally rather than depending on a presenter's
/// private helper, so the binary crate does not take a cross-presenter
/// dependency for one pure string transform. Matches the FUSE/WebDAV
/// scheme (`:` `/` `\` collapse to `_`).
fn safe_filename(id: &str) -> String {
    id.replace([':', '/', '\\'], "_")
}

/// Adapter from `tokio::fs::File` (`AsyncWrite`) to the
/// `dyn AsyncWrite + Unpin + Send` that [`Backend::download`] expects.
/// Mirrors the adapter the FUSE and NFS presenters use.
struct WriterAdapter {
    inner: tokio::fs::File,
}

impl tokio::io::AsyncWrite for WriterAdapter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

impl Unpin for WriterAdapter {}

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

        // 2. Compute the versioned cache path, mirroring the File Provider
        //    scheme: <cache_dir>/projfs/<safe(id)>/<version>. The version
        //    is the modification timestamp, or "unknown" when absent (a
        //    deliberate parity edge, documented at the module level).
        let version = entry
            .mod_time
            .map_or_else(|| "unknown".to_string(), |t| t.timestamp().to_string());
        let item_dir = self.cache_dir.join("projfs").join(safe_filename(&id.0));
        let cache_path = item_dir.join(&version);

        // 3. Materialise once if the cache file is absent. Racing callbacks
        //    each download to a unique temp name and the final rename is
        //    last-writer-wins onto identical content — correct, just mildly
        //    wasteful on a cold race.
        if !cache_path.exists() {
            let backend = self.backend_for(id).ok_or_else(|| {
                io::Error::new(
                    ErrorKind::NotFound,
                    format!("no backend registered for {}", id.backend_id()),
                )
            })?;

            std::fs::create_dir_all(&item_dir)?;

            // Unique temp sibling: thread id + nanos avoids collisions when
            // two callback threads race the same cold file.
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let unique = format!("{:?}-{nanos}", std::thread::current().id());
            let tmp_path = item_dir.join(format!("{version}.{unique}.tmp"));

            let backend = Arc::clone(backend);
            let tmp_for_download = tmp_path.clone();
            // The cold-path download is the only place this provider enters
            // the runtime. `block_on` is safe because this callback runs on
            // a ProjFS kernel thread, never inside a runtime worker.
            let download_result: anyhow::Result<()> = self.handle.block_on(async move {
                let file = tokio::fs::File::create(&tmp_for_download).await?;
                let mut writer = WriterAdapter { inner: file };
                backend.download(&entry, &mut writer).await?;
                writer.inner.flush().await?;
                writer.inner.shutdown().await?;
                Ok(())
            });

            if let Err(err) = download_result {
                // Best-effort cleanup of the partial temp file; ignore the
                // result because the original download error is what matters.
                let _ = std::fs::remove_file(&tmp_path);
                return Err(to_io(err));
            }

            std::fs::rename(&tmp_path, &cache_path)?;
        }

        // 4. Serve the requested range synchronously from the materialised
        //    file. A short read (or empty Vec at/after EOF) satisfies the
        //    trait's short-read-at-EOF contract; the presenter maps an
        //    empty Vec to S_OK.
        let mut file = std::fs::File::open(&cache_path)?;
        file.seek(SeekFrom::Start(offset))?;

        let want = usize::try_from(length).unwrap_or(usize::MAX);
        let mut buf = vec![0u8; want];
        let mut filled = 0usize;
        while filled < want {
            let Some(slice) = buf.get_mut(filled..) else {
                break;
            };
            let n = file.read(slice)?;
            if n == 0 {
                break; // EOF
            }
            filled += n;
        }
        buf.truncate(filled);
        Ok(buf)
    }
}
