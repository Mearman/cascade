//! Session registry and pairing.
//!
//! The registry holds at most one "parked" client per session ID. When a
//! second client connects with the same session ID the two are removed from
//! the registry and handed off to the byte-pipe.
//!
//! Parking is bounded by both an absolute count (`max_sessions`) and a
//! per-session timeout. A session that has not been paired by the time its
//! `expires_at` fires is reaped — the parked peer receives a
//! `SessionEvent::TimedOut` message and the registry slot is freed.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, oneshot};

/// Identifier for a rendezvous session — the URL path component clients
/// dial on. Two peers wishing to meet agree on this out-of-band (typically
/// derived from the sorted pair of device IDs).
pub type SessionId = String;

/// Outcome reported to a parked client when its slot resolves.
#[derive(Debug, Clone, Copy)]
pub enum SessionEvent {
    /// A peer joined with the same session ID. The two clients are paired
    /// and the consumer should hand both sockets off to the byte-pipe.
    Paired,
    /// The session expired before a second peer arrived.
    TimedOut,
    /// The server is shutting down and dropping every parked session.
    ServerShutdown,
}

/// A client that has authenticated and is waiting for a peer.
struct ParkedPeer {
    notify: oneshot::Sender<SessionEvent>,
    expires_at: Instant,
}

#[derive(Default)]
struct RegistryInner {
    parked: HashMap<SessionId, ParkedPeer>,
}

/// Concurrency-safe registry of parked sessions.
#[derive(Clone, Debug, Default)]
pub struct SessionRegistry {
    inner: Arc<Mutex<RegistryInner>>,
}

impl std::fmt::Debug for RegistryInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryInner")
            .field("parked_count", &self.parked.len())
            .finish()
    }
}

impl std::fmt::Debug for ParkedPeer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The `notify` field is a `oneshot::Sender<SessionEvent>` — its
        // contents are not meaningfully debuggable so we record only its
        // presence. `expires_at` is the only field a human reading a
        // debug log actually wants to see.
        f.debug_struct("ParkedPeer")
            .field("notify", &"<oneshot::Sender>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Result of attempting to register a client under a session ID.
#[derive(Debug)]
pub enum RegisterOutcome {
    /// Slot was empty: this client is now parked. The receiver resolves
    /// when a peer arrives, the session times out, or the server stops.
    Parked {
        wait_for_peer: oneshot::Receiver<SessionEvent>,
        expires_at: Instant,
    },
    /// A peer was already waiting: this call returns success and notifies
    /// the parked peer. Both sides should now proceed to the byte-pipe.
    Paired,
    /// The registry is at capacity. The caller must reject the client.
    AtCapacity,
}

impl SessionRegistry {
    /// Construct a fresh empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a client. If a peer is already parked under `session_id`,
    /// the two are paired immediately. Otherwise the caller is parked and
    /// receives a `oneshot::Receiver` that resolves when paired or expired.
    pub async fn register(
        &self,
        session_id: &str,
        timeout: Duration,
        max_sessions: u32,
    ) -> RegisterOutcome {
        let mut inner = self.inner.lock().await;

        // If a peer is already parked, hand it the notification and tell
        // the caller to proceed.
        if let Some(parked) = inner.parked.remove(session_id) {
            // Ignore send failure — the parked peer may have already
            // dropped its receiver (e.g. its connection was severed).
            let _ = parked.notify.send(SessionEvent::Paired);
            return RegisterOutcome::Paired;
        }

        // No peer present. Enforce capacity before parking.
        if u32::try_from(inner.parked.len()).unwrap_or(u32::MAX) >= max_sessions {
            return RegisterOutcome::AtCapacity;
        }

        let (tx, rx) = oneshot::channel();
        let expires_at = Instant::now() + timeout;
        inner.parked.insert(
            session_id.to_owned(),
            ParkedPeer {
                notify: tx,
                expires_at,
            },
        );
        RegisterOutcome::Parked {
            wait_for_peer: rx,
            expires_at,
        }
    }

    /// Remove a parked entry without notifying. Used when the parked
    /// peer's connection died before its peer arrived.
    pub async fn drop_parked(&self, session_id: &str) {
        let mut inner = self.inner.lock().await;
        inner.parked.remove(session_id);
    }

    /// Sweep expired entries. Each timed-out peer receives a `TimedOut`
    /// event. Returns the number of sessions reaped.
    pub async fn reap_expired(&self) -> usize {
        let now = Instant::now();
        let mut inner = self.inner.lock().await;
        let expired: Vec<SessionId> = inner
            .parked
            .iter()
            .filter(|(_, peer)| peer.expires_at <= now)
            .map(|(id, _)| id.clone())
            .collect();
        let count = expired.len();
        for id in expired {
            if let Some(peer) = inner.parked.remove(&id) {
                let _ = peer.notify.send(SessionEvent::TimedOut);
            }
        }
        count
    }

    /// Send `ServerShutdown` to every parked peer and clear the registry.
    pub async fn shutdown(&self) {
        let mut inner = self.inner.lock().await;
        for (_id, peer) in inner.parked.drain() {
            let _ = peer.notify.send(SessionEvent::ServerShutdown);
        }
    }

    /// Number of currently parked sessions. Snapshot value; the registry
    /// may change immediately after the call returns.
    pub async fn parked_count(&self) -> usize {
        let inner = self.inner.lock().await;
        inner.parked.len()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn first_caller_is_parked_second_is_paired() {
        let registry = SessionRegistry::new();
        let first = registry.register("s", Duration::from_secs(10), 8).await;
        let second = registry.register("s", Duration::from_secs(10), 8).await;

        let RegisterOutcome::Parked { wait_for_peer, .. } = first else {
            panic!("expected first to park, got {first:?}");
        };
        assert!(matches!(second, RegisterOutcome::Paired));
        assert!(matches!(wait_for_peer.await.unwrap(), SessionEvent::Paired));
    }

    #[tokio::test]
    async fn registry_rejects_at_capacity() {
        let registry = SessionRegistry::new();
        let _first = registry.register("a", Duration::from_secs(10), 1).await;
        let second = registry.register("b", Duration::from_secs(10), 1).await;
        assert!(matches!(second, RegisterOutcome::AtCapacity));
    }

    #[tokio::test]
    async fn reap_drops_expired_parked_peer() {
        let registry = SessionRegistry::new();
        let outcome = registry.register("x", Duration::from_millis(5), 8).await;
        let RegisterOutcome::Parked { wait_for_peer, .. } = outcome else {
            panic!("expected parked outcome");
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        let reaped = registry.reap_expired().await;
        assert_eq!(reaped, 1);
        assert!(matches!(
            wait_for_peer.await.unwrap(),
            SessionEvent::TimedOut
        ));
    }

    #[tokio::test]
    async fn drop_parked_removes_without_notifying() {
        let registry = SessionRegistry::new();
        let outcome = registry.register("d", Duration::from_secs(10), 8).await;
        let RegisterOutcome::Parked { wait_for_peer, .. } = outcome else {
            panic!("expected parked outcome");
        };
        registry.drop_parked("d").await;
        // The receiver should hang up: future is dropped on the sender side.
        assert!(wait_for_peer.await.is_err());
        assert_eq!(registry.parked_count().await, 0);
    }
}
