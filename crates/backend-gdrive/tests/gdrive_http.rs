#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::string_slice
)]
//! Mocked-HTTP tests for the Google Drive backend's low-level transport and
//! supporting types.
//!
//! Where `gdrive_integration.rs` exercises the public `Backend` trait against
//! a wiremock server, this file drives [`DriveClient`] directly. That gives
//! tests the ability to:
//!
//! - Pin the exact request the client issues — query parameters, headers, the
//!   shape of the multipart body, the `Range` header on a partial read.
//! - Cover the typed error mapping in [`client::drive_api_error`] (HTTP 403/404/409
//!   map to `BackendError::Forbidden`/`NotFound`/`Conflict`; everything else
//!   is a plain `anyhow::Error`).
//! - Round-trip a full multipart upload body and assert that both the JSON
//!   metadata and the binary part land in the request.
//! - Drive the pagination loop by stacking two pages of responses.
//! - Deserialize raw Drive API v3 response shapes (camelCase, optional fields,
//!   ISO-8601 timestamps) into [`model::DriveFile`] and friends.
//! - Exercise the [`token_store::TokenStore`] trait with an in-memory
//!   implementation, treating the trait surface as the contract under test.

use async_trait::async_trait;
use cascade_backend_gdrive::auth::AuthTokens;
use cascade_backend_gdrive::client::{DriveClient, ListQuery};
use cascade_backend_gdrive::model::{
    AboutResponse, ChangesResponse, DriveFile, FileListResponse, SharedDriveListResponse,
};
use cascade_backend_gdrive::token_store::TokenStore;
use serde_json::json;
use tokio::sync::Mutex;
use wiremock::matchers::{body_string_contains, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

// ── Test helpers ─────────────────────────────────────────────────────────────

/// Build a `DriveClient` whose `base_url` and `upload_url` both point at the
/// wiremock server. The `OAuth2` endpoint is left at its production URL and
/// never called by these tests; the refresh path is exercised separately
/// against the [`token_store::TokenStore`] trait below.
fn make_client(server: &MockServer) -> DriveClient {
    let uri = server.uri();
    DriveClient::with_urls(uri.clone(), uri)
}

/// In-memory [`TokenStore`] for the round-trip test. Stores per-account slots
/// in a `HashMap` so the `account` key matters, matching the trait's contract.
#[derive(Debug, Default)]
struct InMemoryTokenStore {
    slots: Mutex<std::collections::HashMap<String, AuthTokens>>,
}

#[async_trait]
impl TokenStore for InMemoryTokenStore {
    async fn load(&self, account: &str) -> anyhow::Result<Option<AuthTokens>> {
        Ok(self.slots.lock().await.get(account).cloned())
    }

    async fn save(&self, account: &str, tokens: &AuthTokens) -> anyhow::Result<()> {
        self.slots
            .lock()
            .await
            .insert(account.to_string(), tokens.clone());
        Ok(())
    }
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

// ── 1. File listing ──────────────────────────────────────────────────────────

#[tokio::test]
async fn list_files_parses_response_into_drive_files() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "files": [
                {
                    "id": "file-1",
                    "name": "alpha.txt",
                    "mimeType": "text/plain",
                    "parents": ["root"],
                    "size": "1234",
                    "modifiedTime": "2026-05-28T10:00:00Z",
                    "md5Checksum": "deadbeef",
                    "trashed": false
                },
                {
                    "id": "file-2",
                    "name": "beta.txt",
                    "mimeType": "text/plain",
                    "parents": ["root"],
                    "size": "5678",
                    "modifiedTime": "2026-05-28T10:05:00Z",
                    "md5Checksum": "feedface",
                    "trashed": false
                }
            ]
        })))
        .mount(&server)
        .await;

    let client = make_client(&server);
    let resp: FileListResponse = client
        .list_files(
            &ListQuery::ChildrenOf {
                parent_id: "root".to_string(),
                drive_id: None,
            },
            "test-token",
            None,
        )
        .await
        .unwrap();

    assert_eq!(resp.files.len(), 2);
    assert_eq!(resp.files[0].id, "file-1");
    assert_eq!(resp.files[0].name, "alpha.txt");
    assert_eq!(resp.files[0].size.as_deref(), Some("1234"));
    assert_eq!(
        resp.files[0].modified_time.as_deref(),
        Some("2026-05-28T10:00:00Z")
    );
    assert_eq!(resp.files[0].md5_checksum.as_deref(), Some("deadbeef"));
    assert!(!resp.files[0].trashed);
    assert_eq!(resp.files[1].id, "file-2");
    assert_eq!(resp.files[1].size.as_deref(), Some("5678"));
    assert!(resp.next_page_token.is_none());
}

