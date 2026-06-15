//! Test module for `protocol.rs`, split out to keep the
//! parent file under the source-length cap. Declared from there via
//! `#[cfg(test)] #[path = "protocol_tests.rs"] mod tests;`, so it stays a child
//! module with full access to the parent's private items.

use super::*;

fn round_trip(msg: BepMessage) {
    let encoded = encode_message(&msg).unwrap();
    let decoded = decode_message(&encoded).unwrap();
    assert_eq!(decoded, msg);
}

#[test]
fn encode_decode_cluster_config() {
    round_trip(BepMessage::ClusterConfig {
        folders: vec![
            Folder {
                id: "folder-1".into(),
                label: "Documents".into(),
            },
            Folder {
                id: "folder-2".into(),
                label: "Photos".into(),
            },
        ],
        data_token: None,
    });
}

#[test]
fn encode_decode_cluster_config_with_data_token() {
    round_trip(BepMessage::ClusterConfig {
        folders: vec![Folder {
            id: "folder-1".into(),
            label: "Documents".into(),
        }],
        data_token: Some("{\"signed\":\"token-json\"}".into()),
    });
}

#[test]
fn encode_decode_cluster_config_empty() {
    round_trip(BepMessage::ClusterConfig {
        folders: vec![],
        data_token: None,
    });
}

#[test]
fn encode_decode_index() {
    round_trip(BepMessage::Index {
        folder: "folder-1".into(),
        files: vec![FileInfo {
            name: "test.txt".into(),
            file_type: 0,
            size: 1024,
            modified: 1_700_000_000,
            sequence: 0,
            block_size: 128 * 1024,
            deleted: false,
            invalid: false,
            no_permissions: false,
            version: Version::default(),
            block_hashes: vec![[0xAB; 32]],
        }],
    });
}

#[test]
fn encode_decode_index_multiple_files() {
    round_trip(BepMessage::Index {
        folder: "docs".into(),
        files: vec![
            FileInfo {
                name: "a.txt".into(),
                file_type: 0,
                size: 500,
                modified: 100,
                sequence: 0,
                block_size: 128 * 1024,
                deleted: false,
                invalid: false,
                no_permissions: false,
                version: Version::default(),
                block_hashes: vec![[1u8; 32]],
            },
            FileInfo {
                name: "b.txt".into(),
                file_type: 0,
                size: 200_000,
                modified: 200,
                sequence: 0,
                block_size: 128 * 1024,
                deleted: false,
                invalid: false,
                no_permissions: false,
                version: Version::default(),
                block_hashes: vec![[2u8; 32], [3u8; 32]],
            },
        ],
    });
}

#[test]
fn encode_decode_index_update() {
    round_trip(BepMessage::IndexUpdate {
        folder: "folder-1".into(),
        files: vec![FileInfo {
            name: "updated.bin".into(),
            file_type: 0,
            size: 999_999,
            modified: 1_700_000_001,
            sequence: 0,
            block_size: 512 * 1024,
            deleted: false,
            invalid: false,
            no_permissions: false,
            version: Version::default(),
            block_hashes: vec![[0xFF; 32], [0xEE; 32]],
        }],
    });
}

#[test]
fn encode_decode_index_tombstone_round_trip() {
    round_trip(BepMessage::IndexUpdate {
        folder: "folder-1".into(),
        files: vec![FileInfo {
            name: "gone.txt".into(),
            file_type: 0,
            size: 0,
            modified: 1_700_000_002,
            sequence: 0,
            block_size: 128 * 1024,
            deleted: true,
            invalid: false,
            no_permissions: false,
            version: Version::default(),
            block_hashes: vec![],
        }],
    });
}

#[test]
fn encode_decode_index_negative_modified() {
    round_trip(BepMessage::IndexUpdate {
        folder: "folder-1".into(),
        files: vec![FileInfo {
            name: "ancient.txt".into(),
            file_type: 0,
            size: 42,
            modified: -1_000_000,
            sequence: 0,
            block_size: 128 * 1024,
            deleted: false,
            invalid: false,
            no_permissions: false,
            version: Version::default(),
            block_hashes: vec![[0x77; 32]],
        }],
    });
}

#[test]
fn encode_decode_index_with_version_vector() {
    round_trip(BepMessage::Index {
        folder: "folder-1".into(),
        files: vec![FileInfo {
            name: "doc.txt".into(),
            file_type: 0,
            size: 99,
            modified: 1_700_000_000,
            sequence: 0,
            block_size: 128 * 1024,
            deleted: false,
            invalid: false,
            no_permissions: false,
            version: Version {
                counters: vec![(7, 3), (42, 1), (1024, 9)],
            },
            block_hashes: vec![[0x11; 32]],
        }],
    });
}

