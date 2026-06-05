#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::string_slice
)]
//! Integration tests for the Google Drive backend against a wiremock server.
//!
//! Each test starts a fresh mock HTTP server, registers the Drive API responses it
//! needs, and exercises the Backend trait against it.  This validates both the HTTP
//! plumbing (correct paths, auth header, query params) and the JSON deserialisation
//! (camelCase field mapping) without touching the real Drive API or the macOS Keychain.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use cascade_backend_gdrive::auth::AuthTokens;
use cascade_backend_gdrive::token_store::TokenStore;
use cascade_backend_gdrive::{create_backend, create_backend_with_store};
use cascade_engine::backend::Backend;
use cascade_engine::types::{FileId, ItemId};
use serde_json::json;
use tokio::sync::Mutex;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ── Test helpers ─────────────────────────────────────────────────────────────

/// Build a backend config that points all HTTP traffic at `server`.
/// The `access_token` key bypasses Keychain lookup for the duration of the test.
fn make_backend(server: &MockServer) -> Box<dyn Backend> {
    let uri = server.uri();
    let mut table = toml::map::Map::new();
    table.insert(
        "client_id".to_string(),
        toml::Value::String("test-id".to_string()),
    );
    table.insert(
        "client_secret".to_string(),
        toml::Value::String("test-secret".to_string()),
    );
    table.insert(
        "account".to_string(),
        toml::Value::String("test-account".to_string()),
    );
    table.insert("base_url".to_string(), toml::Value::String(uri.clone()));
    table.insert("upload_url".to_string(), toml::Value::String(uri));
    table.insert(
        "access_token".to_string(),
        toml::Value::String("test-token".to_string()),
    );
    create_backend(&toml::Value::Table(table)).unwrap()
}

/// Register a default `/files/root` response so tests that exercise paths
/// invoking `my_drive_root()` (changes stream, upload, `create_dir`, move)
/// don't fail with a 404 from the mock server.
async fn mock_drive_root(server: &MockServer, real_root_id: &str) {
    Mock::given(method("GET"))
        .and(path("/files/root"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": real_root_id,
            "name": "My Drive",
            "mimeType": "application/vnd.google-apps.folder",
            "trashed": false
        })))
        .mount(server)
        .await;
}

/// Minimal camelCase JSON for a regular file.
fn file_json(id: &str, name: &str, parent: &str, size: u64) -> serde_json::Value {
    json!({
        "id": id,
        "name": name,
        "mimeType": "text/plain",
        "parents": [parent],
        "size": size.to_string(),
        "modifiedTime": "2026-05-28T10:00:00Z",
        "md5Checksum": "abcdef01",
        "trashed": false
    })
}

/// Minimal camelCase JSON for a folder.
fn folder_json(id: &str, name: &str, parent: &str) -> serde_json::Value {
    json!({
        "id": id,
        "name": name,
        "mimeType": "application/vnd.google-apps.folder",
        "parents": [parent],
        "trashed": false
    })
}

// ── quota ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn quota_returns_storage_info() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/about"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "storageQuota": {
                "limit": "107374182400",
                "usage": "10737418240"
            }
        })))
        .mount(&server)
        .await;

    let backend = make_backend(&server);
    let quota = backend.quota().await.unwrap().unwrap();
    assert_eq!(quota.total, Some(107_374_182_400));
    assert_eq!(quota.used, Some(10_737_418_240));
    assert_eq!(quota.available, Some(96_636_764_160));
}

#[tokio::test]
async fn quota_returns_none_when_no_storage_quota() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/about"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&server)
        .await;

    let backend = make_backend(&server);
    let quota = backend.quota().await.unwrap();
    assert!(quota.is_none());
}

// ── changes ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn changes_initial_call_fetches_start_token_and_returns_empty() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/changes/startPageToken"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "startPageToken": "token-1"
        })))
        .mount(&server)
        .await;

    let backend = make_backend(&server);
    let (changes, cursor) = backend.changes(None).await.unwrap();

    // The initial snapshot returns the four virtual root directories rather
    // than listing actual Drive files. Real content is loaded on demand.
    assert_eq!(changes.len(), 4);
    let names: Vec<&str> = changes
        .iter()
        .filter_map(|c| match c {
            cascade_engine::types::Change::Created(e) => Some(e.name.as_str()),
            _ => None,
        })
        .collect();
    assert!(names.contains(&"My Drive"));
    assert!(names.contains(&"Shared drives"));
    assert!(names.contains(&"Shared with me"));
    assert!(names.contains(&"Bin"));
    assert_eq!(cursor.0, "token-1");
}

