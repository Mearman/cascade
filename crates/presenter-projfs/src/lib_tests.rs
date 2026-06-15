//! Test module for `lib.rs`, split out to keep the
//! parent file under the source-length cap. Declared from there via
//! `#[cfg(test)] #[path = "lib_tests.rs"] mod tests;`, so it stays a child
//! module with full access to the parent's private items.

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
        path: "test.txt".to_string(),
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

/// `fetch_contents` without a `ContentProvider` bails with a clear
/// message directing callers to the `GetFileData` callback path.
#[tokio::test]
async fn fetch_contents_bails_without_content_provider() {
    let presenter = ProjFsPresenter::new(PathBuf::from("/tmp/cascade-projfs-test"));
    let id = ItemId::new("backend", "file");
    let err = presenter.fetch_contents(&id).await.unwrap_err();
    assert!(
        err.to_string().contains("no ContentProvider installed"),
        "expected provider-missing error, got: {err}"
    );
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
        path: "dir1".to_string(),
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
        path: "dir1/file.txt".to_string(),
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
        path: "dir1/good.txt".to_string(),
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
        path: "dir1/bad?.txt".to_string(),
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
            path: format!("dir1/{name}"),
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
                path: format!("dir1/{name}"),
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
                path: name.to_string(),
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
            path: "Documents/report.txt".to_string(),
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

/// `fetch_contents` reads the full file through the `ContentProvider`,
/// writes it to the cache directory, and returns the cache path. A
/// second call for the same id returns the cached file without
/// consulting the provider again.
#[tokio::test]
async fn fetch_contents_reads_via_provider_and_caches() {
    let cache_dir = std::env::temp_dir().join("cascade-projfs-test-fetch-contents");
    let _ = tokio::fs::remove_dir_all(&cache_dir).await;

    let id = ItemId::new("backend", "file");
    let provider = MockContentProvider::default();
    let data = b"hello, projfs world!".to_vec();
    provider.insert(id.clone(), data.clone());

    let presenter = ProjFsPresenter::new(PathBuf::from("/tmp/cascade-projfs-test"))
        .with_content_provider(Arc::new(provider))
        .with_cache_dir(&cache_dir);

    // Upsert the item so `fetch_contents` can resolve its size.
    presenter
        .upsert_item(VfsItem {
            id: id.clone(),
            parent_id: ItemId::new("backend", "root"),
            name: "file.txt".to_string(),
            path: "file.txt".to_string(),
            is_dir: false,
            size: Some(data.len() as u64),
            mod_time: None,
            cache_state: CacheState::Online,
            mime_type: None,
        })
        .await
        .unwrap();

    let path = presenter.fetch_contents(&id).await.unwrap();
    assert!(path.exists(), "cached file should exist on disk");
    let bytes = tokio::fs::read(&path).await.unwrap();
    assert_eq!(bytes, data);

    // The cache path follows the id-slug convention.
    let expected_name = super::cache_slug(&id);
    assert_eq!(path.file_name().unwrap().to_str().unwrap(), expected_name);

    let _ = tokio::fs::remove_dir_all(&cache_dir).await;
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