#[test]
fn encode_decode_request() {
    round_trip(BepMessage::Request {
        request_id: 42,
        folder: "folder-1".into(),
        name: "bigfile.iso".into(),
        block_offset: 524_288,
        block_size: 524_288,
        block_hash: [0x42; 32],
    });
}

#[test]
fn encode_decode_response() {
    round_trip(BepMessage::Response {
        request_id: 42,
        data: vec![0xDE, 0xAD, 0xBE, 0xEF],
    });
}

#[test]
fn encode_decode_response_empty() {
    round_trip(BepMessage::Response {
        request_id: 1,
        data: vec![],
    });
}

#[test]
fn encode_decode_ping() {
    round_trip(BepMessage::Ping);
}

#[test]
fn encode_decode_close() {
    round_trip(BepMessage::Close {
        reason: "shutdown".into(),
    });
}

#[test]
fn encode_decode_gossip() {
    round_trip(BepMessage::Gossip {
        peers: vec![
            GossipPeer {
                device_id: "AAAA".to_string(),
                addresses: vec!["1.2.3.4:5000".to_string()],
                snapshot_unix_seconds: 1_700_000_000,
            },
            GossipPeer {
                device_id: "BBBB".to_string(),
                addresses: vec!["10.0.0.1:22000".to_string(), "[fe80::1]:22000".to_string()],
                snapshot_unix_seconds: 1_700_000_100,
            },
        ],
    });
}

#[test]
fn encode_decode_gossip_empty() {
    round_trip(BepMessage::Gossip { peers: vec![] });
}

#[test]
fn decode_gossip_rejects_excessive_peer_count() {
    // Hand-build a frame whose declared peer count exceeds the cap.
    // The body length prefix is honest (matches the trailing body
    // bytes) but the inner gossip count tries to make the receiver
    // allocate a 20k-entry vector — exactly the kind of frame the
    // bound is there to reject.
    let mut body = Vec::new();
    encode_u32(&mut body, MSG_GOSSIP);
    encode_u32(&mut body, 20_000);
    let mut frame = Vec::new();
    let body_len = u32::try_from(body.len()).unwrap_or(0);
    encode_u32(&mut frame, body_len);
    frame.extend_from_slice(&body);
    let result = decode_message(&frame);
    assert!(result.is_err(), "excessive peer count must fail");
    assert!(
        result
            .err()
            .map(|e| e.to_string())
            .is_some_and(|msg| msg.contains("exceeds maximum")),
        "error should mention the cap",
    );
}

#[test]
fn decode_gossip_rejects_excessive_address_count() {
    let mut body = Vec::new();
    encode_u32(&mut body, MSG_GOSSIP);
    encode_u32(&mut body, 1);
    // String encoding is infallible for short inputs; ignore the
    // Result to avoid an unwrap that clippy would flag at test
    // build time.
    let _ = encode_string(&mut body, "PEER-A");
    encode_u32(&mut body, 100); // > MAX_GOSSIP_ADDRESSES_PER_PEER
    let mut frame = Vec::new();
    let body_len = u32::try_from(body.len()).unwrap_or(0);
    encode_u32(&mut frame, body_len);
    frame.extend_from_slice(&body);
    let result = decode_message(&frame);
    assert!(result.is_err(), "excessive address count must fail");
    assert!(
        result
            .err()
            .map(|e| e.to_string())
            .is_some_and(|msg| msg.contains("exceeding maximum")),
        "error should mention the cap",
    );
}

#[test]
fn decode_invalid_frame_length() {
    let result = decode_message(&[0, 0]);
    assert!(result.is_err());
}

#[test]
fn decode_unknown_message_type() {
    let mut frame = Vec::new();
    // Body: msg type 99 (unknown), no further data.
    let mut body = Vec::new();
    encode_u32(&mut body, 99);
    let body_len = u32::try_from(body.len()).unwrap_or(0);
    encode_u32(&mut frame, body_len);
    frame.extend_from_slice(&body);

    let result = decode_message(&frame);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("unknown message type")
    );
}

#[test]
fn decode_truncated_body() {
    let mut frame = Vec::new();
    encode_u32(&mut frame, 100); // claim 100 bytes of body
    frame.extend_from_slice(&[0, 0, 0, 1]); // only 4 bytes present

    let result = decode_message(&frame);
    assert!(result.is_err());
}