#[tokio::test]
async fn changes_detects_created_file() {
    let server = MockServer::start().await;
    mock_drive_root(&server, "real-root-id").await;
    Mock::given(method("GET"))
        .and(path("/changes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "changes": [
                {
                    "kind": "drive#change",
                    "fileId": "file001",
                    "removed": false,
                    "file": file_json("file001", "report.pdf", "root", 4096)
                }
            ],
            "newStartPageToken": "token-3"
        })))
        .mount(&server)
        .await;

    let backend = make_backend(&server);
    let cursor = cascade_engine::types::Cursor("token-2".to_string());
    let (changes, next_cursor) = backend.changes(Some(&cursor)).await.unwrap();
    assert_eq!(changes.len(), 1);
    match &changes[0] {
        cascade_engine::types::Change::Created(entry) => {
            assert_eq!(entry.name, "report.pdf");
            assert!(!entry.is_dir);
            assert_eq!(entry.size, Some(4096));
        }
        other => panic!("expected Created, got {other:?}"),
    }
    assert_eq!(next_cursor.0, "token-3");
}

#[tokio::test]
async fn changes_detects_deleted_file() {
    let server = MockServer::start().await;
    mock_drive_root(&server, "real-root-id").await;
    Mock::given(method("GET"))
        .and(path("/changes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "changes": [
                {
                    "kind": "drive#change",
                    "fileId": "file002",
                    "removed": true,
                    "file": file_json("file002", "old.txt", "root", 100)
                }
            ],
            "newStartPageToken": "token-4"
        })))
        .mount(&server)
        .await;

    let backend = make_backend(&server);
    let cursor = cascade_engine::types::Cursor("token-3".to_string());
    let (changes, _) = backend.changes(Some(&cursor)).await.unwrap();
    assert_eq!(changes.len(), 1);
    match &changes[0] {
        cascade_engine::types::Change::Deleted(entry) => {
            assert_eq!(entry.name, "old.txt");
        }
        other => panic!("expected Deleted, got {other:?}"),
    }
}

#[tokio::test]
async fn changes_handles_pagination() {
    let server = MockServer::start().await;
    mock_drive_root(&server, "real-root-id").await;
    // Page 1 — has a nextPageToken.
    Mock::given(method("GET"))
        .and(path("/changes"))
        .and(query_param("pageToken", "token-p1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "changes": [
                {
                    "kind": "drive#change",
                    "fileId": "file-a",
                    "removed": false,
                    "file": file_json("file-a", "a.txt", "root", 10)
                }
            ],
            "nextPageToken": "token-p2"
        })))
        .mount(&server)
        .await;
    // Page 2 — terminates.
    Mock::given(method("GET"))
        .and(path("/changes"))
        .and(query_param("pageToken", "token-p2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "changes": [
                {
                    "kind": "drive#change",
                    "fileId": "file-b",
                    "removed": false,
                    "file": file_json("file-b", "b.txt", "root", 20)
                }
            ],
            "newStartPageToken": "token-p3"
        })))
        .mount(&server)
        .await;

    let backend = make_backend(&server);
    let cursor = cascade_engine::types::Cursor("token-p1".to_string());
    let (changes, next_cursor) = backend.changes(Some(&cursor)).await.unwrap();
    assert_eq!(changes.len(), 2);
    assert_eq!(next_cursor.0, "token-p3");
}

// ── metadata ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn metadata_root_returns_folder_entry() {
    // The root is now a synthetic entry — no Drive API call is made.
    let server = MockServer::start().await;
    let backend = make_backend(&server);
    let entry = backend.metadata(Path::new("/")).await.unwrap();
    assert!(entry.is_dir);
    assert_eq!(entry.id, ItemId::new("gdrive-test-account", "root"));
}

