//! Connectivity strategy selection for `NAT` traversal.
//!
//! Given the local and remote `NAT` types plus the remote's advertised
//! candidates, [`decide_connectivity`] picks one of three strategies
//! described in `docs/nat-hole-punching.md`:
//!
//! 1. `Direct` — at least one side is `Open` (no `NAT`). Dial the
//!    highest-priority remote candidate.
//! 2. `HolePunch` — both sides are punchable, or one side is
//!    `FullCone` paired with a `Symmetric` peer. Run a synchronised
//!    probe burst against the remote candidate set.
//! 3. `Relay` — at least one side is `Symmetric` and the partner is
//!    not `FullCone`. Tunnel traffic through a known relay endpoint.
//!
//! This module declares only the pure decision function. The probe
//! loop, candidate gathering and reconnect machinery land in a follow-up
//! round (see `run_hole_punch` placeholder in `TODO(nat)` below).
//!
//! Sources:
//! - RFC 4787 — `NAT` Behavioral Requirements for Unicast UDP.
//!   <https://datatracker.ietf.org/doc/html/rfc4787>
//! - RFC 5780 — `NAT` Behavior Discovery Using STUN.
//!   <https://datatracker.ietf.org/doc/html/rfc5780>

use std::net::SocketAddr;

use crate::candidate::Candidate;

/// Detected `NAT` classification for one peer.
///
/// Matches the four-way RFC 4787 split, plus `Open` for hosts on a
/// public address and `Unknown` for detection failures where no
/// classification is available. Distinct from
/// [`crate::nat::NatType`] — the existing enum in `nat.rs` predates
/// this module and reports only `Public` / `Symmetric` from the
/// current single-server STUN probe. The two will be reconciled when
/// the RFC 5780 two-server detection lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NatType {
    /// Host is directly reachable on a public address (no `NAT`).
    Open,
    /// Full-cone `NAT` — mapping is endpoint-independent and
    /// filtering accepts any source once a mapping exists.
    FullCone,
    /// Address-restricted cone `NAT` — mapping is
    /// endpoint-independent but filtering requires the local side to
    /// have first sent to the remote `IP`.
    RestrictedCone,
    /// Port-restricted cone `NAT` — both mapping and filtering are
    /// address-and-port-dependent; less permissive than `FullCone`
    /// but still punchable.
    PortRestrictedCone,
    /// Symmetric `NAT` — mapping changes per destination. Cannot
    /// hole-punch reliably except against a `FullCone` partner.
    Symmetric,
    /// Detection failed or has not yet run. Treated conservatively as
    /// requiring relay.
    Unknown,
}

/// The chosen path for connecting to a remote peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectivityStrategy {
    /// Dial the remote directly at the given address. Used when at
    /// least one side is on a public address; the dialer is whichever
    /// side is `NAT`-ed (or either side if both are `Open`).
    Direct {
        /// Target address — the highest-priority candidate the remote
        /// advertised. Falls back to the lowest non-`Relayed`
        /// candidate when no `Host` candidate is present.
        addr: SocketAddr,
    },
    /// Run a synchronised hole-punch burst against every advertised
    /// remote candidate, then settle on the first pair that succeeds.
    HolePunch {
        /// Every reachable address the remote advertised. The caller
        /// pairs each against the local candidate set and orders
        /// pairs by `Candidate::pairing_score`.
        remote_candidates: Vec<Candidate>,
    },
    /// Tunnel traffic through a known relay endpoint. Selected when
    /// either side is `Symmetric` and the partner is not `FullCone`,
    /// or when one side's `NAT` type is `Unknown`.
    Relay {
        /// Relay endpoint chosen from the caller-provided pool. The
        /// first reachable entry wins; the next round will replace
        /// this with a latency-aware selector.
        relay: SocketAddr,
    },
}

