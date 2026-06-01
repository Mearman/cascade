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

use crate::traversal::NatType;

/// A known peer, potentially learned via introducer gossip.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KnownPeer {
    /// Device ID (base32-encoded SHA-256 of TLS certificate).
    pub device_id: String,
    /// Known addresses where this device can be reached.
    pub addresses: Vec<SocketAddr>,
    /// Device IDs that introduced this peer (empty if manually added).
    pub introduced_by: Vec<String>,
    /// Unix-seconds timestamp when this peer was last reached (outbound)
    /// or accepted from (inbound). `0` means "never confirmed reachable"
    /// — applies to peers introduced via gossip but never contacted
    /// directly.
    pub last_seen: i64,
    /// Most recent `NAT` classification observed for this peer. `None`
    /// means we have never received a candidate exchange from them
    /// (or the field was loaded from an older on-disk record). Used by
    /// the traversal coordinator to pre-select a strategy without
    /// repeating STUN detection.
    #[serde(default)]
    pub last_known_nat_type: Option<NatType>,
    /// Relay endpoint the peer advertised as its own (or one it can be
    /// reached through). `None` if the peer never advertised a relay
    /// — typical for peers on directly-reachable addresses.
    #[serde(default)]
    pub relay_endpoint: Option<SocketAddr>,
}

impl KnownPeer {
    /// Construct a `KnownPeer` with the given `last_seen` for tests.
    /// Production code should call [`PeerBook::add_peer`] or
    /// [`PeerBook::merge_gossip`] and rely on [`PeerBook::mark_seen`] to
    /// update the timestamp.
    #[must_use]
    pub const fn manual(device_id: String, addresses: Vec<SocketAddr>, last_seen: i64) -> Self {
        Self {
            device_id,
            addresses,
            introduced_by: Vec::new(),
            last_seen,
            last_known_nat_type: None,
            relay_endpoint: None,
        }
    }
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
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Manually add a trusted peer. Not introduced by anyone.
    ///
    /// The new entry is recorded with `last_seen = 0` ("never confirmed
    /// reachable"). Callers that have just completed a successful
    /// outbound connect (or accepted an inbound one) should follow this
    /// with [`PeerBook::mark_seen`] to stamp the contact time.
    pub fn add_peer(&mut self, device_id: String, addresses: Vec<SocketAddr>) {
        self.peers.insert(
            device_id.clone(),
            KnownPeer {
                device_id,
                addresses,
                introduced_by: Vec::new(),
                last_seen: 0,
                last_known_nat_type: None,
                relay_endpoint: None,
            },
        );
    }

    /// Stamp `device_id` as just-seen with `now_unix_seconds`. Used
    /// after a successful outbound connect or accepted inbound session,
    /// and on receipt of any frame from the peer.
    ///
    /// The stored value is `max(prior, now_unix_seconds)` — older
    /// snapshots can never regress a fresher contact time. A call for
    /// an unknown peer is a silent no-op (we don't speculatively insert
    /// an addressless entry).
    pub fn mark_seen(&mut self, device_id: &str, now_unix_seconds: i64) {
        if let Some(entry) = self.peers.get_mut(device_id)
            && now_unix_seconds > entry.last_seen
        {
            entry.last_seen = now_unix_seconds;
        }
    }