#[tokio::test]
async fn metadata_resolves_nested_path() {
    let server = MockServer::start().await;
    // Listing root to find "docs" folder.
    Mock::given(method("GET"))
        .and(path("/files"))
        .and(query_param("q", "'root' in parents and trashed = false"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "files": [folder_json("folder-1", "docs", "root")]
        })))
        .mount(&server)
        .await;
    // Listing "docs" to find "note.txt".
    Mock::given(method("GET"))
        .and(path("/files"))
        .and(query_param(
            "q",
            "'folder-1' in parents and trashed = false",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "files": [file_json("file-1", "note.txt", "folder-1", 512)]
        })))
        .mount(&server)
        .await;
    // Final get_file call to fetch the resolved file's full metadata.
    Mock::given(method("GET"))
        .and(path("/files/file-1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(file_json("file-1", "note.txt", "folder-1", 512)),
        )
        .mount(&server)
        .await;

    let backend = make_backend(&server);
    let entry = backend.metadata(Path::new("docs/note.txt")).await.unwrap();
    assert_eq!(entry.name, "note.txt");
    assert!(!entry.is_dir);
    assert_eq!(entry.size, Some(512));
    assert_eq!(entry.id, ItemId::new("gdrive-test-account", "file-1"));
}

// ── download ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn download_writes_file_content() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/file-dl"))
        .and(query_param("alt", "media"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"hello drive"))
        .mount(&server)
        .await;

    let backend = make_backend(&server);
    let entry = cascade_engine::types::FileEntry {
        id: ItemId::new("gdrive-test-account", "file-dl"),
        parent_id: ItemId::new("gdrive-test-account", "root"),
        name: "file.txt".to_string(),
        is_dir: false,
        size: Some(11),
        mod_time: None,
        mime_type: Some("text/plain".to_string()),
        hash: None,
    };

    let buf = backend.download(&entry).await.unwrap();
    assert_eq!(buf, b"hello drive");
}

// ── read_range ──────────────────────────────────────────────────────────────────

/// Build a non-directory `FileEntry` for the range-read tests.
fn range_entry(native_id: &str, size: u64) -> cascade_engine::types::FileEntry {
    cascade_engine::types::FileEntry {
        id: ItemId::new("gdrive-test-account", native_id),
        parent_id: ItemId::new("gdrive-test-account", "root"),
        name: "file.txt".to_string(),
        is_dir: false,
        size: Some(size),
        mod_time: None,
        mime_type: Some("text/plain".to_string()),
        hash: None,
    }
}

#[tokio::test]
async fn read_range_returns_partial_content_slice() {
    // A compliant server honours the Range header and replies 206 with exactly
    // the requested window. read_range must return it verbatim.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/file-rr"))
        .and(query_param("alt", "media"))
        .and(header("range", "bytes=6-10"))
        .respond_with(ResponseTemplate::new(206).set_body_bytes(b"world"))
        .mount(&server)
        .await;

    let backend = make_backend(&server);
    let entry = range_entry("file-rr", 11);
    let bytes = backend.read_range(&entry, 6, 5).await.unwrap();
    assert_eq!(bytes, b"world");
}

#[tokio::test]
async fn read_range_slices_when_server_ignores_range() {
    // A server that ignores the Range header replies 200 with the whole body.
    // read_range must slice [offset, offset + length) client-side so the
    // contract still holds.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/file-full"))
        .and(query_param("alt", "media"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"hello world"))
        .mount(&server)
        .await;

    let backend = make_backend(&server);
    let entry = range_entry("file-full", 11);
    // Mid-range slice from the full body.
    assert_eq!(backend.read_range(&entry, 6, 5).await.unwrap(), b"world");
    // Length past EOF clamps to what's available.
    assert_eq!(backend.read_range(&entry, 6, 999).await.unwrap(), b"world");
}

#[tokio::test]
async fn read_range_empty_when_offset_past_eof() {
    // The offset lies at or past the end of the file. Drive answers 416 Range
    // Not Satisfiable, which read_range maps to an empty result, not an error.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/file-eof"))
        .and(query_param("alt", "media"))
        .respond_with(ResponseTemplate::new(416))
        .mount(&server)
        .await;

    let backend = make_backend(&server);
    let entry = range_entry("file-eof", 11);
    let bytes = backend.read_range(&entry, 100, 10).await.unwrap();
    assert!(bytes.is_empty());
}

#[tokio::test]
async fn read_range_zero_length_makes_no_request() {
    // A zero-length read returns empty without issuing any HTTP request — the
    // mock server has no routes registered, so a request would 404 and fail.
    let server = MockServer::start().await;
    let backend = make_backend(&server);
    let entry = range_entry("file-zero", 11);
    let bytes = backend.read_range(&entry, 0, 0).await.unwrap();
    assert!(bytes.is_empty());
}