// ── Version vector semantics ──

#[test]
fn version_dominates_self_is_false_for_equal() {
    let a = Version {
        counters: vec![(1, 2), (2, 3)],
    };
    let b = a.clone();
    assert!(!a.dominates(&b), "equal vectors do not dominate");
    assert!(!b.dominates(&a));
    assert_eq!(a, b);
}

#[test]
fn version_dominates_strictly_greater() {
    let a = Version {
        counters: vec![(1, 5)],
    };
    let b = Version {
        counters: vec![(1, 2)],
    };
    assert!(a.dominates(&b));
    assert!(!b.dominates(&a));
}

#[test]
fn version_dominates_with_extra_device() {
    // a has an entry b does not — a covers strictly more history.
    let a = Version {
        counters: vec![(1, 1), (2, 4)],
    };
    let b = Version {
        counters: vec![(2, 4)],
    };
    assert!(a.dominates(&b));
    assert!(!b.dominates(&a));
}

#[test]
fn version_dominates_concurrent_returns_false_both_ways() {
    // Device 1 advanced only in a; device 2 advanced only in b.
    let a = Version {
        counters: vec![(1, 1)],
    };
    let b = Version {
        counters: vec![(2, 1)],
    };
    assert!(!a.dominates(&b));
    assert!(!b.dominates(&a));
    assert_ne!(a, b);
}

#[test]
fn version_merge_takes_per_device_max() {
    let mut a = Version {
        counters: vec![(1, 5), (2, 3)],
    };
    let b = Version {
        counters: vec![(1, 2), (3, 9)],
    };
    a.merge(&b);
    assert_eq!(a.counters, vec![(1, 5), (2, 3), (3, 9)]);
}

#[test]
fn version_bump_appends_or_increments() {
    let mut v = Version::default();
    v.bump(42);
    assert_eq!(v.counters, vec![(42, 1)]);
    v.bump(42);
    assert_eq!(v.counters, vec![(42, 2)]);
    v.bump(7);
    // Sorted ascending by device id.
    assert_eq!(v.counters, vec![(7, 1), (42, 2)]);
}

#[test]
fn version_bump_keeps_counters_sorted() {
    let mut v = Version::default();
    v.bump(100);
    v.bump(5);
    v.bump(50);
    let ids: Vec<u64> = v.counters.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, vec![5, 50, 100]);
}

// ── Candidate / SyncPunch wire format ──

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::candidate::{Candidate, CandidateKind, compute_priority};

fn v4(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7)), port)
}

fn v6(port: u16) -> SocketAddr {
    SocketAddr::new(
        IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
        port,
    )
}

#[test]
fn encode_decode_candidates_empty() {
    round_trip(BepMessage::Candidates { candidates: vec![] });
}

#[test]
fn encode_decode_candidates_single_ipv4_host() {
    round_trip(BepMessage::Candidates {
        candidates: vec![Candidate::new(v4(22000), CandidateKind::Host, 65_535)],
    });
}

#[test]
fn encode_decode_candidates_mixed_kinds_and_families() {
    round_trip(BepMessage::Candidates {
        candidates: vec![
            Candidate::new(v4(22000), CandidateKind::Host, 65_535),
            Candidate::new(v6(22000), CandidateKind::Host, 65_534),
            Candidate::new(v4(54321), CandidateKind::ServerReflexive, 0),
            Candidate::new(v6(54321), CandidateKind::ServerReflexive, 0),
            Candidate::new(v4(3478), CandidateKind::Relayed, 0),
        ],
    });
}

#[test]
fn encode_decode_candidates_at_cap() {
    // Exactly MAX_CANDIDATES_PER_FRAME entries must encode and
    // decode cleanly; one above the cap is exercised separately.
    let mut candidates = Vec::with_capacity(MAX_CANDIDATES_PER_FRAME as usize);
    for i in 0..MAX_CANDIDATES_PER_FRAME {
        let port = u16::try_from(20_000 + i).unwrap_or(u16::MAX);
        candidates.push(Candidate::new(v4(port), CandidateKind::Host, 65_535));
    }
    round_trip(BepMessage::Candidates { candidates });
}