/// Decide the best connectivity strategy for a peer.
///
/// The decision table follows `docs/nat-hole-punching.md`:
///
/// |  | `Open` | `FullCone` | `RestrictedCone` | `PortRestrictedCone` | `Symmetric` | `Unknown` |
/// |---|---|---|---|---|---|---|
/// | `Open` | Direct | Direct | Direct | Direct | Direct | Direct |
/// | `FullCone` | Direct | Punch | Punch | Punch | Punch | Relay |
/// | `RestrictedCone` | Direct | Punch | Punch | Punch | Relay | Relay |
/// | `PortRestrictedCone` | Direct | Punch | Punch | Punch | Relay | Relay |
/// | `Symmetric` | Direct | Punch | Relay | Relay | Relay | Relay |
/// | `Unknown` | Direct | Relay | Relay | Relay | Relay | Relay |
///
/// When the table calls for `Direct` but no remote candidates are
/// available, the caller cannot dial anywhere — the function falls
/// back to `Relay` if a relay is known, otherwise `HolePunch` with an
/// empty candidate set (the state machine will retry and ultimately
/// give up).
///
/// When the table calls for `Relay` but no relay is configured, the
/// function falls back to `HolePunch`. This best-effort path matches
/// libp2p's behaviour: a doomed punch is more useful than refusing to
/// connect at all, because the next round of STUN detection might
/// reclassify one side.
#[must_use]
pub fn decide_connectivity(
    local: NatType,
    remote: NatType,
    remote_candidates: &[Candidate],
    known_relays: &[SocketAddr],
) -> ConnectivityStrategy {
    // Either side being `Open` short-circuits to `Direct`. The dialer
    // picks the highest-priority remote candidate so a host address
    // beats a server-reflexive one when both are advertised.
    if matches!(local, NatType::Open) || matches!(remote, NatType::Open) {
        if let Some(addr) = highest_priority_addr(remote_candidates) {
            return ConnectivityStrategy::Direct { addr };
        }
        // Direct is the table's first choice but we have no remote
        // address to dial. Try relay; if no relay either, fall through
        // to an empty hole-punch attempt that the state machine will
        // surface as unreachable.
        return relay_or_punch(remote_candidates, known_relays);
    }

    if is_punchable(local, remote) {
        return ConnectivityStrategy::HolePunch {
            remote_candidates: remote_candidates.to_vec(),
        };
    }
    // Everything else (including `Unknown` on either side and
    // symmetric paired with restricted/port-restricted) goes to
    // relay. `Unknown` is conservative — the next STUN refresh
    // can promote the connection.
    relay_or_punch(remote_candidates, known_relays)
}

/// `true` when the pair can hole-punch directly.
///
/// Both sides being a cone of any flavour is always punchable
/// (`FullCone` / `RestrictedCone` / `PortRestrictedCone` cross-product).
/// Mixed cone/symmetric is punchable only when the cone side is
/// `FullCone` — the full-cone mapping survives the symmetric side's
/// destination-dependent rewriting. Every other combination, including
/// `Unknown` on either side, returns `false`.
const fn is_punchable(local: NatType, remote: NatType) -> bool {
    let local_is_cone = matches!(
        local,
        NatType::FullCone | NatType::RestrictedCone | NatType::PortRestrictedCone,
    );
    let remote_is_cone = matches!(
        remote,
        NatType::FullCone | NatType::RestrictedCone | NatType::PortRestrictedCone,
    );
    if local_is_cone && remote_is_cone {
        return true;
    }
    matches!(
        (local, remote),
        (NatType::FullCone, NatType::Symmetric) | (NatType::Symmetric, NatType::FullCone),
    )
}

// TODO(nat): add `run_hole_punch(local: &[Candidate], remote: &[Candidate],
// signal: &mut SignalChannel) -> Result<SocketAddr>` in the next round.
// It will own the probe-burst state machine described in
// `docs/nat-hole-punching.md` §"Hole-punching protocol".

fn highest_priority_addr(candidates: &[Candidate]) -> Option<SocketAddr> {
    candidates
        .iter()
        .max_by_key(|c| c.priority)
        .map(|c| c.address)
}

