//! Optional announce/lookup directory endpoint.
//!
//! The relay-server's primary job is the opaque byte-pipe (see
//! [`crate::server`]). This module adds a second, independent role behind the
//! `announce` cargo feature: a rendezvous directory where a device publishes
//! the candidates it is reachable on, keyed by its device id, and any other
//! device looks those candidates up by id.
//!
//! The directory carries no payload traffic — it only stores and serves
//! candidate sets. Two devices that have never met use it to learn each
//! other's reachable addresses, then connect directly (or via the byte-pipe
//! relay) using the candidates they retrieved.
//!
//! ## Routes
//!
//! - `POST /announce/<device_id>` with a
//!   [`cascade_p2p::discovery::announce::AnnounceRequest`] body — replaces the
//!   stored signed candidate set for `device_id`.
//! - `GET  /announce/<device_id>` — returns a
//!   [`cascade_p2p::discovery::announce::LookupResponse`]. An unknown id
//!   yields `signed: None` rather than a `404`, so the client models absence
//!   as "no candidates".
//!
//! The candidate set is a self-signed
//! [`cascade_p2p::discovery::signing::SignedCandidates`] envelope. The server
//! is a *blind, untrusted carrier* for the *contents* of that envelope: it
//! stores and serves the signed blob verbatim and never inspects, validates, or
//! vouches for the candidates, the key, or the signature — the looking-up client
//! is the only party that verifies the signature. The wire types are owned by
//! `cascade-p2p` so the client and server cannot drift. This module supplies
//! only the storage and the HTTP surface.
//!
//! ## Write authentication
//!
//! Blind on *contents* does not mean open on *writers*. This endpoint and the
//! Cloudflare Worker host the same announce contract, and both gate the write
//! path identically: a register must carry an
//! [`HMAC-SHA256`](cascade_announce_wire::auth) tag over the path device id and
//! the exact request body in the [`ANNOUNCE_AUTH_HEADER`] header, keyed by the
//! relay's shared secret — the same secret that
//! authenticates the byte-pipe handshake. A missing, malformed, or
//! non-verifying tag is a `401`, and nothing is stored. This keeps the two
//! hosts of the contract behaviourally identical, so the announce client (which
//! always stamps the header) succeeds against either carrier and never silently
//! against one and `401`s against the other. Verifying the *writer* is
//! orthogonal to trusting the *blob*: the envelope is still self-certifying on
//! read, and binding the body into the tag stops a man-in-the-middle swapping
//! one stored blob for another in flight.
//!
//! Blind on *contents* does not mean unbounded on *size*. Because the carrier
//! does not trust whoever posts to it, it cannot assume the candidate count was
//! capped client-side — a hostile client skips the honest client's
//! [`cascade_p2p::discovery::MAX_ANNOUNCE_CANDIDATES`] cap entirely. The server
//! therefore enforces the cap itself, two ways: a [`DefaultBodyLimit`] rejects an
//! oversized body before it is even read, and the register handler rejects a
//! registration whose candidate count exceeds the cap before storing it. Neither
//! truncates (which would invalidate the signature); both reject. `signed.candidates.len()`
//! is a plain struct field, so reading it implies no trust in and no verification
//! of the envelope's contents.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use cascade_announce_wire::auth::{self, ANNOUNCE_AUTH_HEADER, SHARED_SECRET_LEN};
use cascade_p2p::discovery::MAX_ANNOUNCE_CANDIDATES;
use cascade_p2p::discovery::announce::{AnnounceRequest, LookupResponse};
use cascade_p2p::discovery::signing::SignedCandidates;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::warn;

/// Upper bound, in bytes, on a single JSON-encoded [`AnnounceRequest`] body the
/// register route will read off the wire.
///
/// This is the request-body ceiling that backstops the register handler's count
/// check: a hostile client that posts a body fatter than a
/// legitimate [`MAX_ANNOUNCE_CANDIDATES`]-candidate set is refused by axum before
/// the body is buffered, so it never reaches the handler or the store. The
/// ceiling is *derived* from the cap, not picked arbitrarily — it is the cap
/// times a generous per-candidate JSON budget, plus a fixed envelope budget for
/// the device id, the base64 signature, the expiry, and JSON punctuation.
///
/// Per-candidate budget: a [`cascade_p2p::discovery::announce::WireCandidate`]
/// serialises as `{"address":"[<ipv6>]:<port>","kind":<u8>,"priority":<u32>}`.
/// The widest IPv6 socket-address string, the three-digit `kind`, the ten-digit
/// `priority`, the field names, and the punctuation all fit inside this budget
/// with comfortable headroom, so a legitimately-capped set is never rejected by
/// the body limit (the count check is the precise gate; this is the coarse one).
const MAX_WIRE_CANDIDATE_JSON_BYTES: usize = 96;

