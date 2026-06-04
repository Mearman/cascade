//! Request routing for the WASM engine.
//!
//! Mirrors the daemon's HTTP surface over the worker's postMessage protocol: a
//! [`WorkerRequest`] (method + path) routes to a handler that returns a
//! [`WorkerResponse`] (status + body). The shapes match
//! `apps/web/src/wasm/messages.ts` so the worker forwards them to the main
//! thread unchanged.
//!
//! `GET /v1/health` and `GET /v1/capabilities` are answered fully; `GET
//! /v1/session` and `GET /v1/backends` project the in-memory [`crate::state`].
//! The remaining routes are recognised but return `501 Not Implemented` until
//! the backing async interop is wired up — that work is incremental and arrives
//! through the crate's mutator exports.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{caps, state};

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
pub fn route(request: &WorkerRequest) -> WorkerResponse {
    let segments: Vec<&str> = request.path.split('/').filter(|s| !s.is_empty()).collect();

    match (request.method.as_str(), segments.as_slice()) {
        ("GET", ["health"] | ["v1", "health"]) => {
            ok(&request.id, json!({ "status": "ok", "mode": "wasm" }))
        }
        ("GET", ["v1", "capabilities"]) => ok(&request.id, caps::detect()),
        ("GET", ["v1", "session"]) => {
            let (established, body) = state::session();
            if established {
                ok(&request.id, body)
            } else {
                error_response(&request.id, 401, "no_session", "no active session")
            }
        }
        ("GET", ["v1", "backends"]) => ok(&request.id, state::list_backends()),
        // Recognised but not yet wired to the engine — these depend on async
        // backend and browser interop that is added incrementally.
        ("GET", ["v1", "folders", _, "children"])
        | ("POST", ["v1", "backends"] | ["v1", "auth", "gdrive"] | ["v1", "fsaccess", "pick"])
        | ("DELETE", ["v1", "backends", _]) => {
            not_implemented(&request.id, &request.method, &request.path)
        }
        _ => not_found(&request.id, &request.method, &request.path),
    }
}

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
