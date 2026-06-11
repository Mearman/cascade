//! Test module for `engine_handlers.rs`, split out to keep the
//! parent file under the source-length cap. Declared from there via
//! `#[cfg(test)] #[path = "engine_handlers_tests.rs"] mod tests;`, so it stays a child
//! module with full access to the parent's private items.

use super::*;
use crate::handlers::ErrorCode;
use async_trait::async_trait;
use cascade_engine::types::{Change, Cursor, Quota};
use chrono::Utc;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

/// Minimal in-memory backend used to exercise `EngineHandlers` without
/// the real cloud transports. Implements just enough of the trait
/// surface to cover the seven RPC paths.
#[derive(Debug, Default)]
struct InMemoryBackend {
    id: String,
    files: Mutex<HashMap<String, FileEntry>>,
    content: Mutex<HashMap<String, Vec<u8>>>,
    next_id: Mutex<u64>,
}

impl InMemoryBackend {
    fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            files: Mutex::new(HashMap::new()),
            content: Mutex::new(HashMap::new()),
            next_id: Mutex::new(0),
        }
    }

    fn allocate_id(&self) -> String {
        let mut next = self
            .next_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *next += 1;
        format!("n{next}")
    }

    fn insert(&self, entry: FileEntry) {
        let mut files = self
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        files.insert(entry.id.0.clone(), entry);
    }
}

#[async_trait]
impl Backend for InMemoryBackend {
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
        Ok((vec![], Cursor("static".to_string())))
    }

    async fn metadata(&self, _path: &Path) -> anyhow::Result<FileEntry> {
        anyhow::bail!("metadata not implemented for stub")
    }

    async fn download(&self, file: &FileEntry) -> anyhow::Result<Vec<u8>> {
        let content = self
            .content
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        content
            .get(&file.id.0)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no content for {}", file.id))
    }

    async fn upload(
        &self,
        path: &Path,
        data: &[u8],
        parent_id: &FileId,
    ) -> anyhow::Result<FileEntry> {
        let bytes = data.to_vec();

        let new_id = self.allocate_id();
        let item_id = ItemId::new(&self.id, &new_id);
        let parent_item = ItemId(parent_id.0.clone());
        let entry = FileEntry {
            id: item_id.clone(),
            parent_id: parent_item,
            name: path.file_name().map_or_else(
                || "unnamed".to_string(),
                |os| os.to_string_lossy().into_owned(),
            ),
            path: path.file_name().map_or_else(
                || "unnamed".to_string(),
                |os| os.to_string_lossy().into_owned(),
            ),
            is_dir: false,
            size: Some(bytes.len() as u64),
            mod_time: Some(Utc::now()),
            mime_type: None,
            hash: None,
        };

        self.content
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(item_id.0, bytes);
        self.insert(entry.clone());
        Ok(entry)
    }

    async fn update(&self, file_id: &FileId, data: &[u8]) -> anyhow::Result<FileEntry> {
        let bytes = data.to_vec();

        let mut files = self
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = files
            .get_mut(&file_id.0)
            .ok_or_else(|| anyhow::anyhow!("file not found"))?;
        entry.size = Some(bytes.len() as u64);
        entry.mod_time = Some(Utc::now());
        let updated = entry.clone();
        drop(files);

        self.content
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(file_id.0.clone(), bytes);
        Ok(updated)
    }

    async fn create_dir(&self, _path: &Path) -> anyhow::Result<FileEntry> {
        anyhow::bail!("create_dir is not used by the stub; use create_dir_with_parent")
    }

    async fn create_dir_with_parent(
        &self,
        name: &str,
        parent_id: &FileId,
    ) -> anyhow::Result<FileEntry> {
        let new_id = self.allocate_id();
        let item_id = ItemId::new(&self.id, &new_id);
        let parent_item = ItemId(parent_id.0.clone());
        let entry = FileEntry {
            id: item_id,
            parent_id: parent_item,
            name: name.to_string(),
            path: name.to_string(),
            is_dir: true,
            size: None,
            mod_time: Some(Utc::now()),
            mime_type: None,
            hash: None,
        };
        self.insert(entry.clone());
        Ok(entry)
    }

    async fn delete(&self, file: &FileEntry) -> anyhow::Result<()> {
        let mut files = self
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        files.remove(&file.id.0);
        drop(files);
        self.content
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&file.id.0);
        Ok(())
    }

    async fn move_entry(&self, _src: &Path, _dst: &Path) -> anyhow::Result<FileEntry> {
        anyhow::bail!("move_entry is not used by the stub; use move_by_id")
    }

    async fn move_by_id(
        &self,
        src_id: &FileId,
        dst_parent_id: &FileId,
        new_name: &str,
    ) -> anyhow::Result<FileEntry> {
        let mut files = self
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = files
            .get_mut(&src_id.0)
            .ok_or_else(|| anyhow::anyhow!("file not found"))?;
        entry.parent_id = ItemId(dst_parent_id.0.clone());
        entry.name = new_name.to_string();
        entry.mod_time = Some(Utc::now());
        Ok(entry.clone())
    }

    async fn list_children(&self, parent_native_id: &str) -> anyhow::Result<Vec<FileEntry>> {
        let files = self
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prefix = format!("{}:", self.id);
        let parent_full = format!("{prefix}{parent_native_id}");
        let entries = files
            .values()
            .filter(|entry| entry.parent_id.0 == parent_full)
            .cloned()
            .collect();
        Ok(entries)
    }

    async fn poll_interval(&self) -> Option<Duration> {
        None
    }
}

/// Build an empty [`ChangeFeed`]. The feed is a passive index fed by
/// the sync runner in production; tests that exercise the feed-delta
/// path call [`ChangeFeed::record`] directly, and tests that exercise
/// the snapshot fallback simply leave it empty (every query returns
/// `Unknown`).
fn make_feed() -> Arc<ChangeFeed> {
    Arc::new(ChangeFeed::new())
}