/// Fixed JSON budget for everything in the request that is not a candidate: the
/// `{"signed":{...}}` wrapping, the `device_id` string (a base32 SHA-256, 52
/// bytes), the `expires_at_unix_ms` integer, the 88-byte base64 signature
/// string, and all field names and punctuation. Sized with headroom so the limit
/// never clips an honest request.
const ANNOUNCE_REQUEST_ENVELOPE_JSON_BYTES: usize = 512;

/// Request-body ceiling for the announce register route, derived from
/// [`MAX_ANNOUNCE_CANDIDATES`].
const MAX_ANNOUNCE_REQUEST_BYTES: usize =
    MAX_ANNOUNCE_CANDIDATES * MAX_WIRE_CANDIDATE_JSON_BYTES + ANNOUNCE_REQUEST_ENVELOPE_JSON_BYTES;

/// In-memory directory of signed candidate sets keyed by device id.
///
/// A registration replaces the signed set for its device id in full (matching
/// the announce client's replace-in-full semantics). The store is process-local
/// and non-persistent: an announce directory is a soft-state rendezvous hint,
/// not a source of truth, so a restart simply forces devices to re-announce on
/// their next loop tick.
///
/// The stored value is the opaque [`SignedCandidates`] blob. The directory is a
/// blind carrier — it never inspects, verifies, or trims the candidates inside
/// the envelope, because doing so would either break the signature or imply a
/// trust the server does not hold. Per-device storage is bounded not by trusting
/// the client to cap its candidate count, but by the server rejecting any
/// registration over [`MAX_ANNOUNCE_CANDIDATES`] in the register handler and
/// by the [`DefaultBodyLimit`] on the route. Reading `candidates.len()` to apply
/// that bound inspects no signed content and implies no trust — it is the one
/// structural fact the carrier may act on.
#[derive(Debug, Default)]
pub struct AnnounceDirectory {
    entries: RwLock<HashMap<String, SignedCandidates>>,
}

impl AnnounceDirectory {
    /// Create an empty directory.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Replace the signed candidate set registered for `device_id`.
    ///
    /// The blob is stored verbatim — the server does not inspect or validate
    /// the signature or the candidates. Verification is the looking-up client's
    /// job; the carrier's only role is storage and retrieval. The size bound
    /// (candidate count and request body) is enforced by the register handler
    /// before this is reached, so a set that gets here is already within
    /// [`MAX_ANNOUNCE_CANDIDATES`].
    pub async fn register(&self, device_id: String, signed: SignedCandidates) {
        self.entries.write().await.insert(device_id, signed);
    }

    /// Return the signed set last registered for `device_id`, or `None` when
    /// the id is unknown.
    pub async fn lookup(&self, device_id: &str) -> Option<SignedCandidates> {
        self.entries.read().await.get(device_id).cloned()
    }
}

/// Router state for the announce endpoint: the soft-state directory plus the
/// shared secret that authenticates writers.
///
/// The secret is the relay's existing `HMAC` key — the same one the byte-pipe
/// handshake uses — so an operator configures one secret for both surfaces. It
/// is held by value (`Copy`) so the state clones cheaply across requests.
#[derive(Clone)]
struct AnnounceState {
    directory: Arc<AnnounceDirectory>,
    secret: [u8; SHARED_SECRET_LEN],
}

/// Bind a small HTTP server on `bind` exposing the announce routes.
///
/// Returns the actual bound address (useful when binding to port 0) and a
/// task handle that keeps the server alive until dropped. Mirrors the
/// metrics endpoint's lifecycle so the relay's two optional HTTP surfaces are
/// managed the same way.
pub async fn serve_announce(
    bind: SocketAddr,
    directory: Arc<AnnounceDirectory>,
    secret: [u8; SHARED_SECRET_LEN],
) -> Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding announce endpoint to {bind}"))?;
    let local = listener
        .local_addr()
        .context("reading local address for bound announce listener")?;

    let state = AnnounceState { directory, secret };
    let app = Router::new()
        .route("/announce/{device_id}", post(register_handler))
        .route("/announce/{device_id}", get(lookup_handler))
        .layer(DefaultBodyLimit::max(MAX_ANNOUNCE_REQUEST_BYTES))
        .with_state(state);

    let join = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            warn!(error = %err, "announce server exited with error");
        }
    });
    Ok((local, join))
}

