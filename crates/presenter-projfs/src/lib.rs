//! `ProjFS` presenter — exposes the VFS tree as a native Windows
//! Projected File System mount.
//!
//! Implements the engine's [`VfsPresenter`] trait. On Windows, the mount
//! is served via the Projected File System API exposed through the
//! `windows` crate's `Win32_Storage_ProjectedFileSystem` module. On
//! other platforms, every operation that actually touches the OS
//! returns an error — the crate compiles but does not mount.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use cascade_engine::presenter::VfsPresenter;
use cascade_engine::types::{CacheState, ItemId, VfsItem};
use tokio::sync::RwLock;

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
    /// Live `ProjFS` virtualisation handle. `Some` only between
    /// [`Self::start`] and [`Self::stop`] on Windows; always `None` on
    /// other platforms — but the field is kept on every target so the
    /// struct shape does not vary with `cfg`.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    handle: Arc<tokio::sync::Mutex<Option<NamespaceHandle>>>,
}

impl ProjFsPresenter {
    /// Create a new `ProjFS` presenter rooted at `mount_point`.
    #[must_use]
    pub fn new(mount_point: impl Into<PathBuf>) -> Self {
        Self {
            mount_point: mount_point.into(),
            items: Arc::new(RwLock::new(HashMap::new())),
            handle: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    /// Override the mount point after construction. Mirrors the
    /// builder-style API used by the `FSKit` and `WebDAV` presenters.
    #[must_use]
    pub fn with_mount_point(mut self, path: impl Into<PathBuf>) -> Self {
        self.mount_point = path.into();
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
#[allow(clippy::unwrap_used)]
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
}
