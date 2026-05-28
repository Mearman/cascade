//! Peer gossip — fully P2P device discovery with no central server.
//!
//! Devices learn about each other through introducer referrals. When two
//! trusted devices connect, they exchange peer lists. If device A marks
//! device B as an introducer, A auto-adds any previously-unknown peers
//! that B mentions — but only for folders that A and B share.
//!
//! There is no global discovery server. WAN bootstrapping is manual
//! (share device ID + address out-of-band), then the network grows
//! organically through gossip.

use std::collections::HashMap;
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

/// A known peer, potentially learned via introducer gossip.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KnownPeer {
    /// Device ID (base32-encoded SHA-256 of TLS certificate).
    pub device_id: String,
    /// Known addresses where this device can be reached.
    pub addresses: Vec<SocketAddr>,
    /// Device IDs that introduced this peer (empty if manually added).
    pub introduced_by: Vec<String>,
}

/// Gossip message exchanged between connected peers.
///
/// Sent after the TLS handshake and device ID verification, before
/// any BEP protocol messages.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GossipMessage {
    /// Peers this device knows about. Includes addresses but NOT
    /// introducer-only metadata — the receiver records the sender
    /// as the introducer.
    pub peers: Vec<GossipPeer>,
}

/// Peer entry in a gossip message. Stripped of internal bookkeeping.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GossipPeer {
    pub device_id: String,
    pub addresses: Vec<SocketAddr>,
}

/// Local peer database — tracks known peers and introducer relationships.
#[derive(Debug, Clone, Default)]
pub struct PeerBook {
    /// Device ID → known peer entry.
    peers: HashMap<String, KnownPeer>,
}

impl PeerBook {
    /// Create an empty peer book.
    #[must_use] pub fn new() -> Self {
        Self::default()
    }

    /// Manually add a trusted peer. Not introduced by anyone.
    pub fn add_peer(&mut self, device_id: String, addresses: Vec<SocketAddr>) {
        self.peers.insert(
            device_id.clone(),
            KnownPeer {
                device_id,
                addresses,
                introduced_by: Vec::new(),
            },
        );
    }

    /// Merge gossip from an introducer. Peers not already known are
    /// auto-added with the introducer recorded. Existing peers are
    /// updated with any new addresses but their introducer list is
    /// preserved.
    pub fn merge_gossip(&mut self, introducer_id: &str, gossip: &GossipMessage) {
        for gossip_peer in &gossip.peers {
            // Never add self.
            if let Some(existing) = self.peers.get_mut(&gossip_peer.device_id) {
                // Merge addresses.
                for addr in &gossip_peer.addresses {
                    if !existing.addresses.contains(addr) {
                        existing.addresses.push(*addr);
                    }
                }
            } else {
                self.peers.insert(
                    gossip_peer.device_id.clone(),
                    KnownPeer {
                        device_id: gossip_peer.device_id.clone(),
                        addresses: gossip_peer.addresses.clone(),
                        introduced_by: vec![introducer_id.to_string()],
                    },
                );
            }
        }
    }

    /// Remove a peer. If the peer was introduced by someone, record
    /// the removal so it isn't immediately re-added.
    pub fn remove_peer(&mut self, device_id: &str) {
        self.peers.remove(device_id);
    }

    /// Remove all peers introduced by a specific introducer.
    /// Called when an introducer itself is removed.
    pub fn remove_introduced_by(&mut self, introducer_id: &str) {
        self.peers
            .retain(|_, peer| !peer.introduced_by.contains(&introducer_id.to_string()));
    }

    /// Get a peer by device ID.
    #[must_use] pub fn get(&self, device_id: &str) -> Option<&KnownPeer> {
        self.peers.get(device_id)
    }

    /// All known peers.
    #[must_use] pub const fn peers(&self) -> &HashMap<String, KnownPeer> {
        &self.peers
    }

    /// Number of known peers.
    #[must_use] pub fn len(&self) -> usize {
        self.peers.len()
    }

    /// Whether the peer book is empty.
    #[must_use] pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// Build a gossip message to send to a specific peer. Excludes
    /// the target peer itself and the sender's own device ID.
    #[must_use] pub fn build_gossip(&self, exclude_device_id: &str, self_device_id: &str) -> GossipMessage {
        let peers = self
            .peers
            .values()
            .filter(|p| p.device_id != exclude_device_id && p.device_id != self_device_id)
            .map(|p| GossipPeer {
                device_id: p.device_id.clone(),
                addresses: p.addresses.clone(),
            })
            .collect();

        GossipMessage { peers }
    }
}

/// Encode a gossip message as JSON bytes for sending over the wire.
pub fn encode_gossip(msg: &GossipMessage) -> serde_json::Result<Vec<u8>> {
    serde_json::to_vec(msg)
}

