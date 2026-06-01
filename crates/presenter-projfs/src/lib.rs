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
//! This is the v8 roadmap scaffold. The crate exposes a [`ProjFsPresenter`]
//! that implements [`VfsPresenter`], but most of the methods that talk to
//! the OS are stubs:
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
//!   `PrjStartVirtualizing`, but every callback in the table immediately
//!   returns `S_OK` with empty results (or
//!   `HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)` where empty is not a
//!   legal answer). The mount appears as an empty directory until the
//!   callbacks are filled in.
//! - [`ProjFsPresenter::stop`] calls `PrjStopVirtualizing` against the
//!   stored namespace handle.
//!
//! On non-Windows targets, [`ProjFsPresenter::start`] returns
//! `Err("ProjFS presenter is only supported on Windows")` and
//! [`ProjFsPresenter::stop`] is a no-op.
//!
//! # Follow-up work
//!
//! The eight `ProjFS` callbacks that need real implementations are:
//!
//! 1. `StartDirectoryEnumerationCallback` — open a directory iterator
//!    keyed by enumeration `GUID`, backed by the engine `VfsTree`.
//! 2. `EndDirectoryEnumerationCallback` — release the iterator.
//! 3. `GetDirectoryEnumerationCallback` — yield the next batch of
//!    entries into the `PRJ_DIR_ENTRY_BUFFER_HANDLE` honouring the
//!    `PCWSTR` search expression.
//! 4. `GetPlaceholderInfoCallback` — translate a path to
//!    `PRJ_PLACEHOLDER_INFO`, populating `FileBasicInfo` from the
//!    matching `VfsItem`.
//! 5. `GetFileDataCallback` — stream file bytes from the backend via
//!    `PrjWriteFileData`, respecting the requested offset and length.
//! 6. `QueryFileNameCallback` — case-insensitive existence check used
//!    by `ProjFS` to decide whether to descend into the projection.
//! 7. `NotificationCallback` — react to user-driven changes
//!    (open/close/rename/delete) and forward them back into the engine.
//! 8. `CancelCommandCallback` — abort the in-flight Tokio task for a
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
            windows_impl::start_virtualising(mount_point, &self.handle).await?;
            tracing::info!(
                mount_point = %mount_display,
                "ProjFS virtualisation started (callbacks are stubs)"
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
            windows_impl::stop_virtualising(&self.handle).await
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
    //! The callback table is intentionally stubbed: every callback
    //! returns `S_OK` with no results (or an "empty" `HRESULT` where
    //! `S_OK` would lie about success). The mount appears as an empty
    //! directory until the full callback set is implemented.

    use std::ffi::c_void;
    use std::path::Path;
    use std::sync::Arc;

    use anyhow::{Context as _, Result};
    use windows::Win32::Foundation::{
        ERROR_CALL_NOT_IMPLEMENTED, ERROR_FILE_NOT_FOUND, HRESULT, S_OK,
    };
    use windows::Win32::Storage::ProjectedFileSystem::{
        PRJ_CALLBACK_DATA, PRJ_CALLBACKS, PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT, PRJ_NOTIFICATION,
        PRJ_NOTIFICATION_PARAMETERS, PRJ_PLACEHOLDER_VERSION_INFO, PrjMarkDirectoryAsPlaceholder,
        PrjStartVirtualizing, PrjStopVirtualizing,
    };
    use windows::core::{GUID, HSTRING, PCWSTR};

    use super::handle::NamespaceHandle;

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

    /// Stub callback for `PRJ_START_DIRECTORY_ENUMERATION_CB`.
    #[allow(unsafe_code)]
    unsafe extern "system" fn start_directory_enumeration(
        _callback_data: *const PRJ_CALLBACK_DATA,
        _enumeration_id: *const GUID,
    ) -> HRESULT {
        S_OK
    }

    /// Stub callback for `PRJ_END_DIRECTORY_ENUMERATION_CB`.
    #[allow(unsafe_code)]
    unsafe extern "system" fn end_directory_enumeration(
        _callback_data: *const PRJ_CALLBACK_DATA,
        _enumeration_id: *const GUID,
    ) -> HRESULT {
        S_OK
    }

    /// Stub callback for `PRJ_GET_DIRECTORY_ENUMERATION_CB`.
    ///
    /// Returning `S_OK` with no entries written tells `ProjFS` the
    /// directory is empty — the safest stub answer until the real
    /// enumeration logic lands.
    #[allow(unsafe_code)]
    unsafe extern "system" fn get_directory_enumeration(
        _callback_data: *const PRJ_CALLBACK_DATA,
        _enumeration_id: *const GUID,
        _search_expression: PCWSTR,
        _dir_entry_buffer_handle: windows::Win32::Storage::ProjectedFileSystem::PRJ_DIR_ENTRY_BUFFER_HANDLE,
    ) -> HRESULT {
        S_OK
    }

    /// Stub callback for `PRJ_GET_PLACEHOLDER_INFO_CB`.
    ///
    /// Returning `ERROR_FILE_NOT_FOUND` is the documented way to tell
    /// `ProjFS` "this path does not exist in the projection" —
    /// appropriate for an empty stub.
    #[allow(unsafe_code)]
    unsafe extern "system" fn get_placeholder_info(
        _callback_data: *const PRJ_CALLBACK_DATA,
    ) -> HRESULT {
        hresult_from_win32(ERROR_FILE_NOT_FOUND.0)
    }

    /// Stub callback for `PRJ_GET_FILE_DATA_CB`.
    #[allow(unsafe_code)]
    unsafe extern "system" fn get_file_data(
        _callback_data: *const PRJ_CALLBACK_DATA,
        _byte_offset: u64,
        _length: u32,
    ) -> HRESULT {
        hresult_from_win32(ERROR_CALL_NOT_IMPLEMENTED.0)
    }

    /// Stub callback for `PRJ_QUERY_FILE_NAME_CB`.
    #[allow(unsafe_code)]
    unsafe extern "system" fn query_file_name(_callback_data: *const PRJ_CALLBACK_DATA) -> HRESULT {
        hresult_from_win32(ERROR_FILE_NOT_FOUND.0)
    }

    /// Stub callback for `PRJ_NOTIFICATION_CB`.
    #[allow(unsafe_code)]
    unsafe extern "system" fn notification(
        _callback_data: *const PRJ_CALLBACK_DATA,
        _is_directory: windows::Win32::Foundation::BOOLEAN,
        _notification: PRJ_NOTIFICATION,
        _destination_file_name: PCWSTR,
        _operation_parameters: *mut PRJ_NOTIFICATION_PARAMETERS,
    ) -> HRESULT {
        S_OK
    }

    /// Stub callback for `PRJ_CANCEL_COMMAND_CB`.
    #[allow(unsafe_code)]
    unsafe extern "system" fn cancel_command(_callback_data: *const PRJ_CALLBACK_DATA) {
        // No outstanding work to cancel in the stub implementation.
    }

    /// Build the stub callback table.
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

    /// Mark the mount directory as a `ProjFS` placeholder and start
    /// virtualising. Stores the resulting namespace handle in the
    /// presenter so [`stop_virtualising`] can release it later.
    pub(super) async fn start_virtualising(
        mount_point: &Path,
        handle_slot: &Arc<tokio::sync::Mutex<Option<NamespaceHandle>>>,
    ) -> Result<()> {
        let mount_hstring = HSTRING::from(mount_point.as_os_str());
        let mount_pcwstr = PCWSTR(mount_hstring.as_ptr());

        // SAFETY: PrjMarkDirectoryAsPlaceholder reads the path string
        // for the duration of the call. `mount_hstring` outlives the
        // call because it is held on the stack until after the FFI
        // returns. The other parameters are nullable per the API
        // contract; we pass null for target_path (no overlay source),
        // version_info (no provider version tracking yet), and
        // virtualisation_instance_id (let ProjFS pick).
        #[allow(unsafe_code)]
        let mark_result = unsafe {
            PrjMarkDirectoryAsPlaceholder(
                mount_pcwstr,
                PCWSTR::null(),
                std::ptr::null::<PRJ_PLACEHOLDER_VERSION_INFO>(),
                std::ptr::null::<GUID>(),
            )
        };
        mark_result
            .ok()
            .context("PrjMarkDirectoryAsPlaceholder failed")?;

        let callbacks = build_callbacks();

        // SAFETY: PrjStartVirtualizing takes ownership of the callback
        // table by copying its function pointer fields. The stack-local
        // `callbacks` is valid for the duration of the call. The
        // out-parameter receives an owned PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT
        // that we must release with PrjStopVirtualizing.
        let mut ctx = PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT::default();
        #[allow(unsafe_code)]
        let start_result = unsafe {
            PrjStartVirtualizing(
                mount_pcwstr,
                &callbacks,
                std::ptr::null::<c_void>(),
                std::ptr::null(),
                &mut ctx,
            )
        };
        start_result.ok().context("PrjStartVirtualizing failed")?;

        let mut slot = handle_slot.lock().await;
        *slot = Some(NamespaceHandle(ctx));
        Ok(())
    }

    /// Release the stored `ProjFS` namespace handle, if any.
    pub(super) async fn stop_virtualising(
        handle_slot: &Arc<tokio::sync::Mutex<Option<NamespaceHandle>>>,
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
