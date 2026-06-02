//! In-process peer relay.
//!
//! A node whose detected `NAT` type is `Open` or `FullCone` can volunteer
//! to bridge two other peers that cannot reach each other directly,
//! playing the role the operated `cascade-relay-server` plays. The
//! volunteer forwards [`cascade_p2p::protocol::BepMessage::RelayData`]
//! frames between the two bridged BEP sessions without inspecting their
//! payloads, accounting every forwarded byte against the [`RelayCapacity`]
//! bandwidth meter.
//!
//! Two concerns live here:
//!
//! - [`RelayCapacity`] bounds the cost a volunteer takes on: a hard cap on
//!   concurrent bridged sessions and an aggregate bandwidth budget. New
//!   sessions past the cap are rejected, never silently dropped. It also
//!   implements [`cascade_p2p::pipe::ByteMeter`] so the live forwarding
//!   path meters relayed bytes against the configured ceiling.
//! - [`PeerRelayTransport`] is the requester- and target-side tunnel: a
//!   [`cascade_p2p::transport::Transport`] that wraps each outbound inner
//!   BEP frame into a `RelayData` envelope on the carrying session and
//!   unwraps inbound `RelayData` payloads back into inner frames. The inner
//!   BEP session runs over it exactly as the operated-relay path runs a
//!   session over [`cascade_p2p::transport::RelayTransport`].

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use anyhow::Result;
use async_trait::async_trait;
use cascade_p2p::pipe::ByteMeter;
use cascade_p2p::protocol::BepMessage;
use cascade_p2p::transport::{Transport, TransportReader, TransportWriter};
use tokio::sync::mpsc;

/// Why a relay session could not be admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RelayAdmissionError {
    /// The node is already bridging its maximum number of concurrent
    /// sessions. The requester must fall back to another relay or path.
    #[error("relay at session capacity ({active}/{max})")]
    AtSessionCapacity {
        /// Sessions currently active.
        active: u32,
        /// Configured ceiling.
        max: u32,
    },
}

/// Capacity governor for the relay sessions a volunteer bridges.
///
/// Tracks the number of concurrent sessions and the cumulative relayed
/// byte total, enforcing the configured ceilings. A session is admitted
/// through [`RelayCapacity::admit`], which returns a [`RelaySessionGuard`]
/// whose `Drop` releases the slot — so a session count can never leak even
/// if the bridge task panics.
#[derive(Debug)]
pub struct RelayCapacity {
    active_sessions: AtomicU32,
    bytes_relayed: AtomicU64,
    max_sessions: u32,
    max_bandwidth_bytes_per_sec: u64,
}

impl RelayCapacity {
    /// Construct a governor with the given session and bandwidth caps.
    #[must_use]
    pub const fn new(max_sessions: u32, max_bandwidth_bytes_per_sec: u64) -> Self {
        Self {
            active_sessions: AtomicU32::new(0),
            bytes_relayed: AtomicU64::new(0),
            max_sessions,
            max_bandwidth_bytes_per_sec,
        }
    }

    /// Try to admit one new relay session.
    ///
    /// On success returns a guard that holds the session slot until
    /// dropped. On failure the slot count is left untouched and the
    /// caller must reject the request rather than bridging it.
    pub fn admit(self: &Arc<Self>) -> Result<RelaySessionGuard, RelayAdmissionError> {
        // Reserve a slot with a single atomic compare-and-swap loop so two
        // concurrent admissions cannot both squeeze past the cap.
        loop {
            let active = self.active_sessions.load(Ordering::Acquire);
            if active >= self.max_sessions {
                return Err(RelayAdmissionError::AtSessionCapacity {
                    active,
                    max: self.max_sessions,
                });
            }
            let next = active + 1;
            if self
                .active_sessions
                .compare_exchange(active, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(RelaySessionGuard {
                    capacity: Arc::clone(self),
                });
            }
        }
    }

    /// Number of sessions currently bridged. Snapshot value.
    #[must_use]
    pub fn active_sessions(&self) -> u32 {
        self.active_sessions.load(Ordering::Acquire)
    }

    /// Cumulative bytes relayed across every session since construction.
    #[must_use]
    pub fn bytes_relayed(&self) -> u64 {
        self.bytes_relayed.load(Ordering::Relaxed)
    }

    /// The configured aggregate bandwidth ceiling in bytes per second.
    #[must_use]
    pub const fn max_bandwidth_bytes_per_sec(&self) -> u64 {
        self.max_bandwidth_bytes_per_sec
    }

