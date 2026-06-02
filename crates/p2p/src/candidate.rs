//! ICE-style candidate descriptions and priority arithmetic.
//!
//! A candidate is one reachable transport address advertised by a peer
//! to its remote counterpart before hole-punching begins. Each candidate
//! carries a kind (host, server-reflexive, relayed) and a priority
//! derived from RFC 8445 §5.1.2.1. Candidate pairs are scored per
//! RFC 8445 §5.7.2 so the two ends of a connection always evaluate the
//! same pair in the same order — that determinism is what lets a punch
//! attempt synchronise without an arbiter.
//!
//! This module is the wire-shape and arithmetic only. The probe loop,
//! the punch state machine and the network I/O live elsewhere.
//!
//! Sources:
//! - RFC 8445 — Interactive Connectivity Establishment.
//!   <https://datatracker.ietf.org/doc/html/rfc8445>

use std::net::SocketAddr;

use cascade_announce_wire::WireCandidate;

/// Kind of address a [`Candidate`] represents.
///
/// The three kinds correspond directly to the ICE taxonomy. `Host`
/// addresses come straight off a local network interface;
/// `ServerReflexive` addresses are discovered through STUN and reflect
/// the mapping a `NAT` has installed; `Relayed` addresses live on a
/// TURN-style relay and forward traffic on behalf of the originator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CandidateKind {
    /// A locally bound address (host candidate). Highest preference —
    /// no `NAT` in the path means the lowest latency.
    Host,
    /// A server-reflexive address obtained from a `STUN` server. Useful
    /// when the local side is behind a `NAT` but the mapping has been
    /// observed by a third party.
    ServerReflexive,
    /// A relayed address allocated on a `TURN`-style relay server. Last
    /// resort because every byte traverses the relay.
    Relayed,
}

impl CandidateKind {
    /// `type_preference` value as defined by RFC 8445 §5.1.2.2.
    ///
    /// The values follow the IANA-registered defaults: host = 126,
    /// server-reflexive = 100, relayed = 0. Wire-encoded as the
    /// `kind` tag in the protocol so a recipient can recompute the
    /// priority deterministically.
    #[must_use]
    pub const fn type_preference(self) -> u8 {
        match self {
            Self::Host => 126,
            Self::ServerReflexive => 100,
            Self::Relayed => 0,
        }
    }

    /// Wire-format tag — stable across releases. `0` = host, `1` =
    /// server-reflexive, `2` = relayed; matches the design doc's
    /// candidate-kind byte.
    #[must_use]
    pub const fn wire_tag(self) -> u8 {
        match self {
            Self::Host => 0,
            Self::ServerReflexive => 1,
            Self::Relayed => 2,
        }
    }

    /// Inverse of [`Self::wire_tag`]. Returns `None` for unknown tags
    /// so callers can reject malformed frames cleanly.
    #[must_use]
    pub const fn from_wire_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Host),
            1 => Some(Self::ServerReflexive),
            2 => Some(Self::Relayed),
            _ => None,
        }
    }
}

/// `component_id` slot for every candidate this crate produces.
///
/// Cascade carries exactly one ICE component (the data channel), so the
/// id is always `1`. RFC 8445 reserves `component_id = 1` for the data
/// component (`RTP` in WebRTC parlance, the only one we need).
pub const COMPONENT_ID: u8 = 1;

/// Compute the RFC 8445 §5.1.2.1 candidate priority.
///
/// ```text
/// priority = (2^24 * type_preference)
///          + (2^8  * local_preference)
///          + (2^0  * (256 - component_id))
/// ```
///
/// Overflow analysis:
/// - `type_preference` is bounded by 126 (host) per
///   [`CandidateKind::type_preference`], so the high term is at most
///   `126 * 2^24 = 2_113_929_216`, comfortably within `u32`.
/// - `local_preference` is `u16` so the middle term is at most
///   `65_535 * 256 = 16_776_960`.
/// - `(256 - component_id)` with `component_id = 1` is `255`.
/// - Sum maximum: `2_113_929_216 + 16_776_960 + 255 = 2_130_706_431`,
///   which fits in `u32` without truncation.
#[must_use]
pub const fn compute_priority(kind: CandidateKind, local_preference: u16) -> u32 {
    let type_pref = kind.type_preference() as u32;
    let local_pref = local_preference as u32;
    let component_term = 256u32 - COMPONENT_ID as u32;
    (type_pref << 24) + (local_pref << 8) + component_term
}

