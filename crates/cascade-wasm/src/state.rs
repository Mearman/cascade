//! In-memory state for the WASM engine.
//!
//! [`handle_request`](crate::handle_request) is synchronous and pure, but the
//! browser-side interop it serves (the OAuth redirect dance, the directory
//! picker, WebRTC signalling) is asynchronous and lives in JavaScript. The
//! worker drives that async work and then calls the crate's mutator exports
//! (`register_backend`, `store_auth_token`, `set_peer_connection`, and friends)
//! to record the resulting handles here. The read side — the `GET` request
//! handlers — projects this state back out as JSON.
//!
//! The browser runs WASM single-threaded, so the state lives behind a
//! `thread_local!` [`RefCell`] rather than a thread-safe lock: the engine's
//! `Send + Sync` requirements do not apply on this target, and the opaque JS
//! handles (`FileSystemDirectoryHandle`, `RTCPeerConnection`) are `!Send`
//! anyway.

use std::cell::RefCell;
use std::collections::HashMap;

use serde_json::{Value, json};
use wasm_bindgen::JsValue;

thread_local! {
    static STATE: RefCell<WasmState> = RefCell::new(WasmState::new());
}

/// The engine's live browser-session state.
#[derive(Debug)]
struct WasmState {
    /// Configured backends, in registration order.
    backends: Vec<BackendEntry>,
    /// Cached OAuth token metadata, keyed by provider (e.g. `"gdrive"`). The
    /// durable copy lives in `IndexedDB` on the JS side; this is the in-memory
    /// view the session handler reports.
    auth_tokens: HashMap<String, StoredToken>,
    /// Open peer connections, keyed by the relay session id that established
    /// them. Values are opaque `RTCPeerConnection`-backed transports.
    peers: HashMap<String, JsValue>,
}

impl WasmState {
    fn new() -> Self {
        Self {
            backends: Vec::new(),
            auth_tokens: HashMap::new(),
            peers: HashMap::new(),
        }
    }
}

/// A configured backend: its id, its type, and the opaque JS handle that backs
/// it (the granted directory handle for `fsaccess`; absent for cloud backends
/// whose authority is reached over HTTP).
#[derive(Debug)]
struct BackendEntry {
    id: String,
    backend_type: String,
    handle: Option<JsValue>,
}

/// The session-relevant fields of a stored OAuth token.
#[derive(Debug)]
struct StoredToken {
    scope: String,
    /// Expiry as a unix timestamp in milliseconds, matching `oauth.ts`.
    expiry: i64,
}

// ─────────────────────────── Mutators ───────────────────────────

/// Register a backend, replacing any existing entry with the same id.
pub fn set_backend(id: String, backend_type: String, handle: Option<JsValue>) {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if let Some(existing) = state.backends.iter_mut().find(|b| b.id == id) {
            existing.backend_type = backend_type;
            existing.handle = handle;
        } else {
            state.backends.push(BackendEntry {
                id,
                backend_type,
                handle,
            });
        }
    });
}

/// Remove a backend by id, returning whether one was present.
pub fn remove_backend(id: &str) -> bool {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        let before = state.backends.len();
        state.backends.retain(|b| b.id != id);
        state.backends.len() != before
    })
}

/// Cache (or replace) the token metadata for a provider.
pub fn set_token(provider: String, scope: String, expiry: i64) {
    STATE.with(|state| {
        state
            .borrow_mut()
            .auth_tokens
            .insert(provider, StoredToken { scope, expiry });
    });
}

/// Drop a provider's cached token, returning whether one was present.
pub fn remove_token(provider: &str) -> bool {
    STATE.with(|state| state.borrow_mut().auth_tokens.remove(provider).is_some())
}

/// Record (or replace) the peer connection for a relay session id.
pub fn set_peer(session_id: String, connection: JsValue) {
    STATE.with(|state| {
        state.borrow_mut().peers.insert(session_id, connection);
    });
}

/// Drop a peer connection by relay session id, returning whether one was present.
pub fn remove_peer(session_id: &str) -> bool {
    STATE.with(|state| state.borrow_mut().peers.remove(session_id).is_some())
}

// ─────────────────────────── Read projections ───────────────────────────

/// The configured-backend listing for `GET /v1/backends`.
pub fn list_backends() -> Value {
    STATE.with(|state| {
        let backends: Vec<Value> = state
            .borrow()
            .backends
            .iter()
            .map(|backend| {
                json!({
                    "id": backend.id,
                    "type": backend.backend_type,
                    "hasHandle": backend.handle.is_some(),
                })
            })
            .collect();
        json!({ "backends": backends })
    })
}

/// The session view for `GET /v1/session`, paired with whether any session has
/// been established at all (an authenticated provider, a granted local
/// directory, or any configured backend). The router returns an error when no
/// session exists.
pub fn session() -> (bool, Value) {
    STATE.with(|state| {
        let state = state.borrow();
        let providers: Vec<Value> = state
            .auth_tokens
            .iter()
            .map(|(provider, token)| {
                json!({
                    "provider": provider,
                    "scope": token.scope,
                    "expiry": token.expiry,
                })
            })
            .collect();
        let fs_access_granted = state
            .backends
            .iter()
            .any(|backend| backend.backend_type == "fsaccess" && backend.handle.is_some());
        let peer_count = state.peers.len();
        let authenticated = !providers.is_empty();
        let established = authenticated || fs_access_granted || !state.backends.is_empty();

        let body = json!({
            "authenticated": authenticated,
            "providers": providers,
            "fsAccessGranted": fs_access_granted,
            "peerCount": peer_count,
        });
        (established, body)
    })
}
