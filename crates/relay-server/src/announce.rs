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
//! is a *blind, untrusted carrier*: it stores and serves the signed blob
//! verbatim and never inspects, validates, or vouches for it — the looking-up
//! client is the only party that verifies the signature. The wire types are
//! owned by `cascade-p2p` so the client and server cannot drift. This module
//! supplies only the storage and the HTTP surface.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use cascade_p2p::discovery::announce::{AnnounceRequest, LookupResponse};
use cascade_p2p::discovery::signing::SignedCandidates;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::warn;

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
/// trust the server does not hold. The per-device storage is still bounded
/// because the wire type itself caps the candidate count at the client.
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
    /// it. Verification is the looking-up client's job; the carrier's only role
    /// is durable-ish storage and retrieval.
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
}