fn make_handlers() -> (EngineHandlers, Arc<InMemoryBackend>, tempfile::TempDir) {
    let backend = Arc::new(InMemoryBackend::new("stub"));
    let vfs = Arc::new(RwLock::new(VfsTree::new(backend.clone())));
    let cache_dir = tempfile::tempdir().unwrap();

    let db = Arc::new(StateDb::open_in_memory().unwrap());
    db.register_backend("stub", "stub", "Stub", None, None)
        .unwrap();

    // Seed one parent directory and one child file.
    let parent_id = ItemId::new("stub", "root");
    let parent_entry = FileEntry {
        id: parent_id.clone(),
        parent_id: ItemId::new("stub", ""),
        name: "root".to_string(),
        path: "root".to_string(),
        is_dir: true,
        size: None,
        mod_time: Some(Utc::now()),
        mime_type: None,
        hash: None,
    };
    backend.insert(parent_entry.clone());
    db.upsert_file(&parent_entry).unwrap();

    let child_id = ItemId::new("stub", "file1");
    let child_entry = FileEntry {
        id: child_id.clone(),
        parent_id,
        name: "hello.txt".to_string(),
        path: "hello.txt".to_string(),
        is_dir: false,
        size: Some(11),
        mod_time: Some(Utc::now()),
        mime_type: Some("text/plain".to_string()),
        hash: None,
    };
    backend.insert(child_entry.clone());
    db.upsert_file(&child_entry).unwrap();
    backend
        .content
        .lock()
        .unwrap()
        .insert(child_id.0, b"hello world".to_vec());

    let feed = make_feed();
    let handlers = EngineHandlers::new(vfs, db, cache_dir.path().to_path_buf(), feed);
    (handlers, backend, cache_dir)
}

#[tokio::test]
async fn get_item_returns_metadata_for_known_id() {
    let (handlers, _backend, _tempdir) = make_handlers();
    let item = handlers.get_item("stub:file1").await.unwrap();
    assert_eq!(item.id, "stub:file1");
    assert_eq!(item.filename, "hello.txt");
    assert!(!item.is_directory);
}

