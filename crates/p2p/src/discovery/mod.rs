//! Pluggable peer discovery.
//!
//! Discovery is the act of turning a peer's device ID into a set of
//! reachable [`Candidate`] transport
//! addresses. Different mechanisms reach different peers: LAN multicast
//! finds devices on the same segment ([`lan::LanDiscovery`]); introducer
//! gossip surfaces peers learned transitively through trusted devices
//! ([`gossip::GossipDiscovery`]). Both implement the [`Discovery`] trait
//! so the rest of the engine depends only on the contract, never on a
//! concrete mechanism.
//!
//! [`DiscoveryService`] composes any number of [`Discovery`] sources. It
//! resolves them concurrently, deduplicates the union of their
//! candidates by `(address, kind)`, and orders the result by descending
//! RFC 8445 priority — the same priority arithmetic
//! ([`compute_priority`](crate::candidate::compute_priority)) the rest of
//! the connectivity stack uses, so the highest-ranked candidate a peer
//! offers sits first regardless of which source produced it.

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
