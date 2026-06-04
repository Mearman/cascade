#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::string_slice
    )
)]
//! WASM entry point — exposes the Cascade engine's request surface, config
//! parser, and expression evaluator to JavaScript.
//!
//! Compiles to `wasm32-unknown-unknown`. On native targets this crate builds as
//! an empty lib with no exports — all public functions and the inner modules are
//! gated on `#[cfg(target_arch = "wasm32")]`.
//!
//! ## API surface
//!
//! - `handle_request` — the unified entry point the PWA worker drives. It
//!   takes a JSON `WorkerRequest` (method + path), routes it, and returns a JSON
//!   `WorkerResponse`. `GET /v1/health` and `GET /v1/capabilities` are answered
//!   fully; `GET /v1/session` and `GET /v1/backends` project in-memory state;
//!   the rest return `501` until their async interop is wired up.
//! - State mutators (`register_backend`, `deregister_backend`,
//!   `store_auth_token`, `clear_auth_token`, `set_peer_connection`,
//!   `remove_peer_connection`) — the worker calls these after it has driven the
//!   browser-side async work (OAuth, the directory picker, WebRTC signalling) to
//!   record the resulting handles in the engine's session state.
//! - `parse_config` / `eval_expression` — standalone helpers for the config
//!   and expression languages.
//!
//! Verify the WASM build with:
//! ```text
//! cargo check -p cascade-wasm --target wasm32-unknown-unknown
//! ```

#[cfg(target_arch = "wasm32")]
mod caps;
#[cfg(target_arch = "wasm32")]
mod context;
#[cfg(target_arch = "wasm32")]
mod router;
#[cfg(target_arch = "wasm32")]
mod state;

#[cfg(target_arch = "wasm32")]
use serde::Deserialize;
#[cfg(target_arch = "wasm32")]
use serde::Serialize;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

/// A failure crossing the WASM boundary that is not itself an HTTP-shaped
/// response — a malformed payload from a mutator call, or a response that could
/// not be serialised back to the worker.
#[cfg(target_arch = "wasm32")]
#[derive(Debug, thiserror::Error)]
enum WasmError {
    /// A token payload handed to [`store_auth_token`] was not valid JSON or did
    /// not match the expected shape.
    #[error("invalid token JSON: {0}")]
    InvalidToken(String),
    /// A built response could not be serialised back to the worker.
    #[error("could not serialise response: {0}")]
    Serialise(String),
}

#[cfg(target_arch = "wasm32")]
impl From<WasmError> for JsValue {
    fn from(error: WasmError) -> Self {
        Self::from_str(&error.to_string())
    }
}

/// The session-relevant fields of an OAuth token payload, as produced by
/// `oauth.ts`. Other fields (`access_token`, `refresh_token`) live only in the
/// JS-side `IndexedDB` store and are not mirrored here.
#[cfg(target_arch = "wasm32")]
#[derive(Deserialize)]
struct TokenInput {
    scope: String,
    /// Expiry as a unix timestamp in milliseconds.
    expiry: i64,
}

/// Handle a worker request and return the response as a JavaScript object.
///
/// `request_json` is a JSON-encoded `WorkerRequest` (`id`, `method`, `path`).
/// The returned value is a JSON `WorkerResponse` (`id`, `status`, `body`, and an
/// optional `error`). A malformed request yields a `400` response rather than an
/// error — every routing outcome is reported through the response envelope.
///
/// # Errors
///
/// Returns a JS error only if the response could not be serialised, which should
/// not occur for the values this router builds.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn handle_request(request_json: &str) -> Result<JsValue, JsValue> {
    let response = match serde_json::from_str::<router::WorkerRequest>(request_json) {
        Ok(request) => router::route(&request),
        Err(error) => router::bad_request(&format!("malformed request JSON: {error}")),
    };
    to_js(&response)
}

/// Register (or replace) a backend in the engine's session state.
///
/// `handle` is the opaque JS object backing the backend — a granted
/// `FileSystemDirectoryHandle` for the `fsaccess` type — or `undefined`/`null`
/// for cloud backends reached over HTTP.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn register_backend(id: &str, backend_type: &str, handle: JsValue) {
    let handle = if handle.is_undefined() || handle.is_null() {
        None
    } else {
        Some(handle)
    };
    state::set_backend(id.to_string(), backend_type.to_string(), handle);
}