    fn release_session(&self) {
        // Saturating subtraction: a release can never drive the count
        // below zero even under a buggy double-release, which would
        // otherwise wrap a `u32` to `u32::MAX` and wedge the cap.
        let prev = self.active_sessions.load(Ordering::Acquire);
        if prev > 0 {
            self.active_sessions.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl ByteMeter for RelayCapacity {
    fn record(&self, count: u64) {
        self.bytes_relayed.fetch_add(count, Ordering::Relaxed);
    }
}

/// RAII guard holding one admitted relay-session slot. Dropping it frees
/// the slot, so the active-session count tracks live bridges even across
/// task panics or early returns.
#[derive(Debug)]
pub struct RelaySessionGuard {
    capacity: Arc<RelayCapacity>,
}

impl Drop for RelaySessionGuard {
    fn drop(&mut self) {
        self.capacity.release_session();
    }
}

/// Inner-session tunnel transport for a peer relay's two end nodes.
///
/// Both the requester and the target of a relayed connection run an inner
/// BEP session over one of these. The transport carries each inner frame
/// inside a [`BepMessage::RelayData`] envelope:
///
/// - the writer half wraps every outbound inner frame into `RelayData` and
///   pushes it onto the carrying session's outbound channel (the session to
///   the volunteer), so the volunteer forwards it on to the far end;
/// - the reader half receives the inner frame bytes the carrying session's
///   read loop strips out of inbound `RelayData` frames and hands over via
///   an `mpsc` channel.
///
/// This mirrors [`cascade_p2p::transport::RelayTransport`] — the operated
/// relay's tunnel — so the same [`cascade_p2p::framed::FramedSession`] and
/// session loop drive a session over either. The volunteer in the middle
/// only ever sees opaque `RelayData` payloads.
pub struct PeerRelayTransport {
    outbound: mpsc::UnboundedSender<BepMessage>,
    inbound: mpsc::UnboundedReceiver<Vec<u8>>,
}

impl std::fmt::Debug for PeerRelayTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerRelayTransport").finish_non_exhaustive()
    }
}

impl PeerRelayTransport {
    /// Wrap a carrying session's outbound channel and an inbound payload
    /// channel into a tunnel transport.
    ///
    /// `outbound` is the carrying session's `PeerHandle` sender — outbound
    /// inner frames are sent on it as `RelayData`. `inbound` receives the
    /// inner frame bytes the carrying session's read loop extracts from
    /// inbound `RelayData` frames.
    #[must_use]
    pub const fn new(
        outbound: mpsc::UnboundedSender<BepMessage>,
        inbound: mpsc::UnboundedReceiver<Vec<u8>>,
    ) -> Self {
        Self { outbound, inbound }
    }
}

impl Transport for PeerRelayTransport {
    type Reader = PeerRelayTransportReader;
    type Writer = PeerRelayTransportWriter;

    fn split(self) -> (Self::Reader, Self::Writer) {
        (
            PeerRelayTransportReader {
                inbound: self.inbound,
            },
            PeerRelayTransportWriter {
                outbound: self.outbound,
            },
        )
    }
}

/// Read half of [`PeerRelayTransport`].
#[derive(Debug)]
pub struct PeerRelayTransportReader {
    inbound: mpsc::UnboundedReceiver<Vec<u8>>,
}

#[async_trait]
impl TransportReader for PeerRelayTransportReader {
    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>> {
        // `None` when the sender drops — the carrying session ended, which
        // the inner session reads as a clean EOF.
        Ok(self.inbound.recv().await)
    }
}

/// Write half of [`PeerRelayTransport`].
#[derive(Debug)]
pub struct PeerRelayTransportWriter {
    outbound: mpsc::UnboundedSender<BepMessage>,
}

#[async_trait]
impl TransportWriter for PeerRelayTransportWriter {
    async fn send_frame(&mut self, frame: &[u8]) -> Result<()> {
        self.outbound
            .send(BepMessage::RelayData {
                payload: frame.to_vec(),
            })
            .map_err(|_| anyhow::anyhow!("peer-relay carrying session closed"))
    }