async fn register_handler(
    State(state): State<AnnounceState>,
    Path(device_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Authenticate the writer before any parsing or storage. The HMAC binds the
    // path device id and the exact received body, so a missing tag, a forged
    // tag, a wrong secret, or a man-in-the-middle body swap all fail. A missing,
    // malformed, and non-verifying tag collapse to one `401` so an
    // unauthenticated caller learns nothing about why it failed — identical to
    // the Worker's `Outcome::Unauthorized`.
    if !writer_is_authenticated(&state.secret, &device_id, &body, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    // Parse only after authentication: an unauthenticated caller never reaches
    // the JSON parser. The body bytes are exactly what the tag authenticated.
    let request: AnnounceRequest = match serde_json::from_slice(&body) {
        Ok(parsed) => parsed,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    // Bound per-device storage by rejecting an oversized registration rather
    // than storing it. Reading `candidates.len()` is the one structural fact the
    // carrier may act on without trusting the envelope's contents: it does not
    // inspect, parse, or verify a single candidate or the signature. Truncating
    // would invalidate the signature, so the carrier rejects instead — a hostile
    // client that skips the honest client's cap is refused here, and the
    // body-read limit (see `serve_announce`) catches the same abuse one layer
    // earlier for a body too large to even buffer.
    if request.signed.candidates.len() > MAX_ANNOUNCE_CANDIDATES {
        warn!(
            device_id = %device_id,
            count = request.signed.candidates.len(),
            cap = MAX_ANNOUNCE_CANDIDATES,
            "rejecting announce registration: candidate count exceeds cap",
        );
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }

    // Store the signed blob verbatim. The server never inspects the envelope,
    // not even to check the claimed device id against the path — a hostile
    // registration only stores a value that no resolver will accept, because
    // the signature must verify against the *resolved* id.
    state.directory.register(device_id, request.signed).await;
    StatusCode::NO_CONTENT.into_response()
}

/// Verify the write-auth header for a register request against `secret`.
///
/// Returns `true` only when the header is present and its hex tag verifies (in
/// constant time) over the path device id and the exact body. A missing header,
/// a malformed tag, and a non-verifying tag all return `false` — the caller maps
/// every one to a single `401`.
fn writer_is_authenticated(
    secret: &[u8; SHARED_SECRET_LEN],
    device_id: &str,
    body: &[u8],
    headers: &HeaderMap,
) -> bool {
    let Some(header) = headers.get(ANNOUNCE_AUTH_HEADER) else {
        return false;
    };
    let Ok(header) = header.to_str() else {
        return false;
    };
    auth::verify_announce_write(secret, device_id, body, header).unwrap_or(false)
}

async fn lookup_handler(
    State(state): State<AnnounceState>,
    Path(device_id): Path<String>,
) -> Response {
    let signed = state.directory.lookup(&device_id).await;
    Json(LookupResponse { signed }).into_response()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use cascade_p2p::candidate::{Candidate, CandidateKind};
    use cascade_p2p::discovery::announce::WireCandidate;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    /// Far enough ahead that the envelope is unexpired for any sane test clock;
    /// the directory never reads it, so the exact value is immaterial to the
    /// pass-through behaviour under test.
    const EXPIRY_MS: i64 = 1_700_000_000_000 + 3_600_000;

    fn signed_set(device_id: &str, ports: &[u16]) -> SignedCandidates {
        let wire: Vec<WireCandidate> = ports
            .iter()
            .map(|&p| WireCandidate::from(Candidate::new(addr(p), CandidateKind::Host, 0)))
            .collect();
        SignedCandidates::sign(device_id, wire, EXPIRY_MS)
    }

    /// Deterministic 32-byte shared secret the writer and the carrier agree on.
    fn secret() -> [u8; SHARED_SECRET_LEN] {
        let mut s = [0u8; SHARED_SECRET_LEN];
        for (idx, byte) in s.iter_mut().enumerate() {
            *byte = u8::try_from(idx).unwrap_or(0);
        }
        s
    }

    /// State wrapping `directory` and the test [`secret`].
    fn state(directory: &Arc<AnnounceDirectory>) -> AnnounceState {
        AnnounceState {
            directory: Arc::clone(directory),
            secret: secret(),
        }
    }

    /// Serialise `signed` into the wire body and the matching write-auth header
    /// for `device_id`, the way the announce client does.
    fn register_body(device_id: &str, signed: SignedCandidates) -> (Bytes, String) {
        let body = serde_json::to_vec(&AnnounceRequest { signed }).unwrap();
        let tag = auth::announce_write_tag(&secret(), device_id, &body).unwrap();
        (Bytes::from(body), auth::encode_hex(&tag))
    }

    /// A [`HeaderMap`] carrying the write-auth header set to `value`.
    fn auth_headers(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(ANNOUNCE_AUTH_HEADER, value.parse().unwrap());
        headers
    }

    #[tokio::test]
    async fn register_then_lookup_returns_the_blob_unchanged() {
        let directory = AnnounceDirectory::new();
        let signed = signed_set("DEVICE-A", &[22000, 33000]);
        directory
            .register("DEVICE-A".to_string(), signed.clone())
            .await;

        // The carrier returns exactly what it was given — byte-for-byte the
        // same signed envelope, never re-derived or re-ordered.
        assert_eq!(directory.lookup("DEVICE-A").await, Some(signed));
    }

    #[tokio::test]
    async fn lookup_unknown_device_is_none() {
        let directory = AnnounceDirectory::new();
        assert_eq!(directory.lookup("NEVER-REGISTERED").await, None);
    }

    #[tokio::test]
    async fn register_replaces_previous_set_in_full() {
        let directory = AnnounceDirectory::new();
        let first = signed_set("D", &[22000]);
        let second = signed_set("D", &[22001]);
        directory.register("D".to_string(), first).await;
        directory.register("D".to_string(), second.clone()).await;
        assert_eq!(directory.lookup("D").await, Some(second));
    }

    /// A registration carrying more than `MAX_ANNOUNCE_CANDIDATES` candidates is
    /// rejected with `413 Payload Too Large` and never stored, so a hostile
    /// client that skips the honest client's cap cannot inflate per-device
    /// storage. The signature stays intact (no truncation) — the server simply
    /// refuses the oversized set.
    #[tokio::test]
    async fn register_over_the_cap_is_rejected_and_not_stored() {
        let directory = AnnounceDirectory::new();
        // One past the cap: the smallest set that must be refused.
        let ports: Vec<u16> = (0..=u16::try_from(MAX_ANNOUNCE_CANDIDATES).unwrap())
            .map(|i| 22000u16.wrapping_add(i))
            .collect();
        assert_eq!(ports.len(), MAX_ANNOUNCE_CANDIDATES + 1);
        let oversized = signed_set("DEVICE-A", &ports);
        let (body, header) = register_body("DEVICE-A", oversized);

        let response = register_handler(
            State(state(&directory)),
            Path("DEVICE-A".to_string()),
            auth_headers(&header),
            body,
        )
        .await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

        // Nothing was stored: the oversized set never entered the directory.
        assert_eq!(directory.lookup("DEVICE-A").await, None);
    }

    /// A registration at exactly the cap is accepted and stored, so the bound
    /// rejects only the genuinely oversized — it never clips a legitimate
    /// full-sized candidate set.
    #[tokio::test]
    async fn register_at_the_cap_is_accepted() {
        let directory = AnnounceDirectory::new();
        let ports: Vec<u16> = (0..u16::try_from(MAX_ANNOUNCE_CANDIDATES).unwrap())
            .map(|i| 22000u16.wrapping_add(i))
            .collect();
        assert_eq!(ports.len(), MAX_ANNOUNCE_CANDIDATES);
        let at_cap = signed_set("DEVICE-A", &ports);
        let (body, header) = register_body("DEVICE-A", at_cap.clone());

        let response = register_handler(
            State(state(&directory)),
            Path("DEVICE-A".to_string()),
            auth_headers(&header),
            body,
        )
        .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(directory.lookup("DEVICE-A").await, Some(at_cap));
    }

    /// An authenticated register is stored; a lookup returns it byte-for-byte.
    /// This is the happy path through the HMAC gate, proving an honest writer's
    /// header verifies against the endpoint.
    #[tokio::test]
    async fn an_authenticated_register_is_stored_and_looked_up() {
        let directory = AnnounceDirectory::new();
        let signed = signed_set("DEVICE-A", &[22000, 33000]);
        let (body, header) = register_body("DEVICE-A", signed.clone());

        let response = register_handler(
            State(state(&directory)),
            Path("DEVICE-A".to_string()),
            auth_headers(&header),
            body,
        )
        .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(directory.lookup("DEVICE-A").await, Some(signed));
    }

    /// A register with no write-auth header is `401` and stores nothing, so the
    /// endpoint and the Worker reject an unauthenticated writer identically.
    #[tokio::test]
    async fn a_register_without_the_auth_header_is_unauthorized_and_stores_nothing() {
        let directory = AnnounceDirectory::new();
        let (body, _header) = register_body("DEVICE-A", signed_set("DEVICE-A", &[22000]));

        let response = register_handler(
            State(state(&directory)),
            Path("DEVICE-A".to_string()),
            HeaderMap::new(),
            body,
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(directory.lookup("DEVICE-A").await, None);
    }

    /// A tag built for one path id, replayed onto another id's path, is `401`:
    /// the tag binds the device id, so it cannot be lifted onto another key.
    #[tokio::test]
    async fn a_tag_for_another_device_id_is_unauthorized() {
        let directory = AnnounceDirectory::new();
        // Tag is computed for DEVICE-A's body; posting it to DEVICE-B's path
        // must fail because the verifier recomputes over the path id.
        let (body, header) = register_body("DEVICE-A", signed_set("DEVICE-A", &[22000]));

        let response = register_handler(
            State(state(&directory)),
            Path("DEVICE-B".to_string()),
            auth_headers(&header),
            body,
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(directory.lookup("DEVICE-B").await, None);
    }

    /// A body swapped after the tag was computed fails verification: the tag
    /// binds the exact bytes, so a man-in-the-middle substitution is `401`.
    #[tokio::test]
    async fn a_swapped_body_is_unauthorized() {
        let directory = AnnounceDirectory::new();
        let (_body, header) = register_body("DEVICE-A", signed_set("DEVICE-A", &[22000]));
        let (other_body, _other) = register_body("DEVICE-A", signed_set("DEVICE-A", &[44000]));

        let response = register_handler(
            State(state(&directory)),
            Path("DEVICE-A".to_string()),
            auth_headers(&header),
            other_body,
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(directory.lookup("DEVICE-A").await, None);
    }

    /// A malformed (non-hex) tag is `401`, not a `400` or a panic — every
    /// auth-failure reason collapses to one status so the caller learns nothing.
    #[tokio::test]
    async fn a_malformed_auth_header_is_unauthorized() {
        let directory = AnnounceDirectory::new();
        let (body, _header) = register_body("DEVICE-A", signed_set("DEVICE-A", &[22000]));

        let response = register_handler(
            State(state(&directory)),
            Path("DEVICE-A".to_string()),
            auth_headers("not-hex"),
            body,
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(directory.lookup("DEVICE-A").await, None);
    }

    /// The body ceiling is derived from the cap, not hand-picked: it must be
    /// large enough that a legitimate cap-sized request always fits, so the
    /// precise count check — not the coarse body limit — is what rejects an
    /// over-cap set. A serialised cap-sized request must therefore be strictly
    /// smaller than the configured body limit.
    #[test]
    fn the_body_limit_admits_a_full_cap_sized_request() {
        let ports: Vec<u16> = (0..u16::try_from(MAX_ANNOUNCE_CANDIDATES).unwrap())
            .map(|i| 22000u16.wrapping_add(i))
            .collect();
        let request = AnnounceRequest {
            signed: signed_set("DEVICE-A", &ports),
        };
        let serialised = serde_json::to_vec(&request).unwrap();
        assert!(
            serialised.len() <= MAX_ANNOUNCE_REQUEST_BYTES,
            "cap-sized request ({} bytes) must fit the body limit ({} bytes)",
            serialised.len(),
            MAX_ANNOUNCE_REQUEST_BYTES,
        );
    }
}