#[test]
fn encode_candidates_rejects_overflow() {
    // Build a frame with one more candidate than allowed and verify
    // the encoder refuses it. Catches over-eager local code paths
    // (the decoder cap is tested separately below).
    let cap_plus_one = (MAX_CANDIDATES_PER_FRAME + 1) as usize;
    let candidates: Vec<Candidate> = (0..cap_plus_one)
        .map(|i| {
            let port = u16::try_from(20_000 + i).unwrap_or(u16::MAX);
            Candidate::new(v4(port), CandidateKind::Host, 0)
        })
        .collect();
    let err = encode_message(&BepMessage::Candidates { candidates })
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(err.contains("exceeds maximum"), "got: {err}");
}

#[test]
fn decode_candidates_rejects_excessive_count() {
    // Hand-build a frame whose declared candidate count exceeds the
    // cap so the receiver does not allocate an attacker-chosen
    // amount of memory.
    let mut body = Vec::new();
    encode_u32(&mut body, MSG_CANDIDATES);
    encode_u32(&mut body, MAX_CANDIDATES_PER_FRAME + 1);
    let mut frame = Vec::new();
    let body_len = u32::try_from(body.len()).unwrap_or(0);
    encode_u32(&mut frame, body_len);
    frame.extend_from_slice(&body);
    let result = decode_message(&frame);
    assert!(result.is_err(), "excessive candidate count must fail");
    assert!(
        result
            .err()
            .map(|e| e.to_string())
            .is_some_and(|msg| msg.contains("exceeds maximum")),
        "error should mention the cap",
    );
}

#[test]
fn decode_candidate_rejects_unknown_kind_tag() {
    // Build a candidates frame with a single entry whose kind tag
    // does not match any known `CandidateKind`. The decoder must
    // refuse rather than fall back to a default.
    let mut body = Vec::new();
    encode_u32(&mut body, MSG_CANDIDATES);
    encode_u32(&mut body, 1);
    encode_u32(&mut body, 9); // unknown kind tag
    encode_u32(&mut body, 0); // priority
    encode_u32(&mut body, u32::from(ADDR_FAMILY_IPV4));
    encode_u32(&mut body, 22_000);
    let _ = encode_opaque(&mut body, &[127, 0, 0, 1]);
    let mut frame = Vec::new();
    let body_len = u32::try_from(body.len()).unwrap_or(0);
    encode_u32(&mut frame, body_len);
    frame.extend_from_slice(&body);
    let result = decode_message(&frame);
    assert!(result.is_err(), "unknown kind tag must fail");
}

#[test]
fn encode_decode_sync_punch_round_trip() {
    round_trip(BepMessage::SyncPunch {
        nonce: 0xDEAD_BEEF_CAFE_F00D,
        deadline_unix_ms: 1_700_000_000_000,
    });
}

#[test]
fn encode_decode_sync_punch_zero_values() {
    round_trip(BepMessage::SyncPunch {
        nonce: 0,
        deadline_unix_ms: 0,
    });
}

#[test]
fn encode_decode_sync_punch_extremes() {
    round_trip(BepMessage::SyncPunch {
        nonce: u64::MAX,
        deadline_unix_ms: u64::MAX,
    });
}

#[test]
fn encode_decode_observed_address_ipv4() {
    round_trip(BepMessage::ObservedAddress(v4(54_321)));
}

#[test]
fn encode_decode_observed_address_ipv6() {
    round_trip(BepMessage::ObservedAddress(v6(54_321)));
}

#[test]
fn encode_decode_observed_address_zero_port() {
    round_trip(BepMessage::ObservedAddress(v4(0)));
}

#[test]
fn encode_decode_relay_offer_round_trip() {
    round_trip(BepMessage::RelayOffer {
        addresses: vec![v4(22_000), v6(22_000)],
    });
}

#[test]
fn encode_decode_relay_offer_empty() {
    round_trip(BepMessage::RelayOffer { addresses: vec![] });
}

#[test]
fn decode_relay_offer_rejects_excessive_address_count() {
    // Honest body length prefix, but the inner address count exceeds
    // the cap — the receiver must refuse to allocate the oversized
    // vector rather than trusting the declared count.
    let mut body = Vec::new();
    encode_u32(&mut body, MSG_RELAY_OFFER);
    encode_u32(&mut body, MAX_RELAY_OFFER_ADDRESSES + 1);
    let mut frame = Vec::new();
    let body_len = u32::try_from(body.len()).unwrap_or(0);
    encode_u32(&mut frame, body_len);
    frame.extend_from_slice(&body);
    let result = decode_message(&frame);
    assert!(result.is_err(), "excessive address count must fail");
    assert!(
        result
            .err()
            .map(|e| e.to_string())
            .is_some_and(|msg| msg.contains("exceeds maximum")),
        "error should mention the cap",
    );
}

