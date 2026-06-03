//! Rendezvous-by-presence: live matchmaking between two peers that are
//! online at the same instant.
//!
//! This is the *live-pairing* shape of discovery, distinct from the
//! lookup-style sources in [`crate::discovery`]. A lookup source turns a
//! device id into a static set of candidates that may have been published
//! minutes ago; rendezvous-by-presence instead requires both peers to be
//! connected to the broker *simultaneously*, brokers a single transient
//! candidate + hole-punch exchange between them, and forgets everything
//! the moment either side disconnects.
//!
//! It is the close cousin of the operated relay: [`crate::relay`] and the
//! relay server's session registry already park one peer per rendezvous
//! key and pair it with the second arrival. Rendezvous-by-presence reuses
//! that exact parked-then-paired discipline, but instead of opening an
//! opaque byte-pipe it hands each side the other's
//! [`RendezvousOffer`] — a candidate set plus a [`SyncPunchAgreement`] —
//! so the two can immediately drive [`crate::run_hole_punch`] and upgrade
//! to a direct or hole-punched connection without the broker carrying any
//! further traffic.
//!
//! The broker holds **no persistent state**. A registration lives only in
//! memory, only for as long as the registering peer keeps its handle, and
//! only until its TTL fires. Once two peers pair and both drop their
//! handles, nothing about the meeting remains. Capacity is bounded by an
//! absolute count so a flood of registrations cannot exhaust memory, and a
//! TTL sweep reaps registrations whose counterpart never arrived.
//!
//! Activation is governed by the same exposure posture as the other
//! server-assisted paths (`DiscoveryReach::Public`): the broker is a
//! rendezvous endpoint a never-met peer reaches across the wider internet,
//! so it self-activates only when a rendezvous endpoint is configured and
//! the posture permits global-directory–style reach. The broker type
//! itself is posture-agnostic — gating happens at the call site that
//! decides whether to register a presence at all, mirroring how
//! [`crate::discovery::DiscoveryService`] sources are registered only when
//! the posture permits.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::sync::{Mutex, oneshot};

use crate::candidate::Candidate;
use crate::traversal::SyncPunchAgreement;

/// Default ceiling on concurrently parked presences a single broker holds.
///
/// Mirrors `cascade_backend_p2p::DEFAULT_MAX_RELAY_SESSIONS` — the
/// peer-relay session cap — because a parked rendezvous presence occupies a
/// comparable in-memory slot (one entry plus a pending notifier) and the
/// two limits guard the same resource: bounded broker memory under a flood
/// of half-open meetings. Kept as a named constant rather than an inline
/// literal so the relationship to the relay cap is explicit.
pub const DEFAULT_MAX_PRESENCES: u32 = 8;

/// Default time a parked presence waits for its counterpart before the
/// sweep reaps it.
///
/// A live meeting is, by definition, two peers online at once; if the
/// counterpart has not arrived within this window the registering peer is
/// almost certainly not going to meet anyone this round and should release
/// its slot. Thirty seconds is long enough to absorb the round-trips a
/// never-met peer needs to resolve the rendezvous endpoint and dial it, but
/// short enough that an abandoned half-meeting frees its slot promptly.
pub const DEFAULT_PRESENCE_TTL: Duration = Duration::from_secs(30);

/// What one peer offers its counterpart at the rendezvous: the candidate
/// set it wants probed plus the hole-punch agreement both sides will run.
///
/// This is the entire payload the broker brokers. It is deliberately the
/// same data the [`crate::protocol::BepMessage::Candidates`] and
/// [`crate::protocol::BepMessage::SyncPunch`] frames already carry over a
/// live connection — rendezvous-by-presence just delivers it to a peer the
/// registrant has no live connection to *yet*, so the two can open one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RendezvousOffer {
    /// Reachable candidates the offering peer advertises, in arbitrary
    /// order. The receiver orders them by [`Candidate`] priority exactly
    /// as it would candidates arriving over a live connection.
    pub candidates: Vec<Candidate>,
    /// The hole-punch agreement the offering peer proposes. Both sides run
    /// [`crate::run_hole_punch`] against the agreed nonce and deadline.
    pub sync: SyncPunchAgreement,
}

/// The counterpart's offer, delivered once a meeting completes.
///
/// The parked side receives this through the [`PresenceHandle`]; the second
/// arrival receives it inline from [`RendezvousBroker::register`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedPeer {
    /// The offer the counterpart deposited — its candidates and proposed
    /// punch agreement.
    pub offer: RendezvousOffer,
}

