//! Pluggable peer discovery.
//!
//! Discovery is the act of turning a peer's device ID into a set of
//! reachable [`Candidate`] transport
//! addresses. Different mechanisms reach different peers: LAN multicast
//! finds devices on the same segment ([`lan::LanDiscovery`]); introducer
//! gossip surfaces peers learned transitively through trusted devices
//! ([`gossip::GossipDiscovery`]); an optional announce server is a
//! rendezvous directory for devices that have never met and sit on
//! different networks ([`announce::AnnounceDiscovery`], behind the
//! `announce` cargo feature); and a Kademlia/Mainline DHT is the
//! serverless equivalent of that rendezvous role
//! ([`dht::DhtDiscovery`], with the live `mainline`-backed node behind
//! the `dht` cargo feature). All implement the [`Discovery`] trait so the
//! rest of the engine depends only on the contract, never on a concrete
//! mechanism.
//!
//! [`DiscoveryService`] composes any number of [`Discovery`] sources. It
//! resolves them concurrently, deduplicates the union of their
//! candidates by `(address, kind)`, and orders the result by descending
//! RFC 8445 priority — the same priority arithmetic
//! ([`compute_priority`](crate::candidate::compute_priority)) the rest of
//! the connectivity stack uses, so the highest-ranked candidate a peer
//! offers sits first regardless of which source produced it.

pub mod announce;
pub mod dht;
pub mod gossip;
pub mod lan;

use std::collections::HashSet;
use std::net::SocketAddr;

use async_trait::async_trait;

use crate::candidate::{Candidate, CandidateKind};

// Preserve the established `cascade_p2p::discovery::*` surface: the LAN
// multicast free functions and types were previously defined directly in
// this module, and callers (the backend discovery loops, the engine, the
// connection manager) import them from here.
pub use lan::{Announcement, DISCOVERY_PORT, DiscoveredPeer, LanDiscovery, announce, listen};

pub use gossip::GossipDiscovery;

pub use announce::{AnnounceRequest, LookupResponse, MAX_ANNOUNCE_CANDIDATES, WireCandidate};

#[cfg(feature = "announce")]
pub use announce::AnnounceDiscovery;

pub use dht::{
    DHT_KEY_LEN, DHT_REPUBLISH_INTERVAL, DHT_RESOLVE_TIMEOUT, DhtDiscovery, DhtGetOutcome, DhtKey,
    DhtNode, StoredCandidates,
};

#[cfg(feature = "dht")]
pub use dht::{DEFAULT_DHT_BOOTSTRAP_NODES, MainlineDht};

/// A source that turns a peer's device ID into reachable candidates.
///
/// Implementations are composed behind [`DiscoveryService`]. The contract
/// is asynchronous and serialisable-data-only so the same source works
/// whether it talks to the local network, a peer book in memory, or (in
/// future) a remote discovery service across a transport.
#[async_trait]
pub trait Discovery: Send + Sync {
    /// Resolve `device_id` to the candidates this source can offer for
    /// it. Returns an empty vector when the source knows nothing about
    /// the device — absence is modelled as "no candidates", not an
    /// error, because a single source failing to find a peer is normal
    /// when other sources may still succeed.
    async fn resolve(&self, device_id: &str) -> Vec<Candidate>;

    /// Advertise this device's own `candidates` through the source's
    /// mechanism, identifying ourselves by `self_id`. Sources that have
    /// no announce step (their reachability is published elsewhere)
    /// implement this as a no-op.
    async fn announce(&self, self_id: &str, candidates: &[Candidate]);
}

/// Composes multiple [`Discovery`] sources into one resolver.
///
/// Resolution fans out across every registered source concurrently, then
/// merges the results: duplicate `(address, kind)` pairs collapse to the
/// first seen, and the surviving candidates are ordered by descending
/// priority. Announcing fans out to every source concurrently as well.
#[derive(Default)]
pub struct DiscoveryService {
    sources: Vec<Box<dyn Discovery>>,
}