#[tokio::test]
async fn get_item_returns_not_found_for_missing_id() {
    let (handlers, _backend, _tempdir) = make_handlers();
    let err = handlers.get_item("stub:missing").await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn enumerate_items_lists_children_of_directory() {
    let (handlers, _backend, _tempdir) = make_handlers();
    let output = handlers.enumerate_items("stub:root", None).await.unwrap();
    assert_eq!(output.items.len(), 1);
    assert_eq!(output.items[0].filename, "hello.txt");
    assert!(output.next_page.is_none());
}

#[tokio::test]
async fn enumerate_items_fails_for_unknown_backend() {
    let (handlers, _backend, _tempdir) = make_handlers();
    let err = handlers
        .enumerate_items("ghost:root", None)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn fetch_contents_materialises_file_to_cache_dir() {
    let (handlers, _backend, _tempdir) = make_handlers();
    let path = handlers.fetch_contents("stub:file1").await.unwrap();
    assert!(path.exists());
    let bytes = tokio::fs::read(&path).await.unwrap();
    assert_eq!(bytes, b"hello world");
}

#[tokio::test]
async fn fetch_contents_is_idempotent() {
    let (handlers, _backend, _tempdir) = make_handlers();
    let first = handlers.fetch_contents("stub:file1").await.unwrap();
    let second = handlers.fetch_contents("stub:file1").await.unwrap();
    assert_eq!(first, second);
}

#[tokio::test]
async fn fetch_contents_refuses_directories() {
    let (handlers, _backend, _tempdir) = make_handlers();
    let err = handlers.fetch_contents("stub:root").await.unwrap_err();
    assert_eq!(err.code, ErrorCode::PermissionDenied);
}

#[tokio::test]
async fn import_document_uploads_a_new_file() {
    let (handlers, _backend, tempdir) = make_handlers();
    let source = tempdir.path().join("upload.txt");
    tokio::fs::write(&source, b"new bytes").await.unwrap();

    let item = handlers
        .import_document(
            source.to_str().unwrap(),
            "stub:root",
            Some("upload.txt"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(item.filename, "upload.txt");
    assert_eq!(item.parent_id, "stub:root");
    assert_eq!(item.size, Some(9));
}

#[tokio::test]
async fn import_document_decodes_file_url_prefix() {
    let (handlers, _backend, tempdir) = make_handlers();
    let source = tempdir.path().join("u.txt");
    tokio::fs::write(&source, b"ok").await.unwrap();
    let url = format!("file://{}", source.display());

    let item = handlers
        .import_document(&url, "stub:root", Some("u.txt"), None)
        .await
        .unwrap();
    assert_eq!(item.filename, "u.txt");
}

#[tokio::test]
async fn import_document_overwrites_existing_when_existing_id_set() {
    let (handlers, _backend, tempdir) = make_handlers();
    let source = tempdir.path().join("replace.txt");
    tokio::fs::write(&source, b"new contents long enough")
        .await
        .unwrap();

    let item = handlers
        .import_document(
            source.to_str().unwrap(),
            "stub:root",
            None,
            Some("stub:file1"),
        )
        .await
        .unwrap();
    assert_eq!(item.id, "stub:file1");
    assert_eq!(item.size, Some(24));
}

#[tokio::test]
async fn create_directory_creates_under_parent() {
    let (handlers, _backend, _tempdir) = make_handlers();
    let item = handlers
        .create_directory("Photos", "stub:root")
        .await
        .unwrap();
    assert!(item.is_directory);
    assert_eq!(item.filename, "Photos");
    assert_eq!(item.parent_id, "stub:root");
}

#[tokio::test]
async fn delete_item_removes_known_id() {
    let (handlers, _backend, _tempdir) = make_handlers();
    handlers.delete_item("stub:file1").await.unwrap();
    let err = handlers.get_item("stub:file1").await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn delete_item_fails_for_missing_id() {
    let (handlers, _backend, _tempdir) = make_handlers();
    let err = handlers.delete_item("stub:missing").await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn move_item_renames_and_reparents_within_one_backend() {
    let (handlers, _backend, _tempdir) = make_handlers();

    // Create a second directory to move into.
    let new_dir = handlers
        .create_directory("Archive", "stub:root")
        .await
        .unwrap();

    let moved = handlers
        .move_item("stub:file1", &new_dir.id, "renamed.txt")
        .await
        .unwrap();
    assert_eq!(moved.filename, "renamed.txt");
    assert_eq!(moved.parent_id, new_dir.id);
}

#[tokio::test]
async fn move_item_cross_backend_unknown_destination_is_not_found() {
    // No `other` backend is registered, so the cross-backend branch
    // bails out at the destination lookup with NotFound.
    let (handlers, _backend, _tempdir) = make_handlers();
    let err = handlers
        .move_item("stub:file1", "other:root", "renamed.txt")
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
}

/// Stand up an `EngineHandlers` with two backends — `src` (mounted at
/// the root) and `dst` (mounted at `/dst`) — each pre-seeded with a
/// root directory entry. The source backend additionally holds a
/// single file `hello.txt` under its root with known bytes.
fn make_cross_backend_handlers() -> (
    EngineHandlers,
    Arc<InMemoryBackend>,
    Arc<InMemoryBackend>,
    tempfile::TempDir,
) {
    let src = Arc::new(InMemoryBackend::new("src"));
    let dst = Arc::new(InMemoryBackend::new("dst"));

    let mut tree = VfsTree::new(src.clone());
    tree.mount(PathBuf::from("/dst"), dst.clone());
    let vfs = Arc::new(RwLock::new(tree));
    let cache_dir = tempfile::tempdir().unwrap();

    let db = Arc::new(StateDb::open_in_memory().unwrap());
    db.register_backend("src", "stub", "Src", None, None)
        .unwrap();
    db.register_backend("dst", "stub", "Dst", Some("/dst"), None)
        .unwrap();

    for backend in [&src, &dst] {
        let root_id = ItemId::new(backend.id(), "root");
        let root_entry = FileEntry {
            id: root_id,
            parent_id: ItemId::new(backend.id(), ""),
            name: "root".to_string(),
            path: "root".to_string(),
            is_dir: true,
            size: None,
            mod_time: Some(Utc::now()),
            mime_type: None,
            hash: None,
        };
        backend.insert(root_entry.clone());
        db.upsert_file(&root_entry).unwrap();
    }

    let src_file_id = ItemId::new("src", "file1");
    let src_file = FileEntry {
        id: src_file_id.clone(),
        parent_id: ItemId::new("src", "root"),
        name: "hello.txt".to_string(),
        path: "hello.txt".to_string(),
        is_dir: false,
        size: Some(11),
        mod_time: Some(Utc::now()),
        mime_type: Some("text/plain".to_string()),
        hash: None,
    };
    src.insert(src_file.clone());
    db.upsert_file(&src_file).unwrap();
    src.content
        .lock()
        .unwrap()
        .insert(src_file_id.0, b"hello world".to_vec());

    let feed = make_feed();
    let handlers = EngineHandlers::new(vfs, db, cache_dir.path().to_path_buf(), feed);
    (handlers, src, dst, cache_dir)
}

#[tokio::test]
async fn move_item_cross_backend_file_succeeds() {
    let (handlers, src, dst, _tempdir) = make_cross_backend_handlers();

    let moved = handlers
        .move_item("src:file1", "dst:root", "renamed.txt")
        .await
        .unwrap();

    assert_eq!(moved.parent_id, "dst:root");
    assert_eq!(moved.filename, "renamed.txt");
    assert!(!moved.is_directory);

    // Destination has the file with the moved bytes.
    let dst_files = dst.files.lock().unwrap();
    let dst_entry = dst_files
        .values()
        .find(|entry| entry.name == "renamed.txt")
        .expect("destination should now contain the moved file");
    let dst_content = dst
        .content
        .lock()
        .unwrap()
        .get(&dst_entry.id.0)
        .cloned()
        .expect("destination must have content for moved file");
    assert_eq!(dst_content, b"hello world");

    // Source is gone.
    assert!(
        !src.files.lock().unwrap().contains_key("src:file1"),
        "source backend must no longer hold the original entry"
    );
    assert!(
        !src.content.lock().unwrap().contains_key("src:file1"),
        "source backend must no longer hold the original bytes"
    );
}

/// Insert a directory `FileEntry` into both the source backend and
/// the state DB so a cross-backend directory move can use it.
fn seed_src_directory(
    backend: &InMemoryBackend,
    db: &StateDb,
    native_id: &str,
    parent_native: &str,
    name: &str,
) -> FileEntry {
    let entry = FileEntry {
        id: ItemId::new(backend.id(), native_id),
        parent_id: ItemId::new(backend.id(), parent_native),
        name: name.to_string(),
        path: name.to_string(),
        is_dir: true,
        size: None,
        mod_time: Some(Utc::now()),
        mime_type: None,
        hash: None,
    };
    backend.insert(entry.clone());
    db.upsert_file(&entry).unwrap();
    entry
}

/// Insert a file `FileEntry` plus content into the source backend
/// and persist it in the state DB.
fn seed_src_file(
    backend: &InMemoryBackend,
    db: &StateDb,
    native_id: &str,
    parent_native: &str,
    name: &str,
    bytes: &[u8],
) -> FileEntry {
    let entry = FileEntry {
        id: ItemId::new(backend.id(), native_id),
        parent_id: ItemId::new(backend.id(), parent_native),
        name: name.to_string(),
        path: name.to_string(),
        is_dir: false,
        size: Some(bytes.len() as u64),
        mod_time: Some(Utc::now()),
        mime_type: Some("text/plain".to_string()),
        hash: None,
    };
    backend.insert(entry.clone());
    db.upsert_file(&entry).unwrap();
    backend
        .content
        .lock()
        .unwrap()
        .insert(entry.id.0.clone(), bytes.to_vec());
    entry
}

/// Count the entries under `dst_native_parent` in `backend` with
/// the given `name`. Used to assert that a recursive directory
/// copy landed where it was supposed to.
fn find_child(backend: &InMemoryBackend, parent_full_id: &str, name: &str) -> Option<FileEntry> {
    backend
        .files
        .lock()
        .unwrap()
        .values()
        .find(|entry| entry.parent_id.0 == parent_full_id && entry.name == name)
        .cloned()
}

#[tokio::test]
async fn move_item_cross_backend_empty_directory_succeeds() {
    let (handlers, src, dst, _tempdir) = make_cross_backend_handlers();

    // Source: a single empty directory directly under the root.
    seed_src_directory(&src, &handlers.db, "subdir", "root", "subdir");

    let moved = handlers
        .move_item("src:subdir", "dst:root", "subdir")
        .await
        .unwrap();

    assert!(moved.is_directory);
    assert_eq!(moved.parent_id, "dst:root");
    assert_eq!(moved.filename, "subdir");

    // Destination has a directory of the expected name; source
    // does not.
    assert!(
        find_child(&dst, "dst:root", "subdir").is_some(),
        "destination should contain the moved directory"
    );
    assert!(
        !src.files.lock().unwrap().contains_key("src:subdir"),
        "source backend must no longer hold the moved directory"
    );
}

#[tokio::test]
async fn move_item_cross_backend_flat_directory_with_files_succeeds() {
    let (handlers, src, dst, _tempdir) = make_cross_backend_handlers();

    // Source layout: /subdir/{a.txt, b.txt}.
    seed_src_directory(&src, &handlers.db, "subdir", "root", "subdir");
    seed_src_file(&src, &handlers.db, "a", "subdir", "a.txt", b"alpha");
    seed_src_file(&src, &handlers.db, "b", "subdir", "b.txt", b"bravo");

    let moved = handlers
        .move_item("src:subdir", "dst:root", "subdir")
        .await
        .unwrap();
    assert!(moved.is_directory);

    // Destination directory exists and holds both children with
    // their original bytes.
    let dst_dir = find_child(&dst, "dst:root", "subdir")
        .expect("destination should contain the moved directory");
    let dst_a = find_child(&dst, &dst_dir.id.0, "a.txt").expect("destination should contain a.txt");
    let dst_b = find_child(&dst, &dst_dir.id.0, "b.txt").expect("destination should contain b.txt");
    let dst_content = dst.content.lock().unwrap();
    assert_eq!(
        dst_content.get(&dst_a.id.0).map(Vec::as_slice),
        Some(b"alpha".as_slice())
    );
    assert_eq!(
        dst_content.get(&dst_b.id.0).map(Vec::as_slice),
        Some(b"bravo".as_slice())
    );

    // Source no longer holds the directory or its children.
    let src_files = src.files.lock().unwrap();
    assert!(!src_files.contains_key("src:subdir"));
    assert!(!src_files.contains_key("src:a"));
    assert!(!src_files.contains_key("src:b"));
}

#[tokio::test]
async fn move_item_cross_backend_nested_directory_succeeds() {
    let (handlers, src, dst, _tempdir) = make_cross_backend_handlers();

    // Source layout:
    //   /outer/
    //     top.txt
    //     /inner/
    //       deep.txt
    //       /innermost/
    //         leaf.txt
    seed_src_directory(&src, &handlers.db, "outer", "root", "outer");
    seed_src_file(&src, &handlers.db, "top", "outer", "top.txt", b"top-bytes");
    seed_src_directory(&src, &handlers.db, "inner", "outer", "inner");
    seed_src_file(
        &src,
        &handlers.db,
        "deep",
        "inner",
        "deep.txt",
        b"deep-bytes",
    );
    seed_src_directory(&src, &handlers.db, "innermost", "inner", "innermost");
    seed_src_file(
        &src,
        &handlers.db,
        "leaf",
        "innermost",
        "leaf.txt",
        b"leaf-bytes",
    );

    let moved = handlers
        .move_item("src:outer", "dst:root", "outer")
        .await
        .unwrap();
    assert!(moved.is_directory);

    // Walk the destination tree mirror by mirror and confirm
    // every node materialised in the right place with the right
    // bytes.
    let dst_outer = find_child(&dst, "dst:root", "outer").expect("dst/outer");
    let dst_top = find_child(&dst, &dst_outer.id.0, "top.txt").expect("dst/outer/top.txt");
    let dst_inner = find_child(&dst, &dst_outer.id.0, "inner").expect("dst/outer/inner");
    let dst_deep = find_child(&dst, &dst_inner.id.0, "deep.txt").expect("dst/outer/inner/deep.txt");
    let dst_innermost =
        find_child(&dst, &dst_inner.id.0, "innermost").expect("dst/outer/inner/innermost");
    let dst_leaf = find_child(&dst, &dst_innermost.id.0, "leaf.txt")
        .expect("dst/outer/inner/innermost/leaf.txt");

    let dst_content = dst.content.lock().unwrap();
    assert_eq!(
        dst_content.get(&dst_top.id.0).map(Vec::as_slice),
        Some(b"top-bytes".as_slice())
    );
    assert_eq!(
        dst_content.get(&dst_deep.id.0).map(Vec::as_slice),
        Some(b"deep-bytes".as_slice())
    );
    assert_eq!(
        dst_content.get(&dst_leaf.id.0).map(Vec::as_slice),
        Some(b"leaf-bytes".as_slice())
    );

    // Source subtree gone in its entirety.
    let src_files = src.files.lock().unwrap();
    for native in ["outer", "top", "inner", "deep", "innermost", "leaf"] {
        let id = format!("src:{native}");
        assert!(
            !src_files.contains_key(&id),
            "source backend still holds {id} after directory move"
        );
    }
}

/// A backend that wraps `InMemoryBackend` but fails the Nth call
/// to `upload` (1-indexed), letting tests trigger a partial
/// failure halfway through a recursive directory move.
#[derive(Debug)]
struct UploadFailingBackend {
    inner: InMemoryBackend,
    calls: Mutex<usize>,
    fail_on: usize,
}

impl UploadFailingBackend {
    fn new(id: &str, fail_on: usize) -> Self {
        Self {
            inner: InMemoryBackend::new(id),
            calls: Mutex::new(0),
            fail_on,
        }
    }

    fn insert(&self, entry: FileEntry) {
        self.inner.insert(entry);
    }
}

#[async_trait]
impl Backend for UploadFailingBackend {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn display_name(&self) -> &str {
        self.inner.display_name()
    }

    async fn quota(&self) -> anyhow::Result<Option<Quota>> {
        self.inner.quota().await
    }

    async fn changes(&self, cursor: Option<&Cursor>) -> anyhow::Result<(Vec<Change>, Cursor)> {
        self.inner.changes(cursor).await
    }

    async fn metadata(&self, path: &Path) -> anyhow::Result<FileEntry> {
        self.inner.metadata(path).await
    }

    async fn download(&self, file: &FileEntry) -> anyhow::Result<Vec<u8>> {
        self.inner.download(file).await
    }

    async fn upload(
        &self,
        path: &Path,
        data: &[u8],
        parent_id: &FileId,
    ) -> anyhow::Result<FileEntry> {
        let call = {
            let mut counter = self
                .calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *counter += 1;
            *counter
        };
        if call == self.fail_on {
            anyhow::bail!("simulated upload failure on call {call}");
        }
        self.inner.upload(path, data, parent_id).await
    }

    async fn update(&self, file_id: &FileId, data: &[u8]) -> anyhow::Result<FileEntry> {
        self.inner.update(file_id, data).await
    }

    async fn create_dir(&self, path: &Path) -> anyhow::Result<FileEntry> {
        self.inner.create_dir(path).await
    }

    async fn create_dir_with_parent(
        &self,
        name: &str,
        parent_id: &FileId,
    ) -> anyhow::Result<FileEntry> {
        self.inner.create_dir_with_parent(name, parent_id).await
    }

    async fn delete(&self, file: &FileEntry) -> anyhow::Result<()> {
        self.inner.delete(file).await
    }

    async fn move_entry(&self, src: &Path, dst: &Path) -> anyhow::Result<FileEntry> {
        self.inner.move_entry(src, dst).await
    }

    async fn move_by_id(
        &self,
        src_id: &FileId,
        dst_parent_id: &FileId,
        new_name: &str,
    ) -> anyhow::Result<FileEntry> {
        self.inner.move_by_id(src_id, dst_parent_id, new_name).await
    }

    async fn list_children(&self, parent_native_id: &str) -> anyhow::Result<Vec<FileEntry>> {
        self.inner.list_children(parent_native_id).await
    }

    async fn poll_interval(&self) -> Option<Duration> {
        self.inner.poll_interval().await
    }
}

#[tokio::test]
async fn move_item_cross_backend_directory_partial_failure_leaves_source_intact() {
    // Destination upload fails on the second call (i.e. after the
    // first child file has uploaded). The handler must surface an
    // error and leave the source subtree completely untouched.
    let src = Arc::new(InMemoryBackend::new("src"));
    let dst = Arc::new(UploadFailingBackend::new("dst", 2));

    let src_dyn: Arc<dyn Backend> = src.clone();
    let dst_dyn: Arc<dyn Backend> = dst.clone();
    let mut tree = VfsTree::new(src_dyn);
    tree.mount(PathBuf::from("/dst"), dst_dyn);
    let vfs = Arc::new(RwLock::new(tree));
    let cache_dir = tempfile::tempdir().unwrap();

    let db = Arc::new(StateDb::open_in_memory().unwrap());
    db.register_backend("src", "stub", "Src", None, None)
        .unwrap();
    db.register_backend("dst", "stub", "Dst", Some("/dst"), None)
        .unwrap();

    // Seed the source root.
    let src_root = FileEntry {
        id: ItemId::new("src", "root"),
        parent_id: ItemId::new("src", ""),
        name: "root".to_string(),
        path: "root".to_string(),
        is_dir: true,
        size: None,
        mod_time: Some(Utc::now()),
        mime_type: None,
        hash: None,
    };
    src.insert(src_root.clone());
    db.upsert_file(&src_root).unwrap();

    // Seed the destination root via the wrapper's inner backend.
    let dst_root = FileEntry {
        id: ItemId::new("dst", "root"),
        parent_id: ItemId::new("dst", ""),
        name: "root".to_string(),
        path: "root".to_string(),
        is_dir: true,
        size: None,
        mod_time: Some(Utc::now()),
        mime_type: None,
        hash: None,
    };
    dst.insert(dst_root.clone());
    db.upsert_file(&dst_root).unwrap();

    // Source layout: /subdir/{a.txt, b.txt}. The first upload
    // succeeds (a.txt) and the second fails (b.txt), leaving a
    // partial destination tree.
    seed_src_directory(&src, &db, "subdir", "root", "subdir");
    seed_src_file(&src, &db, "a", "subdir", "a.txt", b"alpha");
    seed_src_file(&src, &db, "b", "subdir", "b.txt", b"bravo");

    let feed = make_feed();
    let handlers = EngineHandlers::new(vfs, db.clone(), cache_dir.path().to_path_buf(), feed);

    let err = handlers
        .move_item("src:subdir", "dst:root", "subdir")
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Internal);

    // Source subtree is completely intact — neither the directory
    // nor any child file was deleted.
    let src_files = src.files.lock().unwrap();
    assert!(
        src_files.contains_key("src:subdir"),
        "source directory must still exist after partial failure"
    );
    assert!(
        src_files.contains_key("src:a"),
        "source child a.txt must still exist after partial failure"
    );
    assert!(
        src_files.contains_key("src:b"),
        "source child b.txt must still exist after partial failure"
    );

    // Source bytes intact too.
    let src_content = src.content.lock().unwrap();
    assert_eq!(
        src_content.get("src:a").map(Vec::as_slice),
        Some(b"alpha".as_slice())
    );
    assert_eq!(
        src_content.get("src:b").map(Vec::as_slice),
        Some(b"bravo".as_slice())
    );

    // State DB still knows about the source subtree.
    assert!(
        db.get_file(&ItemId::new("src", "subdir"))
            .unwrap()
            .is_some(),
        "state DB must still hold the source directory after partial failure"
    );
}

#[tokio::test]
async fn move_item_cross_backend_name_collision_returns_already_exists() {
    let (handlers, _src, dst, _tempdir) = make_cross_backend_handlers();

    // Pre-seed the destination with a file of the target name.
    let collide_id = ItemId::new("dst", "existing");
    let collide_entry = FileEntry {
        id: collide_id,
        parent_id: ItemId::new("dst", "root"),
        name: "renamed.txt".to_string(),
        path: "renamed.txt".to_string(),
        is_dir: false,
        size: Some(3),
        mod_time: Some(Utc::now()),
        mime_type: Some("text/plain".to_string()),
        hash: None,
    };
    dst.insert(collide_entry.clone());
    handlers.db.upsert_file(&collide_entry).unwrap();

    let err = handlers
        .move_item("src:file1", "dst:root", "renamed.txt")
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::AlreadyExists);
}

/// A backend whose `delete` always fails, used to simulate the
/// partial-failure path of a cross-backend move where the upload
/// committed but cleaning up the source did not.
#[derive(Debug)]
struct DeleteFailingBackend {
    inner: InMemoryBackend,
}

impl DeleteFailingBackend {
    fn new(id: &str) -> Self {
        Self {
            inner: InMemoryBackend::new(id),
        }
    }

    fn insert(&self, entry: FileEntry) {
        self.inner.insert(entry);
    }
}

#[async_trait]
impl Backend for DeleteFailingBackend {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn display_name(&self) -> &str {
        self.inner.display_name()
    }

    async fn quota(&self) -> anyhow::Result<Option<Quota>> {
        self.inner.quota().await
    }

    async fn changes(&self, cursor: Option<&Cursor>) -> anyhow::Result<(Vec<Change>, Cursor)> {
        self.inner.changes(cursor).await
    }

    async fn metadata(&self, path: &Path) -> anyhow::Result<FileEntry> {
        self.inner.metadata(path).await
    }

    async fn download(&self, file: &FileEntry) -> anyhow::Result<Vec<u8>> {
        self.inner.download(file).await
    }

    async fn upload(
        &self,
        path: &Path,
        data: &[u8],
        parent_id: &FileId,
    ) -> anyhow::Result<FileEntry> {
        self.inner.upload(path, data, parent_id).await
    }

    async fn update(&self, file_id: &FileId, data: &[u8]) -> anyhow::Result<FileEntry> {
        self.inner.update(file_id, data).await
    }

    async fn create_dir(&self, path: &Path) -> anyhow::Result<FileEntry> {
        self.inner.create_dir(path).await
    }

    async fn create_dir_with_parent(
        &self,
        name: &str,
        parent_id: &FileId,
    ) -> anyhow::Result<FileEntry> {
        self.inner.create_dir_with_parent(name, parent_id).await
    }

    async fn delete(&self, _file: &FileEntry) -> anyhow::Result<()> {
        anyhow::bail!("simulated delete failure")
    }

    async fn move_entry(&self, src: &Path, dst: &Path) -> anyhow::Result<FileEntry> {
        self.inner.move_entry(src, dst).await
    }

    async fn move_by_id(
        &self,
        src_id: &FileId,
        dst_parent_id: &FileId,
        new_name: &str,
    ) -> anyhow::Result<FileEntry> {
        self.inner.move_by_id(src_id, dst_parent_id, new_name).await
    }

    async fn list_children(&self, parent_native_id: &str) -> anyhow::Result<Vec<FileEntry>> {
        self.inner.list_children(parent_native_id).await
    }

    async fn poll_interval(&self) -> Option<Duration> {
        self.inner.poll_interval().await
    }
}

#[tokio::test]
async fn move_item_cross_backend_partial_failure_after_upload_returns_internal() {
    // Build a tree where the source backend's `delete` is rigged to
    // fail. The destination upload should still succeed and the
    // handler must report Internal with both IDs.
    let src = Arc::new(DeleteFailingBackend::new("src"));
    let dst = Arc::new(InMemoryBackend::new("dst"));

    let src_dyn: Arc<dyn Backend> = src.clone();
    let mut tree = VfsTree::new(src_dyn);
    tree.mount(PathBuf::from("/dst"), dst.clone());
    let vfs = Arc::new(RwLock::new(tree));
    let cache_dir = tempfile::tempdir().unwrap();

    let db = Arc::new(StateDb::open_in_memory().unwrap());
    db.register_backend("src", "stub", "Src", None, None)
        .unwrap();
    db.register_backend("dst", "stub", "Dst", Some("/dst"), None)
        .unwrap();

    // Seed the source root via the wrapper's insert (DeleteFailingBackend
    // does not own a public `insert` of its own — it delegates to its
    // inner `InMemoryBackend`).
    let src_root = FileEntry {
        id: ItemId::new("src", "root"),
        parent_id: ItemId::new("src", ""),
        name: "root".to_string(),
        path: "root".to_string(),
        is_dir: true,
        size: None,
        mod_time: Some(Utc::now()),
        mime_type: None,
        hash: None,
    };
    src.inner.insert(src_root.clone());
    db.upsert_file(&src_root).unwrap();

    let dst_root = FileEntry {
        id: ItemId::new("dst", "root"),
        parent_id: ItemId::new("dst", ""),
        name: "root".to_string(),
        path: "root".to_string(),
        is_dir: true,
        size: None,
        mod_time: Some(Utc::now()),
        mime_type: None,
        hash: None,
    };
    dst.insert(dst_root.clone());
    db.upsert_file(&dst_root).unwrap();

    let src_file_id = ItemId::new("src", "file1");
    let src_file = FileEntry {
        id: src_file_id.clone(),
        parent_id: ItemId::new("src", "root"),
        name: "hello.txt".to_string(),
        path: "hello.txt".to_string(),
        is_dir: false,
        size: Some(11),
        mod_time: Some(Utc::now()),
        mime_type: Some("text/plain".to_string()),
        hash: None,
    };
    src.insert(src_file.clone());
    db.upsert_file(&src_file).unwrap();
    src.inner
        .content
        .lock()
        .unwrap()
        .insert(src_file_id.0.clone(), b"hello world".to_vec());

    let feed = make_feed();
    let handlers = EngineHandlers::new(vfs, db.clone(), cache_dir.path().to_path_buf(), feed);

    let err = handlers
        .move_item("src:file1", "dst:root", "renamed.txt")
        .await
        .unwrap_err();

    assert_eq!(err.code, ErrorCode::Internal);
    assert!(
        err.message.contains("src:file1"),
        "message must name the source item: {}",
        err.message
    );
    // Destination should have the new file, and a matching entry should
    // have been persisted in the DB before the error returned.
    let dst_entry = dst
        .files
        .lock()
        .unwrap()
        .values()
        .find(|entry| entry.name == "renamed.txt")
        .cloned()
        .expect("destination upload must have committed");
    assert!(
        err.message.contains(&dst_entry.id.0),
        "message must name the destination item: {}",
        err.message
    );
    let persisted = db.get_file(&dst_entry.id).unwrap();
    assert!(
        persisted.is_some(),
        "destination must be in the DB even on partial failure"
    );
}

#[tokio::test]
async fn current_sync_cursor_returns_stable_value_when_no_changes() {
    let (handlers, _backend, _tempdir) = make_handlers();
    let first = handlers.current_sync_cursor("stub:root").await.unwrap();
    let second = handlers.current_sync_cursor("stub:root").await.unwrap();
    assert_eq!(first, second);
    assert!(!first.is_empty());
}

#[tokio::test]
async fn current_sync_cursor_changes_after_upsert() {
    let (handlers, backend, _tempdir) = make_handlers();
    let before = handlers.current_sync_cursor("stub:root").await.unwrap();

    backend.insert(FileEntry {
        id: ItemId::new("stub", "file2"),
        parent_id: ItemId::new("stub", "root"),
        name: "second.txt".to_string(),
        path: "second.txt".to_string(),
        is_dir: false,
        size: Some(4),
        mod_time: Some(Utc::now()),
        mime_type: None,
        hash: None,
    });

    let after = handlers.current_sync_cursor("stub:root").await.unwrap();
    assert_ne!(before, after);
}

#[tokio::test]
async fn current_sync_cursor_fails_for_unknown_backend() {
    let (handlers, _backend, _tempdir) = make_handlers();
    let err = handlers
        .current_sync_cursor("ghost:root")
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
}

/// Populate the stub backend with `count` extra files under `stub:root`
/// with deterministic names so the sort order is stable and easy to
/// reason about across pages.
fn seed_children(backend: &InMemoryBackend, count: usize) {
    let parent = ItemId::new("stub", "root");
    for index in 0..count {
        // Zero-pad to 4 digits so lexicographic sort matches numeric order.
        let native = format!("p{index:04}");
        let item_id = ItemId::new("stub", &native);
        let entry = FileEntry {
            id: item_id,
            parent_id: parent.clone(),
            name: format!("file-{index:04}.txt"),
            path: format!("file-{index:04}.txt"),
            is_dir: false,
            size: Some(1),
            mod_time: Some(Utc::now()),
            mime_type: Some("text/plain".to_string()),
            hash: None,
        };
        backend.insert(entry);
    }
}

#[tokio::test]
async fn enumerate_items_returns_null_next_page_when_done() {
    let (handlers, _backend, _tempdir) = make_handlers();
    let output = handlers.enumerate_items("stub:root", None).await.unwrap();
    // Only one seeded child; page size 256, so we're done in one shot.
    assert_eq!(output.items.len(), 1);
    assert!(output.next_page.is_none());
}

#[tokio::test]
async fn enumerate_items_paginates_when_more_than_one_page_of_children() {
    let (handlers, backend, _tempdir) = make_handlers();
    // Replace the seed child with a clean slate plus a known count.
    backend
        .files
        .lock()
        .unwrap()
        .retain(|_, entry| entry.is_dir);
    let total: usize = 600;
    seed_children(&backend, total);

    let mut seen: Vec<String> = Vec::new();
    let mut page_cursor: Option<String> = None;
    loop {
        let output = handlers
            .enumerate_items("stub:root", page_cursor.as_deref())
            .await
            .unwrap();
        for item in &output.items {
            seen.push(item.id.clone());
        }
        match output.next_page {
            Some(cursor) => page_cursor = Some(cursor),
            None => break,
        }
        assert!(
            seen.len() < total,
            "consumed all items but engine kept emitting next_page"
        );
    }

    assert_eq!(
        seen.len(),
        total,
        "all children must be returned exactly once"
    );
    let mut deduped = seen.clone();
    deduped.sort();
    deduped.dedup();
    assert_eq!(deduped.len(), total, "no duplicates across pages");
}

#[tokio::test]
async fn enumerate_items_rejects_malformed_page_cursor() {
    let (handlers, _backend, _tempdir) = make_handlers();
    let err = handlers
        .enumerate_items("stub:root", Some("!!!not-base64!!!"))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Internal);
}

/// First call with no prior cursor must return every current child
/// as added, an empty deleted set, and a non-empty new cursor.
#[tokio::test]
async fn enumerate_changes_first_call_returns_all_as_added() -> anyhow::Result<()> {
    let (handlers, _backend, _tempdir) = make_handlers();
    let output = handlers
        .enumerate_changes("stub:root", None)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.message))?;

    // Seeded child is `stub:file1` with name "hello.txt".
    assert_eq!(output.added_or_modified.len(), 1);
    assert_eq!(output.added_or_modified[0].id, "stub:file1");
    assert!(output.deleted.is_empty());
    assert!(!output.new_cursor.is_empty());

    // The cursor returned by enumerate_changes is now a V2 wire
    // cursor that names (backend, parent, feed seq); it is no
    // longer the SHA-256 derivation that current_sync_cursor
    // returns. Verify the new cursor decodes back to the right
    // backend/parent pair.
    let decoded = SyncCursorV2::decode(&output.new_cursor)
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .ok_or_else(|| anyhow::anyhow!("V2 cursor must be present"))?;
    assert_eq!(decoded.backend_id, "stub");
    assert_eq!(decoded.parent_id, ItemId::new("stub", "root"));
    Ok(())
}

/// When the change feed has recorded events for the parent, the
/// feed-delta path serves them directly rather than falling back to
/// the snapshot diff. Proven by recording events that the backend
/// does not reflect: the snapshot path could not surface them, so a
/// result that contains them must have come from the feed.
#[tokio::test]
async fn enumerate_changes_uses_feed_delta_when_recorded() -> anyhow::Result<()> {
    let (handlers, _backend, _tempdir) = make_handlers();
    let parent = ItemId::new("stub", "root");

    // Record two events into the feed without touching the backend.
    let ghost1 = FileEntry {
        id: ItemId::new("stub", "ghost1"),
        parent_id: parent.clone(),
        name: "g1.txt".to_string(),
        path: "g1.txt".to_string(),
        is_dir: false,
        size: Some(1),
        mod_time: Some(Utc::now()),
        mime_type: None,
        hash: None,
    };
    let ghost2 = FileEntry {
        id: ItemId::new("stub", "ghost2"),
        parent_id: parent.clone(),
        name: "g2.txt".to_string(),
        path: "g2.txt".to_string(),
        is_dir: false,
        size: Some(2),
        mod_time: Some(Utc::now()),
        mime_type: None,
        hash: None,
    };
    handlers
        .change_feed
        .record("stub", &[Change::Created(ghost1), Change::Created(ghost2)])
        .await;

    // A V2 cursor naming (stub, root) with feed_seq=0 queries the feed
    // for events strictly after seq 0 — i.e. the second recorded
    // event (seq 1).
    let cursor = SyncCursorV2 {
        backend_id: "stub".to_string(),
        parent_id: parent.clone(),
        feed_seq: 0,
        snapshot_hash: Vec::new(),
    }
    .encode();

    let out = handlers
        .enumerate_changes("stub:root", Some(&cursor))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.message))?;

    let ids: Vec<&str> = out
        .added_or_modified
        .iter()
        .map(|item| item.id.as_str())
        .collect();
    assert!(
        ids.contains(&"stub:ghost2"),
        "feed-path delta must surface the recorded event the backend does not have; got {ids:?}"
    );

    // The returned cursor must advance to the feed head (seq 1).
    let decoded = SyncCursorV2::decode(&out.new_cursor)
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .ok_or_else(|| anyhow::anyhow!("V2 cursor expected"))?;
    assert_eq!(decoded.feed_seq, 1);
    Ok(())
}

