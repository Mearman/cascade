//! Request routing for the WASM engine.
//!
//! Mirrors the daemon's HTTP surface over the worker's postMessage protocol: a
//! [`WorkerRequest`] (method + path) routes to a handler that returns a
//! [`WorkerResponse`] (status + body). The shapes match
//! `apps/web/src/wasm/messages.ts` so the worker forwards them to the main
//! thread unchanged.
//!
//! Routes that read engine data (backends, files, pins) go through the
//! [`crate::state::EngineState`] storage adapter. Session data (auth tokens,
//! peer connections, opaque JS handles) is read from [`crate::state`]
//! accessors. The remaining unrecognised routes return `404`; routes that
//! depend on async browser interop return `501`.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::caps;
use crate::state;
use crate::state::EngineState;

/// A request from the worker: an HTTP-shaped method and path. The `body` and
/// `headers` of the wire message are ignored by the routes implemented so far
/// and so are not deserialised.
#[derive(Debug, Deserialize)]
pub struct WorkerRequest {
    pub id: String,
    pub method: String,
    pub path: String,
}

/// A response to the worker. `error` is omitted from the wire form when absent.
#[derive(Debug, Serialize)]
pub struct WorkerResponse {
    pub id: String,
    pub status: u16,
    pub body: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Route a parsed request to its handler.
pub fn route(request: &WorkerRequest, engine: &EngineState) -> WorkerResponse {
    let segments: Vec<&str> = request.path.split('/').filter(|s| !s.is_empty()).collect();

    match (request.method.as_str(), segments.as_slice()) {
        ("GET", ["health"] | ["v1", "health"]) => {
            ok(&request.id, json!({ "status": "ok", "mode": "wasm" }))
        }
        ("GET", ["v1", "capabilities"]) => ok(&request.id, caps::detect()),

        // ── Session ──
        ("GET", ["v1", "session"]) => handle_session(&request.id, engine),

        // ── Backends ──
        ("GET", ["v1", "backends"]) => handle_list_backends(&request.id, engine),
        ("POST", ["v1", "backends"]) => handle_create_backend(&request.id, engine),
        ("DELETE", ["v1", "backends", id]) => handle_delete_backend(&request.id, engine, id),

        // ── Folders / files ──
        ("GET", ["v1", "folders", folder_id, "children"]) => {
            handle_list_children(&request.id, engine, folder_id)
        }

        // ── Pin rules ──
        ("GET", ["v1", "pins"]) => handle_list_pins(&request.id, engine),
        ("POST", ["v1", "pins"]) => handle_create_pin(&request.id, engine),

        // ── Not yet wired ──
        ("POST", ["v1", "auth", "gdrive"] | ["v1", "fsaccess", "pick"]) => {
            not_implemented(&request.id, &request.method, &request.path)
        }
        _ => not_found(&request.id, &request.method, &request.path),
    }
}

// ─────────────────────────── Handlers ───────────────────────────

/// `GET /v1/session` — merge engine backends with auth-token session state.
fn handle_session(id: &str, engine: &EngineState) -> WorkerResponse {
    let backends = engine.storage.list_backends_sync();
    let files = engine.storage.list_all_files_sync();
    let (established, session_body) = state::session();

    // Enrich session body with engine data.
    let online = files.iter().filter(|f| !f.is_dir).count();
    let cache_body = json!({
        "online": online,
        "cached": 0,
        "pinned": 0,
        "totalBytes": 0,
    });

    let mut body = session_body;
    body.as_object_mut()
        .expect("session body is an object")
        .insert("cache".to_owned(), cache_body);
    body.as_object_mut()
        .expect("session body is an object")
        .insert("backendCount".to_owned(), json!(backends.len()));

    if established || !backends.is_empty() {
        ok(id, body)
    } else {
        error_response(id, 401, "no_session", "no active session")
    }
}

/// `GET /v1/backends` — list backends from engine storage merged with session
/// state's JS handle info.
fn handle_list_backends(id: &str, engine: &EngineState) -> WorkerResponse {
    let backends = engine.storage.list_backends_sync();
    let session_backends = state::list_backends();

    // Build a lookup of session-state handles by backend id.
    let session_map: std::collections::HashMap<String, bool> = session_backends
        .get("backends")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|b| {
                    let bid = b.get("id")?.as_str()?;
                    let has_handle = b.get("hasHandle")?.as_bool()?;
                    Some((bid.to_owned(), has_handle))
                })
                .collect()
        })
        .unwrap_or_default();

    let backends_json: Vec<Value> = backends
        .iter()
        .map(|b| {
            json!({
                "id": b.id,
                "type": b.backend_type,
                "display_name": b.display_name,
                "hasHandle": session_map.get(&b.id).copied().unwrap_or(false),
            })
        })
        .collect();

    ok(id, json!({ "backends": backends_json }))
}