#[tokio::test]
async fn list_files_drivers_request_includes_bearer_token() {
    let server = MockServer::start().await;
    let mock = Mock::given(method("GET"))
        .and(path("/files"))
        .and(header("authorization", "Bearer secret-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"files": []})))
        .expect(1)
        .mount_as_scoped(&server)
        .await;

    let client = make_client(&server);
    let _ = client
        .list_files(
            &ListQuery::ChildrenOf {
                parent_id: "root".to_string(),
                drive_id: None,
            },
            "secret-token",
            None,
        )
        .await
        .unwrap();

    drop(mock);
}

// ── 2. Folder hierarchy ──────────────────────────────────────────────────────

#[tokio::test]
async fn list_files_for_parent_includes_in_parents_clause() {
    let server = MockServer::start().await;
    // The Drive query string the client builds contains the parent id in
    // single quotes and the trashed=false predicate. Both must be present.
    Mock::given(method("GET"))
        .and(path("/files"))
        .and(query_param(
            "q",
            "'folder-42' in parents and trashed = false",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "files": [
                {
                    "id": "child-1",
                    "name": "child.txt",
                    "mimeType": "text/plain",
                    "parents": ["folder-42"],
                    "size": "10",
                    "trashed": false
                }
            ]
        })))
        .mount(&server)
        .await;

    let client = make_client(&server);
    let resp = client
        .list_files(
            &ListQuery::ChildrenOf {
                parent_id: "folder-42".to_string(),
                drive_id: None,
            },
            "test-token",
            None,
        )
        .await
        .unwrap();

    assert_eq!(resp.files.len(), 1);
    assert_eq!(resp.files[0].id, "child-1");
    assert_eq!(resp.files[0].parents, vec!["folder-42".to_string()]);
}

#[tokio::test]
async fn list_files_with_shared_drive_scopes_corpora_to_drive() {
    let server = MockServer::start().await;
    // When a drive_id is set, the client must scope the query to that drive
    // (corpora=drive, driveId=<id>, includeItemsFromAllDrives, supportsAllDrives).
    Mock::given(method("GET"))
        .and(path("/files"))
        .and(query_param("corpora", "drive"))
        .and(query_param("driveId", "0AB-shared"))
        .and(query_param("includeItemsFromAllDrives", "true"))
        .and(query_param("supportsAllDrives", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "files": [
                {
                    "id": "shared-child",
                    "name": "shared.txt",
                    "mimeType": "text/plain",
                    "parents": ["0AB-shared"],
                    "size": "5",
                    "trashed": false,
                    "driveId": "0AB-shared"
                }
            ]
        })))
        .mount(&server)
        .await;

    let client = make_client(&server);
    let resp = client
        .list_files(
            &ListQuery::ChildrenOf {
                parent_id: "shared-folder".to_string(),
                drive_id: Some("0AB-shared".to_string()),
            },
            "test-token",
            None,
        )
        .await
        .unwrap();

    assert_eq!(resp.files.len(), 1);
    assert_eq!(resp.files[0].drive_id.as_deref(), Some("0AB-shared"));
}

