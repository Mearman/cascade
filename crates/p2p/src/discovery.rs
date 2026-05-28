//! UDP multicast peer discovery for LAN.
//!
//! Devices announce their presence on a multicast group at port 21027.
//! Announcements carry the device ID and listening port so peers can
//! establish direct BEP connections.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Default multicast port (same as Syncthing's local discovery).
pub const DISCOVERY_PORT: u16 = 21027;

/// Multicast group address for LAN discovery.
const MULTICAST_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);

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
                if let Ok(announcement) = serde_json::from_slice::<Announcement>(&buf[..len]) {
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
