#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::string_slice
    )
)]
//! `ProjFS` presenter — exposes the VFS tree as a native Windows
//! Projected File System mount.
//!
//! Implements the engine's [`VfsPresenter`] trait. On Windows, the mount
//! is served via the [Projected File System][projfs] API exposed through
//! the `windows` crate's `Win32_Storage_ProjectedFileSystem` module.
//! On other platforms, every operation that actually touches the OS
//! returns an error — the crate compiles but does not mount, which keeps
//! the workspace buildable from macOS and Linux while the real callbacks
//! are filled in.
//!
//! # Callback table
//!
//! The full `ProjFS` callback table is implemented and registered with
//! `PrjStartVirtualizing`. Browse, read, notification, and cancel all
//! flow through the in-memory items map shared with the presenter:
//!
//! - [`ProjFsPresenter::upsert_item`] and [`ProjFsPresenter::delete_item`]
//!   update an in-memory `HashMap<String, VfsItem>` exactly like the
//!   other presenters. The enumeration and placeholder callbacks read
//!   from it.
//! - `QueryFileName`, `GetPlaceholderInfo`, and the
//!   `Start`/`Get`/`End` directory enumeration triplet serve directory
//!   listings and placeholder metadata from the items map.
//! - `GetFileData` serves on-demand reads. When a [`ContentProvider`]
//!   has been installed via [`ProjFsPresenter::with_content_provider`],
//!   the callback resolves the path back to an [`ItemId`], asks the
//!   provider for the requested byte range, and forwards the bytes via
//!   `PrjWriteFileData`. Without a provider the callback returns
//!   `ERROR_CALL_NOT_IMPLEMENTED` and the projection stays browse-only.
//! - `Notification` decodes user-driven events via
//!   [`NotificationEvent::from_i32`], logs them, and vetoes deletes the
//!   `allow_delete` policy rejects. The notification mapping registered
//!   at start includes `PRJ_NOTIFY_PRE_DELETE` so the veto hook is
//!   reachable in production.
//! - `CancelCommand` signals the [`CancellationToken`] registered by the
//!   in-flight `GetFileData` for the matching `CommandId`, which aborts
//!   the read at its next checkpoint with `ERROR_OPERATION_ABORTED`.
//! - [`ProjFsPresenter::update_state`] is a no-op log line; `ProjFS` has
//!   no equivalent of `FSKit`'s `update_state` push hook.
//! - [`ProjFsPresenter::evict_item`] logs and returns `Ok(())`; `ProjFS`
//!   manages projection cache eviction at the OS layer.
//! - [`ProjFsPresenter::fetch_contents`] intentionally bails: on-demand
//!   reads flow through the `GetFileData` callback, not through direct
//!   calls into this method, so there is nothing for it to do.
//! - [`ProjFsPresenter::start`] marks the mount directory as a
//!   placeholder via `PrjMarkDirectoryAsPlaceholder` and begins
//!   virtualising via `PrjStartVirtualizing`.
//! - [`ProjFsPresenter::stop`] calls `PrjStopVirtualizing` against the
//!   stored namespace handle, then releases the heap-allocated
//!   callback context.
//!
//! On non-Windows targets, [`ProjFsPresenter::start`] returns
//! `Err("ProjFS presenter is only supported on Windows")` and
//! [`ProjFsPresenter::stop`] is a no-op, so the crate stays buildable
//! from macOS and Linux while the real callbacks only execute on Windows.
//!
//! See the [Projected File System Win32 documentation][projfs] for the
//! full callback contract.
//!
//! [projfs]: https://learn.microsoft.com/en-us/windows/win32/projfs/projected-file-system

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use cascade_engine::presenter::VfsPresenter;
use cascade_engine::types::{CacheState, ItemId, VfsItem};
use tokio::sync::RwLock;

/// One-shot cancellation flag shared between a long-running callback
/// and `CancelCommand`.
///
/// The flag is set to `true` when `ProjFS` invokes
/// [`PRJ_CANCEL_COMMAND_CB`][cancel-cb] for the matching `CommandId`;
/// the running callback polls [`Self::is_cancelled`] between
/// expensive steps and bails with `ERROR_OPERATION_ABORTED` when it
/// sees the flag.
///
/// Kept as a thin newtype around `Arc<AtomicBool>` rather than
/// reaching for `tokio_util::sync::CancellationToken` because the
/// workspace does not depend on `tokio-util` outside the `WebDAV`
/// presenter and the simpler primitive is enough for the
/// `ProjFS` callback model.
///
/// [cancel-cb]: https://learn.microsoft.com/en-us/windows/win32/api/projectedfslib/nc-projectedfslib-prj_cancel_command_cb
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    flag: Arc<AtomicBool>,
}

impl CancellationToken {
    /// A fresh, un-cancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark the token cancelled. Subsequent calls to
    /// [`Self::is_cancelled`] return `true`.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// `true` once [`Self::cancel`] has been called on this token or
    /// any clone.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

/// User-driven filesystem event surfaced by the `ProjFS`
/// `NotificationCallback`.
///
/// The variants mirror the `PRJ_NOTIFICATION_*` flag set so the
/// callback can dispatch on a typed value rather than raw integers,
/// and tests can assert the mapping cross-platform without depending
/// on the `windows` crate.
///
/// The variants carry the destination path only for events where
/// `ProjFS` supplies one (rename, hardlink). For every other event
/// the path is the source path on `PRJ_CALLBACK_DATA::FilePathName`
/// which the callback already has access to via the callback data.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    not(any(target_os = "windows", test)),
    allow(
        dead_code,
        reason = "consumed only by Windows callbacks and unit tests"
    )
)]
pub enum NotificationEvent {
    /// `PRJ_NOTIFICATION_FILE_OPENED` (2). A user-mode handle opened
    /// the file.
    FileOpened,
    /// `PRJ_NOTIFICATION_NEW_FILE_CREATED` (4). A new file appeared
    /// at the path.
    NewFileCreated,
    /// `PRJ_NOTIFICATION_FILE_OVERWRITTEN` (8). An existing file was
    /// truncated and rewritten.
    FileOverwritten,
    /// `PRJ_NOTIFICATION_PRE_DELETE` (16). A delete is about to
    /// occur; the callback may veto with `ERROR_ACCESS_DENIED`.
    PreDelete,
    /// `PRJ_NOTIFICATION_PRE_RENAME` (32). A rename is about to
    /// occur; the destination is supplied separately.
    PreRename {
        /// Future location of the file. May be empty if `ProjFS` did
        /// not supply one (defensive — the documented contract is
        /// non-null for rename notifications).
        destination: String,
    },
    /// `PRJ_NOTIFICATION_PRE_SET_HARDLINK` (64). A hardlink is about
    /// to be created.
    PreSetHardlink {
        /// Future location of the hardlink.
        destination: String,
    },
    /// `PRJ_NOTIFICATION_FILE_RENAMED` (128). A rename completed.
    FileRenamed {
        /// Final location of the file.
        destination: String,
    },
    /// `PRJ_NOTIFICATION_HARDLINK_CREATED` (256). A hardlink was
    /// created.
    HardlinkCreated {
        /// Location of the new hardlink.
        destination: String,
    },
    /// `PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_NO_MODIFICATION` (512).
    FileHandleClosedNoModification,
    /// `PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_MODIFIED` (1024).
    FileHandleClosedFileModified,
    /// `PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_DELETED` (2048).
    FileHandleClosedFileDeleted,
    /// `PRJ_NOTIFICATION_FILE_PRE_CONVERT_TO_FULL` (4096). The OS is
    /// about to convert the placeholder to a full file.
    FilePreConvertToFull,
}

impl NotificationEvent {
    /// Map a raw `PRJ_NOTIFICATION` value to the typed enum. Returns
    /// `None` for codes the presenter does not recognise — callers
    /// should log and continue rather than fail. `destination` is the
    /// path `ProjFS` hands in for rename/hardlink events; ignored for
    /// every other variant.
    #[must_use]
    pub fn from_i32(value: i32, destination: Option<String>) -> Option<Self> {
        let dest = destination.unwrap_or_default();
        match value {
            2 => Some(Self::FileOpened),
            4 => Some(Self::NewFileCreated),
            8 => Some(Self::FileOverwritten),
            16 => Some(Self::PreDelete),
            32 => Some(Self::PreRename { destination: dest }),
            64 => Some(Self::PreSetHardlink { destination: dest }),
            128 => Some(Self::FileRenamed { destination: dest }),
            256 => Some(Self::HardlinkCreated { destination: dest }),
            512 => Some(Self::FileHandleClosedNoModification),
            1024 => Some(Self::FileHandleClosedFileModified),
            2048 => Some(Self::FileHandleClosedFileDeleted),
            4096 => Some(Self::FilePreConvertToFull),
            _ => None,
        }
    }

    /// Short human-readable tag for tracing. Matches the
    /// `PRJ_NOTIFICATION_*` flag name with the prefix stripped.
    #[must_use]
    pub const fn tag(&self) -> &'static str {
        match self {
            Self::FileOpened => "FILE_OPENED",
            Self::NewFileCreated => "NEW_FILE_CREATED",
            Self::FileOverwritten => "FILE_OVERWRITTEN",
            Self::PreDelete => "PRE_DELETE",
            Self::PreRename { .. } => "PRE_RENAME",
            Self::PreSetHardlink { .. } => "PRE_SET_HARDLINK",
            Self::FileRenamed { .. } => "FILE_RENAMED",
            Self::HardlinkCreated { .. } => "HARDLINK_CREATED",
            Self::FileHandleClosedNoModification => "FILE_HANDLE_CLOSED_NO_MODIFICATION",
            Self::FileHandleClosedFileModified => "FILE_HANDLE_CLOSED_FILE_MODIFIED",
            Self::FileHandleClosedFileDeleted => "FILE_HANDLE_CLOSED_FILE_DELETED",
            Self::FilePreConvertToFull => "FILE_PRE_CONVERT_TO_FULL",
        }
    }
}

/// Policy hook for `PRJ_NOTIFICATION_PRE_DELETE`. Returning `false`
/// vetoes the delete with `ERROR_ACCESS_DENIED`. The current stub
/// always allows — the in-memory items map is read-only as far as
/// this presenter is concerned and the engine handles its own
/// write-protection rules. Kept as a discrete function so future
/// policy work has an obvious extension point.
///
/// `const` today only because the body is trivial; real policy work
/// will inspect `items` and drop the modifier.
#[cfg_attr(
    not(any(target_os = "windows", test)),
    allow(
        dead_code,
        reason = "consumed only by Windows callbacks and unit tests"
    )
)]
const fn allow_delete(_path: &str, _is_directory: bool, _items: &HashMap<String, VfsItem>) -> bool {
    true
}

/// Typed `HRESULT` code carried by [`ProviderReadOutcome::Failed`].
///
/// Wraps the same `i32` representation `windows::core::HRESULT` uses,
/// but lives outside the `windows` crate so cross-platform code can
/// construct, compare, and test these values without dragging the
/// Win32 bindings into non-Windows builds. The Windows callback path
/// pulls the inner `i32` out via [`Self::get`] and wraps it back into
/// `windows::core::HRESULT` at the FFI boundary.
///
/// The value follows the `HRESULT_FROM_WIN32` C macro packing for
/// Win32 facility codes: the low 16 bits hold the Win32 error code,
/// `FACILITY_WIN32` (`7`) sits in the facility field, and the
/// severity bit is set. Use [`Self::from_win32`] to construct from a
/// Win32 error number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(
    not(any(target_os = "windows", test)),
    allow(
        dead_code,
        reason = "consumed only by Windows callbacks and unit tests"
    )
)]
pub(crate) struct HResultCode(i32);

#[cfg_attr(
    not(any(target_os = "windows", test)),
    allow(
        dead_code,
        reason = "consumed only by Windows callbacks and unit tests"
    )
)]
impl HResultCode {
    /// Pack a Win32 error number into an `HRESULT` in the
    /// `FACILITY_WIN32` space, matching the `HRESULT_FROM_WIN32` C
    /// macro for positive Win32 codes.
    ///
    /// Win32 error codes (e.g. `ERROR_FILE_NOT_FOUND = 2`) are
    /// positive `u32` values; the resulting `HRESULT` has the high
    /// bit set and the facility set to `7` (`FACILITY_WIN32`).
    #[must_use]
    pub(crate) const fn from_win32(code: u32) -> Self {
        // FACILITY_WIN32 = 7. The packing is
        // `((code & 0xFFFF) | (FACILITY_WIN32 << 16) | 0x80000000)`,
        // reinterpreted as a signed `i32`.
        #[allow(clippy::cast_possible_wrap)]
        let value = ((code & 0x0000_FFFF) | (7 << 16) | 0x8000_0000) as i32;
        Self(value)
    }

    /// The inner `i32` representation. Equivalent to the `.0` field
    /// of `windows::core::HRESULT` on Windows; safe to feed straight
    /// into `windows::core::HRESULT(_)` at the FFI boundary.
    #[must_use]
    pub(crate) const fn get(self) -> i32 {
        self.0
    }
}