/// Why a parked presence resolved without pairing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RendezvousError {
    /// The TTL elapsed before a counterpart arrived. The slot has been
    /// freed; the peer may register again to wait another round.
    #[error("rendezvous presence expired before a counterpart arrived")]
    Expired,
    /// The broker is shutting down and dropped every parked presence.
    #[error("rendezvous broker shut down before a counterpart arrived")]
    BrokerShutdown,
    /// The registering peer dropped its handle, or the broker forgot the
    /// slot, before a counterpart arrived. Surfaced when the wait future
    /// observes the notifier's sender vanish.
    #[error("rendezvous presence was cancelled before a counterpart arrived")]
    Cancelled,
}

/// Handle a parked peer holds while waiting for its counterpart.
///
/// Dropping the handle without awaiting it leaves the slot parked until the
/// TTL sweep reaps it; to release the slot eagerly on disconnect the caller
/// calls [`RendezvousBroker::drop_presence`] (typically from a
/// cancellation-watch branch). Awaiting [`PresenceHandle::paired`] resolves
/// to the counterpart's offer once one arrives, or a [`RendezvousError`].
#[derive(Debug)]
pub struct PresenceHandle {
    wait: oneshot::Receiver<RendezvousResolution>,
    /// The wall-clock instant the broker will reap this presence if no
    /// counterpart has arrived. Exposed so the caller can drive its own
    /// timeout/UI without reaching into the broker.
    expires_at: Instant,
}

impl PresenceHandle {
    /// The instant the broker will reap this presence absent a counterpart.
    #[must_use]
    pub const fn expires_at(&self) -> Instant {
        self.expires_at
    }

    /// Wait for a counterpart to arrive.
    ///
    /// Resolves to the counterpart's [`PairedPeer`] offer once a second
    /// peer registers under the same rendezvous key, or to a
    /// [`RendezvousError`] if the presence expires, the broker shuts down,
    /// or the slot is dropped first.
    ///
    /// # Errors
    ///
    /// Returns [`RendezvousError::Expired`], [`RendezvousError::BrokerShutdown`],
    /// or [`RendezvousError::Cancelled`] as described on each variant.
    pub async fn paired(self) -> Result<PairedPeer, RendezvousError> {
        match self.wait.await {
            Ok(RendezvousResolution::Paired(peer)) => Ok(peer),
            Ok(RendezvousResolution::Expired) => Err(RendezvousError::Expired),
            Ok(RendezvousResolution::BrokerShutdown) => Err(RendezvousError::BrokerShutdown),
            // The sender was dropped without sending — the slot was removed
            // by `drop_presence` (or the broker itself was dropped) before a
            // counterpart arrived.
            Err(_) => Err(RendezvousError::Cancelled),
        }
    }
}

/// Outcome the broker pushes to a parked peer when its slot resolves.
///
/// Internal to the channel between [`RendezvousBroker`] and
/// [`PresenceHandle`]; callers see the public [`RendezvousError`] /
/// [`PairedPeer`] split via [`PresenceHandle::paired`].
#[derive(Debug)]
enum RendezvousResolution {
    Paired(PairedPeer),
    Expired,
    BrokerShutdown,
}

/// Result of registering a presence at the broker.
#[derive(Debug)]
pub enum RegisterOutcome {
    /// No counterpart was waiting: this peer is now parked. Await the
    /// handle to receive the counterpart's offer when it arrives.
    Parked(PresenceHandle),
    /// A counterpart was already parked: the two are paired immediately.
    /// Carries the counterpart's offer; the now-departed parked peer has
    /// been handed *this* registrant's offer through its handle.
    Paired(PairedPeer),
    /// The broker is at its presence capacity. The caller must reject the
    /// registration rather than park it.
    AtCapacity,
}

/// A peer parked under a rendezvous key, waiting for its counterpart.
struct ParkedPresence {
    /// The offer this parked peer deposited — handed to the counterpart on
    /// pairing.
    offer: RendezvousOffer,
    /// Notifier that resolves this peer's [`PresenceHandle`] when the slot
    /// is paired, expired, or dropped at shutdown.
    notify: oneshot::Sender<RendezvousResolution>,
    /// Instant the sweep reaps this presence absent a counterpart.
    expires_at: Instant,
}

