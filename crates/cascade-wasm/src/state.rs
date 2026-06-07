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
//!
//! # Engine state
//!
//! [`EngineState`] is the single value the router reads from: it owns the
//! engine [`storage`](EngineState::storage) (the source of truth for backends,
//! files, pin rules, and lifecycle policies) and the [`SessionStore`] holding
//! session data that has no engine representation (auth tokens, peer
//! connections, opaque JS handles). The router receives an `&EngineState`
//! rather than reaching into a global, which is what lets the request contract
//! be exercised against a freshly-constructed state in unit tests.

// `peers` maps relay session ids to `OpaqueHandle`, which is a non-ZST
// `wasm_bindgen::JsValue` on the wasm32 target. Only the host placeholder is
// zero-sized, so clippy's "use a HashSet" suggestion does not hold across
// targets — the value carries the live `RTCPeerConnection` transport on wasm.
#![allow(clippy::zero_sized_map_values)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use cascade_engine::portable::wasm::WasmStateStorage;
use serde_json::{Value, json};

/// An opaque handle minted on the JavaScript side — a granted
/// `FileSystemDirectoryHandle` for a local backend, or an `RTCPeerConnection`
/// for a peer. On the browser target it is a [`wasm_bindgen::JsValue`]; off-wasm
/// (host unit tests) there is no JS runtime, so it is a zero-sized placeholder
/// ([`NativeOpaqueHandle`]). The session state never inspects the handle — it
/// only records its presence — so the placeholder is sufficient for exercising
/// every read projection.
#[cfg(target_arch = "wasm32")]
pub type OpaqueHandle = wasm_bindgen::JsValue;
#[cfg(not(target_arch = "wasm32"))]
pub type OpaqueHandle = NativeOpaqueHandle;

/// Host-side placeholder for an opaque JS handle. See [`OpaqueHandle`].
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone)]
pub struct NativeOpaqueHandle;

// The worker reuses one engine state for its lifetime, reached through
// [`with_engine`]. Host unit tests construct their own [`EngineState`] instead,
// so the singleton is wasm-only.
#[cfg(target_arch = "wasm32")]
thread_local! {
    static ENGINE_STATE: RefCell<Option<EngineState>> = const { RefCell::new(None) };
}

/// The engine state the router reads: portable storage plus browser-session
/// state. Initialised once per worker and reused for the session lifetime;
/// unit tests construct their own instance via [`EngineState::new`].
pub struct EngineState {
    /// Source of truth for engine data (backends, files, pin rules, policies).
    pub storage: Arc<WasmStateStorage>,
    /// Session data with no engine representation: auth tokens, peer
    /// connections, and opaque JS handles.
    pub session: SessionStore,
}

impl EngineState {
    /// Construct a fresh engine state with empty storage and session.
    pub fn new() -> Self {
        Self {
            storage: Arc::new(WasmStateStorage::new()),
            session: SessionStore::new(),
        }
    }
}

/// Run a closure with shared access to the worker's engine state, initialising
/// it on first call. Idempotent for subsequent calls.
///
/// Both the storage and the session use interior mutability, so handlers and
/// mutators take an `&EngineState` and still record changes.
#[cfg(target_arch = "wasm32")]
pub fn with_engine<F, R>(f: F) -> R
where
    F: FnOnce(&EngineState) -> R,
{
    ENGINE_STATE.with(|state| {
        if state.borrow().is_none() {
            *state.borrow_mut() = Some(EngineState::new());
        }
        f(state
            .borrow()
            .as_ref()
            .expect("engine state just initialised"))
    })
}

/// The engine's live browser-session state, behind interior mutability so the
/// mutator exports can record changes through a shared `&EngineState`.
pub struct SessionStore {
    inner: RefCell<SessionData>,
}

#[derive(Debug)]
struct SessionData {
    /// Configured backends, in registration order.
    backends: Vec<BackendEntry>,
    /// Cached OAuth token metadata, keyed by provider (e.g. `"gdrive"`). The
    /// durable copy lives in `IndexedDB` on the JS side; this is the in-memory
    /// view the session handler reports.
    auth_tokens: HashMap<String, StoredToken>,
    /// Open peer connections, keyed by the relay session id that established
    /// them. Values are opaque `RTCPeerConnection`-backed transports.
    peers: HashMap<String, OpaqueHandle>,
}

/// A configured backend: its id, its type, and the opaque JS handle that backs
/// it (the granted directory handle for `fsaccess`; absent for cloud backends
/// whose authority is reached over HTTP).
#[derive(Debug)]
struct BackendEntry {
    id: String,
    backend_type: String,
    handle: Option<OpaqueHandle>,
}

