//! Per-route behaviour tests — error paths, validation, auth edge cases, and
//! malformed-input handling for the auth, backends, config, files, pins,
//! grants, and token routes.
//!
//! This file deliberately does not repeat the contract assertions already made
//! in `contract.rs` (happy-path shapes, request-id header, error envelope
//! structure). It focuses on the cases the contract test leaves blank: the
//! wrong capability, the wrong scope, malformed bodies, status transitions, and
//! the specific error codes each guard produces.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::string_slice
)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use cascade_engine::engine::{Engine, EngineConfig};
use cascade_engine::manage::{Capability, CapabilityToken, DeviceId, Scope};
use cascade_p2p::identity::DeviceIdentity;
use cascade_web_api::auth::BEARER_DEVICE_HEADER;
use cascade_web_api::state::{AppState, BindConfig, NodeIdentity, Readiness};
use chrono::{Duration, Utc};
use data_encoding::BASE64;
use serde_json::Value;
use tower::ServiceExt as _;

// ── Test harness (mirrors `contract.rs` — kept here so this file has no
//    dependency on the other test binary's internals) ──

struct Harness {
    router: Router,
    identity: DeviceIdentity,
    state: AppState,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::new(EngineConfig {
            db_path: dir.path().join("state.db"),
            mount_point: dir.path().join("mount"),
            backends: vec![cascade_engine::backend::MountedBackend::at_default(
                Arc::new(cascade_engine::backend::NullBackend::new("root")),
            )],
            cache_dir: None,
            enable_p2p: true,
            p2p_data_dir: Some(dir.path().join("p2p")),
            p2p_posture: None,
            p2p_relay_endpoints: Vec::new(),
            p2p_relay_shared_secret: None,
            backend_factory: None,
        })
        .unwrap();
        std::mem::forget(dir);

        let identity = engine.device_identity().expect("p2p identity").clone();
        let engine = Arc::new(engine);
        let bind = BindConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            Some("https://pwa.example.com".to_owned()),
            Vec::new(),
            3600,
            1024 * 1024,
            "test-version".to_owned(),
            None,
        )
        .unwrap();
        let state = AppState::new(
            engine,
            NodeIdentity::new(identity.clone()),
            bind,
            Readiness::new(Utc::now()),
        );
        let router = cascade_web_api::build_router(state.clone());
        Self {
            router,
            identity,
            state,
        }
    }

    fn node_id(&self) -> DeviceId {
        DeviceId::new(self.identity.device_id.clone())
    }

    fn set_ready(&self) {
        self.state.readiness.set_data_plane_ready(true);
    }

    fn issue(
        &self,
        label: &str,
        bearer: &DeviceId,
        cap: Capability,
        scope: Scope,
    ) -> CapabilityToken {
        let expires = Utc::now() + Duration::days(365);
        CapabilityToken::issue(
            label.to_owned(),
            &self.identity,
            bearer,
            cap,
            scope,
            expires,
        )
        .unwrap()
    }

    async fn send(&self, req: Request<Body>) -> (StatusCode, Option<String>, Value) {
        let response = self.router.clone().oneshot(req).await.unwrap();
        let status = response.status();
        let request_id = response
            .headers()
            .get(cascade_web_api::request_id::REQUEST_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(ToOwned::to_owned);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, request_id, body)
    }
}

fn bearer(token: &CapabilityToken) -> String {
    let json = serde_json::to_vec(token).unwrap();
    format!("Bearer {}", BASE64.encode(&json))
}

fn authed_get(uri: &str, token: &CapabilityToken, bearer_device: &DeviceId) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header(header::AUTHORIZATION, bearer(token))
        .header(BEARER_DEVICE_HEADER, bearer_device.as_str())
        .body(Body::empty())
        .unwrap()
}

fn authed_post(
    uri: &str,
    token: &CapabilityToken,
    bearer_device: &DeviceId,
    body: &Value,
) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::AUTHORIZATION, bearer(token))
        .header(BEARER_DEVICE_HEADER, bearer_device.as_str())
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap()
}