impl std::fmt::Debug for ParkedPresence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The notifier is a `oneshot::Sender` whose contents are not
        // meaningfully debuggable; record only the slot's shape.
        f.debug_struct("ParkedPresence")
            .field("candidate_count", &self.offer.candidates.len())
            .field("notify", &"<oneshot::Sender>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[derive(Default)]
struct BrokerInner {
    parked: HashMap<String, ParkedPresence>,
}

impl std::fmt::Debug for BrokerInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrokerInner")
            .field("parked_count", &self.parked.len())
            .finish()
    }
}

/// In-memory rendezvous-by-presence broker.
///
/// Holds at most one parked presence per rendezvous key. The second peer to
/// register under a key pairs with the first and both receive the other's
/// [`RendezvousOffer`]. State is purely in-memory and bounded: at most
/// `max_presences` keys are parked at once, and each parked presence is
/// reaped by [`RendezvousBroker::reap_expired`] once its TTL fires.
///
/// Two peers agree on the rendezvous key out-of-band, exactly as they do
/// for the relay session id — typically the sorted pair of device ids — so
/// only the intended counterpart meets them.
#[derive(Clone, Debug, Default)]
pub struct RendezvousBroker {
    inner: Arc<Mutex<BrokerInner>>,
}

impl RendezvousBroker {
    /// Construct a fresh broker with no parked presences.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a presence under `key`, offering `offer` to whichever peer
    /// meets it.
    ///
    /// If a counterpart is already parked under `key`, the two pair
    /// immediately: the counterpart's handle resolves with *this* peer's
    /// `offer`, and this call returns [`RegisterOutcome::Paired`] carrying
    /// the counterpart's offer. Otherwise this peer is parked and the call
    /// returns [`RegisterOutcome::Parked`] with a [`PresenceHandle`] to
    /// await — unless the broker is already at `max_presences`, in which
    /// case it returns [`RegisterOutcome::AtCapacity`] and parks nothing.
    pub async fn register(
        &self,
        key: &str,
        offer: RendezvousOffer,
        ttl: Duration,
        max_presences: u32,
    ) -> RegisterOutcome {
        let mut inner = self.inner.lock().await;

        // A counterpart is already parked: pair the two. Hand the parked
        // peer this registrant's offer; return the parked peer's offer to
        // this caller. The parked slot is removed — a meeting is one-shot.
        if let Some(parked) = inner.parked.remove(key) {
            // Send failure means the parked peer dropped its handle while we
            // held the lock; the slot is gone either way, so this caller has
            // no live counterpart. Fall through to park this peer instead so
            // a still-present counterpart can meet it.
            let counterpart_offer = parked.offer;
            match parked
                .notify
                .send(RendezvousResolution::Paired(PairedPeer { offer }))
            {
                Ok(()) => {
                    return RegisterOutcome::Paired(PairedPeer {
                        offer: counterpart_offer,
                    });
                }
                Err(RendezvousResolution::Paired(PairedPeer { offer: returned })) => {
                    // Reclaim this caller's own offer (moved into the failed
                    // send) and park it: the previously-parked peer is gone.
                    return Self::park_locked(&mut inner, key, returned, ttl, max_presences);
                }
                // `send` only ever returns the value we passed, which is
                // always the `Paired` variant; the other arms are
                // unreachable but enumerated to avoid a catch-all that could
                // mask a future variant being mishandled.
                Err(RendezvousResolution::Expired | RendezvousResolution::BrokerShutdown) => {
                    return RegisterOutcome::AtCapacity;
                }
            }
        }

        Self::park_locked(&mut inner, key, offer, ttl, max_presences)
    }

    /// Park `offer` under `key`, enforcing the capacity cap. Caller holds
    /// the lock.
    fn park_locked(
        inner: &mut BrokerInner,
        key: &str,
        offer: RendezvousOffer,
        ttl: Duration,
        max_presences: u32,
    ) -> RegisterOutcome {
        if u32::try_from(inner.parked.len()).unwrap_or(u32::MAX) >= max_presences {
            return RegisterOutcome::AtCapacity;
        }

        let (tx, rx) = oneshot::channel();
        let expires_at = Instant::now() + ttl;
        inner.parked.insert(
            key.to_owned(),
            ParkedPresence {
                offer,
                notify: tx,
                expires_at,
            },
        );
        RegisterOutcome::Parked(PresenceHandle {
            wait: rx,
            expires_at,
        })
    }

