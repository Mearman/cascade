//! Introducer-gossip discovery source.
//!
//! Wraps the [`PeerBook`] — the peer database fed
//! by introducer gossip (see [`crate::wan`]) — behind the [`Discovery`]
//! trait. Resolution is a pure read of what gossip has already learned:
//! the most recently advertised remote candidates for a device, falling
//! back to host candidates synthesised from the peer's known reachable
//! addresses when no candidate exchange has happened yet.
//!
//! This source does not change gossip semantics. It neither initiates
//! nor merges gossip frames — that machinery lives in [`crate::wan`] and
//! the backend sync engine. [`GossipDiscovery`] only reads the peer book
//! those components maintain.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use super::Discovery;
use crate::candidate::{Candidate, CandidateKind};
use crate::wan::PeerBook;

/// `local_preference` assigned to a host candidate synthesised from a
/// peer's known address when no candidate exchange has occurred. Set to
/// the maximum so a directly-known address ranks above externally-derived
/// candidates of the same RFC 8445 type preference.
const KNOWN_ADDRESS_LOCAL_PREFERENCE: u16 = u16::MAX;

/// Gossip-backed discovery source.
///
/// Holds a shared handle to the same [`PeerBook`] the sync engine
/// maintains, so [`Discovery::resolve`] always reflects the live state
/// of introducer gossip.
#[derive(Debug, Clone)]
pub struct GossipDiscovery {
    peer_book: Arc<RwLock<PeerBook>>,
}

impl GossipDiscovery {
    /// Wrap a shared peer book as a discovery source.
    #[must_use]
    pub const fn new(peer_book: Arc<RwLock<PeerBook>>) -> Self {
        Self { peer_book }
    }
}

#[async_trait]
impl Discovery for GossipDiscovery {
    /// Return the candidates gossip already knows for `device_id`.
    ///
    /// Prefers the remote candidate set the peer last advertised over a
    /// `BepMessage::Candidates` exchange. When no exchange has happened,
    /// falls back to host candidates built from the peer's known
    /// reachable addresses. An unknown device yields no candidates.
    async fn resolve(&self, device_id: &str) -> Vec<Candidate> {
        let book = self.peer_book.read().await;
        if let Some(candidates) = book.remote_candidates(device_id) {
            return candidates.to_vec();
        }
        book.get(device_id).map_or_else(Vec::new, |peer| {
            peer.addresses
                .iter()
                .map(|addr| {
                    Candidate::new(*addr, CandidateKind::Host, KNOWN_ADDRESS_LOCAL_PREFERENCE)
                })
                .collect()
        })
    }

    /// Gossip announces through the `BepMessage::Gossip` frame driven by
    /// the sync engine, not through this trait. Announcing here would
    /// duplicate that path and change gossip semantics, so it is a
    /// deliberate no-op.
    async fn announce(&self, _self_id: &str, _candidates: &[Candidate]) {}
}