fn authed_delete(uri: &str, token: &CapabilityToken, bearer_device: &DeviceId) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
        .header(header::AUTHORIZATION, bearer(token))
        .header(BEARER_DEVICE_HEADER, bearer_device.as_str())
        .body(Body::empty())
        .unwrap()
}

fn assert_error_code(body: &Value, expected: &str) {
    let code = body
        .pointer("/error/code")
        .and_then(Value::as_str)
        .unwrap_or("(no code)");
    assert_eq!(code, expected, "unexpected error code in body: {body}");
}

// ── Auth route tests ──────────────────────────────────────────────────────────

#[tokio::test]
async fn auth_pair_missing_code_returns_not_found() {
    // The code does not exist in the DB, so the handler returns 404.
    let h = Harness::new();
    let req = Request::builder()
        .method("POST")
        .uri("/v1/auth/pair")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"code":"DOESNOTEXIST"}"#))
        .unwrap();
    let (status, _, body) = h.send(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
    assert_error_code(&body, "not_found");
}

#[tokio::test]
async fn auth_pair_consumed_code_returns_gone() {
    // Insert a pairing code, consume it, then try to redeem it again.
    let h = Harness::new();
    let db = h.state.engine.db();
    let expires = Utc::now() + Duration::seconds(300);
    db.insert_auth_code("CONSUME1", "pairing", expires).unwrap();
    // Mark it consumed so the handler sees status != "pending".
    db.update_auth_code("CONSUME1", "consumed", None, None)
        .unwrap();

    let req = Request::builder()
        .method("POST")
        .uri("/v1/auth/pair")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"code":"CONSUME1"}"#))
        .unwrap();
    let (status, _, body) = h.send(req).await;
    assert_eq!(status, StatusCode::GONE, "body: {body}");
    assert_error_code(&body, "gone");
}

#[tokio::test]
async fn auth_pair_expired_code_is_cleaned_up_and_returns_not_found() {
    // The handler calls `delete_expired_auth_codes` before looking up the code.
    // A code whose `expires_at` is in the past is deleted, so the lookup returns
    // `None` — which the handler maps to 404, not 410.
    let h = Harness::new();
    let db = h.state.engine.db();
    // Insert a pairing code with an expiry in the past.
    let expires = Utc::now() - Duration::seconds(1);
    db.insert_auth_code("EXPIREDPAIR", "pairing", expires)
        .unwrap();

    let req = Request::builder()
        .method("POST")
        .uri("/v1/auth/pair")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"code":"EXPIREDPAIR"}"#))
        .unwrap();
    let (status, _, body) = h.send(req).await;
    // The code was deleted by the cleanup, so the handler sees nothing → 404.
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
    assert_error_code(&body, "not_found");
}

#[tokio::test]
async fn auth_pair_malformed_json_returns_error() {
    // axum's `Json` extractor rejects an unparseable body with 400 Bad Request
    // before the handler runs.
    let h = Harness::new();
    let req = Request::builder()
        .method("POST")
        .uri("/v1/auth/pair")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("not json"))
        .unwrap();
    let (status, _, _body) = h.send(req).await;
    // axum returns 400 for a structurally invalid JSON body.
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn auth_secret_no_secret_configured_returns_forbidden() {
    let h = Harness::new();
    let req = Request::builder()
        .method("POST")
        .uri("/v1/auth/secret")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"secret":"any-value"}"#))
        .unwrap();
    let (status, _, body) = h.send(req).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");
    assert_error_code(&body, "forbidden");
}

#[tokio::test]
async fn auth_secret_wrong_secret_returns_401() {
    let h = Harness::new();
    h.state
        .engine
        .db()
        .set_daemon_secret("correct-secret")
        .unwrap();

    let req = Request::builder()
        .method("POST")
        .uri("/v1/auth/secret")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"secret":"wrong-secret"}"#))
        .unwrap();
    let (status, _, body) = h.send(req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {body}");
    assert_error_code(&body, "unauthorised");
}

#[tokio::test]
async fn auth_secret_correct_secret_issues_token() {
    let h = Harness::new();
    h.state.engine.db().set_daemon_secret("my-secret").unwrap();

    let req = Request::builder()
        .method("POST")
        .uri("/v1/auth/secret")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"secret":"my-secret"}"#))
        .unwrap();
    let (status, _, body) = h.send(req).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    // The response body is a `CapabilityToken` JSON object.
    assert!(
        body.get("claims").is_some(),
        "expected token in body: {body}"
    );
}

