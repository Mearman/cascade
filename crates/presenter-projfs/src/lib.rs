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
//! # Scaffold status
//!
//! The crate is partway through the v8 roadmap. Browse callbacks are
//! live; read and write callbacks remain stubbed.
//!
//! - [`ProjFsPresenter::upsert_item`] and [`ProjFsPresenter::delete_item`]
//!   are real — they update an in-memory `HashMap<String, VfsItem>`
//!   exactly like the other presenters.
//! - [`ProjFsPresenter::update_state`] is a no-op log line; `ProjFS` has
//!   no equivalent of `FSKit`'s `update_state` push hook.
//! - [`ProjFsPresenter::evict_item`] logs and returns `Ok(())`; `ProjFS`
//!   manages projection cache eviction at the OS layer.
//! - [`ProjFsPresenter::fetch_contents`] returns an "not yet
//!   implemented" error. In the full implementation, on-demand reads are
//!   driven by the `GetFileDataCallback` rather than by direct calls
//!   into this method.
//! - [`ProjFsPresenter::start`] registers the virtualisation root and
//!   begins virtualising via `PrjMarkDirectoryAsPlaceholder` and
//!   `PrjStartVirtualizing`. The callback table now serves
//!   `QueryFileName`, `GetPlaceholderInfo`, and the
//!   `Start`/`Get`/`End` directory enumeration triplet from the
//!   in-memory items map. `dir`-style listings work; reading a file's
//!   bytes does not (see follow-up work).
//! - [`ProjFsPresenter::stop`] calls `PrjStopVirtualizing` against the
//!   stored namespace handle, then releases the heap-allocated
//!   callback context.
//!
//! On non-Windows targets, [`ProjFsPresenter::start`] returns
//! `Err("ProjFS presenter is only supported on Windows")` and
//! [`ProjFsPresenter::stop`] is a no-op.
//!
//! # Follow-up work
//!
//! Three callbacks remain stubbed:
//!
//! 1. `GetFileDataCallback` — stream file bytes from the backend via
//!    `PrjWriteFileData`, respecting the requested offset and length.
//! 2. `NotificationCallback` — react to user-driven changes
//!    (open/close/rename/delete) and forward them back into the engine.
//! 3. `CancelCommandCallback` — abort the in-flight Tokio task for a
//!    given `PRJ_CALLBACK_DATA.CommandId`.
//!
//! Each requires translating between `ProjFS`'s enumeration session model
//! (keyed by `GUID`s) and the engine's `VfsTree`/`Backend` API. See the
//! [Projected File System Win32 documentation][projfs] for the full
//! callback contract.
//!
//! [projfs]: https://learn.microsoft.com/en-us/windows/win32/projfs/projected-file-system

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use cascade_engine::presenter::VfsPresenter;
use cascade_engine::types::{CacheState, ItemId, VfsItem};
use tokio::sync::RwLock;

/// State tracked for an in-flight directory enumeration session.
///
/// `ProjFS` opens an enumeration with `StartDirectoryEnumeration`, pulls
/// entries one batch at a time through `GetDirectoryEnumeration`, and
/// closes it with `EndDirectoryEnumeration`. Each session is identified
/// by a [`windows::core::GUID`] (stored here as the equivalent `u128`
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
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
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
}