/// One advertised transport address with its kind and priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Candidate {
    /// Reachable address (`IPv4` or `IPv6`) plus port.
    pub address: SocketAddr,
    /// Address kind — host, server-reflexive or relayed.
    pub kind: CandidateKind,
    /// Precomputed priority. Set with [`compute_priority`] at gather
    /// time. Stored on the wire so the recipient need not re-derive
    /// (and so a misbehaving peer is forced to honour its own claimed
    /// priority for sort ordering).
    pub priority: u32,
}

impl Candidate {
    /// Convenience constructor that sets `priority` via
    /// [`compute_priority`] from the supplied `local_preference`.
    #[must_use]
    pub const fn new(address: SocketAddr, kind: CandidateKind, local_preference: u16) -> Self {
        Self {
            address,
            kind,
            priority: compute_priority(kind, local_preference),
        }
    }

    /// Compute the candidate-pair score per RFC 8445 §5.7.2.
    ///
    /// ```text
    /// pair_priority = (2^32 * MIN(G, D)) + (2 * MAX(G, D)) + (G > D ? 1 : 0)
    /// ```
    ///
    /// Where `G` is the controlling peer's candidate priority and `D`
    /// the controlled peer's. The caller is responsible for putting
    /// the correct candidate in each role — `self` is treated as the
    /// controlling side, `other` as the controlled side.
    #[must_use]
    pub const fn pairing_score(&self, other: &Self) -> u64 {
        let g = self.priority as u64;
        let d = other.priority as u64;
        let min = if g < d { g } else { d };
        let max = if g > d { g } else { d };
        let tiebreak = if g > d { 1 } else { 0 };
        (min << 32) + (max << 1) + tiebreak
    }

    /// Convert a [`WireCandidate`] back to an in-memory [`Candidate`].
    ///
    /// Returns `None` when the `kind` tag is not one of the three known values
    /// so a malformed or hostile directory entry is rejected rather than
    /// silently coerced. The stored `priority` is preserved exactly — the
    /// recipient honours the announcer's claimed priority, which the discovery
    /// merge is designed to tolerate. This is the inverse of the
    /// [`From<Candidate>`] projection; it lives here (not as an inherent method
    /// on `WireCandidate`) because `WireCandidate` is owned by the shared
    /// wasm-safe wire crate.
    #[must_use]
    pub fn from_wire(wire: WireCandidate) -> Option<Self> {
        let kind = CandidateKind::from_wire_tag(wire.kind)?;
        Some(Self {
            address: wire.address,
            kind,
            priority: wire.priority,
        })
    }
}