/// `POST /v1/backends` — register a backend through engine storage and session
/// state. Body: `{ "id": "...", "type": "..." }`.
fn handle_create_backend(id: &str, _engine: &EngineState) -> WorkerResponse {
    // The body comes through the request but isn't deserialised into
    // WorkerRequest yet. For now the mutator path (register_backend export)
    // is the primary registration route; the HTTP POST path returns 501
    // until request body parsing is added.
    not_implemented(id, "POST", "/v1/backends")
}

/// `DELETE /v1/backends/:id` — remove from engine storage and session state.
fn handle_delete_backend(id: &str, engine: &EngineState, backend_id: &str) -> WorkerResponse {
    let storage_removed = engine.storage.remove_backend_sync(backend_id);
    let session_removed = state::remove_backend(backend_id);

    if storage_removed || session_removed {
        ok(id, json!({ "removed": backend_id }))
    } else {
        error_response(
            id,
            404,
            "not_found",
            &format!("no backend with id '{backend_id}'"),
        )
    }
}

/// `GET /v1/folders/:id/children` — list children from engine storage.
fn handle_list_children(id: &str, engine: &EngineState, folder_id: &str) -> WorkerResponse {
    let children = engine.storage.list_children_sync(folder_id);
    let children_json: Vec<Value> = children
        .iter()
        .map(|f| {
            json!({
                "id": f.id.0,
                "name": f.name,
                "is_dir": f.is_dir,
                "size": f.size.unwrap_or(0),
                "mime_type": f.mime_type,
            })
        })
        .collect();
    ok(id, json!({ "children": children_json }))
}

/// `GET /v1/pins` — list pin rules from engine storage.
fn handle_list_pins(id: &str, engine: &EngineState) -> WorkerResponse {
    let rules = engine.storage.list_pin_rules_sync();
    let rules_json: Vec<Value> = rules
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "path": r.path_glob,
                "recursive": r.recursive,
                "conditions": r.conditions,
            })
        })
        .collect();
    ok(id, json!({ "pins": rules_json }))
}

/// `POST /v1/pins` — add a pin rule through engine storage.
fn handle_create_pin(id: &str, _engine: &EngineState) -> WorkerResponse {
    // Body parsing not yet wired — the mutator path is the primary route.
    not_implemented(id, "POST", "/v1/pins")
}

// ─────────────────────────── Response helpers ───────────────────────────

/// Build a `400 Bad Request` response for a message that could not be parsed.
/// The id is unknown because parsing is what failed.
pub fn bad_request(message: &str) -> WorkerResponse {
    error_response("unknown", 400, "bad_request", message)
}

/// A `200 OK` response carrying `body`.
fn ok(id: &str, body: Value) -> WorkerResponse {
    WorkerResponse {
        id: id.to_string(),
        status: 200,
        body,
        error: None,
    }
}

/// An error response whose body matches the daemon's `{ error: { code,
/// message, request_id } }` envelope.
fn error_response(id: &str, status: u16, code: &str, message: &str) -> WorkerResponse {
    WorkerResponse {
        id: id.to_string(),
        status,
        body: json!({
            "error": {
                "code": code,
                "message": message,
                "request_id": id,
            }
        }),
        error: None,
    }
}

fn not_found(id: &str, method: &str, path: &str) -> WorkerResponse {
    error_response(
        id,
        404,
        "not_found",
        &format!("no WASM handler for {method} {path}"),
    )
}

fn not_implemented(id: &str, method: &str, path: &str) -> WorkerResponse {
    error_response(
        id,
        501,
        "not_implemented",
        &format!("{method} {path} is not yet implemented in the WASM engine"),
    )
}