#[tokio::test]
async fn list_files_trashed_query_omits_corpora_drive() {
    let server = MockServer::start().await;
    // The trashed query must use corpora=user and not include the
    // shared-drive scoping keys — they would force the server to return
    // shared-drive trashed items instead of My Drive trash.
    Mock::given(method("GET"))
        .and(path("/files"))
        .and(query_param("q", "trashed = true"))
        .and(query_param("corpora", "user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "files": [
                {
                    "id": "trashed-1",
                    "name": "old.txt",
                    "mimeType": "text/plain",
                    "parents": ["root"],
                    "size": "1",
                    "trashed": true
                }
            ]
        })))
        .mount(&server)
        .await;

    let client = make_client(&server);
    let resp = client
        .list_files(&ListQuery::Trashed, "test-token", None)
        .await
        .unwrap();

    assert_eq!(resp.files.len(), 1);
    assert!(resp.files[0].trashed);
}

#[tokio::test]
async fn list_files_shared_with_me_uses_shared_with_me_clause() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files"))
        .and(query_param("q", "sharedWithMe = true and trashed = false"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "files": [
                {
                    "id": "swm-1",
                    "name": "shared.txt",
                    "mimeType": "text/plain",
                    "parents": ["other-user"],
                    "size": "1",
                    "trashed": false
                }
            ]
        })))
        .mount(&server)
        .await;

    let client = make_client(&server);
    let resp = client
        .list_files(&ListQuery::SharedWithMe, "test-token", None)
        .await
        .unwrap();

    assert_eq!(resp.files.len(), 1);
    assert_eq!(resp.files[0].id, "swm-1");
}

// ── 3. File download ─────────────────────────────────────────────────────────

#[tokio::test]
async fn download_content_returns_full_body_bytes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/file-dl"))
        .and(query_param("alt", "media"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"the quick brown fox"))
        .mount(&server)
        .await;

    let client = make_client(&server);
    let resp = client
        .download_content("file-dl", "test-token")
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, b"the quick brown fox");
}

#[tokio::test]
async fn download_content_returns_error_on_404() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/missing"))
        .and(query_param("alt", "media"))
        .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
        .mount(&server)
        .await;

    let client = make_client(&server);
    let err = client
        .download_content("missing", "test-token")
        .await
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("404"), "expected 404 in error, got: {msg}");
    // 404 must be a typed NotFound error.
    let downcast = err.downcast_ref::<cascade_engine::backend::BackendError>();
    assert!(
        matches!(
            downcast,
            Some(cascade_engine::backend::BackendError::NotFound(_))
        ),
        "expected NotFound, got {downcast:?}"
    );
}

// ── 4. File upload ───────────────────────────────────────────────────────────

#[tokio::test]
async fn upload_file_sends_multipart_with_metadata_and_content() {
    let server = MockServer::start().await;
    // Drive's upload endpoint lives under the upload URL, not the API URL,
    // and the multipart type is announced via a boundary in the content type.
    Mock::given(method("POST"))
        .and(path("/files"))
        .and(query_param("uploadType", "multipart"))
        .and(header(
            "content-type",
            "multipart/related; boundary=cascade_upload_boundary",
        ))
        // Both the JSON metadata part and the binary content part must
        // appear in the request body. `serde_json` produces compact output
        // (no spaces), so the substrings are also compact.
        .and(body_string_contains("\"name\":\"report.txt\""))
        .and(body_string_contains("\"parents\":[\"parent-x\"]"))
        .and(body_string_contains("hello, drive"))
        .and(body_string_contains("--cascade_upload_boundary--"))
        .respond_with(ResponseTemplate::new(200).set_body_json(file_json(
            "uploaded-99",
            "report.txt",
            "parent-x",
            12,
        )))
        .mount(&server)
        .await;

    let client = make_client(&server);
    let file = client
        .upload_file("report.txt", "parent-x", b"hello, drive", "test-token")
        .await
        .unwrap();

    assert_eq!(file.id, "uploaded-99");
    assert_eq!(file.name, "report.txt");
    assert_eq!(file.parents, vec!["parent-x".to_string()]);
}

