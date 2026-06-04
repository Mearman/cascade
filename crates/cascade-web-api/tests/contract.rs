//! Contract tests — the single source of truth for the v1 wire shape.
//!
//! Each test spins up a real [`Engine`] over a tempdir (with P2P enabled, so the
//! node has the device identity a capability token's chain roots in), stands up
//! the `axum` router against it, and walks the route table asserting the
//! documented status codes, response shapes, the error envelope, and the
//! `X-Cascade-Request-Id` response header.

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
use cascade_web_api::request_id::{REQUEST_ID_HEADER, is_valid_request_id};
use cascade_web_api::state::{AppState, BindConfig, NodeIdentity, Readiness};
use chrono::{Duration, Utc};
use data_encoding::BASE64;
use serde_json::Value;
use tower::ServiceExt as _;

/// A test harness bundling the router, the node identity, and the engine.
struct Harness {
    router: Router,
    identity: DeviceIdentity,
    state: AppState,
}

impl Harness {
    /// Build a harness over a fresh engine. The data plane starts not-ready.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::new(EngineConfig {
            db_path: dir.path().join("state.db"),
            mount_point: dir.path().join("mount"),
            backends: vec![Arc::new(cascade_engine::backend::NullBackend::new("root"))],
            cache_dir: None,
            enable_p2p: true,
            p2p_data_dir: Some(dir.path().join("p2p")),
            p2p_posture: None,
            p2p_relay_endpoints: Vec::new(),
            p2p_relay_shared_secret: None,
            backend_factory: None,
        })
        .unwrap();
        // Keep the tempdir alive for the engine's lifetime by leaking it; the
        // test process is short-lived and the OS reclaims it.
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
            Some("deadbeef".to_owned()),
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

    /// This node's device id.
    fn node_id(&self) -> DeviceId {
        DeviceId::new(self.identity.device_id.clone())
    }

    /// Flip the F3 data-plane readiness bit.
    fn set_ready(&self) {
        self.state.readiness.set_data_plane_ready(true);
    }

    /// Issue a node-signed token for `bearer`.
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

    /// Send a request and return the status, the request-id header, and the
    /// parsed JSON body (or `Value::Null` for an empty body).
    async fn send(&self, req: Request<Body>) -> (StatusCode, Option<String>, Value) {
        let response = self.router.clone().oneshot(req).await.unwrap();
        let status = response.status();
        let request_id = response
            .headers()
            .get(REQUEST_ID_HEADER)
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

/// Build a `Bearer` Authorization header value from a token (base64 of its JSON).
fn bearer(token: &CapabilityToken) -> String {
    let json = serde_json::to_vec(token).unwrap();
    format!("Bearer {}", BASE64.encode(&json))
}

/// Build an authenticated GET request.
fn authed_get(uri: &str, token: &CapabilityToken, bearer_device: &DeviceId) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header(header::AUTHORIZATION, bearer(token))
        .header(BEARER_DEVICE_HEADER, bearer_device.as_str())
        .body(Body::empty())
        .unwrap()
}

/// Build an authenticated POST request with a JSON body.
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

/// Assert an error body matches the envelope shape and carries `code`.
fn assert_error(body: &Value, request_id: Option<&str>, code: &str) {
    let error = body.get("error").expect("error envelope");
    assert_eq!(
        error.get("code").and_then(Value::as_str),
        Some(code),
        "code mismatch in {body}"
    );
    assert!(
        error.get("message").and_then(Value::as_str).is_some(),
        "message missing"
    );
    let envelope_id = error
        .get("request_id")
        .and_then(Value::as_str)
        .expect("request_id in envelope");
    assert!(
        is_valid_request_id(envelope_id),
        "envelope request_id not valid: {envelope_id}"
    );
    assert_eq!(
        Some(envelope_id),
        request_id,
        "envelope request_id must match the header",
    );
}

/// Every response carries a valid request-id header.
fn assert_request_id(request_id: Option<&str>) {
    let id = request_id.expect("X-Cascade-Request-Id header present");
    assert!(
        is_valid_request_id(id),
        "request id not 26-char base32: {id}"
    );
}