    /// Remove a parked presence under `key` without notifying it.
    ///
    /// Called when the registering peer disconnects before its counterpart
    /// arrives — typically from a cancellation-watch branch — so the slot
    /// is released immediately rather than waiting for the TTL sweep. The
    /// dropped handle's [`PresenceHandle::paired`] future resolves with
    /// [`RendezvousError::Cancelled`].
    pub async fn drop_presence(&self, key: &str) {
        let mut inner = self.inner.lock().await;
        inner.parked.remove(key);
    }

    /// Reap every presence whose TTL has elapsed.
    ///
    /// Each reaped peer's handle resolves with [`RendezvousError::Expired`].
    /// Returns the number of presences reaped. Intended to be driven on a
    /// timer by the task that owns the broker, mirroring the relay server's
    /// session sweep.
    pub async fn reap_expired(&self) -> usize {
        let now = Instant::now();
        let mut inner = self.inner.lock().await;
        let expired: Vec<String> = inner
            .parked
            .iter()
            .filter(|(_, p)| p.expires_at <= now)
            .map(|(key, _)| key.clone())
            .collect();
        let count = expired.len();
        for key in expired {
            if let Some(presence) = inner.parked.remove(&key) {
                // Ignore send failure: the peer may have already dropped its
                // handle, in which case there is nobody to notify.
                let _ = presence.notify.send(RendezvousResolution::Expired);
            }
        }
        count
    }

    /// Drop every parked presence, notifying each with
    /// [`RendezvousError::BrokerShutdown`]. Leaves the broker empty.
    pub async fn shutdown(&self) {
        let mut inner = self.inner.lock().await;
        for (_key, presence) in inner.parked.drain() {
            let _ = presence.notify.send(RendezvousResolution::BrokerShutdown);
        }
    }