#[tokio::test]
async fn upload_file_preserves_binary_content_in_request() {
    // The previous test asserts that the content appears as a substring; this
    // one captures the raw body and verifies the bytes round-trip exactly,
    // including the boundary framing.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(file_json(
            "uploaded-1",
            "blob.bin",
            "parent-1",
            6,
        )))
        .mount(&server)
        .await;

    let client = make_client(&server);
    let payload: Vec<u8> = (0u8..=255).cycle().take(1024).collect();
    let file = client
        .upload_file("blob.bin", "parent-1", &payload, "test-token")
        .await
        .unwrap();
    assert_eq!(file.id, "uploaded-1");

    // The wiremock request is reachable through `server.received_requests()`
    // for assertion; capture the most recent POST and inspect its body.
    let requests = server.received_requests().await.unwrap_or_default();
    let post = requests
        .iter()
        .rev()
        .find(|r: &&Request| r.method.as_str() == "POST")
        .expect("expected a POST request to /files");
    let body = post.body.clone();

    // Body must be valid multipart: leading boundary, JSON metadata part,
    // binary part, trailing boundary.
    assert!(
        body.windows(b"--cascade_upload_boundary\r\n".len())
            .any(|w| w == b"--cascade_upload_boundary\r\n")
    );
    assert!(
        body.windows(b"\r\n--cascade_upload_boundary--\r\n".len())
            .any(|w| w == b"\r\n--cascade_upload_boundary--\r\n")
    );
    // The whole binary payload must be in the body verbatim.
    let payload_pos = body
        .windows(payload.len())
        .position(|w| w == payload.as_slice());
    assert!(
        payload_pos.is_some(),
        "expected binary payload to appear in upload body"
    );
}

// ── 5. Pagination ────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_files_paginates_until_next_page_token_absent() {
    let server = MockServer::start().await;

    // Page 1: two files, nextPageToken points at page 2.
    Mock::given(method("GET"))
        .and(path("/files"))
        .and(query_param("pageToken", "page-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"files": []})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files"))
        .and(query_param("pageToken", "page-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "files": [file_json("f-a", "a.txt", "root", 1)],
            "nextPageToken": "page-2"
        })))
        .mount(&server)
        .await;
    // Initial request (no page token) — the first page.
    Mock::given(method("GET"))
        .and(path("/files"))
        .and(query_param("pageSize", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "files": [file_json("f-start", "start.txt", "root", 1)],
            "nextPageToken": "page-1"
        })))
        .mount(&server)
        .await;

    let client = make_client(&server);

    // DriveClient::list_files returns a single page; the caller (the
    // GdriveBackend's `list_files_all_pages`) is what accumulates pages.
    // The integration tests cover that loop; here we verify the per-call
    // contract: a follow-up with the returned `nextPageToken` yields the
    // next batch and the chain terminates when the token disappears.
    let mut all_ids: Vec<String> = Vec::new();
    let mut page_token: Option<String> = None;
    loop {
        let resp = client
            .list_files(
                &ListQuery::ChildrenOf {
                    parent_id: "root".to_string(),
                    drive_id: None,
                },
                "test-token",
                page_token.as_deref(),
            )
            .await
            .unwrap();
        all_ids.extend(resp.files.into_iter().map(|f| f.id));
        match resp.next_page_token {
            Some(next) => page_token = Some(next),
            None => break,
        }
    }

    assert_eq!(all_ids, vec!["f-start".to_string(), "f-a".to_string()]);
}

// ── 6. Error handling ────────────────────────────────────────────────────────

#[tokio::test]
async fn list_files_maps_403_to_forbidden_backend_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files"))
        .respond_with(
            ResponseTemplate::new(403)
                .set_body_string(r#"{"error":{"code":403,"message":"Rate Limit Exceeded"}}"#),
        )
        .mount(&server)
        .await;

    let client = make_client(&server);
    let err = client
        .list_files(
            &ListQuery::ChildrenOf {
                parent_id: "root".to_string(),
                drive_id: None,
            },
            "test-token",
            None,
        )
        .await
        .unwrap_err();
    let downcast = err.downcast_ref::<cascade_engine::backend::BackendError>();
    assert!(
        matches!(
            downcast,
            Some(cascade_engine::backend::BackendError::Forbidden(_))
        ),
        "expected Forbidden, got {downcast:?}"
    );
    let msg = format!("{err}");
    assert!(msg.contains("403"), "expected status in error: {msg}");
}

