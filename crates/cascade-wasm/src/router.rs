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

use cascade_engine::types::FileEntry;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::caps;
use crate::state;
use crate::state::EngineState;

/// A request from the worker: an HTTP-shaped method, path, and optional body.
#[derive(Debug, Deserialize)]
pub struct WorkerRequest {
    pub id: String,
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub body: Option<Value>,
    #[serde(default)]
    #[allow(dead_code)]
    pub headers: Option<std::collections::HashMap<String, String>>,
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
    // Split path from query string. The worker sends "/v1/folders/id/children?path=X"
    // as a single path string, so separate the segments from any trailing query.
    let (path_part, query_part) = match request.path.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (request.path.as_str(), None),
    };
    let segments: Vec<&str> = path_part.split('/').filter(|s| !s.is_empty()).collect();

    match (request.method.as_str(), segments.as_slice()) {
        ("GET", ["health"] | ["v1", "health"]) => {
            ok(&request.id, json!({ "status": "ok", "mode": "wasm" }))
        }
        ("GET", ["v1", "capabilities"]) => ok(&request.id, caps::detect()),

        // ── Session ──
        ("GET", ["v1", "session"]) => handle_session(&request.id, engine),

        // ── Backends ──
        ("GET", ["v1", "backends"]) => handle_list_backends(&request.id, engine),
        ("POST", ["v1", "backends"]) => handle_create_backend(request, engine),
        ("DELETE", ["v1", "backends", id]) => handle_delete_backend(&request.id, engine, id),

        // ── Folders / files ──
        ("GET", ["v1", "folders", folder_id, "children"]) => {
            handle_list_children(&request.id, engine, folder_id, query_part.as_deref())
        }
        ("DELETE", ["v1", "files", backend_id, "entries", ..]) => {
            handle_delete_file_entry(&request.id, engine, backend_id, &segments[4..])
        }

        // ── Pin rules ──
        ("GET", ["v1", "pins"]) => handle_list_pins(&request.id, engine),
        ("POST", ["v1", "pins"]) => handle_create_pin(request, engine),
        ("DELETE", ["v1", "pins", id]) => handle_delete_pin(&request.id, engine, id),

        // ── Lifecycle policies ──
        ("GET", ["v1", "policies"]) => handle_list_policies(&request.id, engine),
        ("POST", ["v1", "policies"]) => handle_create_policy(request, engine),
        ("DELETE", ["v1", "policies", id]) => handle_delete_policy(&request.id, engine, id),

        // ── Peers ──
        ("GET", ["v1", "peers"]) => handle_list_peers(&request.id, engine),

        // ── Auth ──
        ("POST", ["v1", "auth", "gdrive"]) => handle_auth_gdrive(&request.id),

        // ── Not yet wired ──
        ("POST", ["v1", "fsaccess", "pick"]) => {
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
fn handle_create_backend(request: &WorkerRequest, _engine: &EngineState) -> WorkerResponse {
    let body = match request.body {
        Some(ref b) => b,
        None => {
            return error_response(
                &request.id,
                400,
                "bad_request",
                "POST /v1/backends requires a JSON body with 'id' and 'type'",
            );
        }
    };

    let id_val = body.get("id").and_then(Value::as_str);
    let type_val = body.get("type").and_then(Value::as_str);

    match (id_val, type_val) {
        (Some(backend_id), Some(backend_type)) => {
            // Persist to engine storage.
            state::with_engine(|engine| {
                engine.storage.register_backend_sync(
                    backend_id,
                    backend_type,
                    backend_id,
                    None,
                    None,
                );
            });
            // Record in session state (no JS handle for HTTP-created backends).
            state::set_backend(backend_id.to_owned(), backend_type.to_owned(), None);
            created(
                &request.id,
                json!({ "id": backend_id, "type": backend_type }),
            )
        }
        _ => error_response(
            &request.id,
            400,
            "bad_request",
            "body must include string 'id' and 'type' fields",
        ),
    }
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
fn handle_list_children(
    id: &str,
    engine: &EngineState,
    folder_id: &str,
    query: Option<&str>,
) -> WorkerResponse {
    let children = engine.storage.list_children_sync(folder_id);

    // Parse optional query parameters: path, limit, cursor.
    let path_filter = query.and_then(|q| {
        q.split('&')
            .find(|p| p.starts_with("path="))
            .map(|p| &p[5..])
    });
    let limit = query.and_then(|q| {
        q.split('&')
            .find(|p| p.starts_with("limit="))
            .and_then(|p| p[7..].parse::<usize>().ok())
    });

    let mut filtered: Vec<&FileEntry> = children
        .iter()
        .filter(|f| path_filter.is_none_or(|pf| f.name.contains(pf)))
        .collect();

    // Cursor is not yet meaningful for in-memory storage (no ordering guarantee),
    // but we accept it silently for API compatibility.
    if let Some(lim) = limit {
        filtered.truncate(lim);
    }

    let children_json: Vec<Value> = filtered
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

/// `DELETE /v1/files/:backend_id/entries/*path` — remove a file entry from
/// engine storage. The trailing path segments are joined to form the native file
/// id, then scoped as `{backend_id}:{native_id}`.
fn handle_delete_file_entry(
    id: &str,
    engine: &EngineState,
    backend_id: &str,
    path_segments: &[&str],
) -> WorkerResponse {
    let native_id = path_segments.join("/");
    let scoped = format!("{backend_id}:{native_id}");
    let removed = engine.storage.remove_file_sync(&scoped);
    if removed {
        ok(id, json!({ "deleted": true }))
    } else {
        error_response(
            id,
            404,
            "not_found",
            &format!("no file entry with id '{scoped}'"),
        )
    }
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
/// Body: `{ "path": "...", "recursive": true, "conditions": "..." }`.
fn handle_create_pin(request: &WorkerRequest, _engine: &EngineState) -> WorkerResponse {
    let body = match request.body {
        Some(ref b) => b,
        None => {
            return error_response(
                &request.id,
                400,
                "bad_request",
                "POST /v1/pins requires a JSON body with 'path'",
            );
        }
    };

    let path = body.get("path").and_then(Value::as_str);
    let Some(path_glob) = path else {
        return error_response(
            &request.id,
            400,
            "bad_request",
            "body must include a string 'path' field",
        );
    };

    let recursive = body
        .get("recursive")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let conditions = body.get("conditions").and_then(Value::as_str);

    state::with_engine(|engine| {
        engine
            .storage
            .add_pin_rule_sync(path_glob, recursive, conditions);
    });
    created(
        &request.id,
        json!({ "path": path_glob, "recursive": recursive, "conditions": conditions }),
    )
}

/// `DELETE /v1/pins/:id` — remove a pin rule by id.
fn handle_delete_pin(id: &str, engine: &EngineState, pin_id: &str) -> WorkerResponse {
    let Ok(parsed_id) = pin_id.parse::<i64>() else {
        return error_response(id, 400, "bad_request", "pin id must be an integer");
    };
    let removed = engine.storage.remove_pin_rule_sync(parsed_id);
    if removed {
        ok(id, json!({ "removed": parsed_id }))
    } else {
        error_response(
            id,
            404,
            "not_found",
            &format!("no pin rule with id {pin_id}"),
        )
    }
}

/// `GET /v1/policies` — list lifecycle policies from engine storage.
fn handle_list_policies(id: &str, engine: &EngineState) -> WorkerResponse {
    let policies = engine.storage.list_lifecycle_policies_sync();
    let policies_json: Vec<Value> = policies
        .iter()
        .map(|p| {
            json!({
                "id": p.id,
                "path": p.path_glob,
                "max_age": p.max_age,
                "max_file_size": p.max_file_size,
                "priority": p.priority,
                "conditions": p.conditions,
            })
        })
        .collect();
    ok(id, json!({ "policies": policies_json }))
}

/// `POST /v1/policies` — add a lifecycle policy through engine storage.
/// Body: `{ "path": "...", "max_age": null, "max_file_size": null, "priority": 0, "conditions": null }`.
fn handle_create_policy(request: &WorkerRequest, _engine: &EngineState) -> WorkerResponse {
    let body = match request.body {
        Some(ref b) => b,
        None => {
            return error_response(
                &request.id,
                400,
                "bad_request",
                "POST /v1/policies requires a JSON body with 'path'",
            );
        }
    };

    let path = body.get("path").and_then(Value::as_str);
    let Some(path_glob) = path else {
        return error_response(
            &request.id,
            400,
            "bad_request",
            "body must include a string 'path' field",
        );
    };

    let max_age = body
        .get("max_age")
        .and_then(|v| if v.is_null() { None } else { v.as_i64() });
    let max_file_size = body
        .get("max_file_size")
        .and_then(|v| if v.is_null() { None } else { v.as_i64() });
    let priority = body.get("priority").and_then(Value::as_i64).unwrap_or(0);
    let conditions = body
        .get("conditions")
        .and_then(|v| if v.is_null() { None } else { v.as_str() });

    state::with_engine(|engine| {
        engine.storage.add_lifecycle_policy_sync(
            path_glob,
            max_age,
            max_file_size,
            i32::try_from(priority).unwrap_or(0),
            conditions,
        );
    });
    created(
        &request.id,
        json!({
            "path": path_glob,
            "max_age": max_age,
            "max_file_size": max_file_size,
            "priority": priority,
            "conditions": conditions,
        }),
    )
}

/// `DELETE /v1/policies/:id` — remove a lifecycle policy by id.
fn handle_delete_policy(id: &str, engine: &EngineState, policy_id: &str) -> WorkerResponse {
    let Ok(parsed_id) = policy_id.parse::<i64>() else {
        return error_response(id, 400, "bad_request", "policy id must be an integer");
    };
    let removed = engine.storage.remove_lifecycle_policy_sync(parsed_id);
    if removed {
        ok(id, json!({ "removed": parsed_id }))
    } else {
        error_response(
            id,
            404,
            "not_found",
            &format!("no policy with id {policy_id}"),
        )
    }
}

/// `GET /v1/peers` — list known peers from engine storage.
fn handle_list_peers(id: &str, engine: &EngineState) -> WorkerResponse {
    let peers = engine.storage.list_peers_sync();
    let peers_json: Vec<Value> = peers
        .iter()
        .map(|p| {
            json!({
                "device_id": p.device_id,
                "name": p.name,
                "addresses": p.addresses,
                "last_seen": p.last_seen,
                "online": p.online,
            })
        })
        .collect();
    ok(id, json!({ "peers": peers_json }))
}

/// `POST /v1/auth/gdrive` — return the auth configuration the main thread
/// needs to initiate the Google Drive OAuth PKCE redirect flow. The engine
/// declares what it needs; the main thread executes the browser-specific bits
/// (PKCE generation, popup/redirect, code exchange) and posts the resulting
/// token metadata back via the `store_auth_token` mutator.
fn handle_auth_gdrive(id: &str) -> WorkerResponse {
    ok(
        id,
        json!({
            "action": "oauth_redirect",
            "provider": "gdrive",
            "auth_endpoint": "https://accounts.google.com/o/oauth2/v2/auth",
            "token_endpoint": "https://oauth2.googleapis.com/token",
            "scopes": [
                "https://www.googleapis.com/auth/drive",
                "https://www.googleapis.com/auth/drive.file",
            ],
            "response_type": "code",
            "pkce_required": true,
            "extra_params": {
                "access_type": "offline",
                "prompt": "consent",
            },
        }),
    )
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

/// A `201 Created` response carrying `body`.
fn created(id: &str, body: Value) -> WorkerResponse {
    WorkerResponse {
        id: id.to_string(),
        status: 201,
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