/// Decode a gossip message from JSON bytes received on the wire.
pub fn decode_gossip(data: &[u8]) -> serde_json::Result<GossipMessage> {
    serde_json::from_slice(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    #[test]
    fn add_peer_manual() {
        let mut book = PeerBook::new();
        book.add_peer("DEVICE-A".to_string(), vec![addr(22000)]);

        let peer = book.get("DEVICE-A").unwrap();
        assert_eq!(peer.device_id, "DEVICE-A");
        assert_eq!(peer.addresses, vec![addr(22000)]);
        assert!(peer.introduced_by.is_empty());
    }

    #[test]
    fn merge_gossip_adds_unknown_peers() {
        let mut book = PeerBook::new();
        let gossip = GossipMessage {
            peers: vec![
                GossipPeer {
                    device_id: "DEVICE-B".to_string(),
                    addresses: vec![addr(22001)],
                },
                GossipPeer {
                    device_id: "DEVICE-C".to_string(),
                    addresses: vec![addr(22002)],
                },
            ],
        };

        book.merge_gossip("INTRODUCER", &gossip);

        let peer_b = book.get("DEVICE-B").unwrap();
        assert_eq!(peer_b.introduced_by, vec!["INTRODUCER"]);

        let peer_c = book.get("DEVICE-C").unwrap();
        assert_eq!(peer_c.introduced_by, vec!["INTRODUCER"]);
    }

    #[test]
    fn merge_gossip_merges_addresses_for_known_peer() {
        let mut book = PeerBook::new();
        book.add_peer("DEVICE-B".to_string(), vec![addr(22001)]);

        let gossip = GossipMessage {
            peers: vec![GossipPeer {
                device_id: "DEVICE-B".to_string(),
                addresses: vec![addr(22001), addr(22003)],
            }],
        };

        book.merge_gossip("INTRODUCER", &gossip);

        let peer = book.get("DEVICE-B").unwrap();
        assert_eq!(peer.addresses, vec![addr(22001), addr(22003)]);
        // Manual peer keeps empty introduced_by.
        assert!(peer.introduced_by.is_empty());
    }

    #[test]
    fn remove_introduced_by_cleans_up() {
        let mut book = PeerBook::new();
        book.add_peer("SELF".to_string(), vec![addr(22000)]);

        let gossip = GossipMessage {
            peers: vec![GossipPeer {
                device_id: "DEVICE-B".to_string(),
                addresses: vec![addr(22001)],
            }],
        };
        book.merge_gossip("INTRODUCER", &gossip);
        assert_eq!(book.len(), 2);

        book.remove_introduced_by("INTRODUCER");
        assert_eq!(book.len(), 1); // Only SELF remains.
        assert!(book.get("DEVICE-B").is_none());
    }

    #[test]
    fn remove_peer_drops_entry() {
        let mut book = PeerBook::new();
        book.add_peer("DEVICE-A".to_string(), vec![addr(22000)]);
        book.remove_peer("DEVICE-A");
        assert!(book.get("DEVICE-A").is_none());
    }

    #[test]
    fn build_gossip_excludes_self_and_target() {
        let mut book = PeerBook::new();
        book.add_peer("SELF".to_string(), vec![addr(22000)]);
        book.add_peer("DEVICE-A".to_string(), vec![addr(22001)]);
        book.add_peer("DEVICE-B".to_string(), vec![addr(22002)]);

        let gossip = book.build_gossip("DEVICE-A", "SELF");

        assert_eq!(gossip.peers.len(), 1);
        assert_eq!(gossip.peers[0].device_id, "DEVICE-B");
    }

    #[test]
    fn gossip_json_round_trip() {
        let msg = GossipMessage {
            peers: vec![
                GossipPeer {
                    device_id: "ABC".to_string(),
                    addresses: vec![addr(22000)],
                },
                GossipPeer {
                    device_id: "DEF".to_string(),
                    addresses: vec![addr(22001), addr(22002)],
                },
            ],
        };

        let encoded = encode_gossip(&msg).unwrap();
        let decoded = decode_gossip(&encoded).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn empty_gossip_round_trip() {
        let msg = GossipMessage { peers: vec![] };
        let encoded = encode_gossip(&msg).unwrap();
        let decoded = decode_gossip(&encoded).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn merge_gossip_does_not_duplicate_existing_peer() {
        let mut book = PeerBook::new();
        book.add_peer("DEVICE-B".to_string(), vec![addr(22001)]);

        let gossip = GossipMessage {
            peers: vec![GossipPeer {
                device_id: "DEVICE-B".to_string(),
                addresses: vec![addr(22001)],
            }],
        };

        book.merge_gossip("INTRODUCER", &gossip);
        assert_eq!(book.len(), 1);
    }

    #[test]
    fn build_gossip_empty_book() {
        let book = PeerBook::new();
        let gossip = book.build_gossip("ANYONE", "SELF");
        assert!(gossip.peers.is_empty());
    }
}