#[test]
fn encode_decode_relay_connect_round_trip() {
    round_trip(BepMessage::RelayConnect {
        target_device: "TARGET-DEVICE-ID".into(),
    });
}

#[test]
fn encode_decode_relay_data_round_trip() {
    round_trip(BepMessage::RelayData {
        payload: vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01],
    });
}

#[test]
fn encode_decode_relay_data_empty() {
    round_trip(BepMessage::RelayData { payload: vec![] });
}

#[test]
fn encode_decode_relay_inbound_round_trip() {
    round_trip(BepMessage::RelayInbound {
        source_device: "SOURCE-DEVICE-ID".into(),
    });
}

// ── Management-plane frames ──

#[test]
fn encode_decode_manage_request_status_read_node_scope() {
    round_trip(BepMessage::ManageRequest {
        request_id: 7,
        command: ManageCommand::StatusRead,
        scope: ManageScope::Node,
        token: None,
    });
}

#[test]
fn encode_decode_manage_request_pin_folder_scope() {
    round_trip(BepMessage::ManageRequest {
        request_id: 0xDEAD_BEEF,
        command: ManageCommand::Pin {
            path_glob: "/work/reports/*.pdf".into(),
            recursive: true,
        },
        scope: ManageScope::Folder {
            path: "/work".into(),
        },
        token: None,
    });
}

#[test]
fn encode_decode_manage_request_unpin() {
    round_trip(BepMessage::ManageRequest {
        request_id: 1,
        command: ManageCommand::Unpin {
            path_glob: "/work/old".into(),
        },
        scope: ManageScope::Folder {
            path: "/work".into(),
        },
        token: None,
    });
}

#[test]
fn encode_decode_manage_request_cache_evict() {
    round_trip(BepMessage::ManageRequest {
        request_id: u64::MAX,
        command: ManageCommand::CacheEvict,
        scope: ManageScope::Node,
        token: None,
    });
}

#[test]
fn encode_decode_manage_response_ok() {
    round_trip(BepMessage::ManageResponse {
        request_id: 42,
        result: ManageResult::Ok {
            summary: "evicted 3 files, freed 1024 bytes".into(),
        },
    });
}

#[test]
fn encode_decode_manage_response_unauthorised() {
    round_trip(BepMessage::ManageResponse {
        request_id: 42,
        result: ManageResult::Err {
            kind: ManageErrorKind::Unauthorised,
            message: "caller MANAGER lacks pin:write over /work".into(),
        },
    });
}

#[test]
fn encode_decode_manage_response_failed() {
    round_trip(BepMessage::ManageResponse {
        request_id: 99,
        result: ManageResult::Err {
            kind: ManageErrorKind::Failed,
            message: "pin matcher rejected glob".into(),
        },
    });
}

#[test]
fn encode_decode_manage_request_cache_warm() {
    round_trip(BepMessage::ManageRequest {
        request_id: 3,
        command: ManageCommand::CacheWarm {
            path_glob: "/work/**".into(),
        },
        scope: ManageScope::Folder {
            path: "/work".into(),
        },
        token: None,
    });
}

#[test]
fn encode_decode_manage_request_config_push_every_format() {
    for format in [
        ManageConfigFormat::Gitignore,
        ManageConfigFormat::Toml,
        ManageConfigFormat::Yaml,
        ManageConfigFormat::Json,
    ] {
        round_trip(BepMessage::ManageRequest {
            request_id: 11,
            command: ManageCommand::ConfigPush {
                format,
                folder: "/work".into(),
                body: "ignore = []\n".into(),
            },
            scope: ManageScope::Folder {
                path: "/work".into(),
            },
            token: None,
        });
    }
}

#[test]
fn encode_decode_manage_request_policy_set_full_and_unbounded() {
    round_trip(BepMessage::ManageRequest {
        request_id: 12,
        command: ManageCommand::PolicySet {
            path_glob: "/work/*.tmp".into(),
            max_age_secs: Some(86_400),
            max_file_size: Some(1_048_576),
            priority: 5,
        },
        scope: ManageScope::Folder {
            path: "/work".into(),
        },
        token: None,
    });
    // Both dimensions unbounded — the None sentinels must round-trip
    // distinctly from a zero value.
    round_trip(BepMessage::ManageRequest {
        request_id: 13,
        command: ManageCommand::PolicySet {
            path_glob: "/work".into(),
            max_age_secs: None,
            max_file_size: None,
            priority: -3,
        },
        scope: ManageScope::Folder {
            path: "/work".into(),
        },
        token: None,
    });
}