/// The session-relevant fields of a stored OAuth token.
#[derive(Debug)]
struct StoredToken {
    scope: String,
    /// Expiry as a unix timestamp in milliseconds, matching `oauth.ts`.
    expiry: i64,
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            inner: RefCell::new(SessionData {
                backends: Vec::new(),
                auth_tokens: HashMap::new(),
                peers: HashMap::new(),
            }),
        }
    }

    // ─────────────────────────── Mutators ───────────────────────────

    /// Register a backend, replacing any existing entry with the same id.
    pub fn set_backend(&self, id: String, backend_type: String, handle: Option<OpaqueHandle>) {
        let mut state = self.inner.borrow_mut();
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
    }

    /// Remove a backend by id, returning whether one was present.
    pub fn remove_backend(&self, id: &str) -> bool {
        let mut state = self.inner.borrow_mut();
        let before = state.backends.len();
        state.backends.retain(|b| b.id != id);
        state.backends.len() != before
    }

    /// Cache (or replace) the token metadata for a provider.
    pub fn set_token(&self, provider: String, scope: String, expiry: i64) {
        self.inner
            .borrow_mut()
            .auth_tokens
            .insert(provider, StoredToken { scope, expiry });
    }

    /// Drop a provider's cached token, returning whether one was present.
    pub fn remove_token(&self, provider: &str) -> bool {
        self.inner
            .borrow_mut()
            .auth_tokens
            .remove(provider)
            .is_some()
    }

    /// Record (or replace) the peer connection for a relay session id.
    pub fn set_peer(&self, session_id: String, connection: OpaqueHandle) {
        self.inner.borrow_mut().peers.insert(session_id, connection);
    }

    /// Drop a peer connection by relay session id, returning whether one was
    /// present.
    pub fn remove_peer(&self, session_id: &str) -> bool {
        self.inner.borrow_mut().peers.remove(session_id).is_some()
    }

    // ─────────────────────────── Read projections ───────────────────────────

    /// The configured-backend listing for `GET /v1/backends`.
    pub fn list_backends(&self) -> Value {
        let state = self.inner.borrow();
        let backends: Vec<Value> = state
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
    }

    /// The session view for `GET /v1/session`, paired with whether any session
    /// has been established at all (an authenticated provider, a granted local
    /// directory, or any configured backend). The router returns an error when
    /// no session exists.
    pub fn session(&self) -> (bool, Value) {
        let state = self.inner.borrow();
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_backends_reports_handle_presence() {
        let session = SessionStore::new();
        session.set_backend("cloud".to_owned(), "gdrive".to_owned(), None);
        session.set_backend(
            "local".to_owned(),
            "fsaccess".to_owned(),
            Some(NativeOpaqueHandle),
        );

        let listed = session.list_backends();
        let backends = listed
            .get("backends")
            .and_then(Value::as_array)
            .expect("backends array");
        assert_eq!(backends.len(), 2);
        let local = backends
            .iter()
            .find(|b| b.get("id").and_then(Value::as_str) == Some("local"))
            .expect("local backend present");
        assert_eq!(local.get("hasHandle").and_then(Value::as_bool), Some(true));
        let cloud = backends
            .iter()
            .find(|b| b.get("id").and_then(Value::as_str) == Some("cloud"))
            .expect("cloud backend present");
        assert_eq!(cloud.get("hasHandle").and_then(Value::as_bool), Some(false));
    }

    #[test]
    fn set_backend_replaces_an_existing_entry() {
        let session = SessionStore::new();
        session.set_backend("b".to_owned(), "gdrive".to_owned(), None);
        session.set_backend("b".to_owned(), "s3".to_owned(), None);

        let listed = session.list_backends();
        let backends = listed
            .get("backends")
            .and_then(Value::as_array)
            .expect("backends array");
        assert_eq!(backends.len(), 1, "same id must replace, not append");
        assert_eq!(backends[0].get("type").and_then(Value::as_str), Some("s3"));
    }

    #[test]
    fn remove_backend_reports_whether_one_was_present() {
        let session = SessionStore::new();
        session.set_backend("b".to_owned(), "gdrive".to_owned(), None);
        assert!(session.remove_backend("b"));
        assert!(!session.remove_backend("b"));
    }

    #[test]
    fn tokens_drive_the_authenticated_flag() {
        let session = SessionStore::new();
        let (established, _) = session.session();
        assert!(!established);

        session.set_token("gdrive".to_owned(), "drive.file".to_owned(), 99);
        let (established, body) = session.session();
        assert!(established);
        assert_eq!(
            body.get("authenticated").and_then(Value::as_bool),
            Some(true)
        );
        let providers = body
            .get("providers")
            .and_then(Value::as_array)
            .expect("providers array");
        assert_eq!(providers.len(), 1);

        assert!(session.remove_token("gdrive"));
        assert!(!session.remove_token("gdrive"));
        let (established, _) = session.session();
        assert!(!established);
    }

    #[test]
    fn fs_access_is_granted_only_with_a_handle() {
        let session = SessionStore::new();
        // A directory backend without a granted handle does not count.
        session.set_backend("d".to_owned(), "fsaccess".to_owned(), None);
        let (_, body) = session.session();
        assert_eq!(
            body.get("fsAccessGranted").and_then(Value::as_bool),
            Some(false)
        );

        session.set_backend(
            "d".to_owned(),
            "fsaccess".to_owned(),
            Some(NativeOpaqueHandle),
        );
        let (_, body) = session.session();
        assert_eq!(
            body.get("fsAccessGranted").and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn peers_are_counted_and_removable() {
        let session = SessionStore::new();
        session.set_peer("s1".to_owned(), NativeOpaqueHandle);
        session.set_peer("s2".to_owned(), NativeOpaqueHandle);
        let (_, body) = session.session();
        assert_eq!(body.get("peerCount").and_then(Value::as_u64), Some(2));

        assert!(session.remove_peer("s1"));
        assert!(!session.remove_peer("s1"));
        let (_, body) = session.session();
        assert_eq!(body.get("peerCount").and_then(Value::as_u64), Some(1));
    }
}
