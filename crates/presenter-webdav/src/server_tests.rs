//! Test module for `server.rs`, split out to keep the
//! parent file under the source-length cap. Declared from there via
//! `#[cfg(test)] #[path = "server_tests.rs"] mod tests;`, so it stays a child
//! module with full access to the parent's private items.

use super::*;
use cascade_engine::types::{CacheState, ItemId};

#[test]
fn parse_range_full_form() {
    assert_eq!(parse_range("bytes=0-99", 1000), Ok((0, 99)));
    assert_eq!(parse_range("bytes=500-999", 1000), Ok((500, 999)));
}

#[test]
fn parse_range_open_ended() {
    assert_eq!(parse_range("bytes=500-", 1000), Ok((500, 999)));
    assert_eq!(parse_range("bytes=0-", 1000), Ok((0, 999)));
}

#[test]
fn parse_range_suffix() {
    assert_eq!(parse_range("bytes=-100", 1000), Ok((900, 999)));
    // Suffix larger than total clamps to whole file.
    assert_eq!(parse_range("bytes=-2000", 1000), Ok((0, 999)));
}

#[test]
fn parse_range_end_clamps_to_size() {
    assert_eq!(parse_range("bytes=0-9999", 1000), Ok((0, 999)));
}

#[test]
fn parse_range_rejects_invalid() {
    assert!(parse_range("bytes=", 1000).is_err());
    assert!(parse_range("bytes=-", 1000).is_err());
    assert!(parse_range("bytes=abc", 1000).is_err());
    assert!(parse_range("range=0-99", 1000).is_err());
    // start past end of file
    assert!(parse_range("bytes=1000-", 1000).is_err());
    // start > end
    assert!(parse_range("bytes=500-100", 1000).is_err());
    // multi-range not supported
    assert!(parse_range("bytes=0-99,200-299", 1000).is_err());
    // zero-byte file
    assert!(parse_range("bytes=0-", 0).is_err());
}

fn make_item(name: &str, is_dir: bool) -> VfsItem {
    VfsItem {
        id: ItemId::new("gdrive", name),
        parent_id: ItemId::new("gdrive", ""),
        // In tests the mount is "gdrive", so the path is "gdrive/<name>".
        path: format!("gdrive/{name}"),
        name: name.to_string(),
        is_dir,
        size: None,
        mod_time: None,
        cache_state: CacheState::Online,
        mime_type: None,
    }
}

#[test]
fn propfind_xml_root_collection() {
    let empty = HashMap::new();
    let xml = build_propfind_response("/", None, &[], &empty);
    assert!(xml.contains("<?xml version=\"1.0\""));
    assert!(xml.contains("DAV:"));
    assert!(xml.contains("<D:collection/>"));
    assert!(xml.contains("HTTP/1.1 200 OK"));
}

#[test]
fn propfind_xml_with_file() {
    let item = make_item("test.txt", false);
    let empty = HashMap::new();
    let xml = build_propfind_response("/gdrive/test.txt", Some(&item), &[], &empty);
    // Item has size=None, so no content-length element.
    assert!(!xml.contains("<D:collection/>"));
    assert!(xml.contains("/gdrive/test.txt"));
}

#[test]
fn propfind_xml_with_file_with_size() {
    let mut item = make_item("test.txt", false);
    item.size = Some(1024);
    let empty = HashMap::new();
    let xml = build_propfind_response("/gdrive/test.txt", Some(&item), &[], &empty);
    assert!(xml.contains("<D:getcontentlength>1024</D:getcontentlength>"));
    assert!(!xml.contains("<D:collection/>"));
    assert!(xml.contains("/gdrive/test.txt"));
}

#[test]
fn propfind_xml_with_directory() {
    let item = make_item("Documents", true);
    let empty = HashMap::new();
    let xml = build_propfind_response("/gdrive/Documents", Some(&item), &[], &empty);
    assert!(xml.contains("<D:collection/>"));
    assert!(xml.contains("/gdrive/Documents"));
}

#[test]
fn propfind_xml_with_children() {
    let parent = make_item("Documents", true);
    let child = make_item("readme.txt", false);
    let empty = HashMap::new();
    let xml = build_propfind_response("/gdrive/Documents", Some(&parent), &[&child], &empty);
    assert!(xml.contains("<D:collection/>"));
    assert!(xml.contains("readme.txt"));
    // Two responses — parent + child.
    assert_eq!(xml.matches("<D:response>").count(), 2);
}

