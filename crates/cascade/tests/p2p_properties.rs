//! Property-based tests for P2P block operations, protocol messages, and gossip.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use cascade_p2p::block::{BlockHash, reassemble_blocks, split_data};
use cascade_p2p::protocol::{
    BepMessage, FileInfo, Folder, Version, decode_message, encode_message,
};
use cascade_p2p::wan::{GossipMessage, GossipPeer, PeerBook};
use proptest::prelude::*;
use std::net::SocketAddr;

/// Block splitting is deterministic: same data always produces same blocks.
#[test]
fn block_splitting_deterministic() {
    proptest!(|(data in prop::collection::vec(any::<u8>(), 0..1024 * 10))| {
        let blocks1 = split_data(&data);
        let blocks2 = split_data(&data);
        assert_eq!(blocks1.size, blocks2.size);
        assert_eq!(blocks1.block_size, blocks2.block_size);
        assert_eq!(blocks1.blocks, blocks2.blocks);
    });
}

/// Block reassembly is lossless: reassemble(split(data)) == data.
#[test]
fn block_reassembly_lossless() {
    proptest!(|(data in prop::collection::vec(any::<u8>(), 0..1024 * 10))| {
        let file_blocks = split_data(&data);
        let block_data: Vec<Vec<u8>> = data
            .chunks(file_blocks.block_size as usize)
            .map(<[u8]>::to_vec)
            .collect();
        let reassembled = reassemble_blocks(&block_data);
        assert_eq!(reassembled, data);
    });
}

/// Different data produces different block hashes (statistical).
/// Generates pairs of distinct byte vectors and checks they hash differently.
#[test]
fn different_data_different_hash() {
    proptest!(|(pair in distinct_byte_pairs())| {
        let (a, b) = pair;
        let ha = BlockHash::from_data(&a);
        let hb = BlockHash::from_data(&b);
        assert_ne!(ha, hb);
    });
}

/// BEP message round-trip: encode then decode produces the same message.
#[test]
fn bep_message_round_trip() {
    proptest!(|(msg in bep_message_strategy())| {
        let encoded = encode_message(&msg).unwrap();
        let decoded = decode_message(&encoded).unwrap();
        assert_eq!(decoded, msg);
    });
}

/// Gossip merge is idempotent: merging the same gossip twice doesn't
/// duplicate peers.
#[test]
fn gossip_merge_idempotent() {
    proptest!(|(peers in prop::collection::vec(gossip_peer_strategy(), 0..10))| {
        let mut book = PeerBook::new();
        let gossip = GossipMessage { peers };

        book.merge_gossip("INTRO", &gossip);
        let len_after_first = book.len();

        book.merge_gossip("INTRO", &gossip);
        assert_eq!(book.len(), len_after_first);
    });
}

/// Gossip merge is commutative: merge(A, B) == merge(B, A).
#[test]
fn gossip_merge_commutative() {
    proptest!(|(pair in distinct_peer_sets())| {
        let (peers_a, peers_b) = pair;
        let gossip_a = GossipMessage { peers: peers_a };
        let gossip_b = GossipMessage { peers: peers_b };

        // Order 1: A then B.
        let mut book1 = PeerBook::new();
        book1.merge_gossip("X", &gossip_a);
        book1.merge_gossip("Y", &gossip_b);

        // Order 2: B then A.
        let mut book2 = PeerBook::new();
        book2.merge_gossip("Y", &gossip_b);
        book2.merge_gossip("X", &gossip_a);

        // Both should have the same set of device IDs.
        let mut ids1: Vec<_> = book1.peers().keys().collect();
        let mut ids2: Vec<_> = book2.peers().keys().collect();
        ids1.sort();
        ids2.sort();
        assert_eq!(ids1, ids2);
    });
}

// ── Strategies ──

/// Generate pairs of distinct byte vectors.
fn distinct_byte_pairs() -> impl Strategy<Value = (Vec<u8>, Vec<u8>)> {
    (
        prop::collection::vec(any::<u8>(), 1..1024),
        prop::collection::vec(any::<u8>(), 1..1024),
    )
        .prop_filter("values must differ", |(a, b)| a != b)
}

/// Generate pairs of distinct gossip peer sets.
fn distinct_peer_sets() -> impl Strategy<Value = (Vec<GossipPeer>, Vec<GossipPeer>)> {
    (
        prop::collection::vec(gossip_peer_strategy(), 0..5),
        prop::collection::vec(gossip_peer_strategy(), 0..5),
    )
}

fn bep_message_strategy() -> impl Strategy<Value = BepMessage> {
    let folder = (".{0,20}", ".{0,20}").prop_map(|(id, label)| Folder { id, label });

    let file_info = (
        ".{0,20}",
        0u32..2,
        0u64..1_000_000,
        0i64..1_000_000,
        prop::array::uniform32(any::<u8>()),
    )
        .prop_map(|(name, file_type, size, modified, hash)| FileInfo {
            name,
            file_type,
            size,
            modified,
            block_size: 128 * 1024,
            deleted: false,
            version: Version::default(),
            block_hashes: vec![hash],
        });

    let cluster_config = prop::collection::vec(folder, 0..3)
        .prop_map(|folders| BepMessage::ClusterConfig { folders });

    let index = (".{0,20}", prop::collection::vec(file_info.clone(), 0..3))
        .prop_map(|(folder, files)| BepMessage::Index { folder, files });

    let index_update = (".{0,20}", prop::collection::vec(file_info, 0..3))
        .prop_map(|(folder, files)| BepMessage::IndexUpdate { folder, files });

    let request = (
        ".{0,20}",
        ".{0,20}",
        0u64..1_000_000,
        prop::array::uniform32(any::<u8>()),
    )
        .prop_map(
            |(folder, name, block_offset, block_hash)| BepMessage::Request {
                folder,
                name,
                block_offset,
                block_size: 128 * 1024,
                block_hash,
            },
        );

    let response =
        prop::collection::vec(any::<u8>(), 0..100).prop_map(|data| BepMessage::Response { data });

    let close = ".{0,50}".prop_map(|reason| BepMessage::Close { reason });

    prop_oneof![
        cluster_config,
        index,
        index_update,
        request,
        response,
        Just(BepMessage::Ping),
        close,
    ]
}

fn gossip_peer_strategy() -> impl Strategy<Value = GossipPeer> {
    (".{0,20}", any::<u16>()).prop_map(|(device_id, port)| GossipPeer {
        device_id,
        addresses: vec![SocketAddr::from(([127, 0, 0, 1], port))],
    })
}