/// After the snapshot is taken, an unchanged view must yield no
/// deltas and the same cursor.
#[tokio::test]
async fn enumerate_changes_unchanged_view_returns_empty_delta() {
    let (handlers, _backend, _tempdir) = make_handlers();
    let first = handlers.enumerate_changes("stub:root", None).await.unwrap();

    let second = handlers
        .enumerate_changes("stub:root", Some(&first.new_cursor))
        .await
        .unwrap();

    assert!(second.added_or_modified.is_empty());
    assert!(second.deleted.is_empty());
    assert_eq!(second.new_cursor, first.new_cursor);
}

/// Adding a child, deleting a child, and modifying a child between
/// two snapshots must surface in the delta as added/deleted/modified.
#[tokio::test]
async fn enumerate_changes_reflects_add_delete_modify() {
    let (handlers, backend, _tempdir) = make_handlers();
    let first = handlers.enumerate_changes("stub:root", None).await.unwrap();

    // Add a new file.
    let new_file_id = ItemId::new("stub", "file2");
    backend.insert(FileEntry {
        id: new_file_id.clone(),
        parent_id: ItemId::new("stub", "root"),
        name: "second.txt".to_string(),
        path: "second.txt".to_string(),
        is_dir: false,
        size: Some(4),
        mod_time: Some(Utc::now()),
        mime_type: None,
        hash: None,
    });

    // Modify the existing file (bump size).
    {
        let mut files = backend
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = files.get_mut("stub:file1").unwrap();
        entry.size = Some(42);
        entry.mod_time = Some(Utc::now());
    }

    // Note: no deletion yet — set up a second snapshot first so we
    // can split add / modify from delete cleanly.
    let second = handlers
        .enumerate_changes("stub:root", Some(&first.new_cursor))
        .await
        .unwrap();

    let ids: Vec<&str> = second
        .added_or_modified
        .iter()
        .map(|item| item.id.as_str())
        .collect();
    assert!(
        ids.contains(&"stub:file2"),
        "newly inserted file must appear as added"
    );
    assert!(
        ids.contains(&"stub:file1"),
        "metadata change must appear as modified"
    );
    assert!(second.deleted.is_empty(), "no deletes yet");
    assert_ne!(second.new_cursor, first.new_cursor);

    // Now delete the original file and check the delta.
    backend
        .files
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove("stub:file1");

    let third = handlers
        .enumerate_changes("stub:root", Some(&second.new_cursor))
        .await
        .unwrap();
    assert!(
        third.added_or_modified.is_empty(),
        "no further additions after delete"
    );
    assert_eq!(third.deleted, vec!["stub:file1".to_string()]);
    assert_ne!(third.new_cursor, second.new_cursor);
}