#[test]
fn encode_decode_manage_request_backend_add() {
    round_trip(BepMessage::ManageRequest {
        request_id: 14,
        command: ManageCommand::BackendAdd {
            name: "personal".into(),
            backend_type: "gdrive".into(),
            mount_path: "/drive".into(),
            config_toml: "type = \"gdrive\"\nclient_id = \"abc\"\n".into(),
        },
        scope: ManageScope::Folder {
            path: "/drive".into(),
        },
        token: None,
    });
}

#[test]
fn encode_decode_manage_request_backend_remove() {
    round_trip(BepMessage::ManageRequest {
        request_id: 15,
        command: ManageCommand::BackendRemove {
            name: "personal".into(),
            mount_path: "/drive".into(),
        },
        scope: ManageScope::Folder {
            path: "/drive".into(),
        },
        token: None,
    });
}

#[test]
fn encode_decode_manage_request_restart_and_stop() {
    round_trip(BepMessage::ManageRequest {
        request_id: 16,
        command: ManageCommand::Restart,
        scope: ManageScope::Folder {
            path: "/work".into(),
        },
        token: None,
    });
    round_trip(BepMessage::ManageRequest {
        request_id: 17,
        command: ManageCommand::Stop,
        scope: ManageScope::Folder {
            path: "/work".into(),
        },
        token: None,
    });
}

#[test]
fn encode_decode_manage_request_grant_add_with_and_without_expiry() {
    round_trip(BepMessage::ManageRequest {
        request_id: 18,
        command: ManageCommand::GrantAdd {
            grant: ManageGrant {
                grantee: "SUBORDINATE".into(),
                capability: "pin:write".into(),
                scope: ManageScope::Folder {
                    path: "/work/reports".into(),
                },
                expires: Some("2026-12-31T00:00:00Z".into()),
            },
        },
        scope: ManageScope::Folder {
            path: "/work/reports".into(),
        },
        token: None,
    });
    round_trip(BepMessage::ManageRequest {
        request_id: 19,
        command: ManageCommand::GrantAdd {
            grant: ManageGrant {
                grantee: "SUBORDINATE".into(),
                capability: "status:read".into(),
                scope: ManageScope::Node,
                expires: None,
            },
        },
        scope: ManageScope::Node,
        token: None,
    });
}

#[test]
fn encode_decode_manage_request_grant_revoke() {
    round_trip(BepMessage::ManageRequest {
        request_id: 20,
        command: ManageCommand::GrantRevoke {
            grant_id: 42,
            scope: ManageScope::Folder {
                path: "/work".into(),
            },
        },
        scope: ManageScope::Folder {
            path: "/work".into(),
        },
        token: None,
    });
}

#[test]
fn encode_decode_manage_request_with_presented_token() {
    // A ManageRequest carrying a presented capability token (its opaque JSON
    // form) must round-trip the token field intact.
    round_trip(BepMessage::ManageRequest {
        request_id: 99,
        command: ManageCommand::Pin {
            path_glob: "/work/reports".into(),
            recursive: true,
        },
        scope: ManageScope::Folder {
            path: "/work".into(),
        },
        token: Some("{\"claims\":{\"token_id\":\"t1\"}}".into()),
    });
}

#[test]
fn decode_manage_request_rejects_unknown_config_format_tag() {
    let mut body = Vec::new();
    encode_u32(&mut body, MSG_MANAGE_REQUEST);
    encode_u64(&mut body, 1);
    encode_u32(&mut body, MANAGE_CMD_CONFIG_PUSH);
    encode_u32(&mut body, 99); // unknown config format tag
    let mut frame = Vec::new();
    let body_len = u32::try_from(body.len()).unwrap_or(0);
    encode_u32(&mut frame, body_len);
    frame.extend_from_slice(&body);
    let result = decode_message(&frame);
    assert!(result.is_err(), "unknown config format tag must fail");
}

#[test]
fn decode_manage_request_rejects_bad_option_sentinel() {
    // A PolicySet whose first option sentinel is neither 0 nor 1 must be
    // rejected rather than mis-parsed.
    let mut body = Vec::new();
    encode_u32(&mut body, MSG_MANAGE_REQUEST);
    encode_u64(&mut body, 1);
    encode_u32(&mut body, MANAGE_CMD_POLICY_SET);
    encode_string(&mut body, "/work").unwrap();
    encode_u32(&mut body, 7); // invalid option sentinel
    let mut frame = Vec::new();
    let body_len = u32::try_from(body.len()).unwrap_or(0);
    encode_u32(&mut frame, body_len);
    frame.extend_from_slice(&body);
    let result = decode_message(&frame);
    assert!(result.is_err(), "invalid option sentinel must fail");
}