/// Remove a backend by id, returning whether one was registered.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
#[must_use]
pub fn deregister_backend(id: &str) -> bool {
    state::remove_backend(id)
}

/// Cache OAuth token metadata for a provider (e.g. `"gdrive"`). The durable copy
/// is held in `IndexedDB` by `oauth.ts`; this records the in-memory view the
/// session handler reports.
///
/// # Errors
///
/// Returns a JS error if `token_json` is not a valid token payload.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn store_auth_token(provider: &str, token_json: &str) -> Result<(), JsValue> {
    let token: TokenInput = serde_json::from_str(token_json)
        .map_err(|error| WasmError::InvalidToken(error.to_string()))?;
    state::set_token(provider.to_string(), token.scope, token.expiry);
    Ok(())
}

/// Drop a provider's cached token, returning whether one was present.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
#[must_use]
pub fn clear_auth_token(provider: &str) -> bool {
    state::remove_token(provider)
}

/// Record (or replace) a peer connection, keyed by the relay session id that
/// established it. `connection` is the opaque transport object owned by
/// `webrtc.ts`.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn set_peer_connection(session_id: &str, connection: JsValue) {
    state::set_peer(session_id.to_string(), connection);
}

/// Drop a peer connection by relay session id, returning whether one was present.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
#[must_use]
pub fn remove_peer_connection(session_id: &str) -> bool {
    state::remove_peer(session_id)
}

/// Serialise a value to a JavaScript object via its JSON form.
#[cfg(target_arch = "wasm32")]
fn to_js<T: Serialize>(value: &T) -> Result<JsValue, JsValue> {
    let json =
        serde_json::to_string(value).map_err(|error| WasmError::Serialise(error.to_string()))?;
    js_sys::JSON::parse(&json)
        .map_err(|_| JsValue::from_str("serialised response was not valid JSON"))
}

/// Parse a `.cascade` TOML string and return the config as a JavaScript object.
///
/// The returned value matches the `CascadeConfig` shape: `ignore`, `pin`,
/// `unpin`, `lifecycle`, `cache`, `p2p`, and `device` arrays / objects.
///
/// # Errors
///
/// Returns a JS error string if the TOML is malformed or cannot be converted to
/// a JS value.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn parse_config(toml_str: &str) -> Result<JsValue, JsValue> {
    let config = cascade_config::parse::toml::parse(toml_str)
        .map_err(|e| JsValue::from_str(&e.to_string()))?;
    let json = serde_json::to_string(&config).map_err(|e| JsValue::from_str(&e.to_string()))?;
    js_sys::JSON::parse(&json)
        .map_err(|_| JsValue::from_str("serialised config was not valid JSON"))
}

/// Evaluate a cascade expression string against a JSON context object.
///
/// `context_json` is a JSON object with optional fields that map to the
/// expression evaluation context. Absent fields default to zero / unknown.
/// Expected shape (all fields optional):
///
/// ```json
/// {
///   "file":    { "size": 4096, "mime": "text/plain", "ext": "txt",
///                "name": "notes.txt", "cached": false, "pinned": false,
///                "shared": false, "starred": false, "dirty": false },
///   "device":  { "id": "abc123", "name": "laptop",
///                "arch": "aarch64", "os": "macos" },
///   "disk":    { "total_bytes": 1000000000, "free_bytes": 500000000 },
///   "network": { "type": "wifi", "metered": false },
///   "power":   { "source": "ac", "battery_pct": 95 },
///   "peer":    { "online_count": 3, "peers_with_file": 1 }
/// }
/// ```
///
/// # Errors
///
/// Returns a JS error string if the expression is syntactically invalid or
/// `context_json` cannot be deserialised.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn eval_expression(expr: &str, context_json: &str) -> Result<bool, JsValue> {
    let ast =
        cascade_expr::eval::parse_expr(expr).map_err(|e| JsValue::from_str(&e.to_string()))?;
    let ctx = context::from_json(context_json).map_err(|e| JsValue::from_str(&e.to_string()))?;
    Ok(cascade_expr::eval::evaluate(&ast, &ctx))
}