/// A cursor the engine does not recognise (either because the
/// snapshot was dropped or because the client is on a stale cursor
/// from a different run) must behave like a first call: every child
/// reported as added, nothing reported as deleted.
#[tokio::test]
async fn enumerate_changes_cursor_mismatch_behaves_like_first_call() {
    let (handlers, _backend, _tempdir) = make_handlers();
    // Seed the snapshot first so we know mismatched cursors aren't
    // simply "no snapshot stored".
    let _ = handlers.enumerate_changes("stub:root", None).await.unwrap();

    let bogus = SyncCursor::new(vec![0xde, 0xad, 0xbe, 0xef]);
    let output = handlers
        .enumerate_changes("stub:root", Some(&bogus))
        .await
        .unwrap();

    // Everything seeded under the parent must come back as added.
    assert_eq!(output.added_or_modified.len(), 1);
    assert_eq!(output.added_or_modified[0].id, "stub:file1");
    assert!(output.deleted.is_empty());
    assert!(!output.new_cursor.is_empty());
}

/// Snapshots are keyed by parent ID; deltas for one parent must not
/// bleed into another. Sanity check that two independent parents
/// each carry their own snapshot.
#[tokio::test]
async fn enumerate_changes_per_parent_isolation() {
    let (handlers, backend, _tempdir) = make_handlers();

    // Add a second directory with its own child.
    let other_dir = ItemId::new("stub", "other");
    backend.insert(FileEntry {
        id: other_dir.clone(),
        parent_id: ItemId::new("stub", "root"),
        name: "other".to_string(),
        path: "other".to_string(),
        is_dir: true,
        size: None,
        mod_time: Some(Utc::now()),
        mime_type: None,
        hash: None,
    });
    backend.insert(FileEntry {
        id: ItemId::new("stub", "other-child"),
        parent_id: other_dir.clone(),
        name: "child.txt".to_string(),
        path: "child.txt".to_string(),
        is_dir: false,
        size: Some(1),
        mod_time: Some(Utc::now()),
        mime_type: None,
        hash: None,
    });

    let root_first = handlers.enumerate_changes("stub:root", None).await.unwrap();
    let other_first = handlers
        .enumerate_changes("stub:other", None)
        .await
        .unwrap();

    // Modify only the `other` parent.
    backend
        .files
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove("stub:other-child");

    let root_second = handlers
        .enumerate_changes("stub:root", Some(&root_first.new_cursor))
        .await
        .unwrap();
    let other_second = handlers
        .enumerate_changes("stub:other", Some(&other_first.new_cursor))
        .await
        .unwrap();

    assert!(
        root_second.added_or_modified.is_empty() && root_second.deleted.is_empty(),
        "root parent must be unchanged"
    );
    assert_eq!(
        other_second.deleted,
        vec!["stub:other-child".to_string()],
        "only the touched parent should see the delete"
    );
}