#[tokio::test]
async fn health_needs_no_auth() {
    let h = Harness::new();
    let req = Request::builder()
        .uri("/v1/health")
        .body(Body::empty())
        .unwrap();
    let (status, request_id, body) = h.send(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_request_id(request_id.as_deref());
    assert_eq!(body.get("status").and_then(Value::as_str), Some("ok"));
    assert_eq!(
        body.get("version").and_then(Value::as_str),
        Some("test-version")
    );
    assert_eq!(
        body.get("node_device_id").and_then(Value::as_str),
        Some(h.node_id().as_str()),
    );
}

#[tokio::test]
async fn bundle_needs_no_auth() {
    let h = Harness::new();
    let req = Request::builder()
        .uri("/v1/bundle")
        .body(Body::empty())
        .unwrap();
    let (status, request_id, body) = h.send(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_request_id(request_id.as_deref());
    assert_eq!(
        body.get("bundle_url").and_then(Value::as_str),
        Some("https://pwa.example.com"),
    );
    assert_eq!(
        body.get("build_sha").and_then(Value::as_str),
        Some("deadbeef")
    );
}

#[tokio::test]
async fn missing_token_is_unauthorised() {
    let h = Harness::new();
    let req = Request::builder()
        .uri("/v1/session")
        .body(Body::empty())
        .unwrap();
    let (status, request_id, body) = h.send(req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_request_id(request_id.as_deref());
    assert_error(&body, request_id.as_deref(), "unauthorised");
}

#[tokio::test]
async fn missing_bearer_device_header_is_unauthorised() {
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue("sess", &node, Capability::StatusRead, Scope::Node);
    let req = Request::builder()
        .uri("/v1/session")
        .header(header::AUTHORIZATION, bearer(&token))
        .body(Body::empty())
        .unwrap();
    let (status, _request_id, _body) = h.send(req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn session_returns_owner_view() {
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue("sess", &node, Capability::StatusRead, Scope::Node);
    let (status, request_id, body) = h.send(authed_get("/v1/session", &token, &node)).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_request_id(request_id.as_deref());
    // The token was issued directly by this node, so the class is owner.
    assert_eq!(
        body.pointer("/session/class").and_then(Value::as_str),
        Some("owner"),
    );
    assert_eq!(
        body.pointer("/session/verified_bearer")
            .and_then(Value::as_str),
        Some(node.as_str()),
    );
    assert_eq!(
        body.pointer("/abilities/status_read")
            .and_then(Value::as_bool),
        Some(true),
    );
    // The abilities view parses as the typed schema.
    let parsed: cascade_web_api::schemas::session::SessionResponse =
        serde_json::from_value(body).unwrap();
    assert_eq!(parsed.token.capability, Capability::StatusRead);
}

#[tokio::test]
async fn ready_is_unavailable_until_data_plane_up() {
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue("sess", &node, Capability::StatusRead, Scope::Node);

    let (status, _id, body) = h.send(authed_get("/v1/ready", &token, &node)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body.pointer("/error/code").and_then(Value::as_str),
        Some("unavailable")
    );

    h.set_ready();
    let (status, _id, body) = h.send(authed_get("/v1/ready", &token, &node)).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        body.get("data_plane_ready").and_then(Value::as_bool),
        Some(true)
    );
}

#[tokio::test]
async fn data_route_is_blocked_until_ready() {
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue(
        "dr",
        &node,
        Capability::DataRead,
        Scope::folder("p2p-shared"),
    );
    let (status, request_id, body) = h
        .send(authed_get(
            "/v1/files/p2p-shared/entries/file.txt",
            &token,
            &node,
        ))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_error(&body, request_id.as_deref(), "data_plane_not_ready");
}

#[tokio::test]
async fn unknown_folder_is_not_found() {
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue("sr", &node, Capability::StatusRead, Scope::Node);
    let (status, request_id, body) = h
        .send(authed_get("/v1/folders/p2p-nope/children", &token, &node))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
    assert_error(&body, request_id.as_deref(), "not_found");
}

#[tokio::test]
async fn capability_held_over_wrong_scope_is_forbidden() {
    // A status:read token scoped to a folder does not cover the node-wide audit
    // route, but the caller does hold status:read — so it is 403, not 401.
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue(
        "sr-folder",
        &node,
        Capability::StatusRead,
        Scope::folder("/x"),
    );
    let (status, request_id, body) = h.send(authed_get("/v1/audit", &token, &node)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");
    assert_error(&body, request_id.as_deref(), "forbidden");
}

#[tokio::test]
async fn audit_with_status_read_lists_entries() {
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue("sr", &node, Capability::StatusRead, Scope::Node);
    let (status, request_id, body) = h.send(authed_get("/v1/audit", &token, &node)).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_request_id(request_id.as_deref());
    let parsed: cascade_web_api::schemas::audit::AuditResponse =
        serde_json::from_value(body).unwrap();
    assert!(parsed.entries.is_empty());
}

#[tokio::test]
async fn token_data_verb_node_wide_is_forbidden() {
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue("admin", &node, Capability::GrantAdmin, Scope::folder("/x"));
    let body = serde_json::json!({
        "bearer": "PEER-DEVICE",
        "capability": "data:read",
        "scope": { "kind": "node" },
        "expires": (Utc::now() + Duration::days(1)).to_rfc3339(),
    });
    let (status, request_id, resp) = h
        .send(authed_post("/v1/tokens", &token, &node, &body))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body: {resp}");
    assert_error(
        &resp,
        request_id.as_deref(),
        "data_verb_node_wide_forbidden",
    );
}

#[tokio::test]
async fn grant_data_verb_node_wide_is_forbidden() {
    let h = Harness::new();
    let node = h.node_id();
    let token = h.issue(
        "admin",
        &node,
        Capability::GrantAdmin,
        Scope::folder("p2p-shared"),
    );
    let body = serde_json::json!({
        "grantee": "PEER-DEVICE",
        "capability": "data:write",
        "scope": { "kind": "node" },
    });
    let (status, request_id, resp) = h
        .send(authed_post("/v1/grants", &token, &node, &body))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body: {resp}");
    assert_error(
        &resp,
        request_id.as_deref(),
        "data_verb_node_wide_forbidden",
    );
}

#[tokio::test]
async fn non_owner_cannot_exceed_its_own_authority_issuing_a_token() {
    // A delegated leaf token (issuer is the delegating device, not the node) is
    // a non-owner session. Issuing a token beyond its claims is rejected.
    let h = Harness::new();
    let delegator = DeviceIdentity::generate().unwrap();
    let delegator_id = DeviceId::new(delegator.device_id.clone());
    let delegate = DeviceIdentity::generate().unwrap();
    let delegate_id = DeviceId::new(delegate.device_id.clone());

    // node -> delegator: data:read over p2p-shared.
    let parent = h.issue(
        "parent",
        &delegator_id,
        Capability::DataRead,
        Scope::folder("p2p-shared"),
    );
    // delegator -> delegate: a subset delegation (same verb, same scope).
    let leaf = parent
        .delegate(
            "leaf",
            &delegator,
            &delegate_id,
            Capability::DataRead,
            Scope::folder("p2p-shared"),
            Utc::now() + Duration::days(30),
        )
        .unwrap();

    // The delegate tries to issue data:read over a folder it does not hold.
    let body = serde_json::json!({
        "bearer": "SOMEONE",
        "capability": "data:read",
        "scope": { "kind": "folder", "path": "p2p-other" },
        "expires": (Utc::now() + Duration::days(1)).to_rfc3339(),
    });
    let (status, request_id, resp) = h
        .send(authed_post("/v1/tokens", &leaf, &delegate_id, &body))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body: {resp}");
    assert_error(&resp, request_id.as_deref(), "delegation_exceeds_parent");
}

#[tokio::test]
async fn request_id_is_echoed_when_valid() {
    let h = Harness::new();
    // A valid 26-char Crockford id minted by the server's own minter.
    let supplied = cascade_web_api::request_id::mint_request_id();
    let req = Request::builder()
        .uri("/v1/health")
        .header(REQUEST_ID_HEADER, &supplied)
        .body(Body::empty())
        .unwrap();
    let (status, request_id, _body) = h.send(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(request_id.as_deref(), Some(supplied.as_str()));
}