/// Map a [`std::io::Error`] to a typed [`HResultCode`] in the
/// `FACILITY_WIN32` space.
///
/// The mapping is kind-aware and uses the Win32 numeric values listed
/// below. Any [`std::io::ErrorKind`] not covered explicitly falls
/// through to `ERROR_GEN_FAILURE` so the callback never silently
/// erases a failure.
///
/// | [`std::io::ErrorKind`] | Win32 code (numeric) | Win32 constant |
/// |---|---|---|
/// | `NotFound` | 2 | `ERROR_FILE_NOT_FOUND` |
/// | `PermissionDenied` | 5 | `ERROR_ACCESS_DENIED` |
/// | `Interrupted` | 995 | `ERROR_OPERATION_ABORTED` |
/// | `OutOfMemory` | 14 | `ERROR_OUTOFMEMORY` |
/// | `TimedOut` | 1460 | `ERROR_TIMEOUT` |
/// | `BrokenPipe` | 109 | `ERROR_BROKEN_PIPE` |
/// | `UnexpectedEof` | 38 | `ERROR_HANDLE_EOF` |
/// | `WouldBlock` | 997 | `ERROR_IO_PENDING` |
/// | every other variant | 31 | `ERROR_GEN_FAILURE` |
///
/// The Win32 constants live in `windows::Win32::Foundation::*`; the
/// numeric values are reproduced here so the mapping compiles on
/// non-Windows targets and can be tested cross-platform.
///
/// Two further Win32 codes appear in the `GetFileData` callback but
/// are produced at the call site rather than from an
/// [`std::io::ErrorKind`], so they are not in the table above:
///
/// - `ERROR_NOT_ENOUGH_MEMORY` (Win32 8) — returned when
///   `PrjAllocateAlignedBuffer` returns null. The allocator could not
///   produce an aligned buffer for the requested size, typically
///   because the paged pool is exhausted or the requested length is
///   unsatisfiable.
/// - `ERROR_CALL_NOT_IMPLEMENTED` (Win32 120) — returned by design
///   when no [`ContentProvider`] is installed. This is the documented
///   `ProjFS` contract for "the provider intentionally does not serve
///   this callback"; the kernel drops into browse-only mode in
///   response. It is not used as a generic failure sentinel anywhere
///   else in the read path.
#[cfg_attr(
    not(any(target_os = "windows", test)),
    allow(
        dead_code,
        reason = "consumed only by Windows callbacks and unit tests"
    )
)]
pub(crate) fn hresult_for_io_error(err: &std::io::Error) -> HResultCode {
    // The numeric values match the corresponding `WIN32_ERROR`
    // constants in `windows::Win32::Foundation`. Keeping them as
    // literals lets this function compile without the `windows`
    // crate and lets the unit tests assert the exact packed HRESULT
    // produced for each `ErrorKind`.
    let win32_code: u32 = match err.kind() {
        std::io::ErrorKind::NotFound => 2,         // ERROR_FILE_NOT_FOUND
        std::io::ErrorKind::PermissionDenied => 5, // ERROR_ACCESS_DENIED
        std::io::ErrorKind::Interrupted => 995,    // ERROR_OPERATION_ABORTED
        std::io::ErrorKind::OutOfMemory => 14,     // ERROR_OUTOFMEMORY
        std::io::ErrorKind::TimedOut => 1460,      // ERROR_TIMEOUT
        std::io::ErrorKind::BrokenPipe => 109,     // ERROR_BROKEN_PIPE
        std::io::ErrorKind::UnexpectedEof => 38,   // ERROR_HANDLE_EOF
        std::io::ErrorKind::WouldBlock => 997,     // ERROR_IO_PENDING
        _ => 31,                                   // ERROR_GEN_FAILURE
    };
    HResultCode::from_win32(win32_code)
}

/// Source of file bytes consulted by the `ProjFS` `GetFileData`
/// callback when the OS asks for the contents of a virtualised file.
///
/// The presenter holds an optional `Arc<dyn ContentProvider>`; when
/// unset, `GetFileData` returns `ERROR_CALL_NOT_IMPLEMENTED` and the
/// projection remains browse-only (the historic behaviour). When set,
/// the callback resolves the path back to a [`VfsItem`], asks the
/// provider for the requested byte range, and forwards the bytes to
/// `ProjFS` via `PrjWriteFileData`.
///
/// Implementations must be cheap to clone (the trait is consumed
/// through an `Arc`) and safe to call from `ProjFS`'s kernel-owned
/// worker threads — i.e. they must not enter the Tokio runtime
/// implicitly. A backend-backed implementation should perform any
/// async fetch on a separate executor and block on the result, or
/// rely on a pre-warmed cache.
pub trait ContentProvider: Send + Sync + std::fmt::Debug {
    /// Read `length` bytes starting at `offset` from the file
    /// identified by `id`. Returns the bytes on success.
    ///
    /// Implementations should return a short read (fewer bytes than
    /// requested) at end of file rather than an error.
    fn read_range(&self, id: &ItemId, offset: u64, length: u32) -> std::io::Result<Vec<u8>>;
}

/// Decision the `ProjFS` `GetFileData` callback derives from a
/// [`ContentProvider::read_range`] result, before any FFI is invoked.
///
/// Extracting this classification keeps the EOF and error-handling
/// branches testable cross-platform; the Windows-only callback maps
/// each variant to the appropriate `HRESULT` (`S_OK` for `Eof`, the
/// carried [`HResultCode`] for `Failed`, `PrjWriteFileData` for
/// `Bytes`). The carried code is produced by
/// [`hresult_for_io_error`], which translates [`std::io::ErrorKind`]
/// into a kind-aware Win32 `HRESULT` so the OS sees a meaningful
/// failure code instead of a generic "not implemented".
#[cfg_attr(
    not(any(target_os = "windows", test)),
    allow(
        dead_code,
        reason = "consumed only by Windows callbacks and unit tests"
    )
)]
#[derive(Debug)]
pub(crate) enum ProviderReadOutcome {
    /// Provider returned a non-empty buffer; the callback writes it
    /// back via `PrjWriteFileData`.
    Bytes(Vec<u8>),
    /// Provider returned zero bytes — end of file. The callback maps
    /// this to `S_OK`, which `ProjFS` accepts as a legitimate short
    /// read.
    Eof,
    /// Provider read failed. The callback forwards the carried
    /// [`HResultCode`] to `ProjFS`. See [`hresult_for_io_error`] for
    /// the `io::ErrorKind` → Win32 mapping.
    Failed(HResultCode),
}

/// Map a [`ContentProvider::read_range`] result to a
/// [`ProviderReadOutcome`]. Pure function; tested directly without
/// `ProjFS` involvement. Failures route through
/// [`hresult_for_io_error`] so the carried [`HResultCode`] reflects
/// the underlying [`std::io::ErrorKind`] instead of a generic
/// fallback.
#[cfg_attr(
    not(any(target_os = "windows", test)),
    allow(
        dead_code,
        reason = "consumed only by Windows callbacks and unit tests"
    )
)]
pub(crate) fn classify_read(result: std::io::Result<Vec<u8>>) -> ProviderReadOutcome {
    match result {
        Ok(bytes) if bytes.is_empty() => ProviderReadOutcome::Eof,
        Ok(bytes) => ProviderReadOutcome::Bytes(bytes),
        Err(err) => ProviderReadOutcome::Failed(hresult_for_io_error(&err)),
    }
}

/// State tracked for an in-flight directory enumeration session.
///
/// `ProjFS` opens an enumeration with `StartDirectoryEnumeration`, pulls
/// entries one batch at a time through `GetDirectoryEnumeration`, and
/// closes it with `EndDirectoryEnumeration`. Each session is identified
/// by a `windows::core::GUID` (stored here as the equivalent `u128`
/// for `cfg`-independent map keys). The session needs a stable, ordered
/// view of the directory's children plus a cursor into that view so
/// consecutive calls resume where the previous one left off.
#[derive(Debug, Clone)]
#[cfg_attr(
    not(any(target_os = "windows", test)),
    allow(
        dead_code,
        reason = "consumed only by Windows callbacks and unit tests"
    )
)]
struct EnumerationState {
    /// Snapshot of `(filename, basic-info)` pairs taken when the
    /// enumeration was opened. Sorted by `PrjFileNameCompare`-equivalent
    /// case-insensitive ordering so listings are deterministic.
    entries: Vec<EnumerationEntry>,
    /// Cursor into `entries`. The next `GetDirectoryEnumeration` call
    /// resumes here.
    position: usize,
}

/// One entry produced for an enumeration. Stripped of any data the
/// callbacks do not need so the snapshot is cheap to clone.
#[derive(Debug, Clone)]
#[cfg_attr(
    not(any(target_os = "windows", test)),
    allow(
        dead_code,
        reason = "consumed only by Windows callbacks and unit tests"
    )
)]
struct EnumerationEntry {
    /// Display name as it appears in the directory listing.
    name: String,
    /// `true` for directories, `false` for files.
    is_dir: bool,
    /// File size in bytes, or `0` for directories / unknown.
    size: u64,
}

/// Characters Windows forbids in filenames. Any item whose `name`
/// contains one of these is skipped from enumeration with a debug log
/// — emitting it would either fail at the `ProjFS` layer or produce a
/// path the OS could not represent.
const WINDOWS_FORBIDDEN_CHARS: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];

/// Return `true` when `name` is safe to surface through `ProjFS`. The
/// check is intentionally cheap — it only rejects characters the
/// Windows filesystem itself rejects. Higher-level filters (hidden
/// files, reserved DOS names like `CON`/`PRN`) are left to the engine.
#[cfg_attr(
    not(any(target_os = "windows", test)),
    allow(
        dead_code,
        reason = "consumed only by Windows callbacks and unit tests"
    )
)]
fn is_safe_windows_filename(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    !name
        .chars()
        .any(|c| WINDOWS_FORBIDDEN_CHARS.contains(&c) || c == '\0')
}

/// Walk the parent chain of `start` until we find an item that does
/// not exist in `items`. The returned `Vec` is in root-to-leaf order
/// (i.e. `["dir1", "dir2", "file.txt"]`).
#[cfg_attr(
    not(any(target_os = "windows", test)),
    allow(
        dead_code,
        reason = "consumed only by Windows callbacks and unit tests"
    )
)]
fn ancestor_chain(start: &VfsItem, items: &HashMap<String, VfsItem>) -> Vec<String> {
    let mut parts = vec![start.name.clone()];
    let mut current_parent = start.parent_id.0.clone();
    let mut seen = std::collections::HashSet::new();
    while let Some(parent) = items.get(&current_parent) {
        if !seen.insert(current_parent.clone()) {
            break; // cycle defence
        }
        parts.push(parent.name.clone());
        current_parent = parent.parent_id.0.clone();
    }
    parts.reverse();
    parts
}

/// Normalise a `ProjFS` relative path into a slash-separated
/// lowercase form for case-insensitive comparison.
///
/// `ProjFS` paths use backslashes and are case-insensitive on NTFS;
/// the items map stores names with their original casing and no path
/// separators. Comparing in lowercase slash form keeps the lookup
/// independent of the source's casing.
#[cfg_attr(
    not(any(target_os = "windows", test)),
    allow(
        dead_code,
        reason = "consumed only by Windows callbacks and unit tests"
    )
)]
fn normalise_relative_path(raw: &str) -> String {
    raw.trim_matches(|c: char| c == '/' || c == '\\')
        .replace('\\', "/")
        .to_lowercase()
}

/// Resolve a `ProjFS` relative path (e.g. `"dir1\\file.txt"`) to the
/// matching item in `items`. Returns `None` if no item has that path.
///
/// `root_id` is the parent ID that every top-level item points at.
/// When `None`, the lookup treats any item whose `parent_id` is not
/// itself a key in `items` as a root entry — matches the legacy
/// behaviour of presenters that have not been told their root.
#[cfg_attr(
    not(any(target_os = "windows", test)),
    allow(
        dead_code,
        reason = "consumed only by Windows callbacks and unit tests"
    )
)]
fn resolve_path<'a>(
    relative: &str,
    items: &'a HashMap<String, VfsItem>,
    root_id: Option<&ItemId>,
) -> Option<&'a VfsItem> {
    let needle = normalise_relative_path(relative);
    if needle.is_empty() {
        // The root itself is not a regular item; callers handle it
        // separately when they need to enumerate root.
        return None;
    }
    items.values().find(|item| {
        let chain = ancestor_chain(item, items);
        // Only items rooted under `root_id` (if specified) participate
        // in the projection. When no root_id is set, every item is
        // eligible — matches the loose-root fallback documented in
        // `ProjFsPresenter::with_root`.
        if let Some(root) = root_id
            && item_root(item, items).as_ref() != Some(root)
        {
            return false;
        }
        chain.join("/").to_lowercase() == needle
    })
}

/// Compute the root ancestor's `parent_id` for `item`. Used when
/// matching items against the configured `root_id`. The result is the
/// `parent_id` of the topmost ancestor; for a top-level item this is
/// the item's own `parent_id`.
#[cfg_attr(
    not(any(target_os = "windows", test)),
    allow(
        dead_code,
        reason = "consumed only by Windows callbacks and unit tests"
    )
)]
fn item_root(item: &VfsItem, items: &HashMap<String, VfsItem>) -> Option<ItemId> {
    let mut current = item.parent_id.clone();
    let mut seen = std::collections::HashSet::new();
    loop {
        if !seen.insert(current.0.clone()) {
            return None; // cycle
        }
        match items.get(&current.0) {
            Some(parent) => current = parent.parent_id.clone(),
            None => return Some(current),
        }
    }
}

/// Build an [`EnumerationState`] for a directory snapshot. Used by
/// the Windows `StartDirectoryEnumeration` callback and the
/// cross-platform tests covering enumeration semantics.
#[cfg_attr(
    not(any(target_os = "windows", test)),
    allow(
        dead_code,
        reason = "consumed only by Windows callbacks and unit tests"
    )
)]
fn build_enumeration_state(
    parent_id: &ItemId,
    items: &HashMap<String, VfsItem>,
) -> EnumerationState {
    EnumerationState {
        entries: collect_children(parent_id, items),
        position: 0,
    }
}