impl std::fmt::Debug for DiscoveryService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscoveryService")
            .field("source_count", &self.sources.len())
            .finish()
    }
}

impl DiscoveryService {
    /// Create an empty service with no sources.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sources: Vec::new(),
        }
    }

    /// Register a discovery source. Sources are consulted in registration
    /// order, but because resolution runs concurrently and the merged
    /// output is priority-ordered, order only affects the tie-break for
    /// duplicate `(address, kind)` pairs (first registered wins).
    pub fn register(&mut self, source: Box<dyn Discovery>) {
        self.sources.push(source);
    }

    /// Number of registered sources.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sources.len()
    }

    /// Whether any source is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// Resolve `device_id` across every registered source concurrently,
    /// returning the deduplicated, priority-ordered union of their
    /// candidates.
    ///
    /// The concurrency mirrors the fan-out the backend already uses when
    /// bootstrapping peers: each source's `resolve` future is driven in
    /// parallel and their outputs joined. Deduplication is by
    /// `(address, kind)` so the same reachable endpoint reported by two
    /// sources counts once; ordering is by descending priority so the
    /// connectivity layer evaluates the strongest candidate first.
    pub async fn resolve(&self, device_id: &str) -> Vec<Candidate> {
        let per_source =
            futures_util::future::join_all(self.sources.iter().map(|s| s.resolve(device_id))).await;
        let combined: Vec<Candidate> = per_source.into_iter().flatten().collect();
        Self::dedup_and_order(combined)
    }

    /// Announce `candidates` for `self_id` across every source
    /// concurrently. Sources with no announce step no-op.
    pub async fn announce(&self, self_id: &str, candidates: &[Candidate]) {
        futures_util::future::join_all(
            self.sources.iter().map(|s| s.announce(self_id, candidates)),
        )
        .await;
    }

    /// Deduplicate by `(address, kind)` and order by descending priority.
    ///
    /// Matches the merge discipline used elsewhere in the engine for
    /// candidate sets: a stable descending-priority sort followed by a
    /// linear first-wins dedupe, so equal-priority duplicates collapse to
    /// the earliest-seen entry while the priority order is preserved.
    fn dedup_and_order(mut candidates: Vec<Candidate>) -> Vec<Candidate> {
        candidates.sort_by_key(|c| std::cmp::Reverse(c.priority));
        let mut seen: HashSet<(SocketAddr, CandidateKind)> = HashSet::new();
        candidates.retain(|c| seen.insert((c.address, c.kind)));
        candidates
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    /// A stub source that returns a fixed candidate list and records how
    /// many times it was resolved/announced — used to prove fan-out and
    /// merge behaviour without touching the network.
    struct StubSource {
        candidates: Vec<Candidate>,
        resolve_calls: Arc<AtomicUsize>,
        announce_calls: Arc<AtomicUsize>,
    }

    impl StubSource {
        fn new(candidates: Vec<Candidate>) -> Self {
            Self {
                candidates,
                resolve_calls: Arc::new(AtomicUsize::new(0)),
                announce_calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    #[async_trait]
    impl Discovery for StubSource {
        async fn resolve(&self, _device_id: &str) -> Vec<Candidate> {
            self.resolve_calls.fetch_add(1, Ordering::SeqCst);
            self.candidates.clone()
        }

        async fn announce(&self, _self_id: &str, _candidates: &[Candidate]) {
            self.announce_calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn empty_service_resolves_to_nothing() {
        let service = DiscoveryService::new();
        assert!(service.is_empty());
        assert!(service.resolve("DEVICE-A").await.is_empty());
    }

    #[tokio::test]
    async fn resolve_unions_candidates_from_all_sources() {
        let a = Candidate::new(addr(22000), CandidateKind::Host, 100);
        let b = Candidate::new(addr(22001), CandidateKind::ServerReflexive, 0);
        let mut service = DiscoveryService::new();
        service.register(Box::new(StubSource::new(vec![a])));
        service.register(Box::new(StubSource::new(vec![b])));
        assert_eq!(service.len(), 2);

        let resolved = service.resolve("DEVICE-A").await;
        assert_eq!(resolved.len(), 2);
        assert!(resolved.contains(&a));
        assert!(resolved.contains(&b));
    }

    #[tokio::test]
    async fn resolve_dedups_identical_address_and_kind() {
        // The same reachable endpoint reported by two sources must count
        // once. Both are host candidates at the same address, so the
        // priority is identical and the pair collapses.
        let shared = Candidate::new(addr(22000), CandidateKind::Host, 1024);
        let mut service = DiscoveryService::new();
        service.register(Box::new(StubSource::new(vec![shared])));
        service.register(Box::new(StubSource::new(vec![shared])));

        let resolved = service.resolve("DEVICE-A").await;
        assert_eq!(resolved, vec![shared]);
    }

    #[tokio::test]
    async fn resolve_keeps_same_address_with_different_kind() {
        // Same address but different kind is a distinct candidate: a host
        // address and a server-reflexive mapping to the same port reach
        // the peer through different paths and must both survive dedup.
        let host = Candidate::new(addr(22000), CandidateKind::Host, 0);
        let srflx = Candidate::new(addr(22000), CandidateKind::ServerReflexive, 0);
        let mut service = DiscoveryService::new();
        service.register(Box::new(StubSource::new(vec![host])));
        service.register(Box::new(StubSource::new(vec![srflx])));

        let resolved = service.resolve("DEVICE-A").await;
        assert_eq!(resolved.len(), 2);
    }

    #[tokio::test]
    async fn resolve_orders_by_descending_priority() {
        // Host outranks server-reflexive outranks relayed by RFC 8445
        // type preference, independent of registration order. Register
        // them low-to-high to prove the service re-orders.
        let relayed = Candidate::new(addr(22002), CandidateKind::Relayed, u16::MAX);
        let srflx = Candidate::new(addr(22001), CandidateKind::ServerReflexive, 0);
        let host = Candidate::new(addr(22000), CandidateKind::Host, 0);
        let mut service = DiscoveryService::new();
        service.register(Box::new(StubSource::new(vec![relayed])));
        service.register(Box::new(StubSource::new(vec![srflx])));
        service.register(Box::new(StubSource::new(vec![host])));

        let resolved = service.resolve("DEVICE-A").await;
        assert_eq!(resolved, vec![host, srflx, relayed]);
        // The ordering must follow the candidates' own priorities.
        assert!(resolved[0].priority >= resolved[1].priority);
        assert!(resolved[1].priority >= resolved[2].priority);
    }

    #[tokio::test]
    async fn resolve_consults_every_source() {
        let source_a = StubSource::new(vec![]);
        let source_b = StubSource::new(vec![]);
        let calls_a = source_a.resolve_calls.clone();
        let calls_b = source_b.resolve_calls.clone();
        let mut service = DiscoveryService::new();
        service.register(Box::new(source_a));
        service.register(Box::new(source_b));

        let _ = service.resolve("DEVICE-A").await;
        assert_eq!(calls_a.load(Ordering::SeqCst), 1);
        assert_eq!(calls_b.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn announce_fans_out_to_every_source() {
        let source_a = StubSource::new(vec![]);
        let source_b = StubSource::new(vec![]);
        let announce_a = source_a.announce_calls.clone();
        let announce_b = source_b.announce_calls.clone();
        let mut service = DiscoveryService::new();
        service.register(Box::new(source_a));
        service.register(Box::new(source_b));

        let candidate = Candidate::new(addr(22000), CandidateKind::Host, 0);
        service.announce("SELF", &[candidate]).await;
        assert_eq!(announce_a.load(Ordering::SeqCst), 1);
        assert_eq!(announce_b.load(Ordering::SeqCst), 1);
    }
}