#[tokio::test]
async fn list_files_maps_404_to_not_found_backend_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files"))
        .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
        .mount(&server)
        .await;

    let client = make_client(&server);
    let err = client
        .list_files(
            &ListQuery::ChildrenOf {
                parent_id: "root".to_string(),
                drive_id: None,
            },
            "test-token",
            None,
        )
        .await
        .unwrap_err();
    let downcast = err.downcast_ref::<cascade_engine::backend::BackendError>();
    assert!(
        matches!(
            downcast,
            Some(cascade_engine::backend::BackendError::NotFound(_))
        ),
        "expected NotFound, got {downcast:?}"
    );
}

#[tokio::test]
async fn list_files_maps_409_to_conflict_backend_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files"))
        .respond_with(ResponseTemplate::new(409).set_body_string("Conflict"))
        .mount(&server)
        .await;

    let client = make_client(&server);
    let err = client
        .list_files(
            &ListQuery::ChildrenOf {
                parent_id: "root".to_string(),
                drive_id: None,
            },
            "test-token",
            None,
        )
        .await
        .unwrap_err();
    let downcast = err.downcast_ref::<cascade_engine::backend::BackendError>();
    assert!(
        matches!(
            downcast,
            Some(cascade_engine::backend::BackendError::Conflict(_))
        ),
        "expected Conflict, got {downcast:?}"
    );
}

#[tokio::test]
async fn list_files_maps_500_to_plain_anyhow_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
        .mount(&server)
        .await;

    let client = make_client(&server);
    let err = client
        .list_files(
            &ListQuery::ChildrenOf {
                parent_id: "root".to_string(),
                drive_id: None,
            },
            "test-token",
            None,
        )
        .await
        .unwrap_err();
    // A 500 is not in the 403/404/409 set, so the helper produces a plain
    // anyhow::Error rather than a typed BackendError.
    let downcast = err.downcast_ref::<cascade_engine::backend::BackendError>();
    assert!(
        downcast.is_none(),
        "expected plain anyhow error for 500, got {downcast:?}"
    );
    let msg = format!("{err}");
    assert!(msg.contains("500"), "expected status in error: {msg}");
}

#[tokio::test]
async fn upload_file_maps_403_to_forbidden_backend_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/files"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&server)
        .await;

    let client = make_client(&server);
    let err = client
        .upload_file("x.txt", "parent", b"data", "test-token")
        .await
        .unwrap_err();
    let downcast = err.downcast_ref::<cascade_engine::backend::BackendError>();
    assert!(
        matches!(
            downcast,
            Some(cascade_engine::backend::BackendError::Forbidden(_))
        ),
        "expected Forbidden for upload 403, got {downcast:?}"
    );
}

// ── 7. Model parsing ─────────────────────────────────────────────────────────

#[test]
fn drive_file_deserialises_full_api_response() {
    // Real-ish Drive API v3 file response. All optional fields are present.
    let raw = r#"{
        "kind": "drive#file",
        "id": "1A2b3C",
        "name": "Quarterly Report.pdf",
        "mimeType": "application/pdf",
        "parents": ["0AB-parent"],
        "size": "1048576",
        "modifiedTime": "2026-05-28T10:00:00.000Z",
        "createdTime": "2026-05-01T08:30:00.000Z",
        "md5Checksum": "098f6bcd4621d373cade4e832627b4f6",
        "trashed": false,
        "webViewLink": "https://drive.google.com/file/d/1A2b3C/view",
        "driveId": "0AB-shared-drive"
    }"#;

    let file: DriveFile = serde_json::from_str(raw).unwrap();
    assert_eq!(file.id, "1A2b3C");
    assert_eq!(file.name, "Quarterly Report.pdf");
    assert_eq!(file.mime_type, "application/pdf");
    assert_eq!(file.parents, vec!["0AB-parent".to_string()]);
    assert_eq!(file.size.as_deref(), Some("1048576"));
    assert_eq!(
        file.modified_time.as_deref(),
        Some("2026-05-28T10:00:00.000Z")
    );
    assert_eq!(
        file.md5_checksum.as_deref(),
        Some("098f6bcd4621d373cade4e832627b4f6")
    );
    assert!(!file.trashed);
    assert_eq!(file.drive_id.as_deref(), Some("0AB-shared-drive"));
}

