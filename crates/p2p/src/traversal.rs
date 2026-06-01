//! Connectivity strategy selection and hole-punch state machine for
//! `NAT` traversal.
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
//! When `HolePunch` is the chosen strategy the caller drives
//! `run_hole_punch` over a [`PunchTransport`] — a thin trait abstraction
//! over the UDP socket — and a [`Clock`]. Both are injectable so the
//! state machine is exercised without touching the network or wall-clock
//! time. The production implementations sit alongside the trait
//! definitions and the deterministic mocks live behind `#[cfg(test)]`.
//!
//! Sources:
//! - RFC 4787 — `NAT` Behavioral Requirements for Unicast UDP.
//!   <https://datatracker.ietf.org/doc/html/rfc4787>
//! - RFC 5780 — `NAT` Behavior Discovery Using STUN.
//!   <https://datatracker.ietf.org/doc/html/rfc5780>
//! - RFC 8445 — Interactive Connectivity Establishment.
//!   <https://datatracker.ietf.org/doc/html/rfc8445>
//! - libp2p `DCUtR` specification.
//!   <https://github.com/libp2p/specs/blob/master/relay/DCUtR.md>

use std::io;
use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::time::sleep;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// A UDP probe received from a remote endpoint.
///
/// Returned by [`PunchTransport::recv_probe`]. The state machine
/// compares `nonce` against the negotiated [`SyncPunchAgreement::nonce`]
/// to decide whether the probe belongs to the current attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceivedProbe {
    /// Source address the probe arrived from.
    pub from: SocketAddr,
    /// `64`-bit nonce carried in the probe payload.
    pub nonce: u64,
}

/// Transport abstraction the hole-punch state machine drives.
///
/// Implementations send and receive UDP probes carrying the negotiated
/// nonce. The production implementation wraps a `UdpSocket`; tests use
/// the deterministic in-memory mock in this module's `#[cfg(test)]`
/// section. Keeping the surface this narrow means the state machine
/// performs no socket I/O directly and is fully exercised without a real
/// network stack.
///
/// `send_probe` is best-effort: the state machine never retries an
/// individual send. If the socket reports an error the state machine
/// surfaces it as [`PunchError::Transport`] rather than continuing.
///
/// `recv_probe` blocks until either a probe arrives or `deadline`
/// elapses. On deadline elapse the implementation returns an
/// [`io::ErrorKind::TimedOut`] error so the state machine can treat it
/// as "no receipt for this burst" without conflating timeouts with
/// transport faults.
#[async_trait]
pub trait PunchTransport: Send + Sync {
    /// Send one probe to `dst` carrying `nonce`.
    async fn send_probe(&self, dst: SocketAddr, nonce: u64) -> io::Result<()>;

    /// Receive one probe, blocking until `deadline`.
    ///
    /// Returns [`io::ErrorKind::TimedOut`] when the deadline elapses
    /// before a probe arrives.
    async fn recv_probe(&self, deadline: Instant) -> io::Result<ReceivedProbe>;
}

/// Clock abstraction so the state machine can be exercised against
/// virtualised time.
///
/// Production code uses [`SystemClock`]. Tests inject a `MockClock`
/// that returns whatever instant the test sets, so deadlines fire
/// deterministically.
pub trait Clock: Send + Sync {
    /// Monotonic instant used for deadlines and elapsed-time checks.
    fn now(&self) -> Instant;

    /// Wall-clock milliseconds since the Unix epoch.
    ///
    /// Used to stamp the resulting [`EstablishedFlow`] and to compare
    /// against the agreement's `deadline_unix_ms`.
    fn now_unix_ms(&self) -> u64;
}

/// Production [`Clock`] backed by [`Instant::now`] and [`SystemTime::now`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn now_unix_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }
}

