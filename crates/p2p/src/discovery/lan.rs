//! UDP multicast peer discovery for LAN.
//!
//! Devices announce their presence on a multicast group at port 21027.
//! Announcements carry the device ID and listening port so peers can
//! establish direct BEP connections.
//!
//! The free functions [`announce`] and [`listen`] preserve the exact
//! wire behaviour relied upon by the backend's discovery loops. The
//! [`LanDiscovery`] type wraps them behind the [`Discovery`] trait so
//! LAN multicast can be composed with other discovery sources through a
//! [`DiscoveryService`](super::DiscoveryService) without changing what
//! goes on the wire.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::Discovery;
use crate::candidate::{Candidate, CandidateKind};

/// Default multicast port (same as Syncthing's local discovery).
pub const DISCOVERY_PORT: u16 = 21027;

/// Multicast group address for LAN discovery.
const MULTICAST_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);

/// How long a single [`LanDiscovery::resolve`] listens for announcements
/// before returning the peers it has seen. Mirrors the cadence the
/// backend's listen loop already used so the trait surface behaves like
/// the existing path.
const RESOLVE_LISTEN_WINDOW: Duration = Duration::from_secs(15);

/// `local_preference` assigned to a LAN-discovered host candidate.
///
/// LAN-multicast peers expose exactly one reachable address each, so
/// there is no interface ranking to encode here. The maximum value
/// keeps these host candidates ranked above any externally-derived
/// candidate sharing the same RFC 8445 type preference.
const LAN_HOST_LOCAL_PREFERENCE: u16 = u16::MAX;

/// Announcement payload broadcast by each device.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Announcement {
    /// Device ID (base32-encoded SHA-256 of TLS certificate).
    pub device_id: String,
    /// TCP port where the BEP server is listening.
    pub listen_port: u16,
}

/// A discovered peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredPeer {
    /// Device ID.
    pub device_id: String,
    /// Socket address for direct BEP connection.
    pub address: SocketAddr,
}

/// Send a single discovery announcement.
pub fn announce(device_id: &str, listen_port: u16) -> Result<()> {
    let payload = serde_json::to_vec(&Announcement {
        device_id: device_id.to_string(),
        listen_port,
    })?;

    let socket = UdpSocket::bind("0.0.0.0:0").context("binding discovery socket")?;
    let addr = SocketAddrV4::new(MULTICAST_GROUP, DISCOVERY_PORT);
    socket
        .send_to(&payload, addr)
        .context("sending discovery announcement")?;

    Ok(())
}

/// Listen for discovery announcements from other peers.
///
/// Blocks until `timeout` elapses with no new announcements, returning
/// all peers discovered during the listening window.
pub fn listen(timeout: Duration) -> Result<Vec<DiscoveredPeer>> {
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT))
        .context("binding discovery listener")?;

    socket
        .set_read_timeout(Some(timeout))
        .context("setting discovery read timeout")?;

    // Join multicast group.
    socket
        .join_multicast_v4(&MULTICAST_GROUP, &Ipv4Addr::UNSPECIFIED)
        .context("joining multicast group")?;

    let mut peers = Vec::new();
    let mut buf = [0u8; 4096];

    loop {
        match socket.recv_from(&mut buf) {
            Ok((len, src)) => {
                if let Some(slice) = buf.get(..len)
                    && let Ok(announcement) = serde_json::from_slice::<Announcement>(slice)
                {
                    let address = SocketAddr::new(src.ip(), announcement.listen_port);
                    peers.push(DiscoveredPeer {
                        device_id: announcement.device_id,
                        address,
                    });
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                break;
            }
            Err(e) => {
                return Err(e).context("receiving discovery announcement");
            }
        }
    }

    Ok(peers)
}

