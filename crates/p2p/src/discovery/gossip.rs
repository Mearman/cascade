//! Introducer-gossip discovery source.
//!
//! Wraps the [`PeerBook`] — the peer database fed
//! by introducer gossip (see [`crate::wan`]) — behind the [`Discovery`]
//! trait. Resolution is a pure read of what gossip has already learned:
//! the most recently advertised remote candidates for a device, falling
//! back to host candidates synthesised from the peer's known reachable
//! addresses whenever no non-empty candidate set has been advertised yet.
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
    /// `BepMessage::Candidates` exchange. An empty advertised set counts
    /// as nothing advertised: a punch entry may exist with no candidates
    /// (a hole-punch agreement is recorded before any `Candidates` frame
    /// arrives) or a peer may send an explicitly empty `Candidates` frame,
    /// and in both cases the host-address fallback must still fire. So the
    /// remote set short-circuits only when it is non-empty; otherwise this
    /// falls back to host candidates built from the peer's known reachable
    /// addresses. An unknown device yields no candidates.
    async fn resolve(&self, device_id: &str) -> Vec<Candidate> {
        let book = self.peer_book.read().await;
        if let Some(candidates) = book.remote_candidates(device_id)
            && !candidates.is_empty()
        {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    #[tokio::test]
    async fn resolve_prefers_remote_candidates() {
        let mut book = PeerBook::new();
        book.add_peer("DEVICE-A".to_string(), vec![addr(22000)]);
        let advertised = Candidate::new(addr(33000), CandidateKind::ServerReflexive, 0);
        book.set_remote_candidates("DEVICE-A", vec![advertised]);

        let source = GossipDiscovery::new(Arc::new(RwLock::new(book)));
        let resolved = source.resolve("DEVICE-A").await;
        assert_eq!(resolved, vec![advertised]);
    }

    #[tokio::test]
    async fn resolve_falls_back_to_known_addresses() {
        let mut book = PeerBook::new();
        book.add_peer("DEVICE-A".to_string(), vec![addr(22000), addr(22001)]);

        let source = GossipDiscovery::new(Arc::new(RwLock::new(book)));
        let resolved = source.resolve("DEVICE-A").await;
        assert_eq!(resolved.len(), 2);
        assert!(resolved.iter().all(|c| c.kind == CandidateKind::Host));
        let ports: Vec<u16> = resolved.iter().map(|c| c.address.port()).collect();
        assert!(ports.contains(&22000));
        assert!(ports.contains(&22001));
    }

    #[tokio::test]
    async fn resolve_unknown_device_is_empty() {
        let source = GossipDiscovery::new(Arc::new(RwLock::new(PeerBook::new())));
        assert!(source.resolve("UNKNOWN").await.is_empty());
    }

    #[tokio::test]
    async fn announce_is_a_no_op_and_leaves_the_peer_book_untouched() {
        // Gossip publishes through the `BepMessage::Gossip` frame the sync
        // engine drives, not through this trait, so `announce` here must change
        // nothing: it must neither register the announcing device nor record
        // its candidates. A regression that made it write through the trait
        // would duplicate the gossip path and silently alter gossip semantics.
        let book = Arc::new(RwLock::new(PeerBook::new()));
        let source = GossipDiscovery::new(book.clone());

        let candidate = Candidate::new(addr(22000), CandidateKind::Host, 0);
        source.announce("SELF", &[candidate]).await;

        let book = book.read().await;
        assert!(
            book.is_empty(),
            "announce must not register the announcing device",
        );
        assert!(
            book.remote_candidates("SELF").is_none(),
            "announce must not record candidates for the announcing device",
        );
    }

    #[tokio::test]
    async fn resolve_falls_back_when_punch_negotiated_without_candidate_exchange() {
        use crate::traversal::SyncPunchAgreement;

        // A hole-punch agreement is recorded before any `Candidates`
        // frame arrives, which creates a punch entry whose candidate
        // vector is empty. The host-address fallback must still fire.
        let mut book = PeerBook::new();
        book.add_peer("DEVICE-A".to_string(), vec![addr(22000), addr(22001)]);
        book.start_punch_with(
            "DEVICE-A",
            SyncPunchAgreement {
                nonce: 7,
                deadline_unix_ms: 1_700_000_000_000,
            },
        );

        let source = GossipDiscovery::new(Arc::new(RwLock::new(book)));
        let resolved = source.resolve("DEVICE-A").await;
        assert_eq!(resolved.len(), 2);
        assert!(resolved.iter().all(|c| c.kind == CandidateKind::Host));
        let ports: Vec<u16> = resolved.iter().map(|c| c.address.port()).collect();
        assert!(ports.contains(&22000));
        assert!(ports.contains(&22001));
    }

    #[tokio::test]
    async fn resolve_falls_back_when_remote_advertises_empty_candidates() {
        // A peer (buggy or hostile) may send an explicitly empty
        // `Candidates` frame. An empty advertised set is treated as
        // nothing advertised, so the host-address fallback still fires
        // rather than returning an empty result.
        let mut book = PeerBook::new();
        book.add_peer("DEVICE-A".to_string(), vec![addr(22000)]);
        book.set_remote_candidates("DEVICE-A", vec![]);

        let source = GossipDiscovery::new(Arc::new(RwLock::new(book)));
        let resolved = source.resolve("DEVICE-A").await;
        assert_eq!(resolved.len(), 1);
        assert!(resolved.iter().all(|c| c.kind == CandidateKind::Host));
        assert_eq!(resolved.first().map(|c| c.address.port()), Some(22000));
    }
}