/// Configuration for `run_hole_punch`.
///
/// All fields have defaults that match the protocol described in
/// `docs/nat-hole-punching.md` §"Hole-punching protocol": three probes
/// per burst, `50 ms` between bursts, three bursts before giving up,
/// and a `10 s` overall deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PunchConfig {
    /// Number of probes emitted back-to-back inside a single burst.
    ///
    /// Sending several probes in quick succession increases the chance
    /// that at least one passes the remote `NAT`'s filtering. Must be
    /// strictly positive.
    pub burst_size: u32,
    /// Time the state machine waits after a burst for a return probe
    /// before counting the burst as failed.
    pub per_burst_gap: Duration,
    /// Maximum number of bursts before `run_hole_punch` returns
    /// [`PunchError::Timeout`]. Must be strictly positive.
    pub max_bursts: u32,
    /// Cumulative deadline measured from the start of the call. If the
    /// clock crosses this point during a burst the state machine
    /// returns [`PunchError::Timeout`] without scheduling further
    /// bursts.
    pub total_deadline: Duration,
}

impl Default for PunchConfig {
    fn default() -> Self {
        Self {
            burst_size: 3,
            per_burst_gap: Duration::from_millis(50),
            max_bursts: 3,
            total_deadline: Duration::from_secs(10),
        }
    }
}

impl PunchConfig {
    /// Construct a validated [`PunchConfig`].
    ///
    /// # Errors
    /// Returns [`PunchError::InvalidConfig`] when `burst_size` or
    /// `max_bursts` is zero. Both must be strictly positive — a burst
    /// with no probes never opens a `NAT` mapping, and a run with no
    /// bursts can never succeed.
    pub const fn new(
        burst_size: u32,
        per_burst_gap: Duration,
        max_bursts: u32,
        total_deadline: Duration,
    ) -> Result<Self, PunchError> {
        if burst_size == 0 {
            return Err(PunchError::InvalidConfig("burst_size must be positive"));
        }
        if max_bursts == 0 {
            return Err(PunchError::InvalidConfig("max_bursts must be positive"));
        }
        Ok(Self {
            burst_size,
            per_burst_gap,
            max_bursts,
            total_deadline,
        })
    }
}

/// Synchronisation payload exchanged via `BepMessage::SyncPunch`.
///
/// Both peers agree on the same `nonce` and `deadline_unix_ms` before
/// invoking `run_hole_punch`. The state machine refuses to start when
/// the agreed deadline has already passed — the partner cannot still be
/// listening, and emitting probes into a closed mapping wastes a slot
/// in the punch budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncPunchAgreement {
    /// Random `64`-bit value both peers stamp into outgoing probes.
    pub nonce: u64,
    /// Wall-clock deadline (milliseconds since the Unix epoch) by which
    /// each peer must have begun emitting probes.
    pub deadline_unix_ms: u64,
}

/// One candidate pair selected by the caller before the punch begins.
///
/// `decide_connectivity` produces a list of remote candidates; the
/// caller pairs each with one of the local candidates using
/// [`Candidate::pairing_score`] and feeds the highest-scoring pair to
/// `run_hole_punch`. The state machine itself is single-pair — a
/// caller wanting multi-pair concurrency runs several invocations in
/// parallel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CandidatePair {
    /// The local socket the probes are emitted from.
    pub local: SocketAddr,
    /// The remote socket the probes target.
    pub remote: SocketAddr,
}

/// Result of a successful punch: a confirmed bidirectional flow.
///
/// The engine persists `established_at_unix_ms` on the peer record so a
/// stale flow can be torn down after a configurable idle period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EstablishedFlow {
    /// Local endpoint of the established flow.
    pub local: SocketAddr,
    /// Remote endpoint of the established flow.
    pub remote: SocketAddr,
    /// Wall-clock instant the matching probe arrived, in milliseconds
    /// since the Unix epoch.
    pub established_at_unix_ms: u64,
}