#[test]
fn xml_escape_handles_special_chars() {
    assert_eq!(xml_escape("a&b<c>d"), "a&amp;b&lt;c&gt;d");
}

#[test]
fn normalise_path_removes_trailing_slash() {
    assert_eq!(normalise_path("/foo/bar/"), "/foo/bar");
}

#[test]
fn normalise_path_empty_is_root() {
    assert_eq!(normalise_path(""), "/");
}

#[test]
fn normalise_path_strips_absolute_uri() {
    assert_eq!(
        normalise_path("http://localhost:50217/gdrive-personal/a/b.txt"),
        "/gdrive-personal/a/b.txt"
    );
}

#[test]
fn item_path_from_vfs_item() {
    let item = make_item("test.txt", false);
    assert_eq!(item_path(&item), "/gdrive/test.txt");
}

#[tokio::test]
async fn server_starts_and_stops() {
    let items = Arc::new(RwLock::new(HashMap::new()));
    let cache_dir = tempfile::tempdir().unwrap();
    let server = WebDavServer::start(
        "127.0.0.1:0",
        items,
        cache_dir.path().to_path_buf(),
        Arc::new(tokio::sync::RwLock::new(Vec::new())),
        Arc::new(tokio::sync::RwLock::new(Vec::new())),
        None,
    )
    .await
    .unwrap();
    assert!(server.port() > 0);
    server.stop().unwrap();
}

#[tokio::test]
async fn server_propfind_returns_multistatus() {
    let items = Arc::new(RwLock::new(HashMap::new()));
    let cache_dir = tempfile::tempdir().unwrap();
    let server = WebDavServer::start(
        "127.0.0.1:0",
        items.clone(),
        cache_dir.path().to_path_buf(),
        Arc::new(tokio::sync::RwLock::new(Vec::new())),
        Arc::new(tokio::sync::RwLock::new(Vec::new())),
        None,
    )
    .await
    .unwrap();
    let port = server.port();

    let client = reqwest::Client::new();
    let resp = client
        .request(
            reqwest::Method::from_bytes(b"PROPFIND").unwrap(),
            format!("http://127.0.0.1:{port}/"),
        )
        .header("Depth", "0")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::MULTI_STATUS);
    let body = resp.text().await.unwrap();
    assert!(body.contains("multistatus"));
    assert!(body.contains("DAV:"));

    server.stop().unwrap();
}

#[tokio::test]
async fn server_get_returns_not_found_for_missing() {
    let items = Arc::new(RwLock::new(HashMap::new()));
    let cache_dir = tempfile::tempdir().unwrap();
    let server = WebDavServer::start(
        "127.0.0.1:0",
        items,
        cache_dir.path().to_path_buf(),
        Arc::new(tokio::sync::RwLock::new(Vec::new())),
        Arc::new(tokio::sync::RwLock::new(Vec::new())),
        None,
    )
    .await
    .unwrap();
    let port = server.port();

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://127.0.0.1:{port}/nonexistent"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
    server.stop().unwrap();
}