    /// Number of currently parked presences. A snapshot — the count may
    /// change as soon as the call returns.
    pub async fn parked_count(&self) -> usize {
        let inner = self.inner.lock().await;
        inner.parked.len()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use crate::candidate::CandidateKind;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), port)
    }

    fn offer(port: u16, nonce: u64) -> RendezvousOffer {
        RendezvousOffer {
            candidates: vec![Candidate::new(addr(port), CandidateKind::Host, u16::MAX)],
            sync: SyncPunchAgreement {
                nonce,
                deadline_unix_ms: 1_000_000,
            },
        }
    }

    #[tokio::test]
    async fn two_present_peers_pair_and_exchange_offers() {
        let broker = RendezvousBroker::new();
        let alice_offer = offer(22_000, 0xA);
        let bob_offer = offer(22_001, 0xB);

        // Alice arrives first and parks.
        let parked = broker
            .register("alice|bob", alice_offer.clone(), DEFAULT_PRESENCE_TTL, 8)
            .await;
        let RegisterOutcome::Parked(handle) = parked else {
            panic!("expected Alice to park, got {parked:?}");
        };

        // Bob arrives second and pairs, receiving Alice's offer inline.
        let paired = broker
            .register("alice|bob", bob_offer.clone(), DEFAULT_PRESENCE_TTL, 8)
            .await;
        let RegisterOutcome::Paired(PairedPeer { offer: got_alice }) = paired else {
            panic!("expected Bob to pair, got {paired:?}");
        };
        assert_eq!(got_alice, alice_offer);

        // Alice's handle resolves with Bob's offer.
        let got_bob = handle.paired().await.expect("Alice pairs with Bob");
        assert_eq!(got_bob.offer, bob_offer);

        // The candidate set and the punch agreement both survived the
        // round-trip — this is the whole point of the exchange.
        assert_eq!(got_bob.offer.sync.nonce, 0xB);
        assert_eq!(got_alice.candidates[0].address, addr(22_000));
    }

    #[tokio::test]
    async fn no_state_persists_after_pairing() {
        // Once two peers meet, the broker holds nothing about them: a
        // third peer registering under the same key parks afresh rather
        // than pairing with a ghost.
        let broker = RendezvousBroker::new();
        let parked = broker
            .register("k", offer(1, 1), DEFAULT_PRESENCE_TTL, 8)
            .await;
        let RegisterOutcome::Parked(handle) = parked else {
            panic!("expected park");
        };
        let _ = broker
            .register("k", offer(2, 2), DEFAULT_PRESENCE_TTL, 8)
            .await;
        let _ = handle.paired().await.expect("paired");

        // Both sides have now departed; the slot must be empty.
        assert_eq!(broker.parked_count().await, 0);

        // A fresh registration under the same key parks, proving no
        // residual state lingers.
        let again = broker
            .register("k", offer(3, 3), DEFAULT_PRESENCE_TTL, 8)
            .await;
        assert!(matches!(again, RegisterOutcome::Parked(_)));
        assert_eq!(broker.parked_count().await, 1);
    }

    #[tokio::test]
    async fn presence_expires_after_ttl() {
        let broker = RendezvousBroker::new();
        let parked = broker
            .register("lonely", offer(7, 7), Duration::from_millis(5), 8)
            .await;
        let RegisterOutcome::Parked(handle) = parked else {
            panic!("expected park");
        };

        tokio::time::sleep(Duration::from_millis(20)).await;
        let reaped = broker.reap_expired().await;
        assert_eq!(reaped, 1);
        assert_eq!(broker.parked_count().await, 0);

        let err = handle.paired().await.expect_err("expected expiry");
        assert_eq!(err, RendezvousError::Expired);
    }

    #[tokio::test]
    async fn capacity_cap_rejects_excess_presences() {
        let broker = RendezvousBroker::new();
        // Cap of two: two distinct keys park, the third is rejected.
        let first = broker
            .register("a", offer(1, 1), DEFAULT_PRESENCE_TTL, 2)
            .await;
        let second = broker
            .register("b", offer(2, 2), DEFAULT_PRESENCE_TTL, 2)
            .await;
        let third = broker
            .register("c", offer(3, 3), DEFAULT_PRESENCE_TTL, 2)
            .await;

        assert!(matches!(first, RegisterOutcome::Parked(_)));
        assert!(matches!(second, RegisterOutcome::Parked(_)));
        assert!(matches!(third, RegisterOutcome::AtCapacity));
        assert_eq!(broker.parked_count().await, 2);
    }

    #[tokio::test]
    async fn capacity_does_not_block_pairing_an_existing_key() {
        // At capacity, a *second* arrival under an already-parked key must
        // still pair — pairing removes a slot, it does not add one.
        let broker = RendezvousBroker::new();
        let _a = broker
            .register("a", offer(1, 1), DEFAULT_PRESENCE_TTL, 1)
            .await;
        assert!(matches!(broker.parked_count().await, 1));

        let paired = broker
            .register("a", offer(2, 2), DEFAULT_PRESENCE_TTL, 1)
            .await;
        assert!(matches!(paired, RegisterOutcome::Paired(_)));
        assert_eq!(broker.parked_count().await, 0);
    }

    #[tokio::test]
    async fn drop_presence_releases_slot_and_cancels_handle() {
        let broker = RendezvousBroker::new();
        let parked = broker
            .register("d", offer(1, 1), DEFAULT_PRESENCE_TTL, 8)
            .await;
        let RegisterOutcome::Parked(handle) = parked else {
            panic!("expected park");
        };

        broker.drop_presence("d").await;
        assert_eq!(broker.parked_count().await, 0);

        let err = handle.paired().await.expect_err("expected cancellation");
        assert_eq!(err, RendezvousError::Cancelled);
    }

    #[tokio::test]
    async fn shutdown_drops_every_presence() {
        let broker = RendezvousBroker::new();
        let p1 = broker
            .register("a", offer(1, 1), DEFAULT_PRESENCE_TTL, 8)
            .await;
        let p2 = broker
            .register("b", offer(2, 2), DEFAULT_PRESENCE_TTL, 8)
            .await;
        let RegisterOutcome::Parked(h1) = p1 else {
            panic!("expected park");
        };
        let RegisterOutcome::Parked(h2) = p2 else {
            panic!("expected park");
        };

        broker.shutdown().await;
        assert_eq!(broker.parked_count().await, 0);
        assert_eq!(
            h1.paired().await.expect_err("shutdown"),
            RendezvousError::BrokerShutdown
        );
        assert_eq!(
            h2.paired().await.expect_err("shutdown"),
            RendezvousError::BrokerShutdown
        );
    }

    #[tokio::test]
    async fn handle_exposes_expiry_instant() {
        let broker = RendezvousBroker::new();
        let before = Instant::now();
        let parked = broker
            .register("e", offer(1, 1), Duration::from_secs(30), 8)
            .await;
        let RegisterOutcome::Parked(handle) = parked else {
            panic!("expected park");
        };
        // The expiry sits roughly one TTL in the future.
        assert!(handle.expires_at() > before);
        assert!(handle.expires_at() <= Instant::now() + Duration::from_secs(30));
    }
}