/// State the `ProjFS` callbacks need to consult, packaged up so it can
/// be passed through `PrjStartVirtualizing`'s `instance_context`
/// pointer. The actual struct lives on the heap behind a `Box`; we
/// keep the `Box`'s ownership in [`ProjFsPresenter::callback_ctx`] so
/// it outlives the namespace handle.
#[derive(Debug)]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
struct CallbackContext {
    items: Arc<RwLock<HashMap<String, VfsItem>>>,
    root_id: Arc<RwLock<Option<ItemId>>>,
    enumerations: Arc<Mutex<HashMap<u128, EnumerationState>>>,
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
    /// that we handed to ProjFS via `instance_context`. The `VfsPresenter`
    /// trait contract requires `stop()` on shutdown, but the language
    /// can't enforce it — this is the belt-and-braces.
    ///
    /// On non-Windows targets the handle and callback_ctx are always
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
            && let Some(CallbackContext { raw_ptr, .. }) = slot.take()
            && raw_ptr != 0
        {
            // SAFETY: raw_ptr originated from Box::into_raw in
            // start_virtualising. ProjFS has stopped (or was never
            // started) so no callbacks can still dereference it.
            #[allow(unsafe_code)]
            unsafe {
                drop(Box::from_raw(raw_ptr as *mut handle::CallbackContextInner));
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

    /// The configured mount point.
    #[must_use]
    pub fn mount_point(&self) -> &Path {
        &self.mount_point
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
    //! items map shared with the presenter. `GetFileData`,
    //! `Notification` and `CancelCommand` remain stubbed pending the
    //! read/write follow-up callbacks.

    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use anyhow::{Context as _, Result};
    use cascade_engine::types::{ItemId, VfsItem};
    use tokio::sync::RwLock;
    use windows::Win32::Foundation::{
        ERROR_CALL_NOT_IMPLEMENTED, ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER, HRESULT, S_OK,
    };
    use windows::Win32::Storage::ProjectedFileSystem::{
        PRJ_CALLBACK_DATA, PRJ_CALLBACKS, PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN,
        PRJ_DIR_ENTRY_BUFFER_HANDLE, PRJ_FILE_BASIC_INFO, PRJ_NOTIFICATION,
        PRJ_NOTIFICATION_PARAMETERS, PRJ_PLACEHOLDER_INFO, PrjFillDirEntryBuffer,
        PrjMarkDirectoryAsPlaceholder, PrjStartVirtualizing, PrjStopVirtualizing,
        PrjWritePlaceholderInfo,
    };
    use windows::core::{GUID, HSTRING, PCWSTR};

    use super::handle::NamespaceHandle;
    use super::{
        CallbackContext, EnumerationState, build_enumeration_state, collect_children, resolve_path,
    };

    /// Heap-allocated state the callbacks consult. Kept distinct from
    /// the public [`CallbackContext`] wrapper so the wrapper can stay
    /// `Send + Sync` while the raw pointer to this inner struct is
    /// the one ProjFS dereferences.
    pub(super) struct CallbackContextInner {
        items: Arc<RwLock<HashMap<String, VfsItem>>>,
        root_id: Arc<RwLock<Option<ItemId>>>,
        enumerations: Arc<Mutex<HashMap<u128, EnumerationState>>>,
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
    unsafe fn context_from_callback_data<'a>(
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

    /// `PRJ_QUERY_FILE_NAME_CB` — existence check used by ProjFS to
    /// decide whether to descend into the projection.
    ///
    /// **Thread model**: ProjFS dispatches callbacks from kernel-owned
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
                &info,
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

        // Empty path = enumerate the projection root.
        let state = if path.is_empty() {
            let parent_id = root
                .as_ref()
                .cloned()
                .unwrap_or_else(|| ItemId(String::from("root")));
            build_enumeration_state(&parent_id, &items)
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
            // Poisoned mutex — surface a generic failure so ProjFS
            // can retry. ERROR_CALL_NOT_IMPLEMENTED is the closest
            // generic "we cannot serve this right now" signal that
            // the kernel will not retry forever.
            return hresult_from_win32(ERROR_CALL_NOT_IMPLEMENTED.0);
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
    /// entries into the buffer ProjFS provides.
    ///
    /// Filtering by `search_expression` is performed by
    /// `PrjFileNameMatch` server-side per the API contract; we
    /// currently emit every child and let ProjFS drop the ones that
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
            let mut basic = PRJ_FILE_BASIC_INFO::default();
            basic.IsDirectory = entry.is_dir;
            #[allow(clippy::cast_possible_wrap)]
            {
                basic.FileSize = entry.size.min(i64::MAX as u64) as i64;
            }

            // SAFETY: PrjFillDirEntryBuffer reads `filename` and
            // `filebasicinfo` for the duration of the call. Both
            // outlive the FFI; the buffer handle is the one ProjFS
            // handed us.
            let result = unsafe {
                PrjFillDirEntryBuffer(
                    PCWSTR(name_hstring.as_ptr()),
                    Some(&basic),
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

    /// Stub callback for `PRJ_GET_FILE_DATA_CB`. Read implementation
    /// follows in a later commit.
    #[allow(unsafe_code)]
    unsafe extern "system" fn get_file_data(
        _callback_data: *const PRJ_CALLBACK_DATA,
        _byte_offset: u64,
        _length: u32,
    ) -> HRESULT {
        hresult_from_win32(ERROR_CALL_NOT_IMPLEMENTED.0)
    }

    /// Stub callback for `PRJ_NOTIFICATION_CB`. Write-back hooks
    /// follow in a later commit.
    #[allow(unsafe_code)]
    unsafe extern "system" fn notification(
        _callback_data: *const PRJ_CALLBACK_DATA,
        _is_directory: bool,
        _notification: PRJ_NOTIFICATION,
        _destination_file_name: PCWSTR,
        _operation_parameters: *mut PRJ_NOTIFICATION_PARAMETERS,
    ) -> HRESULT {
        S_OK
    }

    /// Stub callback for `PRJ_CANCEL_COMMAND_CB`. There is no
    /// in-flight async work to cancel until `GetFileData` is real.
    #[allow(unsafe_code)]
    unsafe extern "system" fn cancel_command(_callback_data: *const PRJ_CALLBACK_DATA) {}

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
    /// for use as a `HashMap` key. ProjFS treats enumeration IDs as
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
    pub(super) async fn start_virtualising(
        mount_point: &Path,
        handle_slot: &Arc<tokio::sync::Mutex<Option<NamespaceHandle>>>,
        callback_ctx_slot: &Arc<tokio::sync::Mutex<Option<CallbackContext>>>,
        items: Arc<RwLock<HashMap<String, VfsItem>>>,
        root_id: Arc<RwLock<Option<ItemId>>>,
        enumerations: Arc<Mutex<HashMap<u128, EnumerationState>>>,
    ) -> Result<()> {
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

        // Build the callback context. The Box is the owning handle:
        // we hand its raw pointer to ProjFS via instance_context and
        // recover it in `stop_virtualising` to free the allocation.
        let inner = Box::new(CallbackContextInner {
            items: Arc::clone(&items),
            root_id: Arc::clone(&root_id),
            enumerations: Arc::clone(&enumerations),
        });
        let inner_ptr = Box::into_raw(inner);
        let instance_context = inner_ptr.cast::<c_void>();

        let callbacks = build_callbacks();

        // SAFETY: PrjStartVirtualizing copies the callback function
        // pointers out of `callbacks`. The stack-local table is valid
        // for the duration of the call. The instance_context pointer
        // is the raw Box we just allocated; the kernel keeps it until
        // PrjStopVirtualizing returns.
        #[allow(unsafe_code)]
        let start_result = unsafe {
            PrjStartVirtualizing(
                mount_pcwstr,
                &callbacks,
                Some(instance_context.cast_const()),
                None,
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

        let mut handle_slot_guard = handle_slot.lock().await;
        *handle_slot_guard = Some(NamespaceHandle(ctx));

        let mut ctx_slot = callback_ctx_slot.lock().await;
        *ctx_slot = Some(CallbackContext {
            items,
            root_id,
            enumerations,
            raw_ptr: inner_ptr as usize,
        });
        Ok(())
    }

    /// Release the stored `ProjFS` namespace handle, if any.
    pub(super) async fn stop_virtualising(
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
        if let Some(CallbackContext { raw_ptr, .. }) = ctx
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
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
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
}