#[tokio::test]
#[ignore = "requires a registered backend to route write operations"]
async fn server_put_and_get_roundtrip() {
    let items = Arc::new(RwLock::new(HashMap::new()));
    let cache_dir = tempfile::tempdir().unwrap();
    let server = WebDavServer::start(
        "127.0.0.1:0",
        items,
        cache_dir.path().to_path_buf(),
        Arc::new(tokio::sync::RwLock::new(Vec::new())),
        Arc::new(tokio::sync::RwLock::new(Vec::new())),
        None,
    )
    .await
    .unwrap();
    let port = server.port();

    let client = reqwest::Client::new();

    // PUT a file.
    let resp = client
        .put(format!("http://127.0.0.1:{port}/test.txt"))
        .body(b"hello world".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    // GET it back.
    let resp = client
        .get(format!("http://127.0.0.1:{port}/test.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body = resp.bytes().await.unwrap();
    assert_eq!(&*body, b"hello world");

    server.stop().unwrap();
}

#[tokio::test]
#[ignore = "requires a registered backend to route write operations"]
async fn server_mkcol_creates_directory() {
    let items = Arc::new(RwLock::new(HashMap::new()));
    let cache_dir = tempfile::tempdir().unwrap();
    let server = WebDavServer::start(
        "127.0.0.1:0",
        items.clone(),
        cache_dir.path().to_path_buf(),
        Arc::new(tokio::sync::RwLock::new(Vec::new())),
        Arc::new(tokio::sync::RwLock::new(Vec::new())),
        None,
    )
    .await
    .unwrap();
    let port = server.port();

    let client = reqwest::Client::new();
    let resp = client
        .request(
            reqwest::Method::from_bytes(b"MKCOL").unwrap(),
            format!("http://127.0.0.1:{port}/newdir"),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    // Verify it appears in items.
    let items_guard = items.read().await;
    assert!(
        items_guard
            .values()
            .any(|item| item.name == "newdir" && item.is_dir)
    );

    server.stop().unwrap();
}

#[tokio::test]
#[ignore = "requires a registered backend to route write operations"]
async fn server_delete_removes_item() {
    let items = Arc::new(RwLock::new(HashMap::new()));
    let cache_dir = tempfile::tempdir().unwrap();
    let server = WebDavServer::start(
        "127.0.0.1:0",
        items.clone(),
        cache_dir.path().to_path_buf(),
        Arc::new(tokio::sync::RwLock::new(Vec::new())),
        Arc::new(tokio::sync::RwLock::new(Vec::new())),
        None,
    )
    .await
    .unwrap();
    let port = server.port();

    let client = reqwest::Client::new();

    // PUT a file first.
    client
        .put(format!("http://127.0.0.1:{port}/todelete.txt"))
        .body(b"data".to_vec())
        .send()
        .await
        .unwrap();

    // DELETE it.
    let resp = client
        .delete(format!("http://127.0.0.1:{port}/todelete.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);

    server.stop().unwrap();
}

#[tokio::test]
async fn server_options_returns_dav_header() {
    let items = Arc::new(RwLock::new(HashMap::new()));
    let cache_dir = tempfile::tempdir().unwrap();
    let server = WebDavServer::start(
        "127.0.0.1:0",
        items,
        cache_dir.path().to_path_buf(),
        Arc::new(tokio::sync::RwLock::new(Vec::new())),
        Arc::new(tokio::sync::RwLock::new(Vec::new())),
        None,
    )
    .await
    .unwrap();
    let port = server.port();

    let client = reqwest::Client::new();
    let resp = client
        .request(
            reqwest::Method::OPTIONS,
            format!("http://127.0.0.1:{port}/"),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
    let dav_header = resp.headers().get("dav").unwrap();
    assert_eq!(dav_header, "1, 2");

    server.stop().unwrap();
}

// ── Mount-table routing ────────────────────────────────────────────────

use cascade_engine::backend::Backend;
use cascade_engine::types::{Change, Cursor, FileEntry, FileId, Quota};
use std::sync::Mutex as StdMutex;
use std::time::Duration;

/// Backend that records the paths it was asked to upload, so a test can
/// assert which backend a write routed to. `upload` succeeds and returns a
/// file entry under the backend's own root.
///
/// `children` maps a parent native id to the entries `list_children` should
/// return, so a test can drive the presenter's on-demand expansion
/// (`expand_root`/`expand_directory`) and assert the resulting item paths.
/// `download` returns fixed bytes so a `GET` against an expanded file fully
/// succeeds rather than erroring, proving the path resolved.
#[derive(Debug)]
struct RecordingBackend {
    id: String,
    uploads: Arc<StdMutex<Vec<String>>>,
    /// Native id of the parent each `upload` was routed to, so a test can
    /// assert parent resolution (e.g. a nested file lands under its directory,
    /// not the backend root).
    upload_parents: Arc<StdMutex<Vec<String>>>,
    children: HashMap<String, Vec<FileEntry>>,
}

impl RecordingBackend {
    fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            uploads: Arc::new(StdMutex::new(Vec::new())),
            upload_parents: Arc::new(StdMutex::new(Vec::new())),
            children: HashMap::new(),
        }
    }

    /// Register the children `list_children(parent_native_id)` returns.
    fn with_children(mut self, parent_native_id: &str, entries: Vec<FileEntry>) -> Self {
        self.children.insert(parent_native_id.to_string(), entries);
        self
    }
}

#[async_trait::async_trait]
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
        Ok((vec![], Cursor("rec".to_string())))
    }
    async fn metadata(&self, _path: &Path) -> anyhow::Result<FileEntry> {
        anyhow::bail!("no metadata")
    }
    async fn download(&self, _file: &FileEntry) -> anyhow::Result<Vec<u8>> {
        Ok(b"recording-backend-contents".to_vec())
    }
    async fn list_children(&self, parent_native_id: &str) -> anyhow::Result<Vec<FileEntry>> {
        Ok(self
            .children
            .get(parent_native_id)
            .cloned()
            .unwrap_or_default())
    }
    async fn upload(
        &self,
        path: &Path,
        _data: &[u8],
        parent_id: &FileId,
    ) -> anyhow::Result<FileEntry> {
        self.uploads
            .lock()
            .unwrap()
            .push(path.to_string_lossy().into_owned());
        self.upload_parents
            .lock()
            .unwrap()
            .push(parent_id.0.clone());
        let name = path
            .file_name()
            .map_or_else(|| "file".to_string(), |n| n.to_string_lossy().into_owned());
        Ok(FileEntry::file(
            ItemId::new(&self.id, "uploaded"),
            ItemId(parent_id.0.clone()),
            name,
        ))
    }
    async fn update(&self, _file_id: &FileId, _data: &[u8]) -> anyhow::Result<FileEntry> {
        anyhow::bail!("no update")
    }
    async fn create_dir(&self, _path: &Path) -> anyhow::Result<FileEntry> {
        anyhow::bail!("no create_dir")
    }
    async fn delete(&self, _file: &FileEntry) -> anyhow::Result<()> {
        anyhow::bail!("no delete")
    }
    async fn move_entry(&self, _src: &Path, _dst: &Path) -> anyhow::Result<FileEntry> {
        anyhow::bail!("no move")
    }
    async fn poll_interval(&self) -> Option<Duration> {
        None
    }
}

/// Wrap a mount table in the shared-state shape `WebDavServer::start` wants.
fn mount_table(
    pairs: Vec<(PathBuf, Arc<dyn Backend>)>,
) -> Arc<tokio::sync::RwLock<Vec<(PathBuf, Arc<dyn Backend>)>>> {
    Arc::new(tokio::sync::RwLock::new(pairs))
}

fn backend_list(
    backends: Vec<Arc<dyn Backend>>,
) -> Arc<tokio::sync::RwLock<Vec<Arc<dyn Backend>>>> {
    Arc::new(tokio::sync::RwLock::new(backends))
}

fn empty_state(
    mounts: Arc<tokio::sync::RwLock<Vec<(PathBuf, Arc<dyn Backend>)>>>,
    backends: Arc<tokio::sync::RwLock<Vec<Arc<dyn Backend>>>>,
) -> AppState {
    AppState {
        items: Arc::new(RwLock::new(HashMap::new())),
        cache_dir: PathBuf::from("/tmp/cascade-test-cache"),
        backends,
        mounts,
        db: None,
        expanded: Arc::new(RwLock::new(std::collections::HashSet::new())),
        expand_sem: Arc::new(tokio::sync::Semaphore::new(4)),
    }
}

#[tokio::test]
async fn backend_for_path_routes_custom_named_mount() {
    // Backend id differs from its mount name: id `gdrive-personal` mounted
    // at `personal`. A path under `personal/...` must route to it, not be
    // looked up by the literal first segment as a backend id.
    let backend: Arc<dyn Backend> = Arc::new(RecordingBackend::new("gdrive-personal"));
    let mounts = mount_table(vec![(PathBuf::from("personal"), backend.clone())]);
    let state = empty_state(mounts, backend_list(vec![backend.clone()]));

    let (routed, rest) = backend_for_path(&state, "personal/Documents/report.txt").await;
    assert_eq!(routed.unwrap().id(), "gdrive-personal");
    assert_eq!(rest, "Documents/report.txt");
}

#[tokio::test]
async fn backend_for_path_routes_at_root_backend() {
    // A backend mounted at "/" (empty prefix) owns every path, and the
    // backend-relative path is the whole VFS path unchanged.
    let backend: Arc<dyn Backend> = Arc::new(RecordingBackend::new("gdrive"));
    let mounts = mount_table(vec![(PathBuf::new(), backend.clone())]);
    let state = empty_state(mounts, backend_list(vec![backend.clone()]));

    let (routed, rest) = backend_for_path(&state, "Documents/report.txt").await;
    assert_eq!(routed.unwrap().id(), "gdrive");
    assert_eq!(rest, "Documents/report.txt");
}

#[tokio::test]
async fn backend_for_path_nested_mount_resolves_deepest() {
    // `work` mounted under the at-root backend; `work/repo` must reach the
    // deeper backend, while a sibling path stays with the at-root one.
    let root: Arc<dyn Backend> = Arc::new(RecordingBackend::new("rootb"));
    let work: Arc<dyn Backend> = Arc::new(RecordingBackend::new("workb"));
    // Longest-prefix first, matching VfsTree ordering.
    let mounts = mount_table(vec![
        (PathBuf::from("work"), work.clone()),
        (PathBuf::new(), root.clone()),
    ]);
    let state = empty_state(mounts, backend_list(vec![work.clone(), root.clone()]));

    let (routed, rest) = backend_for_path(&state, "work/repo/main.rs").await;
    assert_eq!(routed.unwrap().id(), "workb");
    assert_eq!(rest, "repo/main.rs");

    let (routed, rest) = backend_for_path(&state, "Personal/notes.txt").await;
    assert_eq!(routed.unwrap().id(), "rootb");
    assert_eq!(rest, "Personal/notes.txt");
}

#[tokio::test]
async fn propfind_root_lists_custom_mount_directory() {
    let backend: Arc<dyn Backend> = Arc::new(RecordingBackend::new("gdrive-personal"));
    let mounts = mount_table(vec![(PathBuf::from("personal"), backend.clone())]);
    let server = WebDavServer::start(
        "127.0.0.1:0",
        Arc::new(RwLock::new(HashMap::new())),
        tempfile::tempdir().unwrap().path().to_path_buf(),
        backend_list(vec![backend.clone()]),
        mounts,
        None,
    )
    .await
    .unwrap();
    let port = server.port();

    let client = reqwest::Client::new();
    let body = client
        .request(
            reqwest::Method::from_bytes(b"PROPFIND").unwrap(),
            format!("http://127.0.0.1:{port}/"),
        )
        .header("Depth", "1")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // The mount NAME is listed, not the backend id.
    assert!(body.contains("/personal/"), "body: {body}");
    assert!(!body.contains("gdrive-personal"), "body: {body}");
    server.stop().unwrap();
}

#[tokio::test]
async fn propfind_root_lists_at_root_children_inline() {
    // An at-root backend's root children appear directly under "/", with no
    // synthetic mount directory.
    let backend: Arc<dyn Backend> = Arc::new(RecordingBackend::new("gdrive"));
    let mounts = mount_table(vec![(PathBuf::new(), backend.clone())]);
    let items = Arc::new(RwLock::new(HashMap::new()));
    {
        // Seed a root-level item as the sync runner would: parent is the
        // backend's `:root` alias, path has no mount prefix.
        let mut guard = items.write().await;
        let mut item = make_item("Documents", true);
        item.id = ItemId::new("gdrive", "doc1");
        item.parent_id = ItemId::new("gdrive", "root");
        item.path = "Documents".to_string();
        guard.insert(item.id.0.clone(), item);
    }
    let server = WebDavServer::start(
        "127.0.0.1:0",
        items,
        tempfile::tempdir().unwrap().path().to_path_buf(),
        backend_list(vec![backend.clone()]),
        mounts,
        None,
    )
    .await
    .unwrap();
    let port = server.port();

    let client = reqwest::Client::new();
    let body = client
        .request(
            reqwest::Method::from_bytes(b"PROPFIND").unwrap(),
            format!("http://127.0.0.1:{port}/"),
        )
        .header("Depth", "1")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // The root item is listed directly under "/", not as a "gdrive" dir.
    assert!(body.contains("/Documents/"), "body: {body}");
    assert!(!body.contains("/gdrive/"), "body: {body}");
    server.stop().unwrap();
}

#[tokio::test]
async fn put_routes_to_custom_mount_backend() {
    let backend = Arc::new(RecordingBackend::new("gdrive-personal"));
    let uploads = backend.uploads.clone();
    let backend_dyn: Arc<dyn Backend> = backend;
    let mounts = mount_table(vec![(PathBuf::from("personal"), backend_dyn.clone())]);
    let server = WebDavServer::start(
        "127.0.0.1:0",
        Arc::new(RwLock::new(HashMap::new())),
        tempfile::tempdir().unwrap().path().to_path_buf(),
        backend_list(vec![backend_dyn.clone()]),
        mounts,
        None,
    )
    .await
    .unwrap();
    let port = server.port();

    let resp = reqwest::Client::new()
        .put(format!("http://127.0.0.1:{port}/personal/report.txt"))
        .body(b"hello".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    // The upload reached the custom-mount backend with the mount prefix
    // stripped (backend-relative path).
    let recorded = uploads.lock().unwrap().clone();
    assert_eq!(recorded, vec!["report.txt".to_string()]);
    server.stop().unwrap();
}

/// A PUT into a nested directory under a custom-named mount must resolve the
/// parent from the mount-prefixed item path, not by reconstructing
/// `/{backend_id}/...`. With the backend id (`gdrive-personal`) differing from
/// the mount name (`personal`), the old reconstruction missed the seeded
/// directory item and fell back to the backend root; this pins it to the
/// directory.
#[tokio::test]
async fn put_nested_under_custom_mount_resolves_parent_not_root() {
    let backend = Arc::new(RecordingBackend::new("gdrive-personal"));
    let parents = backend.upload_parents.clone();
    let backend_dyn: Arc<dyn Backend> = backend;
    let mounts = mount_table(vec![(PathBuf::from("personal"), backend_dyn.clone())]);

    // Seed a Documents directory at its full mount-prefixed VFS path.
    let docs = VfsItem {
        id: ItemId::new("gdrive-personal", "docs"),
        parent_id: ItemId::new("gdrive-personal", "root"),
        name: "Documents".to_string(),
        path: "personal/Documents".to_string(),
        is_dir: true,
        size: None,
        mod_time: None,
        cache_state: CacheState::Online,
        mime_type: None,
    };
    let mut seed = HashMap::new();
    seed.insert(docs.id.0.clone(), docs);

    let server = WebDavServer::start(
        "127.0.0.1:0",
        Arc::new(RwLock::new(seed)),
        tempfile::tempdir().unwrap().path().to_path_buf(),
        backend_list(vec![backend_dyn.clone()]),
        mounts,
        None,
    )
    .await
    .unwrap();
    let port = server.port();

    let resp = reqwest::Client::new()
        .put(format!(
            "http://127.0.0.1:{port}/personal/Documents/report.txt"
        ))
        .body(b"hi".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    let recorded = parents.lock().unwrap().clone();
    assert_eq!(
        recorded,
        vec!["gdrive-personal:docs".to_string()],
        "nested PUT must be parented to the Documents dir, not the backend root"
    );
    server.stop().unwrap();
}

#[tokio::test]
async fn put_routes_to_at_root_backend() {
    let backend = Arc::new(RecordingBackend::new("gdrive"));
    let uploads = backend.uploads.clone();
    let backend_dyn: Arc<dyn Backend> = backend;
    let mounts = mount_table(vec![(PathBuf::new(), backend_dyn.clone())]);
    let server = WebDavServer::start(
        "127.0.0.1:0",
        Arc::new(RwLock::new(HashMap::new())),
        tempfile::tempdir().unwrap().path().to_path_buf(),
        backend_list(vec![backend_dyn.clone()]),
        mounts,
        None,
    )
    .await
    .unwrap();
    let port = server.port();

    let resp = reqwest::Client::new()
        .put(format!("http://127.0.0.1:{port}/report.txt"))
        .body(b"hello".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    // The at-root backend receives the path verbatim (no prefix to strip).
    let recorded = uploads.lock().unwrap().clone();
    assert_eq!(recorded, vec!["report.txt".to_string()]);
    server.stop().unwrap();
}

// ── Lazy expansion under a non-root mount ───────────────────────────────

#[tokio::test]
async fn expand_root_under_non_root_mount_prefixes_child_paths() {
    // The whole point of the multi-backend refactor: a backend whose
    // `changes()` returns nothing (like Google Drive on initial sync) has
    // its real content loaded on demand via `list_children`. Under a
    // non-root mount, those expanded children must carry the mount-prefixed
    // VFS path, or PROPFIND/GET 404 because the rendered href omits the
    // mount segment the client requested.
    //
    // Mount `gdrive-personal` at `personal`. `list_children("root")`
    // returns one file `report.txt`. PROPFIND /personal must render the
    // child href as /personal/report.txt, and a GET on that path must
    // resolve and return the file body (not 404).
    let backend = Arc::new(RecordingBackend::new("gdrive-personal").with_children(
        "root",
        vec![FileEntry::file(
            ItemId::new("gdrive-personal", "f1"),
            ItemId::new("gdrive-personal", "root"),
            "report.txt".to_string(),
        )],
    ));
    let backend_dyn: Arc<dyn Backend> = backend;
    let mounts = mount_table(vec![(PathBuf::from("personal"), backend_dyn.clone())]);
    let server = WebDavServer::start(
        "127.0.0.1:0",
        Arc::new(RwLock::new(HashMap::new())),
        tempfile::tempdir().unwrap().path().to_path_buf(),
        backend_list(vec![backend_dyn.clone()]),
        mounts,
        None,
    )
    .await
    .unwrap();
    let port = server.port();

    let client = reqwest::Client::new();
    let body = client
        .request(
            reqwest::Method::from_bytes(b"PROPFIND").unwrap(),
            format!("http://127.0.0.1:{port}/personal"),
        )
        .header("Depth", "1")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // The child href carries the mount prefix; the bare-basename form must
    // not appear as a top-level href.
    assert!(
        body.contains("/personal/report.txt"),
        "expanded child must render under the mount prefix; body: {body}"
    );

    // The expanded file is GET-reachable at the prefixed path.
    let resp = client
        .get(format!("http://127.0.0.1:{port}/personal/report.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "GET under a non-root mount must resolve, not 404"
    );
    let bytes = resp.bytes().await.unwrap();
    assert_eq!(bytes.as_ref(), b"recording-backend-contents");

    server.stop().unwrap();
}

#[tokio::test]
async fn expand_root_at_root_mount_yields_unprefixed_child_paths() {
    // A backend mounted at "/" (empty prefix) must keep the pre-refactor
    // single-backend path shape: expanded children carry the bare basename
    // with no mount segment, so the href and GET path are byte-identical to
    // today.
    let backend = Arc::new(RecordingBackend::new("gdrive").with_children(
        "root",
        vec![FileEntry::file(
            ItemId::new("gdrive", "f1"),
            ItemId::new("gdrive", "root"),
            "report.txt".to_string(),
        )],
    ));
    let backend_dyn: Arc<dyn Backend> = backend;
    let mounts = mount_table(vec![(PathBuf::new(), backend_dyn.clone())]);
    let server = WebDavServer::start(
        "127.0.0.1:0",
        Arc::new(RwLock::new(HashMap::new())),
        tempfile::tempdir().unwrap().path().to_path_buf(),
        backend_list(vec![backend_dyn.clone()]),
        mounts,
        None,
    )
    .await
    .unwrap();
    let port = server.port();

    let client = reqwest::Client::new();
    // PROPFIND "/" drives `expand_root` for the at-root backend, listing
    // its children inline under the root.
    let body = client
        .request(
            reqwest::Method::from_bytes(b"PROPFIND").unwrap(),
            format!("http://127.0.0.1:{port}/"),
        )
        .header("Depth", "1")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // Unprefixed href — no synthetic mount segment.
    assert!(
        body.contains("/report.txt"),
        "at-root child must render unprefixed; body: {body}"
    );

    let resp = client
        .get(format!("http://127.0.0.1:{port}/report.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "at-root expanded child must resolve at the unprefixed path"
    );
    let bytes = resp.bytes().await.unwrap();
    assert_eq!(bytes.as_ref(), b"recording-backend-contents");

    server.stop().unwrap();
}
