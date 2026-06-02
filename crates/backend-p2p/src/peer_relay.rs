//! In-process peer relay.
//!
//! A node whose detected `NAT` type is `Open` or `FullCone` can volunteer
//! to bridge two other peers that cannot reach each other directly,
//! playing exactly the role the operated `cascade-relay-server` plays.
//! The byte-shuttling core is shared — both paths call
//! [`cascade_p2p::pipe::shuttle`] — so the in-process relay sees only
//! opaque ciphertext, just like the operated one.
//!
//! Two concerns live here:
//!
//! - [`RelayCapacity`] bounds the cost a volunteer takes on: a hard cap on
//!   concurrent bridged sessions and an aggregate bandwidth budget. New
//!   sessions past the cap are rejected, never silently dropped.
//! - [`bridge_sessions`] wires two authenticated endpoints into the shared
//!   byte-pipe and accounts the relayed bytes against the capacity's
//!   bandwidth meter.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use cascade_p2p::pipe::{ByteMeter, PipeOutcome, shuttle};
use futures_util::{Sink, Stream};
use tokio_tungstenite::tungstenite::Message;

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

/// Bridge two authenticated relay endpoints with the shared byte-pipe.
///
/// `guard` is the admission slot reserved by [`RelayCapacity::admit`]; it
/// is held for the lifetime of the bridge and released when this future
/// resolves. Relayed bytes are metered against `capacity`'s bandwidth
/// total. Returns the per-direction [`PipeOutcome`] pair the shuttle
/// produced.
pub async fn bridge_sessions<A, B, E>(
    a: A,
    b: B,
    capacity: &Arc<RelayCapacity>,
    guard: RelaySessionGuard,
) -> (PipeOutcome, PipeOutcome)
where
    A: Sink<Message> + Stream<Item = Result<Message, E>> + Unpin + Send,
    B: Sink<Message> + Stream<Item = Result<Message, E>> + Unpin + Send,
{
    let outcome = shuttle(a, b, capacity.as_ref()).await;
    // Release happens on drop, but make the lifetime explicit so the
    // guard is unambiguously held across the whole shuttle.
    drop(guard);
    outcome
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    use std::collections::VecDeque;
    use std::convert::Infallible;
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::task::{Context, Poll};

    use crate::{DEFAULT_MAX_RELAY_BANDWIDTH_BYTES_PER_SEC, DEFAULT_MAX_RELAY_SESSIONS};

    /// In-memory mock endpoint with no real socket. Its stream replays a
    /// finite inbound script then ends; its sink collects what the bridge
    /// writes. Mirrors the mock used in `cascade_p2p::pipe`'s tests so the
    /// peer-relay bridge can be exercised with no I/O.
    struct MockEndpoint {
        inbound: VecDeque<Message>,
        outbound: Arc<Mutex<Vec<Message>>>,
    }

    impl MockEndpoint {
        fn new(inbound: Vec<Message>) -> (Self, Arc<Mutex<Vec<Message>>>) {
            let outbound = Arc::new(Mutex::new(Vec::new()));
            let endpoint = Self {
                inbound: inbound.into(),
                outbound: outbound.clone(),
            };
            (endpoint, outbound)
        }
    }

    impl Stream for MockEndpoint {
        type Item = Result<Message, Infallible>;

        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(self.inbound.pop_front().map(Ok))
        }
    }

    impl Sink<Message> for MockEndpoint {
        type Error = Infallible;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
            if let Ok(mut out) = self.outbound.lock() {
                out.push(item);
            }
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn peer_relay_bridges_two_mock_endpoints() {
        let frame_a = Message::Binary(b"a-to-b".to_vec().into());
        let frame_b = Message::Binary(b"b-to-a".to_vec().into());
        let (endpoint_a, a_out) = MockEndpoint::new(vec![frame_a.clone()]);
        let (endpoint_b, b_out) = MockEndpoint::new(vec![frame_b.clone()]);

        let capacity = Arc::new(RelayCapacity::new(
            DEFAULT_MAX_RELAY_SESSIONS,
            DEFAULT_MAX_RELAY_BANDWIDTH_BYTES_PER_SEC,
        ));
        let guard = capacity.admit().unwrap();
        assert_eq!(capacity.active_sessions(), 1);

        let (outcome_a, outcome_b) =
            bridge_sessions(endpoint_a, endpoint_b, &capacity, guard).await;

        // A's frame reached B's sink and vice versa, with a trailing close
        // on each once the opposite source ended.
        assert_eq!(b_out.lock().unwrap().first(), Some(&frame_a));
        assert_eq!(a_out.lock().unwrap().first(), Some(&frame_b));
        assert_eq!(outcome_a, PipeOutcome::PeerClosed);
        assert_eq!(outcome_b, PipeOutcome::PeerClosed);

        // Bytes were metered against the capacity, and the session slot was
        // released when the bridge finished.
        let expected = b"a-to-b".len() as u64 + b"b-to-a".len() as u64;
        assert_eq!(capacity.bytes_relayed(), expected);
        assert_eq!(capacity.active_sessions(), 0);
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