/// Convert a LAN-discovered peer's reachable address into a host
/// [`Candidate`]. LAN peers are always directly reachable on the local
/// segment, so they are host candidates by definition.
const fn discovered_peer_to_candidate(peer: &DiscoveredPeer) -> Candidate {
    Candidate::new(peer.address, CandidateKind::Host, LAN_HOST_LOCAL_PREFERENCE)
}

/// LAN UDP-multicast discovery source.
///
/// Wraps the blocking [`announce`]/[`listen`] free functions behind the
/// async [`Discovery`] trait. The blocking std-net calls run on
/// `spawn_blocking` so the trait methods are safe to `await` inside a
/// tokio runtime without stalling a worker thread.
#[derive(Debug, Default, Clone, Copy)]
pub struct LanDiscovery;

impl LanDiscovery {
    /// Construct a LAN discovery source.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Listen once and return every peer that announced during the
    /// window, regardless of device ID. The backend's listen loop uses
    /// this to drive outbound connects to all freshly-seen peers; the
    /// [`Discovery::resolve`] trait method filters this to a single
    /// device ID.
    pub async fn listen_all(&self, timeout: Duration) -> Vec<DiscoveredPeer> {
        match tokio::task::spawn_blocking(move || listen(timeout)).await {
            Ok(Ok(peers)) => peers,
            Ok(Err(e)) => {
                tracing::debug!(
                    target: "cascade::p2p::discovery::lan",
                    error = %format!("{e:#}"),
                    "LAN discovery listen failed",
                );
                Vec::new()
            }
            Err(e) => {
                tracing::debug!(
                    target: "cascade::p2p::discovery::lan",
                    error = %e,
                    "LAN discovery listen task panicked",
                );
                Vec::new()
            }
        }
    }
}

#[async_trait]
impl Discovery for LanDiscovery {
    /// Listen for LAN announcements and return the host candidates of
    /// any peer matching `device_id`. A peer may announce more than once
    /// during the window; each announcement becomes one candidate and
    /// the [`DiscoveryService`](super::DiscoveryService) deduplicates
    /// across sources.
    async fn resolve(&self, device_id: &str) -> Vec<Candidate> {
        self.listen_all(RESOLVE_LISTEN_WINDOW)
            .await
            .iter()
            .filter(|peer| peer.device_id == device_id)
            .map(discovered_peer_to_candidate)
            .collect()
    }

    /// Broadcast a single LAN announcement carrying the listen port of
    /// the highest-priority host candidate. Non-host candidates carry
    /// no LAN-routable port, so they are ignored — the announcement
    /// wire shape only conveys the device ID and one BEP listen port.
    async fn announce(&self, self_id: &str, candidates: &[Candidate]) {
        let Some(listen_port) = candidates
            .iter()
            .filter(|c| c.kind == CandidateKind::Host)
            .max_by_key(|c| c.priority)
            .map(|c| c.address.port())
        else {
            return;
        };
        let id = self_id.to_string();
        match tokio::task::spawn_blocking(move || announce(&id, listen_port)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::debug!(
                target: "cascade::p2p::discovery::lan",
                error = %format!("{e:#}"),
                "LAN discovery announce failed",
            ),
            Err(e) => tracing::debug!(
                target: "cascade::p2p::discovery::lan",
                error = %e,
                "LAN discovery announce task panicked",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announcement_serialise_round_trip() {
        let a = Announcement {
            device_id: "ABCDEFGH-1234".to_string(),
            listen_port: 22000,
        };
        let json = serde_json::to_vec(&a).unwrap();
        let decoded: Announcement = serde_json::from_slice(&json).unwrap();
        assert_eq!(decoded, a);
    }

    #[test]
    fn announcement_json_format() {
        let a = Announcement {
            device_id: "DEVICE1".to_string(),
            listen_port: 12345,
        };
        let json = serde_json::to_string(&a).unwrap();
        assert!(json.contains("\"device_id\""));
        assert!(json.contains("\"listen_port\""));
        assert!(json.contains("DEVICE1"));
        assert!(json.contains("12345"));
    }
}