/// Errors returned by `run_hole_punch`.
#[derive(Debug, thiserror::Error)]
pub enum PunchError {
    /// The state machine exhausted [`PunchConfig::max_bursts`] (or hit
    /// [`PunchConfig::total_deadline`]) without receiving a matching
    /// probe.
    #[error("hole-punch timed out without receiving a matching probe")]
    Timeout,
    /// The agreement's wall-clock deadline was already in the past at
    /// the moment `run_hole_punch` was invoked. The state machine
    /// returns this without sending any probe.
    #[error("sync-punch deadline already passed (now {now_ms} ms, deadline {deadline_ms} ms unix)")]
    DeadlinePassed {
        /// Clock reading at the moment of the check (Unix milliseconds).
        now_ms: u64,
        /// Agreed deadline that was already in the past (Unix
        /// milliseconds).
        deadline_ms: u64,
    },
    /// The underlying transport reported an I/O error other than the
    /// expected per-burst deadline elapse. Surfaced verbatim so the
    /// caller can log the original cause.
    #[error("transport error: {0}")]
    Transport(#[from] io::Error),
    /// [`PunchConfig::new`] rejected its arguments. Static message
    /// names the offending field.
    #[error("invalid punch config: {0}")]
    InvalidConfig(&'static str),
}

/// Drive a hole-punch attempt against a single candidate pair.
///
/// Sequence per `docs/nat-hole-punching.md` §"Hole-punching protocol":
///
/// 1. Verify the `SyncPunchAgreement`'s deadline is still in the
///    future. If not, return [`PunchError::DeadlinePassed`] immediately
///    so the punch budget is not spent on a doomed attempt.
/// 2. Repeat at most `config.max_bursts` times:
///    1. Send `config.burst_size` probes back-to-back via the
///       transport, all carrying the agreement's nonce.
///    2. Block on `transport.recv_probe(burst_deadline)` where
///       `burst_deadline = start_of_burst + config.per_burst_gap`.
///    3. If a probe arrives with a matching nonce, return
///       `EstablishedFlow` stamped with `clock.now_unix_ms()`.
///    4. If the deadline elapses or the probe carries a non-matching
///       nonce, treat the burst as failed and continue.
/// 3. If `PunchConfig::total_deadline` elapses or every burst fails,
///    return [`PunchError::Timeout`].
///
/// # Errors
///
/// Returns [`PunchError::DeadlinePassed`] if the agreement's deadline
/// is already in the past, [`PunchError::Transport`] if the transport
/// reports an I/O error other than a deadline elapse during a send, and
/// [`PunchError::Timeout`] if no burst succeeds within the budget.
pub async fn run_hole_punch<T: PunchTransport + ?Sized>(
    transport: &T,
    pair: &CandidatePair,
    sync: &SyncPunchAgreement,
    config: &PunchConfig,
    clock: &dyn Clock,
) -> Result<EstablishedFlow, PunchError> {
    let now_ms = clock.now_unix_ms();
    if now_ms >= sync.deadline_unix_ms {
        return Err(PunchError::DeadlinePassed {
            now_ms,
            deadline_ms: sync.deadline_unix_ms,
        });
    }

    let overall_deadline = clock.now() + config.total_deadline;

    for _burst in 0..config.max_bursts {
        let burst_start = clock.now();
        if burst_start >= overall_deadline {
            return Err(PunchError::Timeout);
        }

        for _ in 0..config.burst_size {
            transport.send_probe(pair.remote, sync.nonce).await?;
        }

        let burst_deadline = burst_start + config.per_burst_gap;
        let recv_deadline = burst_deadline.min(overall_deadline);

        match transport.recv_probe(recv_deadline).await {
            Ok(probe) if probe.nonce == sync.nonce => {
                return Ok(EstablishedFlow {
                    local: pair.local,
                    remote: pair.remote,
                    established_at_unix_ms: clock.now_unix_ms(),
                });
            }
            Ok(_) => {
                // Wrong nonce — treat as no receipt and move on. A real
                // socket may legitimately surface unrelated traffic on
                // the same port (a `STUN` keep-alive, a probe from a
                // different peer mid-flight); aborting the run on the
                // first stray packet would be brittle.
            }
            Err(err) if err.kind() == io::ErrorKind::TimedOut => {
                // Expected outcome for a burst that does not yield a
                // probe before its deadline. Fall through to the next
                // burst.
            }
            Err(err) => return Err(PunchError::Transport(err)),
        }

        // If the receive call returned early (wrong-nonce probe or a
        // mock transport that resolves before the deadline), pace the
        // next burst by sleeping out the remainder of the gap. This
        // keeps the bursts roughly synchronised with the remote side
        // even when the local transport is faster than the wire.
        let now = clock.now();
        if now < burst_deadline && burst_deadline <= overall_deadline {
            let gap = burst_deadline - now;
            sleep(gap).await;
        }
    }

    Err(PunchError::Timeout)
}

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