#[test]
fn drive_file_deserialises_minimal_response() {
    // Real folders in My Drive often return just the bare minimum.
    let raw = r#"{
        "id": "minimal-1",
        "name": "Notes",
        "mimeType": "application/vnd.google-apps.folder"
    }"#;

    let file: DriveFile = serde_json::from_str(raw).unwrap();
    assert_eq!(file.id, "minimal-1");
    assert_eq!(file.name, "Notes");
    assert!(file.parents.is_empty());
    assert!(file.size.is_none());
    assert!(file.modified_time.is_none());
    assert!(file.md5_checksum.is_none());
    assert!(!file.trashed);
    assert!(file.drive_id.is_none());
}

#[test]
fn file_list_response_deserialises_with_pagination() {
    let raw = r#"{
        "kind": "drive#fileList",
        "nextPageToken": "CAIYFAaMKQ",
        "files": [
            {
                "id": "f-1",
                "name": "a.txt",
                "mimeType": "text/plain",
                "parents": ["root"]
            },
            {
                "id": "f-2",
                "name": "b.txt",
                "mimeType": "text/plain",
                "parents": ["root"]
            }
        ]
    }"#;

    let list: FileListResponse = serde_json::from_str(raw).unwrap();
    assert_eq!(list.files.len(), 2);
    assert_eq!(list.files[0].id, "f-1");
    assert_eq!(list.next_page_token.as_deref(), Some("CAIYFAaMKQ"));
}

#[test]
fn file_list_response_deserialises_empty() {
    let raw = r#"{"files": []}"#;
    let list: FileListResponse = serde_json::from_str(raw).unwrap();
    assert!(list.files.is_empty());
    assert!(list.next_page_token.is_none());
}

#[test]
fn changes_response_deserialises_full_shape() {
    let raw = r#"{
        "kind": "drive#changeList",
        "nextPageToken": "page-2",
        "newStartPageToken": "start-9",
        "changes": [
            {
                "kind": "drive#change",
                "type": "file",
                "fileId": "file-x",
                "removed": false,
                "file": {
                    "id": "file-x",
                    "name": "renamed.txt",
                    "mimeType": "text/plain",
                    "parents": ["root"],
                    "trashed": false
                }
            },
            {
                "kind": "drive#change",
                "type": "file",
                "fileId": "file-y",
                "removed": true
            }
        ]
    }"#;

    let changes: ChangesResponse = serde_json::from_str(raw).unwrap();
    assert_eq!(changes.changes.len(), 2);
    assert_eq!(changes.next_page_token.as_deref(), Some("page-2"));
    assert_eq!(changes.new_start_page_token.as_deref(), Some("start-9"));
    assert!(!changes.changes[0].removed.unwrap_or(true));
    assert!(changes.changes[0].file.is_some());
    assert!(changes.changes[1].removed.unwrap_or(false));
    assert!(changes.changes[1].file.is_none());
}

#[test]
fn about_response_deserialises_storage_quota() {
    let raw = r#"{
        "storageQuota": {
            "limit": "161061273600",
            "usage": "1073741824",
            "usageInDrive": "536870912",
            "usageInDriveTrash": "0"
        }
    }"#;

    let about: AboutResponse = serde_json::from_str(raw).unwrap();
    let quota = about.storage_quota.expect("expected storageQuota");
    assert_eq!(quota.limit.as_deref(), Some("161061273600"));
    assert_eq!(quota.usage.as_deref(), Some("1073741824"));
}

#[test]
fn about_response_deserialises_without_quota() {
    // A fresh account with no storage quota yet (e.g. shared drive only)
    // returns an empty `about` payload.
    let raw = r"{}";
    let about: AboutResponse = serde_json::from_str(raw).unwrap();
    assert!(about.storage_quota.is_none());
}