#[tokio::test]
async fn enumerate_changes_fails_for_unknown_backend() {
    let (handlers, _backend, _tempdir) = make_handlers();
    let err = handlers
        .enumerate_changes("ghost:root", None)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
}

/// The cursor returned by the first call to `enumerate_changes` must
/// decode back to a V2 sync cursor that names the requested backend
/// and parent and stores the V1 SHA-256 snapshot hash. Subsequent
/// calls carrying that cursor must drive the change-feed path
/// rather than the snapshot fallback.
#[tokio::test]
async fn enumerate_changes_first_call_stores_v2_cursor_and_snapshot() -> anyhow::Result<()> {
    let (handlers, _backend, _tempdir) = make_handlers();
    let first = handlers
        .enumerate_changes("stub:root", None)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.message))?;

    let decoded = SyncCursorV2::decode(&first.new_cursor)
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .ok_or_else(|| anyhow::anyhow!("V2 cursor must be present"))?;
    assert_eq!(decoded.backend_id, "stub");
    assert_eq!(decoded.parent_id, ItemId::new("stub", "root"));
    // The snapshot hash mirrors derive_sync_cursor for the same
    // child set.
    let live = handlers
        .current_sync_cursor("stub:root")
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.message))?;
    assert_eq!(decoded.snapshot_hash, live.as_bytes());

    // Replaying the same cursor with no change must return an
    // empty delta and the same cursor.
    let again = handlers
        .enumerate_changes("stub:root", Some(&first.new_cursor))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.message))?;
    assert!(again.added_or_modified.is_empty());
    assert!(again.deleted.is_empty());
    assert_eq!(again.new_cursor, first.new_cursor);
    Ok(())
}