impl From<Candidate> for WireCandidate {
    fn from(candidate: Candidate) -> Self {
        Self {
            address: candidate.address,
            kind: candidate.kind.wire_tag(),
            priority: candidate.priority,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    #[test]
    fn type_preference_matches_rfc_8445_defaults() {
        assert_eq!(CandidateKind::Host.type_preference(), 126);
        assert_eq!(CandidateKind::ServerReflexive.type_preference(), 100);
        assert_eq!(CandidateKind::Relayed.type_preference(), 0);
    }

    #[test]
    fn wire_tag_round_trip_for_every_kind() {
        for kind in [
            CandidateKind::Host,
            CandidateKind::ServerReflexive,
            CandidateKind::Relayed,
        ] {
            assert_eq!(CandidateKind::from_wire_tag(kind.wire_tag()), Some(kind));
        }
    }

    #[test]
    fn from_wire_tag_rejects_unknown() {
        assert_eq!(CandidateKind::from_wire_tag(3), None);
        assert_eq!(CandidateKind::from_wire_tag(255), None);
    }

    #[test]
    fn priority_host_max_local_preference_matches_formula() {
        // Worked example: host candidate with the top local preference
        // (highest-priority interface). From the formula:
        //   126 << 24 = 2_113_929_216
        //   65_535 << 8 = 16_776_960
        //   256 - 1 = 255
        //   total = 2_130_706_431
        let priority = compute_priority(CandidateKind::Host, u16::MAX);
        assert_eq!(priority, 2_130_706_431);
    }

    #[test]
    fn priority_server_reflexive_zero_local_preference() {
        // srflx, no local preference:
        //   100 << 24 = 1_677_721_600
        //   0 << 8 = 0
        //   256 - 1 = 255
        //   total = 1_677_721_855
        let priority = compute_priority(CandidateKind::ServerReflexive, 0);
        assert_eq!(priority, 1_677_721_855);
    }

    #[test]
    fn priority_relayed_min() {
        // relayed, lowest local preference:
        //   0 << 24 = 0
        //   0 << 8 = 0
        //   256 - 1 = 255
        let priority = compute_priority(CandidateKind::Relayed, 0);
        assert_eq!(priority, 255);
    }

    #[test]
    fn priority_strict_ordering_host_over_srflx_over_relayed() {
        // Independently of the local preference, type preference must
        // dominate the ordering: a host candidate at local_preference=0
        // must outrank an srflx candidate at local_preference=u16::MAX,
        // and an srflx at 0 must outrank a relayed at u16::MAX.
        let host_low = compute_priority(CandidateKind::Host, 0);
        let srflx_high = compute_priority(CandidateKind::ServerReflexive, u16::MAX);
        let relayed_high = compute_priority(CandidateKind::Relayed, u16::MAX);
        assert!(host_low > srflx_high, "host must outrank srflx");
        assert!(srflx_high > relayed_high, "srflx must outrank relayed");
    }

    #[test]
    fn candidate_new_sets_priority_from_kind_and_local_preference() {
        let c = Candidate::new(addr(22000), CandidateKind::Host, 65_535);
        assert_eq!(c.priority, compute_priority(CandidateKind::Host, 65_535));
        assert_eq!(c.kind, CandidateKind::Host);
        assert_eq!(c.address, addr(22000));
    }

    #[test]
    fn pairing_score_obeys_rfc_8445_5_7_2_formula() {
        // RFC 8445 §5.7.2: pair_priority = 2^32 * MIN(G,D) + 2 * MAX(G,D) + (G>D ? 1 : 0).
        let controlling = Candidate::new(addr(22001), CandidateKind::Host, 65_535);
        let controlled = Candidate::new(addr(22002), CandidateKind::ServerReflexive, 0);
        let g = u64::from(controlling.priority);
        let d = u64::from(controlled.priority);
        let expected = (g.min(d) << 32) + (g.max(d) << 1) + u64::from(g > d);
        assert_eq!(controlling.pairing_score(&controlled), expected);
    }

    #[test]
    fn pairing_score_handles_equal_priorities() {
        // When G == D the tiebreak bit is 0 and the formula collapses
        // to `(p << 32) + (p << 1)`.
        let a = Candidate::new(addr(22001), CandidateKind::Host, 1024);
        let b = Candidate::new(addr(22002), CandidateKind::Host, 1024);
        assert_eq!(a.priority, b.priority);
        let p = u64::from(a.priority);
        assert_eq!(a.pairing_score(&b), (p << 32) + (p << 1));
    }

    #[test]
    fn wire_candidate_round_trips_every_kind() {
        for kind in [
            CandidateKind::Host,
            CandidateKind::ServerReflexive,
            CandidateKind::Relayed,
        ] {
            let candidate = Candidate::new(addr(22000), kind, 1024);
            let wire = WireCandidate::from(candidate);
            assert_eq!(Candidate::from_wire(wire), Some(candidate));
        }
    }

    #[test]
    fn wire_candidate_preserves_priority_exactly() {
        let candidate = Candidate::new(addr(33000), CandidateKind::ServerReflexive, u16::MAX);
        let wire = WireCandidate::from(candidate);
        assert_eq!(wire.priority, candidate.priority);
        assert_eq!(
            Candidate::from_wire(wire).map(|c| c.priority),
            Some(candidate.priority)
        );
    }

    #[test]
    fn from_wire_rejects_unknown_kind_tag() {
        let wire = WireCandidate {
            address: addr(22000),
            kind: 3,
            priority: 0,
        };
        assert_eq!(Candidate::from_wire(wire), None);
    }

    #[test]
    fn pairing_score_is_symmetric_in_min_max_but_tiebreak_swaps() {
        // Symmetric in the MIN/MAX terms — swapping G and D leaves
        // those two terms identical. Only the +1 tiebreak differs:
        // it's 1 when the *first argument* is the higher priority.
        let high = Candidate::new(addr(1), CandidateKind::Host, u16::MAX);
        let low = Candidate::new(addr(2), CandidateKind::Relayed, 0);
        let high_first = high.pairing_score(&low);
        let low_first = low.pairing_score(&high);
        assert_eq!(high_first, low_first + 1);
    }
}