#[test]
fn shared_drive_list_response_deserialises() {
    let raw = r#"{
        "drives": [
            {"id": "0AB-eng", "name": "Engineering"},
            {"id": "0AB-design", "name": "Design"}
        ],
        "nextPageToken": "page-2"
    }"#;

    let list: SharedDriveListResponse = serde_json::from_str(raw).unwrap();
    assert_eq!(list.drives.len(), 2);
    assert_eq!(list.drives[0].id, "0AB-eng");
    assert_eq!(list.drives[0].name, "Engineering");
    assert_eq!(list.next_page_token.as_deref(), Some("page-2"));
}

// ── 8. Token store round-trip ────────────────────────────────────────────────

#[tokio::test]
async fn token_store_save_load_round_trip() {
    let store = InMemoryTokenStore::default();
    let tokens = AuthTokens {
        access_token: "access-abc".to_string(),
        refresh_token: "refresh-xyz".to_string(),
        expires_at: chrono::DateTime::from_timestamp(1_800_000_000, 0)
            .unwrap_or_else(chrono::Utc::now),
    };

    // Initial load returns None.
    assert!(store.load("primary").await.unwrap().is_none());

    // Save and reload: fields round-trip exactly.
    store.save("primary", &tokens).await.unwrap();
    let loaded = store
        .load("primary")
        .await
        .unwrap()
        .expect("tokens must be present after save");
    assert_eq!(loaded.access_token, tokens.access_token);
    assert_eq!(loaded.refresh_token, tokens.refresh_token);
    assert_eq!(loaded.expires_at, tokens.expires_at);
}

#[tokio::test]
async fn token_store_save_overwrites_existing_entry() {
    let store = InMemoryTokenStore::default();
    let first = AuthTokens {
        access_token: "old-access".to_string(),
        refresh_token: "old-refresh".to_string(),
        expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
    };
    let second = AuthTokens {
        access_token: "new-access".to_string(),
        refresh_token: "new-refresh".to_string(),
        expires_at: chrono::Utc::now() + chrono::Duration::hours(2),
    };

    store.save("acct", &first).await.unwrap();
    store.save("acct", &second).await.unwrap();
    let loaded = store.load("acct").await.unwrap().unwrap();
    assert_eq!(loaded.access_token, "new-access");
    assert_eq!(loaded.refresh_token, "new-refresh");
}

#[tokio::test]
async fn token_store_segregates_accounts() {
    let store = InMemoryTokenStore::default();
    let tokens_a = AuthTokens {
        access_token: "a-access".to_string(),
        refresh_token: "a-refresh".to_string(),
        expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
    };
    let tokens_b = AuthTokens {
        access_token: "b-access".to_string(),
        refresh_token: "b-refresh".to_string(),
        expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
    };

    store.save("acct-a", &tokens_a).await.unwrap();
    store.save("acct-b", &tokens_b).await.unwrap();

    let loaded_a = store.load("acct-a").await.unwrap().unwrap();
    let loaded_b = store.load("acct-b").await.unwrap().unwrap();
    assert_eq!(loaded_a.access_token, "a-access");
    assert_eq!(loaded_b.access_token, "b-access");

    // The third account has no slot.
    assert!(store.load("acct-c").await.unwrap().is_none());
}

#[tokio::test]
async fn token_store_refreshed_tokens_preserve_refresh_token_when_missing() {
    // Refresh responses don't always include a new refresh token. The store
    // contract is "save these tokens" — it is the caller's job to decide
    // whether to keep the old refresh token. This test asserts the
    // round-trip preserves whatever the caller chose to store.
    let store = InMemoryTokenStore::default();
    let refreshed = AuthTokens {
        access_token: "fresh-access".to_string(),
        // Empty refresh token: caller decided the OAuth2 server did not
        // rotate it. The store must preserve the empty string verbatim.
        refresh_token: String::new(),
        expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
    };
    store.save("acct", &refreshed).await.unwrap();
    let loaded = store.load("acct").await.unwrap().unwrap();
    assert_eq!(loaded.access_token, "fresh-access");
    assert!(loaded.refresh_token.is_empty());
}