#[test]
fn decode_manage_request_rejects_unknown_command_tag() {
    let mut body = Vec::new();
    encode_u32(&mut body, MSG_MANAGE_REQUEST);
    encode_u64(&mut body, 1);
    encode_u32(&mut body, 99); // unknown command tag
    let mut frame = Vec::new();
    let body_len = u32::try_from(body.len()).unwrap_or(0);
    encode_u32(&mut frame, body_len);
    frame.extend_from_slice(&body);
    let result = decode_message(&frame);
    assert!(result.is_err(), "unknown manage command tag must fail");
    assert!(
        result
            .err()
            .map(|e| e.to_string())
            .is_some_and(|msg| msg.contains("unknown manage command tag")),
        "error should name the unknown command tag",
    );
}

#[test]
fn decode_manage_request_rejects_unknown_scope_tag() {
    let mut body = Vec::new();
    encode_u32(&mut body, MSG_MANAGE_REQUEST);
    encode_u64(&mut body, 1);
    encode_u32(&mut body, MANAGE_CMD_STATUS_READ);
    encode_u32(&mut body, 99); // unknown scope tag
    let mut frame = Vec::new();
    let body_len = u32::try_from(body.len()).unwrap_or(0);
    encode_u32(&mut frame, body_len);
    frame.extend_from_slice(&body);
    let result = decode_message(&frame);
    assert!(result.is_err(), "unknown manage scope tag must fail");
}

#[test]
fn decode_manage_response_rejects_unknown_result_tag() {
    let mut body = Vec::new();
    encode_u32(&mut body, MSG_MANAGE_RESPONSE);
    encode_u64(&mut body, 1);
    encode_u32(&mut body, 99); // unknown result tag
    let mut frame = Vec::new();
    let body_len = u32::try_from(body.len()).unwrap_or(0);
    encode_u32(&mut frame, body_len);
    frame.extend_from_slice(&body);
    let result = decode_message(&frame);
    assert!(result.is_err(), "unknown manage result tag must fail");
}

#[test]
fn candidate_priority_preserved_through_wire() {
    // The wire encodes the precomputed priority. A round-trip
    // through `encode_message` / `decode_message` must preserve
    // the exact value so the recipient can sort pair candidates
    // without recomputing.
    let priority = compute_priority(CandidateKind::ServerReflexive, 42);
    let candidate = Candidate {
        address: v4(22_001),
        kind: CandidateKind::ServerReflexive,
        priority,
    };
    let msg = BepMessage::Candidates {
        candidates: vec![candidate],
    };
    let decoded = decode_message(&encode_message(&msg).unwrap()).unwrap();
    match decoded {
        BepMessage::Candidates { candidates } => {
            assert_eq!(candidates.len(), 1);
            let got = candidates.first().unwrap();
            assert_eq!(got.priority, priority);
            assert_eq!(got.kind, CandidateKind::ServerReflexive);
            assert_eq!(got.address, v4(22_001));
        }
        other => panic!("decoded wrong variant: {other:?}"),
    }
}

// ── Handshake + exec frames ──

#[test]
fn encode_decode_handshake_round_trip() {
    round_trip(BepMessage::Handshake {
        protocol_version: PROTOCOL_VERSION,
        domains: vec![
            CapabilityDomain::Content,
            CapabilityDomain::Management,
            CapabilityDomain::Exec,
        ],
    });
}

#[test]
fn encode_decode_handshake_empty_domains() {
    round_trip(BepMessage::Handshake {
        protocol_version: PROTOCOL_VERSION,
        domains: Vec::new(),
    });
}

#[test]
fn handshake_drops_unknown_domain_discriminant() {
    // A future peer advertising a domain this version does not know must have
    // that domain dropped on decode, never assumed.
    let mut body = Vec::new();
    encode_u32(&mut body, MSG_HANDSHAKE);
    encode_u32(&mut body, PROTOCOL_VERSION);
    encode_u32(&mut body, 2); // two domains
    encode_u32(&mut body, CAP_DOMAIN_EXEC);
    encode_u32(&mut body, 9999); // unknown
    let body_len = u32::try_from(body.len()).unwrap();
    let mut frame = Vec::new();
    encode_u32(&mut frame, body_len);
    frame.extend_from_slice(&body);
    let decoded = decode_message(&frame).unwrap();
    match decoded {
        BepMessage::Handshake { domains, .. } => {
            assert_eq!(domains, vec![CapabilityDomain::Exec]);
        }
        other => panic!("decoded wrong variant: {other:?}"),
    }
}