    /// Merge gossip from an introducer. Peers not already known are
    /// auto-added with the introducer recorded. Existing peers are
    /// updated with any new addresses but their introducer list is
    /// preserved.
    pub fn merge_gossip(
        &mut self,
        introducer_id: &str,
        self_device_id: &str,
        gossip: &GossipMessage,
    ) {
        for gossip_peer in &gossip.peers {
            // Never add self — a malicious or buggy peer that gossips
            // our own device ID back to us must not poison our own
            // record (we'd otherwise insert addresses for ourselves and
            // every future `KnownPeer` lookup for our own ID would
            // succeed with attacker-controlled data).
            if gossip_peer.device_id == self_device_id {
                continue;
            }
            if let Some(existing) = self.peers.get_mut(&gossip_peer.device_id) {
                // Merge addresses.
                for addr in &gossip_peer.addresses {
                    if !existing.addresses.contains(addr) {
                        existing.addresses.push(*addr);
                    }
                }
            } else {
                // Peer is new to us. The introducer's gossip frame may
                // carry a snapshot of when *they* last reached this
                // peer, but we have not confirmed reachability
                // ourselves — record `last_seen = 0` and let a future
                // direct contact stamp the real value via `mark_seen`.
                self.peers.insert(
                    gossip_peer.device_id.clone(),
                    KnownPeer {
                        device_id: gossip_peer.device_id.clone(),
                        addresses: gossip_peer.addresses.clone(),
                        introduced_by: vec![introducer_id.to_string()],
                        last_seen: 0,
                        last_known_nat_type: None,
                        relay_endpoint: None,
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
    #[must_use]
    pub fn get(&self, device_id: &str) -> Option<&KnownPeer> {
        self.peers.get(device_id)
    }

    /// All known peers.
    #[must_use]
    pub const fn peers(&self) -> &HashMap<String, KnownPeer> {
        &self.peers
    }

    /// Number of known peers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    /// Whether the peer book is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// Build a gossip message to send to a specific peer. Excludes
    /// the target peer itself and the sender's own device ID.
    #[must_use]
    pub fn build_gossip(&self, exclude_device_id: &str, self_device_id: &str) -> GossipMessage {
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
        assert_eq!(
            peer.last_seen, 0,
            "freshly-added peers have no confirmed contact time yet",
        );
    }

    #[test]
    fn mark_seen_advances_timestamp_monotonically() {
        let mut book = PeerBook::new();
        book.add_peer("DEVICE-A".to_string(), vec![addr(22000)]);
        book.mark_seen("DEVICE-A", 1000);
        assert_eq!(book.get("DEVICE-A").unwrap().last_seen, 1000);
        // Older snapshot must not regress the cursor.
        book.mark_seen("DEVICE-A", 500);
        assert_eq!(book.get("DEVICE-A").unwrap().last_seen, 1000);
        // Newer snapshot moves it forward.
        book.mark_seen("DEVICE-A", 2000);
        assert_eq!(book.get("DEVICE-A").unwrap().last_seen, 2000);
    }

    #[test]
    fn mark_seen_on_unknown_peer_is_noop() {
        let mut book = PeerBook::new();
        book.mark_seen("UNKNOWN", 1000);
        assert!(book.get("UNKNOWN").is_none());
    }

    #[test]
    fn merge_gossip_records_zero_last_seen_for_new_peers() {
        // A peer learned via gossip has not been confirmed reachable
        // by us, so its `last_seen` must start at 0 regardless of any
        // wire snapshot the introducer carries.
        let mut book = PeerBook::new();
        let gossip = GossipMessage {
            peers: vec![GossipPeer {
                device_id: "DEVICE-B".to_string(),
                addresses: vec![addr(22001)],
            }],
        };
        book.merge_gossip("INTRODUCER", "SELF", &gossip);
        let peer = book.get("DEVICE-B").unwrap();
        assert_eq!(peer.last_seen, 0);
    }

    #[test]
    fn known_peer_manual_constructor_sets_fields() {
        let peer = KnownPeer::manual("DEVICE-A".to_string(), vec![addr(22000)], 1234);
        assert_eq!(peer.device_id, "DEVICE-A");
        assert_eq!(peer.addresses, vec![addr(22000)]);
        assert!(peer.introduced_by.is_empty());
        assert_eq!(peer.last_seen, 1234);
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

        book.merge_gossip("INTRODUCER", "SELF", &gossip);

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

        book.merge_gossip("INTRODUCER", "SELF", &gossip);

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
        book.merge_gossip("INTRODUCER", "SELF", &gossip);
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

        book.merge_gossip("INTRODUCER", "SELF", &gossip);
        assert_eq!(book.len(), 1);
    }

    #[test]
    fn build_gossip_empty_book() {
        let book = PeerBook::new();
        let gossip = book.build_gossip("ANYONE", "SELF");
        assert!(gossip.peers.is_empty());
    }

    #[test]
    fn known_peer_round_trips_with_nat_type_and_relay_endpoint() {
        // Construct a peer with both optional fields populated and
        // round-trip it through `serde_json` to confirm the wire
        // shape is stable. The `#[serde(default)]` attributes mean
        // older records (missing the fields) load with `None`, which
        // is exercised by the next test.
        let relay = SocketAddr::from(([198, 51, 100, 7], 3478));
        let peer = KnownPeer {
            device_id: "DEVICE-A".to_string(),
            addresses: vec![addr(22000)],
            introduced_by: vec!["INTRODUCER".to_string()],
            last_seen: 1_700_000_000,
            last_known_nat_type: Some(NatType::PortRestrictedCone),
            relay_endpoint: Some(relay),
        };
        let encoded = serde_json::to_string(&peer).expect("serialise");
        let decoded: KnownPeer = serde_json::from_str(&encoded).expect("deserialise");
        assert_eq!(decoded, peer);
    }

    #[test]
    fn known_peer_decodes_legacy_records_with_default_none() {
        // Older on-disk records pre-date the two new fields. The
        // `#[serde(default)]` attribute makes them load as `None`,
        // matching the documented "never observed" semantics.
        let legacy_json = serde_json::json!({
            "device_id": "DEVICE-LEGACY",
            "addresses": ["127.0.0.1:22000"],
            "introduced_by": [],
            "last_seen": 0
        })
        .to_string();
        let decoded: KnownPeer =
            serde_json::from_str(&legacy_json).expect("legacy record must load");
        assert_eq!(decoded.last_known_nat_type, None);
        assert_eq!(decoded.relay_endpoint, None);
    }
}