/// A stale V1 cursor (no `CF2` magic) must drop to the snapshot
/// fallback and be treated as a fresh enumeration on the first
/// observation, then resume cleanly on the next call.
#[tokio::test]
async fn enumerate_changes_legacy_v1_cursor_falls_back_to_snapshot() -> anyhow::Result<()> {
    let (handlers, _backend, _tempdir) = make_handlers();
    // Seed the snapshot store so the legacy cursor can be matched
    // against it.
    let _ = handlers
        .enumerate_changes("stub:root", None)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.message))?;

    // Fabricate a V1-shaped cursor that doesn't carry the V2 magic
    // — this is what an older daemon would have stored on disk.
    let legacy = handlers
        .current_sync_cursor("stub:root")
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.message))?;
    let fallback = handlers
        .enumerate_changes("stub:root", Some(&legacy))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.message))?;

    // Snapshot fallback diffs against the stored snapshot, which
    // matches the live state, so this is an empty delta.
    assert!(fallback.added_or_modified.is_empty());
    assert!(fallback.deleted.is_empty());
    // But the returned cursor is now V2 so the next call resumes
    // the feed path.
    let new_decoded = SyncCursorV2::decode(&fallback.new_cursor)
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .ok_or_else(|| anyhow::anyhow!("returned cursor must be V2 even after legacy input"))?;
    assert_eq!(new_decoded.backend_id, "stub");
    Ok(())
}