// ── create_dir ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_dir_posts_to_files_and_returns_entry() {
    let server = MockServer::start().await;
    mock_drive_root(&server, "real-root-id").await;
    // Parent directory lookup — resolving "projects" path.
    Mock::given(method("GET"))
        .and(path("/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "files": [folder_json("parent-1", "projects", "root")]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/parent-1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(folder_json("parent-1", "projects", "root")),
        )
        .mount(&server)
        .await;
    // The actual create-directory POST.
    Mock::given(method("POST"))
        .and(path("/files"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(folder_json("new-dir", "alpha", "parent-1")),
        )
        .mount(&server)
        .await;

    let backend = make_backend(&server);
    let entry = backend
        .create_dir(Path::new("projects/alpha"))
        .await
        .unwrap();
    assert_eq!(entry.name, "alpha");
    assert!(entry.is_dir);
    assert_eq!(entry.id, ItemId::new("gdrive-test-account", "new-dir"));
}

// ── delete ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn delete_patches_file_as_trashed() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/files/file-del"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"id":"file-del","name":"x","mimeType":"text/plain","trashed":true}),
        ))
        .mount(&server)
        .await;

    let backend = make_backend(&server);
    let entry = cascade_engine::types::FileEntry {
        id: ItemId::new("gdrive-test-account", "file-del"),
        parent_id: ItemId::new("gdrive-test-account", "root"),
        name: "x.txt".to_string(),
        is_dir: false,
        size: None,
        mod_time: None,
        mime_type: None,
        hash: None,
    };
    backend.delete(&entry).await.unwrap();
}

// ── upload ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn upload_file_sends_multipart_and_returns_entry() {
    let server = MockServer::start().await;
    mock_drive_root(&server, "real-root-id").await;
    Mock::given(method("POST"))
        .and(path("/files"))
        .and(query_param("uploadType", "multipart"))
        .respond_with(ResponseTemplate::new(200).set_body_json(file_json(
            "uploaded-1",
            "report.txt",
            "root",
            5,
        )))
        .mount(&server)
        .await;

    let backend = make_backend(&server);
    let content = b"hello";
    let parent = FileId("root".to_string());
    let entry = backend
        .upload(Path::new("report.txt"), content, &parent)
        .await
        .unwrap();
    assert_eq!(entry.name, "report.txt");
    assert_eq!(entry.id, ItemId::new("gdrive-test-account", "uploaded-1"));
}

// ── poll_interval ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn poll_interval_is_sixty_seconds() {
    let server = MockServer::start().await;
    let backend = make_backend(&server);
    let interval = backend.poll_interval().await;
    assert_eq!(interval, Some(std::time::Duration::from_mins(1)));
}

// ── concurrent refresh path (TLS-deadlock regression guard) ────────────────────
//
// Background: the Drive backend carries a long-standing workaround for a hang
// that the project memory describes as a "TLS deadlock" — a fresh per-request
// reqwest client with connection pooling disabled (`pool_max_idle_per_host(0)`)
// and `http1_only()`. The hang was only ever observed through the WebDAV
// presenter, where an axum/hyper-1.x *server* and a reqwest/hyper-1.x *client*
// share one tokio runtime: the second backend TLS handshake, opened while the
// server is mid-response, never completed. A minimal repro against a plain
// HTTP echo server never reproduced it; only the real handler against the real
// TLS endpoint did.
//
// What this test *can* pin down: the one place inside the crate where a tokio
// MutexGuard sits near an `.await` is the slow refresh path of
// `GdriveBackend::access_token` — it locks the token mutex, clones the refresh
// token, drops the guard, awaits the OAuth2 refresh, then re-locks to store the
// result. If that guard were ever held across the refresh await, a second task
// arriving at the lock could never acquire it and the task would wedge.
//
// To exercise that path for real the backend is seeded with an *already
// expired* access token plus a refresh token, and the OAuth2 token endpoint is
// pointed at a deliberately delayed mock. Every concurrent caller therefore
// misses the fast path, races into the refresh branch, and contends on the
// mutex re-acquisition. An in-memory token store stands in for the Keychain so
// the refresh's `save` never touches the host. All callers must still complete
// within the deadline; a guard held across the refresh await would trip it.
//
// wiremock speaks plain HTTP with no TLS handshake, so this harness still
// cannot reach the suspected hyper server+client TLS interaction — that lives
// outside this crate. What it proves is the narrower, load-bearing claim the
// workaround comments make: the refresh-path lock discipline does not deadlock.