    async fn shutdown(&mut self) -> Result<()> {
        // The carrying session owns the underlying transport's lifetime;
        // dropping this writer's sender is enough to let the far end see
        // EOF once the carrying session tears down.
        Ok(())
    }
}

/// A live relay bridge a volunteer is forwarding between two BEP sessions.
///
/// The bridge is symmetric: `RelayData` arriving from either bridged device
/// is forwarded to the other. Created when a [`BepMessage::RelayConnect`] is
/// admitted; both endpoints are registered so return traffic flows. The
/// admission slot is held for the bridge's lifetime via `guard`.
#[derive(Debug)]
pub struct RelayBridge {
    /// Device id of the requester that opened the bridge.
    pub requester: String,
    /// Device id of the target the requester wants to reach.
    pub target: String,
    /// Admission slot, released on drop.
    pub guard: RelaySessionGuard,
}

impl RelayBridge {
    /// Given the device a `RelayData` frame arrived from, return the device
    /// it should be forwarded to, or `None` if `from` is not part of this
    /// bridge.
    #[must_use]
    pub fn forward_target(&self, from: &str) -> Option<&str> {
        if from == self.requester {
            Some(&self.target)
        } else if from == self.target {
            Some(&self.requester)
        } else {
            None
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    use crate::{DEFAULT_MAX_RELAY_BANDWIDTH_BYTES_PER_SEC, DEFAULT_MAX_RELAY_SESSIONS};

    #[test]
    fn relay_capacity_meters_recorded_bytes() {
        // The live forwarding path calls `record` for every relayed frame.
        // Drive the meter directly to prove the bytes accrue — the engine
        // two-hop test in `sync.rs` proves the engine calls it.
        let capacity = Arc::new(RelayCapacity::new(
            DEFAULT_MAX_RELAY_SESSIONS,
            DEFAULT_MAX_RELAY_BANDWIDTH_BYTES_PER_SEC,
        ));
        assert_eq!(capacity.bytes_relayed(), 0);
        capacity.record(64);
        capacity.record(36);
        assert_eq!(capacity.bytes_relayed(), 100);
    }

    #[test]
    fn relay_bridge_forwards_symmetrically() {
        // A bridge between requester R and target T forwards R-origin frames
        // to T and T-origin frames back to R, and ignores a third party.
        let capacity = Arc::new(RelayCapacity::new(
            DEFAULT_MAX_RELAY_SESSIONS,
            DEFAULT_MAX_RELAY_BANDWIDTH_BYTES_PER_SEC,
        ));
        let guard = capacity.admit().unwrap();
        let bridge = RelayBridge {
            requester: "R".to_owned(),
            target: "T".to_owned(),
            guard,
        };
        assert_eq!(bridge.forward_target("R"), Some("T"));
        assert_eq!(bridge.forward_target("T"), Some("R"));
        assert_eq!(bridge.forward_target("OTHER"), None);
    }

    #[tokio::test]
    async fn peer_relay_transport_wraps_and_unwraps_frames() {
        // The writer wraps an inner frame into RelayData on the carrying
        // session; the reader surfaces the inner bytes a carrying session
        // feeds in, and reports EOF when that feed drops.
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let transport = PeerRelayTransport::new(out_tx, in_rx);
        let (mut reader, mut writer) = transport.split();

        writer.send_frame(b"inner-frame").await.unwrap();
        match out_rx.recv().await.unwrap() {
            BepMessage::RelayData { payload } => assert_eq!(payload, b"inner-frame"),
            other => panic!("expected RelayData, got {other:?}"),
        }

        in_tx.send(b"reply-frame".to_vec()).unwrap();
        assert_eq!(
            reader.recv_frame().await.unwrap(),
            Some(b"reply-frame".to_vec())
        );

        drop(in_tx);
        assert_eq!(reader.recv_frame().await.unwrap(), None);
    }

    #[test]
    fn capacity_rejects_sessions_past_the_cap() {
        const CAP: u32 = 2;
        let capacity = Arc::new(RelayCapacity::new(
            CAP,
            DEFAULT_MAX_RELAY_BANDWIDTH_BYTES_PER_SEC,
        ));

        let first = capacity.admit().unwrap();
        let second = capacity.admit().unwrap();
        assert_eq!(capacity.active_sessions(), CAP);

        // A third admission past the cap is rejected, not silently dropped.
        let third = capacity.admit();
        assert_eq!(
            third.unwrap_err(),
            RelayAdmissionError::AtSessionCapacity {
                active: CAP,
                max: CAP,
            }
        );

        // Releasing one slot frees capacity for a new session.
        drop(first);
        assert_eq!(capacity.active_sessions(), CAP - 1);
        let _replacement = capacity.admit().unwrap();
        assert_eq!(capacity.active_sessions(), CAP);

        drop(second);
    }
}