fn relay_or_punch(
    remote_candidates: &[Candidate],
    known_relays: &[SocketAddr],
) -> ConnectivityStrategy {
    known_relays.first().map_or_else(
        || ConnectivityStrategy::HolePunch {
            remote_candidates: remote_candidates.to_vec(),
        },
        |relay| ConnectivityStrategy::Relay { relay: *relay },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::{Candidate, CandidateKind};
    use std::net::{IpAddr, Ipv4Addr};

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), port)
    }

    fn host_candidate(port: u16) -> Candidate {
        Candidate::new(addr(port), CandidateKind::Host, u16::MAX)
    }

    fn srflx_candidate(port: u16) -> Candidate {
        Candidate::new(addr(port), CandidateKind::ServerReflexive, 0)
    }

    /// All `NatType` variants for exhaustive table coverage. Order
    /// matters only for readability of failure messages — the tests
    /// assert every (local, remote) pair explicitly.
    const ALL_NAT_TYPES: [NatType; 6] = [
        NatType::Open,
        NatType::FullCone,
        NatType::RestrictedCone,
        NatType::PortRestrictedCone,
        NatType::Symmetric,
        NatType::Unknown,
    ];

    /// Reference table from `docs/nat-hole-punching.md` and the doc
    /// comment on `decide_connectivity`. `Direct` paths are written
    /// out explicitly so an editor can review every cell.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Expected {
        Direct,
        Punch,
        Relay,
    }

    const TABLE: [[Expected; 6]; 6] = [
        // local = Open
        [
            Expected::Direct,
            Expected::Direct,
            Expected::Direct,
            Expected::Direct,
            Expected::Direct,
            Expected::Direct,
        ],
        // local = FullCone
        [
            Expected::Direct,
            Expected::Punch,
            Expected::Punch,
            Expected::Punch,
            Expected::Punch,
            Expected::Relay,
        ],
        // local = RestrictedCone
        [
            Expected::Direct,
            Expected::Punch,
            Expected::Punch,
            Expected::Punch,
            Expected::Relay,
            Expected::Relay,
        ],
        // local = PortRestrictedCone
        [
            Expected::Direct,
            Expected::Punch,
            Expected::Punch,
            Expected::Punch,
            Expected::Relay,
            Expected::Relay,
        ],
        // local = Symmetric
        [
            Expected::Direct,
            Expected::Punch,
            Expected::Relay,
            Expected::Relay,
            Expected::Relay,
            Expected::Relay,
        ],
        // local = Unknown
        [
            Expected::Direct,
            Expected::Relay,
            Expected::Relay,
            Expected::Relay,
            Expected::Relay,
            Expected::Relay,
        ],
    ];

    #[test]
    fn decision_table_covers_every_pair() {
        let candidates = vec![host_candidate(22_000)];
        let relays = vec![addr(3478)];

        for (i, local) in ALL_NAT_TYPES.iter().enumerate() {
            for (j, remote) in ALL_NAT_TYPES.iter().enumerate() {
                let expected = TABLE
                    .get(i)
                    .and_then(|row| row.get(j))
                    .copied()
                    .unwrap_or(Expected::Relay);
                let got = decide_connectivity(*local, *remote, &candidates, &relays);
                let actual = match got {
                    ConnectivityStrategy::Direct { .. } => Expected::Direct,
                    ConnectivityStrategy::HolePunch { .. } => Expected::Punch,
                    ConnectivityStrategy::Relay { .. } => Expected::Relay,
                };
                assert_eq!(
                    actual, expected,
                    "({local:?} ↔ {remote:?}) expected {expected:?}, got {actual:?}",
                );
            }
        }
    }

    #[test]
    fn direct_picks_highest_priority_remote_candidate() {
        // Two candidates: a host (priority dominated by type_pref=126)
        // and an srflx (type_pref=100). Host must win.
        let host = host_candidate(22_000);
        let srflx = srflx_candidate(54_321);
        let candidates = vec![srflx, host];
        let strategy = decide_connectivity(NatType::Open, NatType::Open, &candidates, &[]);
        match strategy {
            ConnectivityStrategy::Direct { addr } => assert_eq!(addr, addr_for(22_000)),
            other => panic!("expected Direct, got {other:?}"),
        }
    }

    #[test]
    fn direct_with_no_candidates_falls_back_to_relay_when_available() {
        let relay_addr = addr(3478);
        let strategy = decide_connectivity(NatType::Open, NatType::Symmetric, &[], &[relay_addr]);
        assert_eq!(strategy, ConnectivityStrategy::Relay { relay: relay_addr });
    }

    #[test]
    fn direct_with_no_candidates_and_no_relays_falls_through_to_empty_punch() {
        let strategy = decide_connectivity(NatType::Open, NatType::Symmetric, &[], &[]);
        assert_eq!(
            strategy,
            ConnectivityStrategy::HolePunch {
                remote_candidates: vec![]
            }
        );
    }

    #[test]
    fn hole_punch_preserves_candidate_order() {
        // The state machine is responsible for sorting by pair score;
        // `decide_connectivity` must not reshuffle the wire order.
        let a = host_candidate(22_000);
        let b = srflx_candidate(54_321);
        let c = host_candidate(22_001);
        let candidates = vec![a, b, c];
        let strategy = decide_connectivity(
            NatType::FullCone,
            NatType::PortRestrictedCone,
            &candidates,
            &[],
        );
        match strategy {
            ConnectivityStrategy::HolePunch { remote_candidates } => {
                assert_eq!(remote_candidates, candidates);
            }
            other => panic!("expected HolePunch, got {other:?}"),
        }
    }

    #[test]
    fn relay_picks_first_known_relay() {
        let relays = vec![addr(3478), addr(3479)];
        let strategy =
            decide_connectivity(NatType::Symmetric, NatType::RestrictedCone, &[], &relays);
        assert_eq!(strategy, ConnectivityStrategy::Relay { relay: addr(3478) });
    }

    #[test]
    fn relay_needed_but_none_known_falls_back_to_punch() {
        // Symmetric/PortRestrictedCone requires relay per the table,
        // but the caller has no relay. The function falls back to
        // punch — the next STUN refresh might reclassify one side, so
        // a best-effort attempt is preferable to silently refusing.
        let candidates = vec![srflx_candidate(54_321)];
        let strategy = decide_connectivity(
            NatType::Symmetric,
            NatType::PortRestrictedCone,
            &candidates,
            &[],
        );
        assert_eq!(
            strategy,
            ConnectivityStrategy::HolePunch {
                remote_candidates: candidates,
            }
        );
    }

    #[test]
    fn full_cone_symmetric_punches_either_direction() {
        // The table makes this pair symmetric: punch either way.
        let strategy_a =
            decide_connectivity(NatType::FullCone, NatType::Symmetric, &[], &[addr(3478)]);
        let strategy_b =
            decide_connectivity(NatType::Symmetric, NatType::FullCone, &[], &[addr(3478)]);
        assert!(matches!(strategy_a, ConnectivityStrategy::HolePunch { .. }));
        assert!(matches!(strategy_b, ConnectivityStrategy::HolePunch { .. }));
    }

    #[test]
    fn unknown_pessimistically_routes_through_relay() {
        // `Unknown` on either side must take the relay path (or fall
        // back to punch if no relay) — never `Direct`, never an
        // optimistic punch.
        let relay_addr = addr(3478);
        for partner in [
            NatType::FullCone,
            NatType::RestrictedCone,
            NatType::PortRestrictedCone,
            NatType::Symmetric,
        ] {
            let strategy = decide_connectivity(NatType::Unknown, partner, &[], &[relay_addr]);
            assert_eq!(
                strategy,
                ConnectivityStrategy::Relay { relay: relay_addr },
                "(Unknown ↔ {partner:?}) must relay",
            );
        }
    }

    fn addr_for(port: u16) -> SocketAddr {
        addr(port)
    }
}