/// In-memory [`TokenStore`] for tests: keeps the refresh path off the host
/// Keychain and config directory while still recording what was persisted.
#[derive(Debug, Default)]
struct InMemoryTokenStore {
    slot: Mutex<Option<AuthTokens>>,
}

#[async_trait]
impl TokenStore for InMemoryTokenStore {
    async fn load(&self, _account: &str) -> anyhow::Result<Option<AuthTokens>> {
        Ok(self.slot.lock().await.clone())
    }

    async fn save(&self, _account: &str, tokens: &AuthTokens) -> anyhow::Result<()> {
        *self.slot.lock().await = Some(tokens.clone());
        Ok(())
    }
}

/// Build a backend whose seeded access token is already expired and whose
/// `OAuth2` token endpoint points at `server`, so any access-token use is forced
/// down the refresh slow path. The in-memory store keeps refreshes off the host.
fn make_backend_forcing_refresh(server: &MockServer) -> Box<dyn Backend> {
    let uri = server.uri();
    let mut table = toml::map::Map::new();
    table.insert(
        "client_id".to_string(),
        toml::Value::String("test-id".to_string()),
    );
    table.insert(
        "client_secret".to_string(),
        toml::Value::String("test-secret".to_string()),
    );
    table.insert(
        "account".to_string(),
        toml::Value::String("test-account".to_string()),
    );
    table.insert("base_url".to_string(), toml::Value::String(uri.clone()));
    table.insert("upload_url".to_string(), toml::Value::String(uri.clone()));
    // Route OAuth2 token refreshes at the mock instead of Google.
    table.insert("token_url".to_string(), toml::Value::String(uri));
    table.insert(
        "access_token".to_string(),
        toml::Value::String("stale-token".to_string()),
    );
    table.insert(
        "refresh_token".to_string(),
        toml::Value::String("refresh-token".to_string()),
    );
    // Inside is_expired()'s 60-second buffer, so the seeded token reads as
    // expired immediately and every caller takes the refresh path.
    table.insert("expires_in_secs".to_string(), toml::Value::Integer(0));

    create_backend_with_store(
        &toml::Value::Table(table),
        Arc::new(InMemoryTokenStore::default()),
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_refresh_through_shared_client_does_not_deadlock() {
    use std::time::Duration;

    let server = MockServer::start().await;

    // The OAuth2 token endpoint is a POST to the configured token_url, which we
    // pointed at the server root. A deliberate delay keeps several refreshes in
    // flight at once so the mutex re-acquisition is genuinely contended.
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(20))
                .set_body_json(json!({
                    "access_token": "fresh-token",
                    "refresh_token": "refresh-token",
                    "expires_in": 3600,
                    "token_type": "Bearer"
                })),
        )
        .mount(&server)
        .await;

    // The Drive list call each task makes once it holds a fresh token.
    Mock::given(method("GET"))
        .and(path("/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "files": [file_json("f1", "a.txt", "shared-parent", 1)]
        })))
        .mount(&server)
        .await;

    let backend: Arc<dyn Backend> = Arc::from(make_backend_forcing_refresh(&server));

    // Enough concurrency to overlap many in-flight refreshes through the one
    // backend. The exact count is not load-bearing; it only has to be large
    // enough that callers genuinely contend on the refresh mutex given the
    // 20ms mock delay.
    let concurrency = 32;
    let mut handles = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        let backend = Arc::clone(&backend);
        handles.push(tokio::spawn(async move {
            backend.list_children("shared-parent").await
        }));
    }

    // A guard held across the refresh await would leave every task after the
    // first unable to acquire the token mutex, so at least one would never
    // complete and the timeout below would fire. With the guard correctly
    // dropped before the await, all tasks finish well inside this deadline.
    let deadline = Duration::from_secs(5);
    let drained = tokio::time::timeout(deadline, async move {
        let mut entries_per_task = Vec::with_capacity(handles.len());
        for handle in handles {
            let entries = handle
                .await
                .expect("task panicked")
                .expect("list_children failed");
            entries_per_task.push(entries);
        }
        entries_per_task
    })
    .await
    .expect("concurrent refreshes deadlocked: in-flight tasks did not complete in time");

    for entries in drained {
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "a.txt");
    }
}
