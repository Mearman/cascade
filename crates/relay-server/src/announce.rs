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
//!   stored candidate set for `device_id`.
//! - `GET  /announce/<device_id>` — returns a
//!   [`cascade_p2p::discovery::announce::LookupResponse`]. An unknown id
//!   yields an empty candidate set rather than a `404`, so the client models
//!   absence as "no candidates".
//!
//! The wire types are owned by `cascade-p2p` so the client and server cannot
//! drift. This module supplies only the storage and the HTTP surface.

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
use cascade_p2p::discovery::announce::{
    AnnounceRequest, LookupResponse, MAX_ANNOUNCE_CANDIDATES, WireCandidate,
};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::warn;

/// In-memory candidate directory keyed by device id.
///
/// A registration replaces the candidate set for its device id in full
/// (matching the announce client's replace-in-full semantics). The store is
/// process-local and non-persistent: an announce directory is a soft-state
/// rendezvous hint, not a source of truth, so a restart simply forces devices
/// to re-announce on their next loop tick.
#[derive(Debug, Default)]
pub struct AnnounceDirectory {
    entries: RwLock<HashMap<String, Vec<WireCandidate>>>,
}

impl AnnounceDirectory {
    /// Create an empty directory.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Replace the candidate set registered for `device_id`.
    ///
    /// Candidates beyond [`MAX_ANNOUNCE_CANDIDATES`] are dropped so a hostile
    /// or buggy client cannot inflate the directory's per-device storage past
    /// the same cap the wire protocol enforces on a candidate frame.
    pub async fn register(&self, device_id: String, mut candidates: Vec<WireCandidate>) {
        candidates.truncate(MAX_ANNOUNCE_CANDIDATES);
        self.entries.write().await.insert(device_id, candidates);
    }

    /// Return the candidate set last registered for `device_id`, or an empty
    /// vector when the id is unknown.
    pub async fn lookup(&self, device_id: &str) -> Vec<WireCandidate> {
        self.entries
            .read()
            .await
            .get(device_id)
            .cloned()
            .unwrap_or_default()
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
    directory.register(device_id, request.candidates).await;
    StatusCode::NO_CONTENT.into_response()
}

async fn lookup_handler(
    State(directory): State<Arc<AnnounceDirectory>>,
    Path(device_id): Path<String>,
) -> Response {
    let candidates = directory.lookup(&device_id).await;
    Json(LookupResponse { candidates }).into_response()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use cascade_p2p::candidate::{Candidate, CandidateKind};

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    #[tokio::test]
    async fn register_then_lookup_returns_same_candidates() {
        let directory = AnnounceDirectory::new();
        let host = WireCandidate::from(Candidate::new(addr(22000), CandidateKind::Host, 65_535));
        let srflx = WireCandidate::from(Candidate::new(
            addr(33000),
            CandidateKind::ServerReflexive,
            0,
        ));
        directory
            .register("DEVICE-A".to_string(), vec![host, srflx])
            .await;

        let looked_up = directory.lookup("DEVICE-A").await;
        assert_eq!(looked_up, vec![host, srflx]);
    }

    #[tokio::test]
    async fn lookup_unknown_device_is_empty() {
        let directory = AnnounceDirectory::new();
        assert!(directory.lookup("NEVER-REGISTERED").await.is_empty());
    }

    #[tokio::test]
    async fn register_replaces_previous_set_in_full() {
        let directory = AnnounceDirectory::new();
        let first = WireCandidate::from(Candidate::new(addr(22000), CandidateKind::Host, 1));
        let second = WireCandidate::from(Candidate::new(addr(22001), CandidateKind::Host, 2));
        directory.register("D".to_string(), vec![first]).await;
        directory.register("D".to_string(), vec![second]).await;
        assert_eq!(directory.lookup("D").await, vec![second]);
    }

    #[tokio::test]
    async fn register_truncates_past_the_cap() {
        let directory = AnnounceDirectory::new();
        let candidates: Vec<WireCandidate> = (0..MAX_ANNOUNCE_CANDIDATES + 10)
            .map(|i| {
                let port = u16::try_from(20_000 + i).unwrap();
                WireCandidate::from(Candidate::new(addr(port), CandidateKind::Host, 0))
            })
            .collect();
        directory.register("D".to_string(), candidates).await;
        assert_eq!(directory.lookup("D").await.len(), MAX_ANNOUNCE_CANDIDATES);
    }
}
