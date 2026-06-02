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
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
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

/// Bind a small HTTP server on `bind` exposing the announce routes.
///
/// Returns the actual bound address (useful when binding to port 0) and a
/// task handle that keeps the server alive until dropped. Mirrors the
/// metrics endpoint's lifecycle so the relay's two optional HTTP surfaces are
/// managed the same way.
pub async fn serve_announce(
    bind: SocketAddr,
    directory: Arc<AnnounceDirectory>,
) -> Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding announce endpoint to {bind}"))?;
    let local = listener
        .local_addr()
        .context("reading local address for bound announce listener")?;

    let app = Router::new()
        .route("/announce/{device_id}", post(register_handler))
        .route("/announce/{device_id}", get(lookup_handler))
        .layer(DefaultBodyLimit::max(MAX_ANNOUNCE_REQUEST_BYTES))
        .with_state(directory);

    let join = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            warn!(error = %err, "announce server exited with error");
        }
    });
    Ok((local, join))
}

async fn register_handler(
    State(directory): State<Arc<AnnounceDirectory>>,
    Path(device_id): Path<String>,
    Json(request): Json<AnnounceRequest>,
) -> Response {
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
    directory.register(device_id, request.signed).await;
    StatusCode::NO_CONTENT.into_response()
}

async fn lookup_handler(
    State(directory): State<Arc<AnnounceDirectory>>,
    Path(device_id): Path<String>,
) -> Response {
    let signed = directory.lookup(&device_id).await;
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

        let response = register_handler(
            State(Arc::clone(&directory)),
            Path("DEVICE-A".to_string()),
            Json(AnnounceRequest { signed: oversized }),
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

        let response = register_handler(
            State(Arc::clone(&directory)),
            Path("DEVICE-A".to_string()),
            Json(AnnounceRequest {
                signed: at_cap.clone(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(directory.lookup("DEVICE-A").await, Some(at_cap));
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