#[test]
fn capability_domain_wire_form_round_trips() {
    for domain in [
        CapabilityDomain::Content,
        CapabilityDomain::Management,
        CapabilityDomain::Exec,
        CapabilityDomain::Oplog,
    ] {
        assert_eq!(CapabilityDomain::from_wire(domain.as_wire()), Some(domain));
        assert_eq!(
            CapabilityDomain::from_wire_tag(domain.wire_tag()),
            Some(domain)
        );
    }
    assert_eq!(CapabilityDomain::from_wire("nonsense"), None);
}

#[test]
fn encode_decode_exec_stream_round_trip() {
    round_trip(BepMessage::ExecStream {
        session: 42,
        seq: 7,
        stream: 1,
        bytes: b"hello world".to_vec(),
    });
    round_trip(BepMessage::ExecStream {
        session: u64::MAX,
        seq: 0,
        stream: 0,
        bytes: Vec::new(),
    });
}

#[test]
fn exec_stream_rejects_unknown_stream_kind() {
    let mut body = Vec::new();
    encode_u32(&mut body, MSG_EXEC_STREAM);
    encode_u64(&mut body, 1);
    encode_u64(&mut body, 0);
    encode_u32(&mut body, 99); // not stdin/stdout/stderr
    encode_opaque(&mut body, b"x").unwrap();
    let body_len = u32::try_from(body.len()).unwrap();
    let mut frame = Vec::new();
    encode_u32(&mut frame, body_len);
    frame.extend_from_slice(&body);
    assert!(decode_message(&frame).is_err());
}

#[test]
fn encode_decode_exec_stream_ack_round_trip() {
    round_trip(BepMessage::ExecStreamAck {
        session: 42,
        ack_seq: 100,
        window: 65_536,
    });
}

// ── Exec management commands ──

fn exec_request(command: ManageCommand) -> BepMessage {
    BepMessage::ManageRequest {
        request_id: 1,
        command,
        scope: ManageScope::Folder {
            path: "/work".to_owned(),
        },
        token: None,
    }
}

#[test]
fn encode_decode_pty_spawn_round_trip() {
    round_trip(exec_request(ManageCommand::PtySpawn {
        shell: Some("/bin/zsh".to_owned()),
        argv: vec!["-l".to_owned()],
        cwd: Some("/work".to_owned()),
        env: vec![
            ("TERM".to_owned(), "xterm-256color".to_owned()),
            ("LANG".to_owned(), "en_GB.UTF-8".to_owned()),
        ],
        cols: 120,
        rows: 40,
    }));
    // No shell, no argv, no cwd, empty env.
    round_trip(exec_request(ManageCommand::PtySpawn {
        shell: None,
        argv: Vec::new(),
        cwd: None,
        env: Vec::new(),
        cols: 80,
        rows: 24,
    }));
}

#[test]
fn encode_decode_pty_write_round_trip() {
    round_trip(exec_request(ManageCommand::PtyWrite {
        session: 7,
        bytes: b"ls -la\n".to_vec(),
    }));
}

#[test]
fn encode_decode_pty_resize_round_trip() {
    round_trip(exec_request(ManageCommand::PtyResize {
        session: 7,
        cols: 200,
        rows: 50,
    }));
}

#[test]
fn encode_decode_pty_kill_round_trip() {
    round_trip(exec_request(ManageCommand::PtyKill {
        session: 7,
        signal: 15,
    }));
}

#[test]
fn encode_decode_proc_spawn_round_trip() {
    round_trip(exec_request(ManageCommand::ProcSpawn {
        argv: vec![
            "/usr/bin/env".to_owned(),
            "node".to_owned(),
            "x.js".to_owned(),
        ],
        cwd: Some("/work/app".to_owned()),
        env: vec![("NODE_ENV".to_owned(), "production".to_owned())],
    }));
}

#[test]
fn encode_decode_proc_signal_and_kill_round_trip() {
    round_trip(exec_request(ManageCommand::ProcSignal {
        session: 9,
        signal: 9,
    }));
    round_trip(exec_request(ManageCommand::ProcKill { session: 9 }));
}

#[test]
fn encode_decode_exec_spawned_result_round_trip() {
    round_trip(BepMessage::ManageResponse {
        request_id: 3,
        result: ManageResult::ExecSpawned { session: 123 },
    });
}