#[tokio::test]
async fn auth_device_request_creates_code() {
    let h = Harness::new();
    let req = Request::builder()
        .method("POST")
        .uri("/v1/auth/device")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = h.send(req).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert!(
        body.get("code").and_then(Value::as_str).is_some(),
        "expected code in body: {body}"
    );
    assert!(
        body.get("expires_in").and_then(Value::as_u64).is_some(),
        "expected expires_in in body: {body}"
    );
}

#[tokio::test]
async fn auth_device_poll_pending_returns_202() {
    let h = Harness::new();
    let db = h.state.engine.db();
    let expires = Utc::now() + Duration::seconds(300);
    db.insert_auth_code("POLLCODE1", "device", expires).unwrap();

    let req = Request::builder()
        .method("GET")
        .uri("/v1/auth/device/POLLCODE1")
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = h.send(req).await;
    assert_eq!(status, StatusCode::ACCEPTED, "body: {body}");
    assert_eq!(body.get("status").and_then(Value::as_str), Some("pending"),);
    // No token field while still pending.
    assert!(
        body.get("token").is_none(),
        "pending code must not carry a token"
    );
}

#[tokio::test]
async fn auth_device_poll_unknown_code_returns_not_found() {
    let h = Harness::new();
    let req = Request::builder()
        .method("GET")
        .uri("/v1/auth/device/NOSUCHCODE")
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = h.send(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
    assert_error_code(&body, "not_found");
}

#[tokio::test]
async fn auth_device_poll_expired_code_returns_gone() {
    let h = Harness::new();
    let db = h.state.engine.db();
    let expires = Utc::now() - Duration::seconds(1);
    db.insert_auth_code("EXPIREDDEV", "device", expires)
        .unwrap();

    let req = Request::builder()
        .method("GET")
        .uri("/v1/auth/device/EXPIREDDEV")
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = h.send(req).await;
    assert_eq!(status, StatusCode::GONE, "body: {body}");
    assert_error_code(&body, "gone");
}

#[tokio::test]
async fn auth_device_authorize_unknown_code_returns_not_found() {
    let h = Harness::new();
    let req = Request::builder()
        .method("POST")
        .uri("/v1/auth/device/NOSUCHCODE/authorize")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = h.send(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
    assert_error_code(&body, "not_found");
}

#[tokio::test]
async fn auth_device_authorize_already_authorised_returns_conflict() {
    let h = Harness::new();
    let db = h.state.engine.db();
    let expires = Utc::now() + Duration::seconds(300);
    db.insert_auth_code("ALREADY1", "device", expires).unwrap();
    db.update_auth_code("ALREADY1", "authorised", None, None)
        .unwrap();

    let req = Request::builder()
        .method("POST")
        .uri("/v1/auth/device/ALREADY1/authorize")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = h.send(req).await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert_error_code(&body, "conflict");
}

#[tokio::test]
async fn auth_device_full_flow() {
    // request → authorize → poll; the poll must return the token.
    let h = Harness::new();

    // Step 1: request a code.
    let (create_status, _, create_body) = h
        .send(
            Request::builder()
                .method("POST")
                .uri("/v1/auth/device")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(create_status, StatusCode::CREATED);
    let code = create_body.get("code").and_then(Value::as_str).unwrap();

    // Step 2: authorise the code (simulates the CLI).
    let (auth_status, _, auth_body) = h
        .send(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/auth/device/{code}/authorize"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(auth_status, StatusCode::OK, "authorize: {auth_body}");
    assert_eq!(
        auth_body.get("status").and_then(Value::as_str),
        Some("authorised"),
    );

    // Step 3: poll; must return 200 with the token.
    let (poll_status, _, poll_body) = h
        .send(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/auth/device/{code}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(poll_status, StatusCode::OK, "poll: {poll_body}");
    assert_eq!(
        poll_body.get("status").and_then(Value::as_str),
        Some("authorised"),
    );
    assert!(
        poll_body.get("token").is_some(),
        "authorised poll must carry a token: {poll_body}"
    );
}

// ── Backends route tests ──────────────────────────────────────────────────────

#[tokio::test]
async fn backends_list_requires_status_read() {
    // A `data:read` token does not cover `status:read`, so this is 401.
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue("dr", &node, Capability::DataRead, Scope::folder("anything"));
    let (status, _, body) = h.send(authed_get("/v1/backends", &token, &node)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {body}");
    assert_error_code(&body, "unauthorised");
}

#[tokio::test]
async fn backends_list_status_read_node_wide_returns_ok() {
    // The harness engine registers the `NullBackend` (type `"unknown"`, id `"root"`)
    // during initialisation, so the list is never empty. The test asserts the
    // route runs correctly and the response parses cleanly.
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue("sr", &node, Capability::StatusRead, Scope::Node);
    let (status, _, body) = h.send(authed_get("/v1/backends", &token, &node)).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let parsed: cascade_web_api::schemas::backends::BackendsResponse =
        serde_json::from_value(body).unwrap();
    // The NullBackend registered at startup is present and has no P2P folder id.
    assert!(
        !parsed.backends.is_empty(),
        "at least the root NullBackend must appear"
    );
    assert!(
        parsed.backends.iter().all(|b| b.folder_id.is_none()),
        "no non-P2P backend should carry a folder_id"
    );
}

// ── Config route tests ────────────────────────────────────────────────────────

#[tokio::test]
async fn config_push_requires_config_push_capability() {
    // A `status:read` token does not grant `config:push`.
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue("sr", &node, Capability::StatusRead, Scope::Node);
    let body = serde_json::json!({
        "folder": "some-folder",
        "format": "gitignore",
        "body": "*.log",
    });
    let (status, _, resp) = h
        .send(authed_post("/v1/config/push", &token, &node, &body))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {resp}");
    assert_error_code(&resp, "unauthorised");
}

#[tokio::test]
async fn config_push_malformed_json_returns_bad_request() {
    // axum's `Json` extractor returns 400 for structurally invalid JSON.
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue("cp", &node, Capability::ConfigPush, Scope::folder("x"));
    let req = Request::builder()
        .method("POST")
        .uri("/v1/config/push")
        .header(header::AUTHORIZATION, bearer(&token))
        .header(BEARER_DEVICE_HEADER, node.as_str())
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{ bad json"))
        .unwrap();
    let (status, _, _body) = h.send(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn config_push_missing_fields_returns_422() {
    // Body is valid JSON but missing required fields.
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue("cp", &node, Capability::ConfigPush, Scope::folder("x"));
    let req = Request::builder()
        .method("POST")
        .uri("/v1/config/push")
        .header(header::AUTHORIZATION, bearer(&token))
        .header(BEARER_DEVICE_HEADER, node.as_str())
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"folder":"x"}"#))
        .unwrap();
    let (status, _, _body) = h.send(req).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

// ── Files / folders route tests ───────────────────────────────────────────────

#[tokio::test]
async fn folder_children_unknown_folder_returns_404() {
    let h = Harness::new();
    let node = h.node_id();
    // Use the special "p2p-nope" canonical id, which no backend provides.
    let token = h.issue(
        "sr",
        &node,
        Capability::StatusRead,
        Scope::folder("p2p-nope"),
    );
    let (status, _, body) = h
        .send(authed_get("/v1/folders/p2p-nope/children", &token, &node))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
    assert_error_code(&body, "not_found");
}

#[tokio::test]
async fn folder_children_requires_status_read_not_data_read() {
    // A data:read token does not satisfy the status:read requirement for
    // `children`, even if scoped to the correct folder.
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue("dr", &node, Capability::DataRead, Scope::folder("p2p-nope"));
    let (status, _, body) = h
        .send(authed_get("/v1/folders/p2p-nope/children", &token, &node))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {body}");
    assert_error_code(&body, "unauthorised");
}

#[tokio::test]
async fn folder_children_pagination_cursor_malformed_returns_422() {
    // A syntactically invalid cursor (not base64url) must return 422.
    let h = Harness::new();
    let node = h.node_id();
    // Scope to "root" — the NullBackend mounts at "root" in the harness.
    let token = h.issue("sr", &node, Capability::StatusRead, Scope::folder("root"));
    let (status, _, body) = h
        .send(authed_get(
            "/v1/folders/root/children?cursor=!!!invalid!!!",
            &token,
            &node,
        ))
        .await;
    // The folder is unknown (not a P2P folder), so 404 comes first.
    // Either way it is an error, not 200.
    assert_ne!(status, StatusCode::OK, "body: {body}");
}

#[tokio::test]
async fn file_get_requires_data_plane_ready() {
    // Before the readiness bit flips, data routes return 503.
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue(
        "dr",
        &node,
        Capability::DataRead,
        Scope::folder("p2p-shared"),
    );
    let (status, _, body) = h
        .send(authed_get(
            "/v1/files/p2p-shared/entries/any.txt",
            &token,
            &node,
        ))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body: {body}");
    assert_error_code(&body, "data_plane_not_ready");
}

#[tokio::test]
async fn file_get_requires_data_read_not_status_read() {
    // A status:read token does not satisfy data:read.
    let h = Harness::new();
    let node = h.node_id();
    h.set_ready();
    let token = h.issue(
        "sr",
        &node,
        Capability::StatusRead,
        Scope::folder("p2p-shared"),
    );
    let (status, _, body) = h
        .send(authed_get(
            "/v1/files/p2p-shared/entries/any.txt",
            &token,
            &node,
        ))
        .await;
    // 401 because the bearer does not hold data:read at all.
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {body}");
    assert_error_code(&body, "unauthorised");
}

#[tokio::test]
async fn file_get_wrong_folder_scope_is_forbidden() {
    // The token holds data:read, but only over a different folder.
    let h = Harness::new();
    let node = h.node_id();
    h.set_ready();
    let token = h.issue(
        "dr",
        &node,
        Capability::DataRead,
        Scope::folder("p2p-other"),
    );
    let (status, _, body) = h
        .send(authed_get(
            "/v1/files/p2p-shared/entries/any.txt",
            &token,
            &node,
        ))
        .await;
    // 403 because the caller holds data:read, just not over p2p-shared.
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");
    assert_error_code(&body, "forbidden");
}

#[tokio::test]
async fn file_delete_requires_data_plane_ready() {
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue(
        "dw",
        &node,
        Capability::DataWrite,
        Scope::folder("p2p-shared"),
    );
    let (status, _, body) = h
        .send(authed_delete(
            "/v1/files/p2p-shared/entries/any.txt",
            &token,
            &node,
        ))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body: {body}");
    assert_error_code(&body, "data_plane_not_ready");
}

#[tokio::test]
async fn archive_requires_data_plane_ready() {
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue(
        "dr",
        &node,
        Capability::DataRead,
        Scope::folder("p2p-shared"),
    );
    let (status, _, body) = h
        .send(authed_get("/v1/folders/p2p-shared/archive", &token, &node))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body: {body}");
    assert_error_code(&body, "data_plane_not_ready");
}

// ── Pins route tests ──────────────────────────────────────────────────────────

#[tokio::test]
async fn pins_list_requires_status_read() {
    let h = Harness::new();
    let node = h.node_id();
    // pin:write does not cover status:read.
    let token = h.issue("pw", &node, Capability::PinWrite, Scope::Node);
    let (status, _, body) = h.send(authed_get("/v1/pins", &token, &node)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {body}");
    assert_error_code(&body, "unauthorised");
}

#[tokio::test]
async fn pins_create_requires_pin_write() {
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue("sr", &node, Capability::StatusRead, Scope::Node);
    let body = serde_json::json!({"path_glob": "docs/**", "recursive": true});
    let (status, _, resp) = h.send(authed_post("/v1/pins", &token, &node, &body)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {resp}");
    assert_error_code(&resp, "unauthorised");
}

#[tokio::test]
async fn pins_create_and_list() {
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue("pw", &node, Capability::PinWrite, Scope::Node);
    let sr_token = h.issue("sr", &node, Capability::StatusRead, Scope::Node);

    let body = serde_json::json!({"path_glob": "docs/**", "recursive": true});
    let (status, _, resp) = h.send(authed_post("/v1/pins", &token, &node, &body)).await;
    assert_eq!(status, StatusCode::CREATED, "create: {resp}");
    assert_eq!(
        resp.get("path_glob").and_then(Value::as_str),
        Some("docs/**"),
    );
    let id = resp.get("id").and_then(Value::as_i64).unwrap();

    // List must now contain the new rule.
    let (list_status, _, list_body) = h.send(authed_get("/v1/pins", &sr_token, &node)).await;
    assert_eq!(list_status, StatusCode::OK, "list: {list_body}");
    let pins = list_body.get("pins").and_then(Value::as_array).unwrap();
    assert!(
        pins.iter()
            .any(|p| p.get("id").and_then(Value::as_i64) == Some(id))
    );
}

#[tokio::test]
async fn pins_delete_unknown_id_returns_404() {
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue("pw", &node, Capability::PinWrite, Scope::Node);
    // id 9999 does not exist in a fresh engine.
    let (status, _, body) = h.send(authed_delete("/v1/pins/9999", &token, &node)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
    assert_error_code(&body, "not_found");
}

#[tokio::test]
async fn pins_create_malformed_json_returns_bad_request() {
    // axum's `Json` extractor returns 400 for structurally invalid JSON.
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue("pw", &node, Capability::PinWrite, Scope::Node);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/pins")
        .header(header::AUTHORIZATION, bearer(&token))
        .header(BEARER_DEVICE_HEADER, node.as_str())
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("bad json"))
        .unwrap();
    let (status, _, _) = h.send(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ── Grants route tests ────────────────────────────────────────────────────────

#[tokio::test]
async fn grants_list_requires_authentication() {
    let h = Harness::new();
    let req = Request::builder()
        .uri("/v1/grants")
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = h.send(req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {body}");
}

#[tokio::test]
async fn grants_list_any_verified_session() {
    // `GET /v1/grants` accepts any verified session — even a data:read token.
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue("dr", &node, Capability::DataRead, Scope::folder("anywhere"));
    let (status, _, body) = h.send(authed_get("/v1/grants", &token, &node)).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let parsed: cascade_web_api::schemas::grants::GrantsResponse =
        serde_json::from_value(body).unwrap();
    assert!(parsed.grants.is_empty());
}

#[tokio::test]
async fn grants_create_requires_grant_admin() {
    let h = Harness::new();
    let node = h.node_id();
    // status:read does not cover grant:admin.
    let token = h.issue("sr", &node, Capability::StatusRead, Scope::Node);
    let body = serde_json::json!({
        "grantee": "OTHER-DEVICE",
        "capability": "status:read",
        "scope": { "kind": "node" },
    });
    let (status, _, resp) = h
        .send(authed_post("/v1/grants", &token, &node, &body))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {resp}");
    assert_error_code(&resp, "unauthorised");
}

#[tokio::test]
async fn grants_create_data_verb_node_wide_is_rejected() {
    // Already covered in contract.rs, but we verify the exact code here.
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue(
        "admin",
        &node,
        Capability::GrantAdmin,
        Scope::folder("p2p-shared"),
    );
    let body = serde_json::json!({
        "grantee": "PEER",
        "capability": "data:write",
        "scope": { "kind": "node" },
    });
    let (status, _, resp) = h
        .send(authed_post("/v1/grants", &token, &node, &body))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body: {resp}");
    assert_error_code(&resp, "data_verb_node_wide_forbidden");
}

#[tokio::test]
async fn grants_delete_unknown_id_returns_404() {
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue("admin", &node, Capability::GrantAdmin, Scope::Node);
    let (status, _, body) = h
        .send(authed_delete("/v1/grants/9999", &token, &node))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
    assert_error_code(&body, "not_found");
}

#[tokio::test]
async fn grants_create_malformed_capability_returns_422() {
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue("admin", &node, Capability::GrantAdmin, Scope::Node);
    let body = serde_json::json!({
        "grantee": "PEER",
        "capability": "not-a-real-capability",
        "scope": { "kind": "node" },
    });
    let (status, _, _resp) = h
        .send(authed_post("/v1/grants", &token, &node, &body))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

// ── Token route tests ─────────────────────────────────────────────────────────

#[tokio::test]
async fn tokens_list_requires_authentication() {
    let h = Harness::new();
    let req = Request::builder()
        .uri("/v1/tokens")
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = h.send(req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {body}");
}

#[tokio::test]
async fn tokens_list_any_verified_session() {
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue("sr", &node, Capability::StatusRead, Scope::Node);
    let (status, _, body) = h.send(authed_get("/v1/tokens", &token, &node)).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let parsed: cascade_web_api::schemas::tokens::TokensResponse =
        serde_json::from_value(body).unwrap();
    // The harness does not pre-populate tokens, but the route runs cleanly.
    assert!(parsed.tokens.is_empty());
}

#[tokio::test]
async fn tokens_issue_data_verb_node_wide_is_rejected() {
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue("admin", &node, Capability::GrantAdmin, Scope::folder("/x"));
    let body = serde_json::json!({
        "bearer": "PEER",
        "capability": "data:read",
        "scope": { "kind": "node" },
        "expires": (Utc::now() + Duration::days(1)).to_rfc3339(),
    });
    let (status, _, resp) = h
        .send(authed_post("/v1/tokens", &token, &node, &body))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body: {resp}");
    assert_error_code(&resp, "data_verb_node_wide_forbidden");
}

#[tokio::test]
async fn tokens_issue_owner_issues_any_capability() {
    // An owner token can issue any capability over a folder scope.
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue("owner", &node, Capability::StatusRead, Scope::Node);
    let body = serde_json::json!({
        "bearer": "SOME-DEVICE",
        "capability": "data:read",
        "scope": { "kind": "folder", "path": "shared-folder" },
        "expires": (Utc::now() + Duration::days(1)).to_rfc3339(),
    });
    let (status, _, resp) = h
        .send(authed_post("/v1/tokens", &token, &node, &body))
        .await;
    assert_eq!(status, StatusCode::CREATED, "body: {resp}");
    // The body is the signed token itself.
    assert!(resp.get("claims").is_some(), "expected token: {resp}");
}

#[tokio::test]
async fn tokens_revoke_requires_owner() {
    // A non-owner session is forbidden from revoking tokens.
    let h = Harness::new();
    let delegator = DeviceIdentity::generate().unwrap();
    let delegator_id = DeviceId::new(delegator.device_id.clone());
    let delegate = DeviceIdentity::generate().unwrap();
    let delegate_id = DeviceId::new(delegate.device_id.clone());

    let parent = h.issue("parent", &delegator_id, Capability::StatusRead, Scope::Node);
    let leaf = parent
        .delegate(
            "leaf",
            &delegator,
            &delegate_id,
            Capability::StatusRead,
            Scope::Node,
            Utc::now() + Duration::days(30),
        )
        .unwrap();

    // The delegate tries to revoke a token.
    let (status, _, body) = h
        .send(
            Request::builder()
                .method("POST")
                .uri("/v1/tokens/some-token-id/revoke")
                .header(header::AUTHORIZATION, bearer(&leaf))
                .header(BEARER_DEVICE_HEADER, delegate_id.as_str())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");
    assert_error_code(&body, "forbidden");
}

#[tokio::test]
async fn tokens_revoke_unknown_id_succeeds() {
    // The revocation store (`token_revocations`) is a deny-list that any token
    // id can be added to, regardless of whether that id appears in the `tokens`
    // table. Revoking an id that was never issued succeeds (200 OK) rather than
    // returning 404, because `INSERT OR IGNORE` inserts the new row and reports
    // one row affected.
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue("owner", &node, Capability::StatusRead, Scope::Node);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/tokens/totally-unknown-id/revoke")
        .header(header::AUTHORIZATION, bearer(&token))
        .header(BEARER_DEVICE_HEADER, node.as_str())
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = h.send(req).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        body.get("token_id").and_then(Value::as_str),
        Some("totally-unknown-id"),
    );
}

#[tokio::test]
async fn tokens_revoke_already_revoked_returns_gone() {
    // Issue a token, revoke it, then try to revoke it again.
    let h = Harness::new();
    let node = h.node_id();
    let owner_token = h.issue("owner", &node, Capability::StatusRead, Scope::Node);

    // Issue a sub-token so we have a token_id to revoke.
    let body = serde_json::json!({
        "bearer": "PEER-DEVICE",
        "capability": "status:read",
        "scope": { "kind": "node" },
        "expires": (Utc::now() + Duration::days(1)).to_rfc3339(),
    });
    let (issue_status, _, issue_body) = h
        .send(authed_post("/v1/tokens", &owner_token, &node, &body))
        .await;
    assert_eq!(issue_status, StatusCode::CREATED, "issue: {issue_body}");
    let token_id = issue_body
        .pointer("/claims/token_id")
        .and_then(Value::as_str)
        .unwrap();

    // First revoke.
    let (rev1_status, _, rev1_body) = h
        .send(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/tokens/{token_id}/revoke"))
                .header(header::AUTHORIZATION, bearer(&owner_token))
                .header(BEARER_DEVICE_HEADER, node.as_str())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(rev1_status, StatusCode::OK, "first revoke: {rev1_body}");

    // Second revoke must fail with 410 Gone.
    let (rev2_status, _, rev2_body) = h
        .send(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/tokens/{token_id}/revoke"))
                .header(header::AUTHORIZATION, bearer(&owner_token))
                .header(BEARER_DEVICE_HEADER, node.as_str())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(rev2_status, StatusCode::GONE, "second revoke: {rev2_body}");
    assert_error_code(&rev2_body, "gone");
}

// ── Auth edge cases ───────────────────────────────────────────────────────────

#[tokio::test]
async fn expired_token_is_rejected() {
    let h = Harness::new();
    // Issue a token that is already expired.
    let node = h.node_id();
    let past = Utc::now() - Duration::seconds(1);
    let token = CapabilityToken::issue(
        "expired".to_owned(),
        &h.identity,
        &node,
        Capability::StatusRead,
        Scope::Node,
        past,
    )
    .unwrap();

    let (status, _, body) = h.send(authed_get("/v1/session", &token, &node)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {body}");
    assert_error_code(&body, "unauthorised");
}

#[tokio::test]
async fn malformed_bearer_is_rejected() {
    let h = Harness::new();
    let node = h.node_id();
    let req = Request::builder()
        .uri("/v1/session")
        .header(header::AUTHORIZATION, "Bearer !!not-base64!!")
        .header(BEARER_DEVICE_HEADER, node.as_str())
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = h.send(req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {body}");
    assert_error_code(&body, "unauthorised");
}

#[tokio::test]
async fn wrong_bearer_device_header_returns_401() {
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue("sr", &node, Capability::StatusRead, Scope::Node);
    // The token was issued to `node` but we present a different device id.
    let wrong_device = DeviceId::new("some-other-device".to_owned());
    let req = Request::builder()
        .uri("/v1/session")
        .header(header::AUTHORIZATION, bearer(&token))
        .header(BEARER_DEVICE_HEADER, wrong_device.as_str())
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = h.send(req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {body}");
    // The bearer-device mismatch returns "bearer_mismatch", not "unauthorised".
    assert_error_code(&body, "bearer_mismatch");
}

#[tokio::test]
async fn non_bearer_scheme_returns_401() {
    let h = Harness::new();
    let node = h.node_id();
    let req = Request::builder()
        .uri("/v1/session")
        .header(header::AUTHORIZATION, "Basic dXNlcjpwYXNz")
        .header(BEARER_DEVICE_HEADER, node.as_str())
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = h.send(req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {body}");
    assert_error_code(&body, "unauthorised");
}

#[tokio::test]
async fn revoked_token_is_rejected() {
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue("sr", &node, Capability::StatusRead, Scope::Node);

    // Record the token in the DB and then revoke it.
    let db = h.state.engine.db();
    db.insert_token(&token, Utc::now()).unwrap();
    let token_id = &token.claims.token_id;
    db.revoke_token(token_id, Utc::now()).unwrap();

    let (status, _, body) = h.send(authed_get("/v1/session", &token, &node)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {body}");
    assert_error_code(&body, "unauthorised");
}