/// Collect the children of `parent_id` from `items`, filter out
/// anything Windows cannot represent, and return them sorted by name
/// in case-insensitive order. The sort is what `ProjFS` expects:
/// `PrjFileNameCompare` orders entries case-insensitively.
#[cfg_attr(
    not(any(target_os = "windows", test)),
    allow(
        dead_code,
        reason = "consumed only by Windows callbacks and unit tests"
    )
)]
fn collect_children(parent_id: &ItemId, items: &HashMap<String, VfsItem>) -> Vec<EnumerationEntry> {
    let mut out: Vec<EnumerationEntry> = items
        .values()
        .filter(|item| &item.parent_id == parent_id)
        .filter_map(|item| {
            if is_safe_windows_filename(&item.name) {
                Some(EnumerationEntry {
                    name: item.name.clone(),
                    is_dir: item.is_dir,
                    size: item.size.unwrap_or(0),
                })
            } else {
                tracing::debug!(
                    name = %item.name,
                    id = %item.id,
                    "skipping item with name unsafe for Windows"
                );
                None
            }
        })
        .collect();
    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

/// Collect the projection root's children when no explicit root id has
/// been configured (the loose-root fallback).
///
/// A top-level item is one whose `parent_id` is not itself a key in
/// `items` — i.e. its parent lies outside the projected map. This is
/// the same "parent-not-a-key = root" rule [`item_root`] applies, and
/// it matches what the sync runner actually produces: top-level items
/// are labelled `<backend>:root` (or a real folder id for a scoped
/// mount), never a hard-coded bare `"root"`. Matching a literal
/// `"root"` would enumerate zero children against a real daemon mount.
///
/// Items are filtered and sorted exactly as [`collect_children`] does,
/// so the two paths agree on what `ProjFS` can represent.
#[cfg_attr(
    not(any(target_os = "windows", test)),
    allow(
        dead_code,
        reason = "consumed only by Windows callbacks and unit tests"
    )
)]
fn collect_root_children(items: &HashMap<String, VfsItem>) -> Vec<EnumerationEntry> {
    let mut out: Vec<EnumerationEntry> = items
        .values()
        .filter(|item| !items.contains_key(&item.parent_id.0))
        .filter_map(|item| {
            if is_safe_windows_filename(&item.name) {
                Some(EnumerationEntry {
                    name: item.name.clone(),
                    is_dir: item.is_dir,
                    size: item.size.unwrap_or(0),
                })
            } else {
                tracing::debug!(
                    name = %item.name,
                    id = %item.id,
                    "skipping item with name unsafe for Windows"
                );
                None
            }
        })
        .collect();
    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

/// Opaque handle to a running `ProjFS` virtualisation instance.
///
/// On Windows this wraps a `PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT`
/// returned by `PrjStartVirtualizing`. On other platforms it is an
/// uninhabited placeholder — `ProjFsPresenter::start` returns an error
/// before this type would ever be constructed.
#[cfg(target_os = "windows")]
mod handle {
    use windows::Win32::Storage::ProjectedFileSystem::PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT;

    /// Wrapper around the raw `ProjFS` handle so we can store it in
    /// `ProjFsPresenter` and call `PrjStopVirtualizing` on shutdown.
    ///
    /// `PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT` is `!Send` and `!Sync` by
    /// default because it is a raw `HANDLE`. `ProjFS` itself is safe to
    /// use from any thread (the documented contract for
    /// `PrjStopVirtualizing` is "call from any thread once
    /// virtualising is finished"), so we manually assert `Send + Sync`
    /// here. The presenter stores the handle behind a `Mutex` so
    /// concurrent access is serialised.
    #[derive(Debug)]
    pub struct NamespaceHandle(pub PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT);

    // SAFETY: see doc-comment above. PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT
    // is an opaque HANDLE that the OS allows callers to share across
    // threads. We never expose it without going through the mutex on
    // ProjFsPresenter, so there is no aliased mutable access.
    #[allow(unsafe_code)]
    unsafe impl Send for NamespaceHandle {}
    #[allow(unsafe_code)]
    unsafe impl Sync for NamespaceHandle {}
}

#[cfg(not(target_os = "windows"))]
mod handle {
    /// Placeholder used on non-Windows targets so the `ProjFsPresenter`
    /// struct shape is the same across platforms. It is never
    /// constructed — `start` returns an error before reaching the code
    /// that would build one.
    #[derive(Debug)]
    pub struct NamespaceHandle;
}

use handle::NamespaceHandle;

/// `ProjFS` presenter — implements [`VfsPresenter`] for Windows
/// Projected File System mounts.
#[derive(Debug)]
pub struct ProjFsPresenter {
    /// Mount point path — the directory `ProjFS` treats as the
    /// virtualisation root.
    mount_point: PathBuf,
    /// In-memory item store keyed by `ItemId`. Populated by
    /// [`Self::upsert_item`] / [`Self::delete_item`] from the engine's
    /// sync runner, and read by the (future) `ProjFS` callbacks.
    items: Arc<RwLock<HashMap<String, VfsItem>>>,
    /// Optional root `ItemId`. When set, only items whose ancestor
    /// chain terminates at this id participate in the projection — the
    /// same pattern used by the FUSE presenter. When unset, the
    /// presenter treats every item in `items` as eligible.
    root_id: Arc<RwLock<Option<ItemId>>>,
    /// In-flight directory enumerations keyed by the `GUID` (stored as
    /// `u128`) `ProjFS` issues at `StartDirectoryEnumeration`. The
    /// callbacks are synchronous and called from kernel threads, so a
    /// `std::sync::Mutex` keeps the access path off the Tokio runtime.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    enumerations: Arc<Mutex<HashMap<u128, EnumerationState>>>,
    /// Live `ProjFS` virtualisation handle. `Some` only between
    /// [`Self::start`] and [`Self::stop`] on Windows; always `None` on
    /// other platforms — but the field is kept on every target so the
    /// struct shape does not vary with `cfg`.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    handle: Arc<tokio::sync::Mutex<Option<NamespaceHandle>>>,
    /// Heap-allocated callback context handed to `PrjStartVirtualizing`
    /// via its `instance_context` parameter. The callbacks dereference
    /// it back to access `items`, `root_id`, and `enumerations`. Held
    /// here so it outlives the namespace handle and is dropped on
    /// [`Self::stop`].
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    callback_ctx: Arc<tokio::sync::Mutex<Option<CallbackContext>>>,
    /// Optional source of file bytes for the `GetFileData` callback.
    /// When `None`, `GetFileData` returns `ERROR_CALL_NOT_IMPLEMENTED`
    /// and the projection stays browse-only — the historic scaffold
    /// behaviour. When `Some`, the callback asks the provider for the
    /// requested byte range and pushes the bytes back via
    /// `PrjWriteFileData`. Configured through
    /// [`Self::with_content_provider`].
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    content_provider: Option<Arc<dyn ContentProvider>>,
    /// In-flight cancellation tokens keyed by
    /// `PRJ_CALLBACK_DATA::CommandId`. Populated by `get_file_data`
    /// on entry, removed on exit, and signalled by `cancel_command`
    /// when `ProjFS` aborts the operation. Shared with the heap
    /// `CallbackContextInner` so both callbacks see the same map.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    cancellation_tokens: Arc<Mutex<HashMap<i32, CancellationToken>>>,
}

/// Owning record of the heap allocation handed to `ProjFS` via
/// `PrjStartVirtualizing`'s `instance_context`. The Arcs the callbacks
/// dereference live on `CallbackContextInner` (heap-Boxed and reached
/// through `raw_ptr`); the presenter already retains its own Arc clones
/// of the same data, so we do not duplicate them here.
#[derive(Debug)]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
struct CallbackContext {
    /// Owning pointer handed to `ProjFS`, stored as `usize` so the
    /// field is `Send + Sync`. The pointer originally came from
    /// `Box::into_raw(Box::new(CallbackContextInner { .. }))` and is
    /// rebuilt with `Box::from_raw` on stop to free the allocation.
    raw_ptr: usize,
}

impl Drop for ProjFsPresenter {
    /// Reclaim the `CallbackContextInner` Box-allocation on drop if the
    /// caller forgot to call `stop()`. Without this, a presenter that
    /// `start()`s and then drops would leak the heap-allocated context
    /// that we handed to `ProjFS` via `instance_context`. The `VfsPresenter`
    /// trait contract requires `stop()` on shutdown, but the language
    /// can't enforce it — this is the belt-and-braces.
    ///
    /// On non-Windows targets the handle and `callback_ctx` are always
    /// `None` (start returns an error before allocation), so this is a
    /// no-op.
    #[cfg(target_os = "windows")]
    fn drop(&mut self) {
        // try_lock is the right primitive for Drop: if another future is
        // holding the lock we'd be dropping under aliasing anyway, and
        // panicking in Drop is unsound. Silently skip in that case —
        // production code always calls stop() first, and tests don't
        // race on these locks.
        if let Ok(mut slot) = self.handle.try_lock()
            && let Some(handle::NamespaceHandle(ctx)) = slot.take()
        {
            // SAFETY: ctx was produced by PrjStartVirtualizing and never
            // released. See stop_virtualising for the full safety note.
            #[allow(unsafe_code)]
            unsafe {
                windows::Win32::Storage::ProjectedFileSystem::PrjStopVirtualizing(ctx);
            }
        }
        if let Ok(mut slot) = self.callback_ctx.try_lock()
            && let Some(CallbackContext { raw_ptr }) = slot.take()
            && raw_ptr != 0
        {
            // SAFETY: raw_ptr originated from Box::into_raw in
            // start_virtualising. ProjFS has stopped (or was never
            // started) so no callbacks can still dereference it.
            #[allow(unsafe_code)]
            unsafe {
                drop(Box::from_raw(
                    raw_ptr as *mut windows_impl::CallbackContextInner,
                ));
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn drop(&mut self) {
        // On non-Windows targets `start()` never reaches the allocation
        // path, so `handle` and `callback_ctx` are always `None`. Nothing
        // to reclaim.
    }
}

impl ProjFsPresenter {
    /// Create a new `ProjFS` presenter rooted at `mount_point`.
    #[must_use]
    pub fn new(mount_point: impl Into<PathBuf>) -> Self {
        Self {
            mount_point: mount_point.into(),
            items: Arc::new(RwLock::new(HashMap::new())),
            root_id: Arc::new(RwLock::new(None)),
            enumerations: Arc::new(Mutex::new(HashMap::new())),
            handle: Arc::new(tokio::sync::Mutex::new(None)),
            callback_ctx: Arc::new(tokio::sync::Mutex::new(None)),
            content_provider: None,
            cancellation_tokens: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Override the mount point after construction. Mirrors the
    /// builder-style API used by the `FSKit` and `WebDAV` presenters.
    #[must_use]
    pub fn with_mount_point(mut self, path: impl Into<PathBuf>) -> Self {
        self.mount_point = path.into();
        self
    }

    /// Set the root [`ItemId`] for the projection. Builder-style; can
    /// be called before [`Self::start`]. When not called, the presenter
    /// treats every item in its in-memory map as eligible — useful for
    /// tests and the boot-time case where the root isn't known yet.
    #[must_use]
    pub fn with_root(self, root: ItemId) -> Self {
        // Replace the inner value rather than the whole `Arc` so
        // callers that already cloned the Arc see the update.
        if let Ok(mut slot) = self.root_id.try_write() {
            *slot = Some(root);
        } else {
            // Fall back to blocking_write — only reachable from a
            // non-tokio runtime, which is the contract for builder
            // methods. The unwrap is unreachable in normal use because
            // a fresh presenter has no contention on its lock.
            let mut slot = self.root_id.blocking_write();
            *slot = Some(root);
        }
        self
    }

    /// Install a [`ContentProvider`] for the `GetFileData` callback.
    /// Without one, the callback returns `ERROR_CALL_NOT_IMPLEMENTED`
    /// and the projection remains browse-only.
    #[must_use]
    pub fn with_content_provider(mut self, provider: Arc<dyn ContentProvider>) -> Self {
        self.content_provider = Some(provider);
        self
    }

    /// The configured mount point.
    #[must_use]
    pub fn mount_point(&self) -> &Path {
        &self.mount_point
    }

    /// Access the configured content provider, if any. Exposed for
    /// tests and the (future) consistency checks that want to confirm
    /// the presenter was built with one before `start()`.
    #[must_use]
    pub fn content_provider(&self) -> Option<&Arc<dyn ContentProvider>> {
        self.content_provider.as_ref()
    }

    /// Access the in-memory item store. Tests and (in the future) the
    /// `ProjFS` callbacks need this to look up items by `ItemId`.
    #[must_use]
    pub const fn items(&self) -> &Arc<RwLock<HashMap<String, VfsItem>>> {
        &self.items
    }

    /// Read the current root `ItemId`, if one has been set.
    pub async fn root(&self) -> Option<ItemId> {
        self.root_id.read().await.clone()
    }
}

#[async_trait]
impl VfsPresenter for ProjFsPresenter {
    async fn upsert_item(&self, item: VfsItem) -> anyhow::Result<()> {
        tracing::debug!(id = %item.id, name = %item.name, "upsert_item");
        let key = item.id.0.clone();
        let mut items = self.items.write().await;
        items.insert(key, item);
        Ok(())
    }

    async fn delete_item(&self, id: &ItemId) -> anyhow::Result<()> {
        tracing::debug!(id = %id, "delete_item");
        let mut items = self.items.write().await;
        items.remove(&id.0);
        Ok(())
    }

    async fn update_state(&self, id: &ItemId, state: CacheState) -> anyhow::Result<()> {
        // ProjFS has no kernel hook for "this file's cache state
        // changed". The next enumeration / placeholder request will
        // observe whatever state the in-memory map holds.
        tracing::debug!(id = %id, state = %state, "update_state (no-op for ProjFS)");
        Ok(())
    }

    async fn fetch_contents(&self, id: &ItemId) -> anyhow::Result<PathBuf> {
        tracing::debug!(id = %id, "fetch_contents (not yet implemented)");
        anyhow::bail!(
            "ProjFS fetch_contents is not yet implemented; the GetFileData callback should drive this"
        )
    }

    async fn evict_item(&self, id: &ItemId) -> anyhow::Result<()> {
        // ProjFS owns the projection cache. Eviction is driven by the
        // OS based on disk pressure and the `PRJ_FILE_STATE` flags
        // returned from callbacks; the presenter has no direct lever.
        tracing::debug!(id = %id, "evict_item (handled by ProjFS cache manager)");
        Ok(())
    }

    async fn start(&self, mount_point: &Path) -> anyhow::Result<()> {
        let mount_display = mount_point.display();

        #[cfg(target_os = "windows")]
        {
            windows_impl::start_virtualising(
                mount_point,
                &self.handle,
                &self.callback_ctx,
                Arc::clone(&self.items),
                Arc::clone(&self.root_id),
                Arc::clone(&self.enumerations),
                self.content_provider.clone(),
                Arc::clone(&self.cancellation_tokens),
            )
            .await?;
            tracing::info!(
                mount_point = %mount_display,
                "ProjFS virtualisation started (browse callbacks live; read/write stubs)"
            );
            Ok(())
        }

        #[cfg(not(target_os = "windows"))]
        {
            tracing::warn!(
                mount_point = %mount_display,
                "ProjFS presenter is not available on this platform (Windows only)"
            );
            anyhow::bail!(
                "ProjFS presenter is only supported on Windows. Current platform cannot start virtualisation."
            )
        }
    }

    async fn stop(&self) -> anyhow::Result<()> {
        #[cfg(target_os = "windows")]
        {
            windows_impl::stop_virtualising(&self.handle, &self.callback_ctx).await
        }

        #[cfg(not(target_os = "windows"))]
        {
            tracing::debug!("ProjFS stop on non-Windows platform is a no-op");
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Windows-only implementation
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod windows_impl {
    //! Real `ProjFS` bindings — only compiled on Windows targets.
    //!
    //! Browse callbacks (`QueryFileName`, `GetPlaceholderInfo`,
    //! `Start/Get/End` directory enumeration) consult the in-memory
    //! items map shared with the presenter. `GetFileData` reads a
    //! byte range from the installed [`ContentProvider`] (when one is
    //! present) and forwards it via `PrjWriteFileData`; without a
    //! provider it returns `ERROR_CALL_NOT_IMPLEMENTED`. The read
    //! registers a [`super::CancellationToken`] keyed by
    //! `PRJ_CALLBACK_DATA::CommandId` and checks it between expensive
    //! steps; `CancelCommandCallback` triggers the matching token to
    //! abort the read with `ERROR_OPERATION_ABORTED`.
    //! `NotificationCallback` decodes the event via
    //! [`super::NotificationEvent`], logs at `debug`, and vetoes
    //! deletes via [`super::allow_delete`] when the policy says no.

    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use anyhow::{Context as _, Result};
    use cascade_engine::types::{ItemId, VfsItem};
    use tokio::sync::RwLock;
    use windows::Win32::Foundation::{
        ERROR_CALL_NOT_IMPLEMENTED, ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER,
        ERROR_INTERNAL_ERROR, ERROR_NOT_ENOUGH_MEMORY, ERROR_OPERATION_ABORTED, S_OK,
    };
    use windows::Win32::Storage::ProjectedFileSystem::{
        PRJ_CALLBACK_DATA, PRJ_CALLBACKS, PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN,
        PRJ_DIR_ENTRY_BUFFER_HANDLE, PRJ_FILE_BASIC_INFO, PRJ_NOTIFICATION,
        PRJ_NOTIFICATION_MAPPING, PRJ_NOTIFICATION_PARAMETERS,
        PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_DELETED, PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_MODIFIED,
        PRJ_NOTIFY_FILE_OVERWRITTEN, PRJ_NOTIFY_NEW_FILE_CREATED, PRJ_NOTIFY_PRE_DELETE,
        PRJ_NOTIFY_PRE_RENAME, PRJ_NOTIFY_TYPES, PRJ_PLACEHOLDER_INFO,
        PRJ_STARTVIRTUALIZING_OPTIONS, PrjAllocateAlignedBuffer, PrjFillDirEntryBuffer,
        PrjFreeAlignedBuffer, PrjMarkDirectoryAsPlaceholder, PrjStartVirtualizing,
        PrjStopVirtualizing, PrjWriteFileData, PrjWritePlaceholderInfo,
    };
    use windows::core::{GUID, HRESULT, HSTRING, PCWSTR};

    use super::handle::NamespaceHandle;
    use super::{
        CallbackContext, CancellationToken, ContentProvider, EnumerationState,
        build_enumeration_state, collect_children, collect_root_children, resolve_path,
    };

    /// Heap-allocated state the callbacks consult. Kept distinct from
    /// the public [`CallbackContext`] wrapper so the wrapper can stay
    /// `Send + Sync` while the raw pointer to this inner struct is
    /// the one `ProjFS` dereferences.
    pub struct CallbackContextInner {
        items: Arc<RwLock<HashMap<String, VfsItem>>>,
        root_id: Arc<RwLock<Option<ItemId>>>,
        enumerations: Arc<Mutex<HashMap<u128, EnumerationState>>>,
        /// Optional source of file bytes for the `GetFileData`
        /// callback. `None` keeps the projection browse-only.
        content_provider: Option<Arc<dyn ContentProvider>>,
        /// Cancellation tokens for in-flight commands, keyed by the
        /// `PRJ_CALLBACK_DATA::CommandId`. `get_file_data` inserts
        /// on entry and removes on exit; `cancel_command` looks up
        /// the entry and triggers the token. Reachable from any
        /// callback thread, so wrapped in a `std::sync::Mutex`.
        cancellation_tokens: Arc<Mutex<HashMap<i32, CancellationToken>>>,
        /// Notification mappings handed to `PrjStartVirtualizing` via
        /// `PRJ_STARTVIRTUALIZING_OPTIONS::NotificationMappings`. ProjFS
        /// retains the pointer for the lifetime of the virtualisation
        /// *instance*, not just the duration of the start call (see the
        /// Microsoft ProjFS-Managed-API `VirtualizationInstance.cs`,
        /// which pins the array and frees it only in `StopVirtualizing`).
        /// Storing the `Vec` on the boxed context — which lives until
        /// `stop_virtualising` does `Box::from_raw` *after*
        /// `PrjStopVirtualizing` has drained outstanding callbacks —
        /// keeps the array alive exactly as long as ProjFS may read it
        /// and frees it in lockstep with the instance.
        _notification_mappings: Vec<PRJ_NOTIFICATION_MAPPING>,
        /// Backing storage for the `NotificationRoot` strings the
        /// entries in `_notification_mappings` point at. Each
        /// `PRJ_NOTIFICATION_MAPPING::NotificationRoot` is a `PCWSTR`
        /// borrowing into one of these `HSTRING`s, so they must outlive
        /// the mappings (and therefore the instance) for the same
        /// reason. Owned here so they are dropped only after
        /// `PrjStopVirtualizing`.
        _notification_roots: Vec<HSTRING>,
    }

    /// Translate a Win32 error code into an `HRESULT` in the
    /// `FACILITY_WIN32` space, matching the `HRESULT_FROM_WIN32` C
    /// macro.
    const fn hresult_from_win32(code: u32) -> HRESULT {
        // The macro is `((HRESULT)(x) <= 0 ? ((HRESULT)(x)) : ((HRESULT)(((x) & 0x0000FFFF) | (FACILITY_WIN32 << 16) | 0x80000000)))`
        // FACILITY_WIN32 = 7. We assume `code` is a positive Win32 error,
        // which is the case for ERROR_FILE_NOT_FOUND and friends.
        #[allow(clippy::cast_possible_wrap)]
        let value = ((code & 0x0000_FFFF) | (7 << 16) | 0x8000_0000) as i32;
        HRESULT(value)
    }

    /// Convert a `PCWSTR` into an owned Rust `String`. Returns `None`
    /// when the pointer is null or the UTF-16 sequence cannot be
    /// decoded.
    #[allow(unsafe_code)]
    unsafe fn pcwstr_to_string(value: PCWSTR) -> Option<String> {
        if value.is_null() {
            return None;
        }
        // SAFETY: caller guarantees `value` is null-terminated and
        // valid for reads up to and including the terminator; ProjFS
        // honours that contract for FilePathName and related fields.
        let s = unsafe { value.to_string() }.ok()?;
        Some(s)
    }

    /// Recover the `CallbackContextInner` from the `PRJ_CALLBACK_DATA`
    /// instance context pointer. Returns `None` when the pointer is
    /// null, which only happens if the caller forgot to set it on
    /// `PrjStartVirtualizing`.
    #[allow(unsafe_code)]
    const unsafe fn context_from_callback_data<'a>(
        data: *const PRJ_CALLBACK_DATA,
    ) -> Option<&'a CallbackContextInner> {
        if data.is_null() {
            return None;
        }
        // SAFETY: ProjFS guarantees `data` points to a valid
        // PRJ_CALLBACK_DATA for the duration of the callback. The
        // InstanceContext field was set to a `Box::into_raw` value of
        // `CallbackContextInner` by `start_virtualising`; the Box is
        // alive until `stop_virtualising` runs, which only happens
        // after PrjStopVirtualizing returns and outstanding callbacks
        // have drained.
        let ctx_ptr = unsafe { (*data).InstanceContext } as *const CallbackContextInner;
        if ctx_ptr.is_null() {
            return None;
        }
        // SAFETY: ctx_ptr is non-null and points to a live
        // CallbackContextInner held by the presenter.
        Some(unsafe { &*ctx_ptr })
    }

    /// `PRJ_QUERY_FILE_NAME_CB` — existence check used by `ProjFS` to
    /// decide whether to descend into the projection.
    ///
    /// **Thread model**: `ProjFS` dispatches callbacks from kernel-owned
    /// worker threads that are NOT part of any Tokio runtime. The
    /// `blocking_read` calls below are therefore correct. Do NOT
    /// invoke these callbacks directly from inside a `#[tokio::test]`
    /// or any other async context — `RwLock::blocking_read` panics
    /// when entered from a Tokio worker. Tests that need to exercise
    /// callback logic should call the platform-independent helpers
    /// (`resolve_path`, `collect_children`, etc.) instead.
    #[allow(unsafe_code)]
    unsafe extern "system" fn query_file_name(callback_data: *const PRJ_CALLBACK_DATA) -> HRESULT {
        // SAFETY: the inner blocks all dereference pointers ProjFS
        // promised are live for the duration of this callback.
        let Some(ctx) = (unsafe { context_from_callback_data(callback_data) }) else {
            return hresult_from_win32(ERROR_FILE_NOT_FOUND.0);
        };
        let Some(path) = (unsafe { pcwstr_to_string((*callback_data).FilePathName) }) else {
            return hresult_from_win32(ERROR_FILE_NOT_FOUND.0);
        };

        let items = ctx.items.blocking_read();
        let root = ctx.root_id.blocking_read();
        if resolve_path(&path, &items, root.as_ref()).is_some() {
            S_OK
        } else {
            hresult_from_win32(ERROR_FILE_NOT_FOUND.0)
        }
    }

    /// `PRJ_GET_PLACEHOLDER_INFO_CB` — emit a placeholder describing
    /// the item at the queried path.
    #[allow(unsafe_code)]
    unsafe extern "system" fn get_placeholder_info(
        callback_data: *const PRJ_CALLBACK_DATA,
    ) -> HRESULT {
        // SAFETY: see `query_file_name`. ProjFS holds `callback_data`
        // alive for the duration of the call.
        let Some(ctx) = (unsafe { context_from_callback_data(callback_data) }) else {
            return hresult_from_win32(ERROR_FILE_NOT_FOUND.0);
        };
        let Some(path) = (unsafe { pcwstr_to_string((*callback_data).FilePathName) }) else {
            return hresult_from_win32(ERROR_FILE_NOT_FOUND.0);
        };

        let items = ctx.items.blocking_read();
        let root = ctx.root_id.blocking_read();
        let Some(item) = resolve_path(&path, &items, root.as_ref()) else {
            return hresult_from_win32(ERROR_FILE_NOT_FOUND.0);
        };

        let mut info = PRJ_PLACEHOLDER_INFO::default();
        info.FileBasicInfo.IsDirectory = item.is_dir;
        // PRJ_FILE_BASIC_INFO uses `i64` for FileSize. `u64 -> i64`
        // saturates here because file sizes that overflow `i64` cannot
        // be represented in NTFS anyway (max file size is `1 << 60`).
        #[allow(clippy::cast_possible_wrap)]
        {
            let size = item.size.unwrap_or(0).min(i64::MAX as u64);
            info.FileBasicInfo.FileSize = size as i64;
        }

        let path_hstring = HSTRING::from(path.as_str());
        // SAFETY: PrjWritePlaceholderInfo reads `destination_file_name`
        // and the placeholder info for the duration of the call; both
        // outlive the FFI on the stack. The namespace context is the
        // one ProjFS itself supplied via callback_data.
        let result = unsafe {
            PrjWritePlaceholderInfo(
                (*callback_data).NamespaceVirtualizationContext,
                PCWSTR(path_hstring.as_ptr()),
                &raw const info,
                u32::try_from(std::mem::size_of::<PRJ_PLACEHOLDER_INFO>()).unwrap_or(u32::MAX),
            )
        };
        match result {
            Ok(()) => S_OK,
            Err(err) => err.code(),
        }
    }

    /// `PRJ_START_DIRECTORY_ENUMERATION_CB` — snapshot the children
    /// of the queried directory and remember them under
    /// `enumeration_id`.
    #[allow(unsafe_code)]
    unsafe extern "system" fn start_directory_enumeration(
        callback_data: *const PRJ_CALLBACK_DATA,
        enumeration_id: *const GUID,
    ) -> HRESULT {
        // SAFETY: ProjFS holds both pointers alive for the duration
        // of the call.
        let Some(ctx) = (unsafe { context_from_callback_data(callback_data) }) else {
            return hresult_from_win32(ERROR_FILE_NOT_FOUND.0);
        };
        if enumeration_id.is_null() {
            return hresult_from_win32(ERROR_FILE_NOT_FOUND.0);
        }
        let id = unsafe { *enumeration_id };
        let key = guid_to_u128(id);

        let path = unsafe { pcwstr_to_string((*callback_data).FilePathName) }.unwrap_or_default();

        let items = ctx.items.blocking_read();
        let root = ctx.root_id.blocking_read();

        // Empty path = enumerate the projection root. With an explicit
        // root id, collect that id's direct children. Without one
        // (loose-root mode), collect every item whose parent is not a
        // key in the map — the sync runner labels top-level items
        // `<backend>:root`, never a bare `"root"`, so matching a literal
        // string would enumerate nothing against a real daemon mount.
        let state = if path.is_empty() {
            match root.as_ref() {
                Some(root_id) => build_enumeration_state(root_id, &items),
                None => EnumerationState {
                    entries: collect_root_children(&items),
                    position: 0,
                },
            }
        } else {
            let Some(dir) = resolve_path(&path, &items, root.as_ref()) else {
                return hresult_from_win32(ERROR_FILE_NOT_FOUND.0);
            };
            if !dir.is_dir {
                return hresult_from_win32(ERROR_FILE_NOT_FOUND.0);
            }
            EnumerationState {
                entries: collect_children(&dir.id, &items),
                position: 0,
            }
        };

        let Ok(mut sessions) = ctx.enumerations.lock() else {
            // Poisoned mutex represents a process-internal failure
            // that the kernel can retry on a fresh enumeration ID;
            // it is *not* a callback-implementation gap and must not
            // be confused with the browse-only sentinel emitted by
            // `get_file_data` when no `ContentProvider` is wired in.
            // `ERROR_INTERNAL_ERROR` (Win32 1359, `HRESULT`
            // `0x8007_054F`) is the documented Win32 code for "an
            // internal error occurred" and `ProjFS` callers surface
            // it as a transient failure.
            return hresult_from_win32(ERROR_INTERNAL_ERROR.0);
        };
        sessions.insert(key, state);
        S_OK
    }

    /// `PRJ_END_DIRECTORY_ENUMERATION_CB` — release the stored
    /// session state.
    #[allow(unsafe_code)]
    unsafe extern "system" fn end_directory_enumeration(
        callback_data: *const PRJ_CALLBACK_DATA,
        enumeration_id: *const GUID,
    ) -> HRESULT {
        // SAFETY: ProjFS guarantees the pointers are live for the
        // duration of the call.
        let Some(ctx) = (unsafe { context_from_callback_data(callback_data) }) else {
            return S_OK;
        };
        if enumeration_id.is_null() {
            return S_OK;
        }
        let key = guid_to_u128(unsafe { *enumeration_id });
        if let Ok(mut sessions) = ctx.enumerations.lock() {
            sessions.remove(&key);
        }
        S_OK
    }

    /// `PRJ_GET_DIRECTORY_ENUMERATION_CB` — yield the next batch of
    /// entries into the buffer `ProjFS` provides.
    ///
    /// Filtering by `search_expression` is performed by
    /// `PrjFileNameMatch` server-side per the API contract; we
    /// currently emit every child and let `ProjFS` drop the ones that
    /// do not match. A future optimisation can short-circuit by
    /// calling `PrjFileNameMatch` from here.
    #[allow(unsafe_code)]
    unsafe extern "system" fn get_directory_enumeration(
        callback_data: *const PRJ_CALLBACK_DATA,
        enumeration_id: *const GUID,
        _search_expression: PCWSTR,
        dir_entry_buffer_handle: PRJ_DIR_ENTRY_BUFFER_HANDLE,
    ) -> HRESULT {
        // SAFETY: ProjFS guarantees the pointers are live for the
        // duration of the call.
        let Some(ctx) = (unsafe { context_from_callback_data(callback_data) }) else {
            return S_OK;
        };
        if enumeration_id.is_null() {
            return S_OK;
        }
        let key = guid_to_u128(unsafe { *enumeration_id });

        // If ProjFS asked for a restart, reset the cursor before
        // filling the buffer. The flag is a bit in `Flags`, not the
        // full value, so use bitwise-and rather than equality.
        let restart_scan =
            unsafe { (*callback_data).Flags.0 & PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN.0 != 0 };

        let Ok(mut sessions) = ctx.enumerations.lock() else {
            return S_OK;
        };
        let Some(state) = sessions.get_mut(&key) else {
            // ProjFS would not normally call Get without Start, but
            // returning S_OK with no entries written tells it the
            // directory is empty rather than crashing.
            return S_OK;
        };
        if restart_scan {
            state.position = 0;
        }

        let buffer_full_hresult = hresult_from_win32(ERROR_INSUFFICIENT_BUFFER.0);
        while let Some(entry) = state.entries.get(state.position) {
            let name_hstring = HSTRING::from(entry.name.as_str());
            #[allow(clippy::cast_possible_wrap)]
            let basic = PRJ_FILE_BASIC_INFO {
                IsDirectory: entry.is_dir,
                FileSize: entry.size.min(i64::MAX as u64) as i64,
                ..Default::default()
            };

            // SAFETY: PrjFillDirEntryBuffer reads `filename` and
            // `filebasicinfo` for the duration of the call. Both
            // outlive the FFI; the buffer handle is the one ProjFS
            // handed us.
            let result = unsafe {
                PrjFillDirEntryBuffer(
                    PCWSTR(name_hstring.as_ptr()),
                    Some(&raw const basic),
                    dir_entry_buffer_handle,
                )
            };
            match result {
                Ok(()) => {
                    state.position += 1;
                }
                Err(err) if err.code() == buffer_full_hresult => {
                    // Buffer full — leave position unchanged; ProjFS
                    // will call us again with a fresh buffer.
                    return S_OK;
                }
                Err(err) => return err.code(),
            }
        }
        S_OK
    }

    /// `PRJ_GET_FILE_DATA_CB` — stream the requested byte range of a
    /// virtualised file back to `ProjFS`.
    ///
    /// Flow:
    /// 1. Recover the [`CallbackContextInner`] and bail with
    ///    `ERROR_CALL_NOT_IMPLEMENTED` if no [`ContentProvider`] is
    ///    installed (browse-only mode).
    /// 2. Register a [`CancellationToken`] keyed by the
    ///    `PRJ_CALLBACK_DATA::CommandId` so the matching
    ///    `CancelCommand` invocation can abort us mid-flight.
    /// 3. Resolve the relative path to a [`VfsItem`] via
    ///    [`resolve_path`]; a directory or unknown path returns
    ///    `ERROR_FILE_NOT_FOUND`.
    /// 4. Ask the provider for `[byte_offset, byte_offset + length)`.
    /// 5. Allocate an aligned buffer via `PrjAllocateAlignedBuffer`,
    ///    copy the bytes in, hand it to `PrjWriteFileData`, then free
    ///    the buffer regardless of the outcome.
    /// 6. Remove the cancellation token from the map on every exit
    ///    path via [`TokenGuard`].
    ///
    /// I/O errors from the provider are mapped to a kind-aware Win32
    /// `HRESULT` via [`super::hresult_for_io_error`] — `NotFound` →
    /// `ERROR_FILE_NOT_FOUND`, `PermissionDenied` →
    /// `ERROR_ACCESS_DENIED`, `Interrupted` → `ERROR_OPERATION_ABORTED`,
    /// `TimedOut` → `ERROR_TIMEOUT`, and so on. Every other
    /// `io::ErrorKind` falls through to `ERROR_GEN_FAILURE` rather
    /// than the historic `ERROR_CALL_NOT_IMPLEMENTED`. Cancellation
    /// returns `ERROR_OPERATION_ABORTED`.
    #[allow(unsafe_code)]
    unsafe extern "system" fn get_file_data(
        callback_data: *const PRJ_CALLBACK_DATA,
        byte_offset: u64,
        length: u32,
    ) -> HRESULT {
        // SAFETY: ProjFS holds `callback_data` alive for the duration
        // of the call. See `query_file_name` for the broader contract.
        let Some(ctx) = (unsafe { context_from_callback_data(callback_data) }) else {
            return hresult_from_win32(ERROR_FILE_NOT_FOUND.0);
        };
        let Some(provider) = ctx.content_provider.as_ref() else {
            // No backend wired in; stay browse-only.
            //
            // `ERROR_CALL_NOT_IMPLEMENTED` is the documented contract
            // with `ProjFS`: it tells the kernel the provider has
            // intentionally not implemented this callback so the
            // projection should drop into browse-only mode. This is
            // *not* an internal failure — it is a deliberate signal
            // distinct from the kind-aware mappings on read failures.
            // Use the typed `HResultCode` constructor here so every
            // exit point in `get_file_data` flows through the same
            // packing helper.
            return HRESULT(super::HResultCode::from_win32(ERROR_CALL_NOT_IMPLEMENTED.0).get());
        };
        let Some(path) = (unsafe { pcwstr_to_string((*callback_data).FilePathName) }) else {
            return hresult_from_win32(ERROR_FILE_NOT_FOUND.0);
        };

        // SAFETY: see context recovery above. ProjFS keeps the struct
        // alive across the entire call.
        let namespace_ctx = unsafe { (*callback_data).NamespaceVirtualizationContext };
        let data_stream_id = unsafe { (*callback_data).DataStreamId };
        let command_id = unsafe { (*callback_data).CommandId };

        // Register a cancellation token before the first expensive
        // step. `TokenGuard` removes the entry on drop regardless of
        // which return path we take.
        let token = CancellationToken::new();
        if let Ok(mut tokens) = ctx.cancellation_tokens.lock() {
            tokens.insert(command_id, token.clone());
        }
        let _guard = TokenGuard {
            tokens: &ctx.cancellation_tokens,
            command_id,
        };

        if token.is_cancelled() {
            return hresult_from_win32(ERROR_OPERATION_ABORTED.0);
        }

        // Resolve the item under the read lock and drop the guards
        // before invoking the provider so a slow read does not block
        // browse callbacks. The provider takes an `ItemId`, so we
        // clone it out and release the lock first.
        let item_id = {
            let items = ctx.items.blocking_read();
            let root = ctx.root_id.blocking_read();
            let Some(item) = resolve_path(&path, &items, root.as_ref()) else {
                return hresult_from_win32(ERROR_FILE_NOT_FOUND.0);
            };
            if item.is_dir {
                return hresult_from_win32(ERROR_FILE_NOT_FOUND.0);
            }
            item.id.clone()
        };

        if token.is_cancelled() {
            return hresult_from_win32(ERROR_OPERATION_ABORTED.0);
        }

        // A panic must never unwind across this `extern "system"`
        // boundary — that is undefined behaviour. `read_range` returns
        // `io::Result<Vec<u8>>` (both `UnwindSafe`); the provider itself
        // is behind `&dyn ContentProvider`, so wrap the call in
        // `AssertUnwindSafe` and map any caught panic to a visible Win32
        // failure (`ERROR_GEN_FAILURE`) rather than aborting the host.
        let read_result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            provider.read_range(&item_id, byte_offset, length)
        })) {
            Ok(result) => result,
            Err(_) => {
                tracing::error!(
                    id = %item_id,
                    offset = byte_offset,
                    length,
                    "ContentProvider::read_range panicked; mapping to ERROR_GEN_FAILURE"
                );
                return HRESULT(
                    super::HResultCode::from_win32(windows::Win32::Foundation::ERROR_GEN_FAILURE.0)
                        .get(),
                );
            }
        };
        if let Err(ref err) = read_result {
            tracing::warn!(
                id = %item_id,
                offset = byte_offset,
                length,
                kind = ?err.kind(),
                error = %err,
                "ContentProvider::read_range failed; mapping io::ErrorKind to HRESULT"
            );
        }
        // Classifying the read result here keeps the EOF and failure
        // branches independently testable cross-platform — see
        // `classify_read` and the `classify_read_*` unit tests. The
        // `Failed` arm carries a typed `HResultCode` already in the
        // `FACILITY_WIN32` space, so we forward it directly without
        // re-deriving via `hresult_from_win32`.
        let bytes = match super::classify_read(read_result) {
            super::ProviderReadOutcome::Bytes(bytes) => bytes,
            super::ProviderReadOutcome::Eof => {
                // End of file — ProjFS treats S_OK with zero bytes as
                // a legitimate short read.
                return S_OK;
            }
            super::ProviderReadOutcome::Failed(code) => {
                return HRESULT(code.get());
            }
        };

        if token.is_cancelled() {
            return hresult_from_win32(ERROR_OPERATION_ABORTED.0);
        }
        // Cap to u32 so the ProjFS length argument fits. ProjFS reads
        // are bounded by what the kernel asked us for (`length` is
        // u32), so in practice `bytes.len() <= length` here, but we
        // still guard against a misbehaving provider returning more.
        let cap = usize::try_from(u32::MAX).unwrap_or(usize::MAX);
        let usable_usize = bytes.len().min(cap);
        let usable = u32::try_from(usable_usize).unwrap_or(u32::MAX);

        // Allocate an aligned buffer. `ProjFS` requires unbuffered I/O
        // alignment; `PrjAllocateAlignedBuffer` is the documented way
        // to get a buffer that satisfies it for the active namespace.
        //
        // SAFETY: `namespace_ctx` is the live namespace handle `ProjFS`
        // gave us. `PrjAllocateAlignedBuffer` returns null on failure;
        // we check before dereferencing. A null return means the
        // allocator could not service the request — typically the
        // paged pool is exhausted or the requested size is unsatisfiable
        // — so we surface `ERROR_NOT_ENOUGH_MEMORY` (Win32 8) rather
        // than the historic `ERROR_CALL_NOT_IMPLEMENTED`. The former
        // is the documented Win32 code for an allocation failure and
        // lets `ProjFS` and any logging layer distinguish a transient
        // resource exhaustion from a "callback not wired up" signal.
        let buffer = unsafe { PrjAllocateAlignedBuffer(namespace_ctx, usable_usize) };
        if buffer.is_null() {
            return hresult_from_win32(ERROR_NOT_ENOUGH_MEMORY.0);
        }

        // SAFETY: `buffer` points to at least `usable_usize` writable
        // bytes (we just asked PrjAllocateAlignedBuffer for that many).
        // `bytes` is at least `usable_usize` bytes long because
        // `usable_usize <= bytes.len()`. The two regions cannot overlap
        // because the aligned buffer was just freshly allocated and is
        // distinct from `bytes`.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.cast::<u8>(), usable_usize);
        }

        // SAFETY: `data_stream_id` is the GUID ProjFS gave us;
        // `PrjWriteFileData` reads it for the duration of the call.
        // The buffer is alive until `PrjFreeAlignedBuffer` below runs,
        // which is after PrjWriteFileData has returned.
        let result = unsafe {
            PrjWriteFileData(
                namespace_ctx,
                &raw const data_stream_id,
                buffer.cast_const(),
                byte_offset,
                usable,
            )
        };

        // SAFETY: `buffer` came from PrjAllocateAlignedBuffer above and
        // has not been freed yet. PrjFreeAlignedBuffer is the matching
        // deallocator. We free here regardless of the PrjWriteFileData
        // outcome — the buffer is no longer needed and ProjFS has
        // copied the bytes it cared about by the time the call returns.
        unsafe {
            PrjFreeAlignedBuffer(buffer.cast_const());
        }

        match result {
            Ok(()) => S_OK,
            Err(err) => err.code(),
        }
    }

    /// `PRJ_NOTIFICATION_CB` — log user-driven filesystem events and
    /// veto deletes the presenter does not allow.
    ///
    /// The full set of `PRJ_NOTIFICATION_*` flags is mapped via
    /// [`super::NotificationEvent::from_i32`]; unrecognised codes are
    /// logged at `warn` and produce `S_OK` so `ProjFS` does not retry.
    /// For `PRE_DELETE` the presenter consults [`super::allow_delete`]
    /// (currently always-allow) and returns `ERROR_ACCESS_DENIED`
    /// when the policy says no.
    ///
    /// The `destination_file_name` PCWSTR is only meaningful for
    /// rename and hardlink notifications. We decode it for those
    /// variants and ignore it elsewhere.
    #[allow(unsafe_code)]
    unsafe extern "system" fn notification(
        callback_data: *const PRJ_CALLBACK_DATA,
        is_directory: bool,
        notification_kind: PRJ_NOTIFICATION,
        destination_file_name: PCWSTR,
        _operation_parameters: *mut PRJ_NOTIFICATION_PARAMETERS,
    ) -> HRESULT {
        // SAFETY: ProjFS holds `callback_data` alive for the duration
        // of the call.
        let Some(ctx) = (unsafe { context_from_callback_data(callback_data) }) else {
            return S_OK;
        };
        let path = unsafe { pcwstr_to_string((*callback_data).FilePathName) }.unwrap_or_default();

        // Only decode the destination for the variants where ProjFS
        // documents a non-null pointer. For everything else the
        // pointer is undefined and decoding would be incorrect.
        let raw = notification_kind.0;
        let destination = if matches!(raw, 32 | 64 | 128 | 256) {
            // SAFETY: ProjFS documents `destination_file_name` as a
            // valid null-terminated wide string for these notification
            // codes. pcwstr_to_string handles null/decode failure.
            unsafe { pcwstr_to_string(destination_file_name) }
        } else {
            None
        };

        let Some(event) = super::NotificationEvent::from_i32(raw, destination) else {
            tracing::warn!(
                path = %path,
                is_directory,
                notification_code = raw,
                "ProjFS notification with unrecognised code; treating as no-op"
            );
            return S_OK;
        };

        tracing::debug!(
            path = %path,
            is_directory,
            event = event.tag(),
            "ProjFS notification"
        );

        if matches!(event, super::NotificationEvent::PreDelete) {
            let items = ctx.items.blocking_read();
            if !super::allow_delete(&path, is_directory, &items) {
                tracing::info!(
                    path = %path,
                    is_directory,
                    "ProjFS PRE_DELETE vetoed by allow_delete policy"
                );
                return hresult_from_win32(windows::Win32::Foundation::ERROR_ACCESS_DENIED.0);
            }
        }

        S_OK
    }

    /// RAII guard — removes the cancellation token entry from the
    /// shared map when the holder is dropped. `get_file_data` holds
    /// one for the duration of its work so the map cannot grow
    /// unboundedly across calls.
    struct TokenGuard<'a> {
        tokens: &'a Arc<Mutex<HashMap<i32, CancellationToken>>>,
        command_id: i32,
    }

    impl Drop for TokenGuard<'_> {
        fn drop(&mut self) {
            if let Ok(mut tokens) = self.tokens.lock() {
                tokens.remove(&self.command_id);
            }
        }
    }

    /// `PRJ_CANCEL_COMMAND_CB` — signal the in-flight callback for
    /// `CommandId` to abort. Looks up the cancellation token in the
    /// shared map and triggers it; the running callback sees the flag
    /// at its next checkpoint and returns `ERROR_OPERATION_ABORTED`.
    ///
    /// `ProjFS` may invoke this concurrently with the callback the
    /// token belongs to, so the lookup is read-only and idempotent.
    #[allow(unsafe_code)]
    unsafe extern "system" fn cancel_command(callback_data: *const PRJ_CALLBACK_DATA) {
        // SAFETY: ProjFS keeps callback_data alive for the duration of
        // this call.
        let Some(ctx) = (unsafe { context_from_callback_data(callback_data) }) else {
            return;
        };
        let command_id = unsafe { (*callback_data).CommandId };
        let token = ctx
            .cancellation_tokens
            .lock()
            .ok()
            .and_then(|tokens| tokens.get(&command_id).cloned());
        if let Some(token) = token {
            tracing::debug!(command_id, "ProjFS CancelCommand received");
            token.cancel();
        }
    }

    /// Build the live callback table used by `PrjStartVirtualizing`.
    fn build_callbacks() -> PRJ_CALLBACKS {
        PRJ_CALLBACKS {
            StartDirectoryEnumerationCallback: Some(start_directory_enumeration),
            EndDirectoryEnumerationCallback: Some(end_directory_enumeration),
            GetDirectoryEnumerationCallback: Some(get_directory_enumeration),
            GetPlaceholderInfoCallback: Some(get_placeholder_info),
            GetFileDataCallback: Some(get_file_data),
            QueryFileNameCallback: Some(query_file_name),
            NotificationCallback: Some(notification),
            CancelCommandCallback: Some(cancel_command),
        }
    }

    /// Convert a `GUID` to its little-endian `u128` representation
    /// for use as a `HashMap` key. `ProjFS` treats enumeration IDs as
    /// opaque, so any total order works as long as it is stable for
    /// the lifetime of the enumeration.
    fn guid_to_u128(guid: GUID) -> u128 {
        let mut bytes = [0u8; 16];
        bytes[..4].copy_from_slice(&guid.data1.to_le_bytes());
        bytes[4..6].copy_from_slice(&guid.data2.to_le_bytes());
        bytes[6..8].copy_from_slice(&guid.data3.to_le_bytes());
        bytes[8..].copy_from_slice(&guid.data4);
        u128::from_le_bytes(bytes)
    }

    /// Mark the mount directory as a `ProjFS` placeholder and start
    /// virtualising. Stores the resulting namespace handle in the
    /// presenter so [`stop_virtualising`] can release it later.
    pub async fn start_virtualising(
        mount_point: &Path,
        handle_slot: &Arc<tokio::sync::Mutex<Option<NamespaceHandle>>>,
        callback_ctx_slot: &Arc<tokio::sync::Mutex<Option<CallbackContext>>>,
        items: Arc<RwLock<HashMap<String, VfsItem>>>,
        root_id: Arc<RwLock<Option<ItemId>>>,
        enumerations: Arc<Mutex<HashMap<u128, EnumerationState>>>,
        content_provider: Option<Arc<dyn ContentProvider>>,
        cancellation_tokens: Arc<Mutex<HashMap<i32, CancellationToken>>>,
    ) -> Result<()> {
        // Acquire both presenter-side locks before any FFI work. The
        // raw pointers and ProjFS namespace handle produced below are
        // `!Send`, so they must not be held across an `.await`. By
        // taking both locks up front, the entire FFI region — Box
        // allocation, PrjMarkDirectoryAsPlaceholder, PrjStartVirtualizing,
        // and slot assignment — runs without crossing any await.
        let mut handle_slot_guard = handle_slot.lock().await;
        let mut ctx_slot_guard = callback_ctx_slot.lock().await;

        let mount_hstring = HSTRING::from(mount_point.as_os_str());
        let mount_pcwstr = PCWSTR(mount_hstring.as_ptr());

        // SAFETY: PrjMarkDirectoryAsPlaceholder reads the path string
        // for the duration of the call. `mount_hstring` outlives the
        // call because it is held on the stack until after the FFI
        // returns. The version_info and virtualisation_instance_id
        // arguments are optional; we pass `None`/null for both since
        // we do not yet track provider version metadata.
        #[allow(unsafe_code)]
        let mark_result = unsafe {
            PrjMarkDirectoryAsPlaceholder(
                mount_pcwstr,
                PCWSTR::null(),
                None,
                std::ptr::null::<GUID>(),
            )
        };
        mark_result.context("PrjMarkDirectoryAsPlaceholder failed")?;

        // Register a notification mapping so the kernel delivers the
        // events the presenter actually acts on. Without an explicit
        // mapping ProjFS uses a default set that excludes
        // `PRJ_NOTIFY_PRE_DELETE`, which would leave `allow_delete`
        // (the delete-veto policy hook) dead code — the callback would
        // never see a pre-delete to veto. The mapping is rooted at the
        // virtualisation root (an empty relative path) so it applies to
        // every projected file.
        //
        // The mask combines the pre-delete veto event with the standard
        // post-hoc events (create / overwrite / rename / handle-close
        // modified / handle-close deleted) so the notification callback
        // observes the full user-driven lifecycle, matching the variants
        // decoded by `NotificationEvent::from_i32`.
        let notification_mask: PRJ_NOTIFY_TYPES = PRJ_NOTIFY_PRE_DELETE
            | PRJ_NOTIFY_PRE_RENAME
            | PRJ_NOTIFY_NEW_FILE_CREATED
            | PRJ_NOTIFY_FILE_OVERWRITTEN
            | PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_MODIFIED
            | PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_DELETED;
        // The root is the empty relative path: the virtualisation root
        // itself. `PCWSTR::null()` is not valid for a notification root,
        // so use an explicit empty wide string. The `HSTRING` is owned
        // by the boxed context (`_notification_roots`) so it lives for
        // the instance lifetime — ProjFS keeps the `NotificationRoot`
        // pointer for as long as the instance runs, not just across the
        // start call.
        let notification_roots = vec![HSTRING::from("")];
        // Build the mapping array referencing the owned root string.
        // ProjFS retains this array's pointer for the instance lifetime,
        // so it must live on the boxed context too (`_notification_mappings`).
        let notification_mappings: Vec<PRJ_NOTIFICATION_MAPPING> = notification_roots
            .iter()
            .map(|root| PRJ_NOTIFICATION_MAPPING {
                NotificationBitMask: notification_mask,
                NotificationRoot: PCWSTR(root.as_ptr()),
            })
            .collect();

        // Build the callback context. The Box is the owning handle:
        // we hand its raw pointer to ProjFS via instance_context and
        // recover it in `stop_virtualising` to free the allocation. The
        // notification mappings and their backing root strings are owned
        // here so they outlive every callback and are freed in lockstep
        // with the instance.
        let inner = Box::new(CallbackContextInner {
            items: Arc::clone(&items),
            root_id: Arc::clone(&root_id),
            enumerations: Arc::clone(&enumerations),
            content_provider,
            cancellation_tokens,
            _notification_mappings: notification_mappings,
            _notification_roots: notification_roots,
        });
        let inner_ptr = Box::into_raw(inner);
        let instance_context = inner_ptr.cast::<c_void>();

        let callbacks = build_callbacks();

        // Point the options at the boxed Vec's storage. SAFETY:
        // `inner_ptr` is the live Box allocation from `Box::into_raw`
        // above; we have not freed it and no other reference exists, so
        // reading `_notification_mappings` through the raw pointer is a
        // sound shared borrow. The Vec's `as_ptr()` is stable for the
        // life of the Box (Vec never reallocates while untouched), which
        // is exactly the instance lifetime ProjFS requires.
        #[allow(unsafe_code)]
        let (mappings_ptr, mappings_len) = unsafe {
            let mappings = &(*inner_ptr)._notification_mappings;
            (mappings.as_ptr(), mappings.len())
        };
        let options = PRJ_STARTVIRTUALIZING_OPTIONS {
            NotificationMappings: mappings_ptr.cast_mut(),
            NotificationMappingsCount: u32::try_from(mappings_len).unwrap_or(0),
            ..PRJ_STARTVIRTUALIZING_OPTIONS::default()
        };

        // SAFETY: PrjStartVirtualizing copies the callback function
        // pointers out of `callbacks`. The stack-local table is valid
        // for the duration of the call. The instance_context pointer is
        // the raw Box we just allocated; the kernel keeps it until
        // PrjStopVirtualizing returns. `options` lives on this stack
        // frame, which does not return (no await) until
        // PrjStartVirtualizing has read it. The `NotificationMappings`
        // array and the `NotificationRoot` strings it borrows are owned
        // by the boxed context (`_notification_mappings` /
        // `_notification_roots`), which outlives the instance and is
        // dropped only by `stop_virtualising` after PrjStopVirtualizing
        // drains — matching ProjFS's instance-lifetime borrow of these
        // pointers.
        #[allow(unsafe_code)]
        let start_result = unsafe {
            PrjStartVirtualizing(
                mount_pcwstr,
                &raw const callbacks,
                Some(instance_context.cast_const()),
                Some(&raw const options),
            )
        };
        let ctx = match start_result {
            Ok(ctx) => ctx,
            Err(err) => {
                // PrjStartVirtualizing failed — reclaim the Box so it
                // is not leaked. SAFETY: inner_ptr originated from
                // Box::into_raw above and has not been freed yet.
                #[allow(unsafe_code)]
                unsafe {
                    drop(Box::from_raw(inner_ptr));
                }
                return Err(anyhow::Error::from(err).context("PrjStartVirtualizing failed"));
            }
        };

        *handle_slot_guard = Some(NamespaceHandle(ctx));
        *ctx_slot_guard = Some(CallbackContext {
            raw_ptr: inner_ptr as usize,
        });
        Ok(())
    }

    /// Release the stored `ProjFS` namespace handle, if any.
    pub async fn stop_virtualising(
        handle_slot: &Arc<tokio::sync::Mutex<Option<NamespaceHandle>>>,
        callback_ctx_slot: &Arc<tokio::sync::Mutex<Option<CallbackContext>>>,
    ) -> Result<()> {
        let handle = {
            let mut slot = handle_slot.lock().await;
            slot.take()
        };
        if let Some(NamespaceHandle(ctx)) = handle {
            // SAFETY: `ctx` was produced by PrjStartVirtualizing in
            // start_virtualising and has not yet been released. ProjFS
            // documents PrjStopVirtualizing as callable from any thread
            // once outstanding callbacks have drained.
            #[allow(unsafe_code)]
            unsafe {
                PrjStopVirtualizing(ctx);
            }
            tracing::info!("ProjFS virtualisation stopped");
        }

        // Reclaim the callback context allocation after ProjFS has
        // stopped — any in-flight callback will have returned before
        // PrjStopVirtualizing returned.
        let ctx = {
            let mut slot = callback_ctx_slot.lock().await;
            slot.take()
        };
        if let Some(CallbackContext { raw_ptr }) = ctx
            && raw_ptr != 0
        {
            // SAFETY: raw_ptr originated from Box::into_raw in
            // start_virtualising. ProjFS has stopped and no further
            // callbacks can dereference it, so it is safe to drop.
            #[allow(unsafe_code)]
            unsafe {
                drop(Box::from_raw(raw_ptr as *mut CallbackContextInner));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Upserting an item exposes it via `items()`, and deleting it
    /// removes it. Platform-independent because the underlying
    /// `HashMap` is the same on every target.
    #[tokio::test]
    async fn upsert_and_delete_round_trip() {
        let presenter = ProjFsPresenter::new(PathBuf::from("/tmp/cascade-projfs-test"));
        let id = ItemId::new("backend", "file");
        let item = VfsItem {
            id: id.clone(),
            parent_id: ItemId::new("backend", "root"),
            name: "test.txt".to_string(),
            is_dir: false,
            size: Some(0),
            mod_time: None,
            cache_state: CacheState::Online,
            mime_type: None,
        };

        presenter.upsert_item(item).await.unwrap();
        {
            let items = presenter.items().read().await;
            assert!(items.contains_key(&id.0));
        }

        presenter.delete_item(&id).await.unwrap();
        {
            let items = presenter.items().read().await;
            assert!(!items.contains_key(&id.0));
        }
    }

    /// The builder API mirrors the other presenters: `new` sets a
    /// default mount point and `with_mount_point` overrides it.
    #[test]
    fn mount_point_round_trips() {
        let presenter = ProjFsPresenter::new(PathBuf::from("C:/cascade"));
        assert_eq!(presenter.mount_point(), Path::new("C:/cascade"));

        let moved = presenter.with_mount_point("D:/cascade");
        assert_eq!(moved.mount_point(), Path::new("D:/cascade"));
    }

    /// On non-Windows platforms `start` must fail loudly so the CLI
    /// dispatch can move on to the next fallback presenter.
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn start_fails_on_non_windows() {
        let presenter = ProjFsPresenter::new(PathBuf::from("/tmp/cascade-projfs-test"));
        let err = presenter
            .start(Path::new("/tmp/cascade-projfs-test"))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("only supported on Windows"),
            "expected Windows-only error, got: {msg}"
        );
    }

    /// On non-Windows platforms `stop` is a no-op so the CLI shutdown
    /// path can call it unconditionally.
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn stop_is_noop_on_non_windows() {
        let presenter = ProjFsPresenter::new(PathBuf::from("/tmp/cascade-projfs-test"));
        presenter.stop().await.unwrap();
    }

    /// `fetch_contents` is documented as unimplemented — the real work
    /// will live in the `GetFileData` callback.
    #[tokio::test]
    async fn fetch_contents_returns_unimplemented_error() {
        let presenter = ProjFsPresenter::new(PathBuf::from("/tmp/cascade-projfs-test"));
        let id = ItemId::new("backend", "file");
        let err = presenter.fetch_contents(&id).await.unwrap_err();
        assert!(err.to_string().contains("not yet implemented"));
    }

    /// `update_state` and `evict_item` are intentional no-ops on
    /// `ProjFS`; verify both succeed for an arbitrary id without
    /// touching disk or the OS.
    #[tokio::test]
    async fn update_state_and_evict_are_ok() {
        let presenter = ProjFsPresenter::new(PathBuf::from("/tmp/cascade-projfs-test"));
        let id = ItemId::new("backend", "file");
        presenter
            .update_state(&id, CacheState::Cached)
            .await
            .unwrap();
        presenter.evict_item(&id).await.unwrap();
    }

    /// Helper used by the path-resolution tests to build a small tree:
    ///
    /// ```text
    /// root (backend:root)
    /// └── dir1 (backend:dir1)
    ///     └── file.txt (backend:file)
    /// ```
    fn three_node_tree() -> (HashMap<String, VfsItem>, ItemId) {
        let root = ItemId::new("backend", "root");
        let dir_id = ItemId::new("backend", "dir1");
        let file_id = ItemId::new("backend", "file");

        let dir = VfsItem {
            id: dir_id.clone(),
            parent_id: root.clone(),
            name: "dir1".to_string(),
            is_dir: true,
            size: None,
            mod_time: None,
            cache_state: CacheState::Online,
            mime_type: None,
        };
        let file = VfsItem {
            id: file_id,
            parent_id: dir_id,
            name: "file.txt".to_string(),
            is_dir: false,
            size: Some(42),
            mod_time: None,
            cache_state: CacheState::Online,
            mime_type: None,
        };

        let mut items = HashMap::new();
        items.insert(dir.id.0.clone(), dir);
        items.insert(file.id.0.clone(), file);
        (items, root)
    }

    /// Walking from a nested item back through the parent chain
    /// reproduces the full path as `[grandparent, parent, leaf]`.
    #[test]
    fn resolve_path_walks_parent_chain() {
        let (items, root) = three_node_tree();
        let file = items.values().find(|i| i.name == "file.txt").unwrap();

        let chain = ancestor_chain(file, &items);
        assert_eq!(chain, vec!["dir1".to_string(), "file.txt".to_string()]);

        let hit = resolve_path("dir1/file.txt", &items, Some(&root)).unwrap();
        assert_eq!(hit.name, "file.txt");

        // Backslashes are normalised to slashes.
        let hit = resolve_path("dir1\\file.txt", &items, Some(&root)).unwrap();
        assert_eq!(hit.name, "file.txt");

        // Case-insensitive on the resolution side.
        let hit = resolve_path("DIR1/File.TXT", &items, Some(&root)).unwrap();
        assert_eq!(hit.name, "file.txt");
    }

    /// Looking up a path that has no matching item yields `None`.
    #[test]
    fn resolve_path_unknown_returns_none() {
        let (items, root) = three_node_tree();
        assert!(resolve_path("does/not/exist.txt", &items, Some(&root)).is_none());
        assert!(resolve_path("dir1/other.txt", &items, Some(&root)).is_none());
    }

    /// Children of a directory are filtered to those Windows can
    /// safely surface — names containing forbidden characters are
    /// dropped.
    #[test]
    fn enumeration_children_filters_unsafe_filenames() {
        let parent_id = ItemId::new("backend", "dir1");
        let good = VfsItem {
            id: ItemId::new("backend", "good"),
            parent_id: parent_id.clone(),
            name: "good.txt".to_string(),
            is_dir: false,
            size: Some(10),
            mod_time: None,
            cache_state: CacheState::Online,
            mime_type: None,
        };
        let bad = VfsItem {
            id: ItemId::new("backend", "bad"),
            parent_id: parent_id.clone(),
            name: "bad?.txt".to_string(),
            is_dir: false,
            size: Some(20),
            mod_time: None,
            cache_state: CacheState::Online,
            mime_type: None,
        };

        let mut items = HashMap::new();
        items.insert(good.id.0.clone(), good);
        items.insert(bad.id.0.clone(), bad);

        let children = collect_children(&parent_id, &items);
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].name, "good.txt");
        assert_eq!(children[0].size, 10);
        assert!(!children[0].is_dir);
    }

    /// `collect_children` returns entries sorted case-insensitively
    /// so directory listings are deterministic across runs.
    #[test]
    fn enumeration_children_sorted_case_insensitive() {
        let parent_id = ItemId::new("backend", "dir1");
        let names = ["banana.txt", "Apple.txt", "cherry.txt"];
        let mut items = HashMap::new();
        for (idx, name) in names.iter().enumerate() {
            let id = ItemId::new("backend", &format!("entry-{idx}"));
            let item = VfsItem {
                id: id.clone(),
                parent_id: parent_id.clone(),
                name: (*name).to_string(),
                is_dir: false,
                size: Some(0),
                mod_time: None,
                cache_state: CacheState::Online,
                mime_type: None,
            };
            items.insert(id.0, item);
        }

        let children = collect_children(&parent_id, &items);
        let sorted_names: Vec<_> = children.iter().map(|e| e.name.clone()).collect();
        assert_eq!(
            sorted_names,
            vec![
                "Apple.txt".to_string(),
                "banana.txt".to_string(),
                "cherry.txt".to_string(),
            ]
        );
    }

    /// `is_safe_windows_filename` is the predicate behind the
    /// enumeration filter — it must reject every character the
    /// Windows kernel rejects and accept normal printable names.
    #[test]
    fn windows_filename_predicate_matches_kernel_rules() {
        assert!(is_safe_windows_filename("ordinary.txt"));
        assert!(is_safe_windows_filename("dir-name"));
        assert!(is_safe_windows_filename("with spaces.md"));
        assert!(!is_safe_windows_filename(""));
        for forbidden in [
            "bad?.txt", "a/b", "a\\b", "a:b", "a*b", "a\"b", "a<b", "a>b", "a|b",
        ] {
            assert!(
                !is_safe_windows_filename(forbidden),
                "expected {forbidden} to be rejected"
            );
        }
    }

    /// `build_enumeration_state` snapshots the children at position 0
    /// and tracks the cursor so consecutive batches resume cleanly.
    #[test]
    fn build_enumeration_state_snapshots_children_with_zero_cursor() {
        let parent_id = ItemId::new("backend", "dir1");
        let mut items = HashMap::new();
        for name in ["a.txt", "b.txt"] {
            let id = ItemId::new("backend", name);
            items.insert(
                id.0.clone(),
                VfsItem {
                    id,
                    parent_id: parent_id.clone(),
                    name: name.to_string(),
                    is_dir: false,
                    size: Some(1),
                    mod_time: None,
                    cache_state: CacheState::Online,
                    mime_type: None,
                },
            );
        }

        let state = build_enumeration_state(&parent_id, &items);
        assert_eq!(state.position, 0);
        assert_eq!(state.entries.len(), 2);
        assert_eq!(state.entries[0].name, "a.txt");
        assert_eq!(state.entries[1].name, "b.txt");
    }

    /// Loose-root enumeration (no explicit root configured) returns the
    /// items the sync runner actually produces: top-level entries are
    /// parented at `<backend>:root`, which is *not* a key in the map, so
    /// the "parent-not-a-key = root" rule must pick them up. A
    /// hard-coded `"root"` match would have returned nothing here.
    #[test]
    fn collect_root_children_matches_sync_runner_parent_ids() {
        let mut items = HashMap::new();
        // Two top-level items parented at the sync-runner's synthetic
        // backend root id. That id is never inserted as an item, so it
        // is not a key in the map.
        let synthetic_root = ItemId(String::from("gdrive:root"));
        for name in ["Documents", "Photos"] {
            let id = ItemId::new("gdrive", name);
            items.insert(
                id.0.clone(),
                VfsItem {
                    id,
                    parent_id: synthetic_root.clone(),
                    name: name.to_string(),
                    is_dir: true,
                    size: None,
                    mod_time: None,
                    cache_state: CacheState::Online,
                    mime_type: None,
                },
            );
        }
        // A nested child whose parent *is* a key in the map — it must
        // not appear at the root.
        let documents_id = ItemId::new("gdrive", "Documents");
        let nested_id = ItemId::new("gdrive", "report.txt");
        items.insert(
            nested_id.0.clone(),
            VfsItem {
                id: nested_id,
                parent_id: documents_id,
                name: "report.txt".to_string(),
                is_dir: false,
                size: Some(10),
                mod_time: None,
                cache_state: CacheState::Online,
                mime_type: None,
            },
        );

        let roots = collect_root_children(&items);
        let names: Vec<&str> = roots.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Documents", "Photos"],
            "loose-root enumeration must return the sync-runner's top-level items, not a bare \"root\" match"
        );
    }

    /// `with_root` records the supplied id and `root()` reads it
    /// back, matching the FUSE presenter's builder-style API.
    #[tokio::test]
    async fn with_root_round_trips() {
        let presenter = ProjFsPresenter::new(PathBuf::from("/tmp/cascade-projfs-test"));
        assert!(presenter.root().await.is_none());

        let root = ItemId::new("backend", "root");
        let presenter = presenter.with_root(root.clone());
        assert_eq!(presenter.root().await, Some(root));
    }

    /// A trivial in-memory [`ContentProvider`] used by the presenter
    /// tests and the `GetFileData` unit tests. Returns bytes from a
    /// pre-populated `HashMap<ItemId, Vec<u8>>` and supports short
    /// reads at end of file. Not used in production.
    #[derive(Debug, Default)]
    struct MockContentProvider {
        files: Mutex<HashMap<ItemId, Vec<u8>>>,
    }

    impl MockContentProvider {
        fn insert(&self, id: ItemId, bytes: Vec<u8>) {
            self.files.lock().unwrap().insert(id, bytes);
        }
    }

    impl ContentProvider for MockContentProvider {
        fn read_range(&self, id: &ItemId, offset: u64, length: u32) -> std::io::Result<Vec<u8>> {
            let files = self.files.lock().unwrap();
            let Some(bytes) = files.get(id) else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no such id",
                ));
            };
            let start = usize::try_from(offset).map_err(|err| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, err.to_string())
            })?;
            if start >= bytes.len() {
                return Ok(Vec::new());
            }
            let want = usize::try_from(length).map_err(|err| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, err.to_string())
            })?;
            let end = start.saturating_add(want).min(bytes.len());
            Ok(bytes.get(start..end).unwrap_or_default().to_vec())
        }
    }

    /// `with_content_provider` installs the provider and `content_provider()`
    /// returns the same Arc back. Without one, the accessor returns
    /// `None` so callers can detect the browse-only mode.
    #[tokio::test]
    async fn with_content_provider_round_trips() {
        let presenter = ProjFsPresenter::new(PathBuf::from("/tmp/cascade-projfs-test"));
        assert!(presenter.content_provider().is_none());

        let provider: Arc<dyn ContentProvider> = Arc::new(MockContentProvider::default());
        let presenter = presenter.with_content_provider(Arc::clone(&provider));
        let installed = presenter.content_provider().unwrap();
        assert!(Arc::ptr_eq(installed, &provider));
    }

    /// Every `PRJ_NOTIFICATION_*` flag maps to a distinct
    /// `NotificationEvent`, the destination is threaded through for
    /// the rename/hardlink variants, and unknown codes yield `None`.
    #[test]
    fn notification_event_from_prj_notification_round_trip() {
        // The integer codes here are the values of the
        // `PRJ_NOTIFICATION_*` constants in
        // windows::Win32::Storage::ProjectedFileSystem. Keeping them
        // as literals lets the test compile cross-platform.
        let cases: &[(i32, NotificationEvent)] = &[
            (2, NotificationEvent::FileOpened),
            (4, NotificationEvent::NewFileCreated),
            (8, NotificationEvent::FileOverwritten),
            (16, NotificationEvent::PreDelete),
            (
                32,
                NotificationEvent::PreRename {
                    destination: "dst".to_string(),
                },
            ),
            (
                64,
                NotificationEvent::PreSetHardlink {
                    destination: "dst".to_string(),
                },
            ),
            (
                128,
                NotificationEvent::FileRenamed {
                    destination: "dst".to_string(),
                },
            ),
            (
                256,
                NotificationEvent::HardlinkCreated {
                    destination: "dst".to_string(),
                },
            ),
            (512, NotificationEvent::FileHandleClosedNoModification),
            (1024, NotificationEvent::FileHandleClosedFileModified),
            (2048, NotificationEvent::FileHandleClosedFileDeleted),
            (4096, NotificationEvent::FilePreConvertToFull),
        ];
        for (code, expected) in cases {
            let got =
                NotificationEvent::from_i32(*code, Some("dst".to_string())).unwrap_or_else(|| {
                    panic!("notification code {code} should map to a variant");
                });
            assert_eq!(&got, expected, "code {code}");
        }

        // Unknown codes do not map.
        assert!(NotificationEvent::from_i32(0, None).is_none());
        assert!(NotificationEvent::from_i32(9999, None).is_none());

        // For variants that do not carry a destination, an empty
        // string is harmless because the variant ignores the field.
        let opened = NotificationEvent::from_i32(2, None).unwrap();
        assert_eq!(opened.tag(), "FILE_OPENED");
    }

    /// The default `allow_delete` policy permits every delete —
    /// future write-protection work hangs off this hook.
    #[test]
    fn allow_delete_default_returns_true() {
        let items: HashMap<String, VfsItem> = HashMap::new();
        // File and directory deletes both pass the always-allow stub.
        assert!(allow_delete("any/path.txt", false, &items));
        assert!(allow_delete("a/dir", true, &items));
        // Empty path also passes — no special case on path shape.
        assert!(allow_delete("", false, &items));
        assert!(allow_delete("", true, &items));
    }

    /// A fresh token is un-cancelled; cancelling one clone is
    /// observable on every other clone because they share the
    /// underlying `Arc<AtomicBool>`. This is the contract
    /// `cancel_command` relies on: it holds a clone of the token the
    /// running callback registered and signals it from a different
    /// thread.
    #[test]
    fn cancellation_token_propagates_across_clones() {
        let token = CancellationToken::new();
        let other = token.clone();
        assert!(!token.is_cancelled());
        assert!(!other.is_cancelled());

        other.cancel();
        assert!(token.is_cancelled());
        assert!(other.is_cancelled());
    }

    /// `MockContentProvider` returns the byte range it was asked for
    /// and a short read at end of file. This exercises the boundary
    /// the real `GetFileData` callback relies on — the loop terminates
    /// when the provider returns fewer bytes than requested.
    #[test]
    fn mock_content_provider_returns_short_read_at_eof() {
        let id = ItemId::new("backend", "file");
        let provider = MockContentProvider::default();
        provider.insert(id.clone(), b"hello, world".to_vec());

        let chunk = provider.read_range(&id, 0, 5).unwrap();
        assert_eq!(chunk, b"hello");

        let tail = provider.read_range(&id, 7, 100).unwrap();
        assert_eq!(tail, b"world");

        // Past end of file returns empty, not error.
        let beyond = provider.read_range(&id, 100, 10).unwrap();
        assert!(beyond.is_empty());
    }

    /// `HResultCode::from_win32` reproduces the `HRESULT_FROM_WIN32` C
    /// macro packing for positive Win32 codes — low 16 bits carry the
    /// error number, facility 7 (`FACILITY_WIN32`) sits in the
    /// facility field, and the severity bit is set. The exact value
    /// `0x8007_0002` is what `HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)`
    /// expands to in Win32 headers.
    #[test]
    fn hresult_code_packs_win32_error_with_facility_seven() {
        // ERROR_FILE_NOT_FOUND = 2 → 0x80070002 as a signed i32.
        let code = HResultCode::from_win32(2);
        #[allow(clippy::cast_possible_wrap)]
        let expected = 0x8007_0002_u32 as i32;
        assert_eq!(code.get(), expected);

        // ERROR_ACCESS_DENIED = 5 → 0x80070005.
        let code = HResultCode::from_win32(5);
        #[allow(clippy::cast_possible_wrap)]
        let expected = 0x8007_0005_u32 as i32;
        assert_eq!(code.get(), expected);

        // ERROR_GEN_FAILURE = 31 → 0x8007001F.
        let code = HResultCode::from_win32(31);
        #[allow(clippy::cast_possible_wrap)]
        let expected = 0x8007_001F_u32 as i32;
        assert_eq!(code.get(), expected);

        // ERROR_SUCCESS = 0 packs to 0x80070000 — the constructor does
        // not special-case the zero code (matching the C macro for the
        // positive branch).
        let code = HResultCode::from_win32(0);
        #[allow(clippy::cast_possible_wrap)]
        let expected = 0x8007_0000_u32 as i32;
        assert_eq!(code.get(), expected);
    }

    /// Only the low 16 bits of the Win32 code participate in the
    /// packed `HRESULT`. Higher bits are masked off, mirroring the
    /// `(x) & 0x0000FFFF` clause of `HRESULT_FROM_WIN32`.
    #[test]
    fn hresult_code_masks_high_bits_of_win32_code() {
        // 0x12345 → low 16 bits are 0x2345, which packs to 0x80072345.
        let code = HResultCode::from_win32(0x0001_2345);
        #[allow(clippy::cast_possible_wrap)]
        let expected = 0x8007_2345_u32 as i32;
        assert_eq!(code.get(), expected);
    }

    /// The `PrjAllocateAlignedBuffer` failure path in `get_file_data`
    /// surfaces `ERROR_NOT_ENOUGH_MEMORY` (Win32 8) — the documented
    /// Win32 code for an allocation failure — rather than the historic
    /// `ERROR_CALL_NOT_IMPLEMENTED`. The actual call only fires on
    /// Windows, but the value the callback would return is computed by
    /// [`HResultCode::from_win32`], which is cross-platform; assert
    /// the packed result here so the mapping survives a refactor even
    /// when the suite runs on macOS or Linux.
    #[test]
    fn hresult_code_packs_error_not_enough_memory() {
        // ERROR_NOT_ENOUGH_MEMORY = 8 → 0x80070008 as a signed i32.
        let code = HResultCode::from_win32(8);
        #[allow(clippy::cast_possible_wrap)]
        let expected = 0x8007_0008_u32 as i32;
        assert_eq!(code.get(), expected);
    }

    /// The poisoned-mutex sentinel in `start_directory_enumeration`
    /// maps to `ERROR_INTERNAL_ERROR` (Win32 1359, `HRESULT`
    /// `0x8007_054F`) so `ProjFS` treats it as a transient internal
    /// failure rather than a "callback not implemented" signal — the
    /// latter is reserved for the browse-only path in `get_file_data`.
    /// The actual mutex poisoning can only fire on Windows under
    /// concurrent panic, but the packed value the callback would
    /// return is computed cross-platform by [`HResultCode::from_win32`];
    /// pin it here so a future refactor cannot silently regress the
    /// mapping when the suite runs on macOS or Linux.
    #[test]
    fn hresult_code_packs_error_internal_error() {
        // ERROR_INTERNAL_ERROR = 1359 → 0x8007054F as a signed i32.
        let code = HResultCode::from_win32(1359);
        #[allow(clippy::cast_possible_wrap)]
        let expected = 0x8007_054F_u32 as i32;
        assert_eq!(code.get(), expected);
    }

    /// Every `io::ErrorKind` documented in the mapping table produces
    /// the matching packed `HRESULT`, and any kind not in the table
    /// falls through to `ERROR_GEN_FAILURE`. The expected codes are
    /// computed via [`HResultCode::from_win32`] so the test stays in
    /// sync with the packing rule rather than hard-coding magic
    /// numbers.
    #[test]
    fn hresult_for_io_error_maps_each_kind() {
        use std::io::{Error, ErrorKind};

        // Pairs of `(ErrorKind, expected Win32 code)`. The numeric
        // codes match `windows::Win32::Foundation::*` and are
        // documented on `hresult_for_io_error`.
        let cases: &[(ErrorKind, u32)] = &[
            (ErrorKind::NotFound, 2),
            (ErrorKind::PermissionDenied, 5),
            (ErrorKind::Interrupted, 995),
            (ErrorKind::OutOfMemory, 14),
            (ErrorKind::TimedOut, 1460),
            (ErrorKind::BrokenPipe, 109),
            (ErrorKind::UnexpectedEof, 38),
            (ErrorKind::WouldBlock, 997),
            // Variants not in the table fall through to
            // `ERROR_GEN_FAILURE` (31). Cover a representative
            // sample including the catch-all `Other`.
            (ErrorKind::Other, 31),
            (ErrorKind::InvalidInput, 31),
            (ErrorKind::InvalidData, 31),
            (ErrorKind::AlreadyExists, 31),
            (ErrorKind::ConnectionRefused, 31),
            (ErrorKind::WriteZero, 31),
        ];

        for (kind, win32) in cases {
            let err = Error::from(*kind);
            let got = hresult_for_io_error(&err);
            let expected = HResultCode::from_win32(*win32);
            assert_eq!(
                got, expected,
                "ErrorKind::{kind:?} should map to Win32 code {win32}"
            );
        }

        // `Error::other` wraps an arbitrary payload as
        // `ErrorKind::Other` — confirm the fall-through applies to a
        // freshly constructed `other` value, not just `Error::from`.
        let synthetic = Error::other("synthetic failure");
        assert_eq!(
            hresult_for_io_error(&synthetic),
            HResultCode::from_win32(31),
            "Error::other should map to ERROR_GEN_FAILURE"
        );
    }

    /// `classify_read` collapses a `ContentProvider::read_range` result
    /// to the three outcomes `GetFileData` cares about. This is the
    /// platform-independent stand-in for end-to-end testing the
    /// callback's `HRESULT` decision: `S_OK` for `Eof`, the carried
    /// [`HResultCode`] for `Failed`, and `PrjWriteFileData` for
    /// `Bytes`. Exercised against the real `MockContentProvider` so
    /// regressions in the EOF threshold surface here rather than only
    /// on Windows CI.
    #[test]
    fn classify_read_handles_eof_bytes_and_failure() {
        let id = ItemId::new("backend", "file");
        let provider = MockContentProvider::default();
        provider.insert(id.clone(), b"hello".to_vec());

        // Non-empty read → Bytes.
        let bytes_result = provider.read_range(&id, 0, 5);
        match classify_read(bytes_result) {
            ProviderReadOutcome::Bytes(b) => assert_eq!(b, b"hello"),
            other => panic!("expected Bytes, got {other:?}"),
        }

        // Out-of-range read returns empty Vec → Eof. This is the path
        // `GetFileData` returns S_OK on — assert it survives a round
        // trip through the real provider, not just an `Ok(vec![])`
        // literal.
        let eof_result = provider.read_range(&id, 100, 10);
        assert!(matches!(
            classify_read(eof_result),
            ProviderReadOutcome::Eof
        ));

        // I/O error → Failed carrying the mapped HRESULT. A synthetic
        // `Error::other(_)` has `ErrorKind::Other`, which falls
        // through to `ERROR_GEN_FAILURE` (Win32 31). Assert the
        // carried code matches that mapping exactly.
        let failed: std::io::Result<Vec<u8>> = Err(std::io::Error::other("synthetic failure"));
        match classify_read(failed) {
            ProviderReadOutcome::Failed(code) => {
                assert_eq!(
                    code,
                    HResultCode::from_win32(31),
                    "Error::other should map to ERROR_GEN_FAILURE (31)"
                );
            }
            other => panic!("expected Failed(ERROR_GEN_FAILURE), got {other:?}"),
        }

        // A zero-byte read that happens to be exactly at end of file
        // (length 0, offset == file end) is still `Eof`; the provider
        // returns `Ok(vec![])` here too.
        let exactly_at_end = provider.read_range(&id, 5, 0);
        assert!(matches!(
            classify_read(exactly_at_end),
            ProviderReadOutcome::Eof
        ));
    }
}
