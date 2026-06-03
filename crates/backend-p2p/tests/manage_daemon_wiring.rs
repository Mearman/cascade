#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::string_slice
)]
//! End-to-end management-plane wiring through the production daemon path.
//!
//! Where `manage_roundtrip.rs` injects a hand-wired `ManageDispatch` double
//! straight onto a bare `SyncEngine`, this test proves the *daemon* path: a
//! real [`P2pBackend`] is built (the listener spun up exactly as `cascade start`
//! does), registered into an [`Engine`], and the engine's own
//! [`ManageDispatch`] is threaded in through
//! [`Engine::wire_manage_dispatch`] — the single seam the daemon uses. A
//! manager then dials the node's listener over loopback TCP + mutual TLS and
//! sends the same `BepMessage::ManageRequest` the CLI's `cascade remote
//! <device-id> status` puts on the wire.
//!
//! Without the wiring the node's `manage_dispatch` slot stays `None` and every
//! request is refused with `Unauthorised "node is not accepting remote
//! management"`. The assertions below — an authorised command succeeds, an
//! unauthorised one is refused by the real grant check — can only hold if the
//! engine dispatcher reached the backend through the production seam.

use std::sync::Arc;

use cascade_backend_p2p::sync::{Peer, SyncEngine};
use cascade_backend_p2p::{ConfiguredPeer, P2pBackend, P2pBackendConfig};
use cascade_engine::backend::Backend;
use cascade_engine::engine::{Engine, EngineConfig};
use cascade_engine::manage::{Capability, DeviceId, Grant, Scope};
use cascade_p2p::identity::DeviceIdentity;
use cascade_p2p::protocol::{ManageCommand, ManageErrorKind, ManageResult, ManageScope};
use tempfile::TempDir;

/// Build a bare manager-side [`SyncEngine`] — it only needs to dial the node and
/// send a request, so a full backend is unnecessary on this side.
fn make_manager(folder_id: &str) -> (TempDir, SyncEngine) {
    let dir = tempfile::tempdir().unwrap();
    let index = Arc::new(
        cascade_backend_p2p::index::FolderIndex::open(&dir.path().join("index.db")).unwrap(),
    );
    let blocks = Arc::new(cascade_p2p::store::BlockStore::new(&dir.path().join("blocks")).unwrap());
    let identity = DeviceIdentity::load_or_generate(&dir.path().join("identity")).unwrap();
    let engine = SyncEngine::new(folder_id.to_owned(), index, blocks, identity);
    (dir, engine)
}

/// Read the device id a [`P2pBackend`] would load from its identity directory,
/// without opening the backend, so the node can be configured to trust the
/// manager before either is opened.
fn device_id_of(identity_dir: &std::path::Path) -> String {
    DeviceIdentity::load_or_generate(identity_dir)
        .unwrap()
        .device_id
}

/// A grant of `capability` over `scope` for `grantee`, issued by `owner`.
fn grant(grantee: &str, capability: Capability, scope: Scope, owner: &str) -> Grant {
    Grant {
        grantee: DeviceId::new(grantee.to_owned()),
        capability,
        scope,
        granted_by: DeviceId::new(owner.to_owned()),
        expires: None,
    }
}

#[tokio::test]
async fn daemon_wired_node_accepts_authorised_management_over_the_wire() {
    // ── Manager identity, fixed up-front so the node can trust it ──
    let (_manager_dir, manager) = make_manager("shared");
    let manager_id = manager.device_id().to_owned();

    // ── Node: a real P2pBackend with a listener, trusting the manager ──
    let node_data = tempfile::tempdir().unwrap();
    let node_identity_dir = node_data.path().join("identity");
    let node_id = device_id_of(&node_identity_dir);

    let node_cfg = P2pBackendConfig {
        instance_id: "p2p-node".to_owned(),
        display_name: "Node".to_owned(),
        index_path: node_data.path().join("index.db"),
        block_store_root: node_data.path().join("blocks"),
        identity_dir: node_identity_dir,
        folder_id: "shared".to_owned(),
        // Port 0 — the OS assigns a free port we read back below.
        listen_addr: Some("127.0.0.1:0".parse().unwrap()),
        // The node trusts the manager so the inbound TLS handshake is accepted.
        peers: vec![ConfiguredPeer {
            device_id: manager_id.clone(),
            address: "127.0.0.1:0".to_owned(),
            name: None,
        }],
        // Keep the test hermetic: LanOnly confines the node to the segment —
        // no gossip, hole punch, peer relay, or global-directory publication.
        // No STUN servers and no announce/DHT config either.
        exposure: cascade_backend_p2p::DiscoveryReach::LanOnly,
        device_name: None,
        stun_servers: Vec::new(),
        announce_servers: Vec::new(),
        dht: cascade_backend_p2p::DhtConfig::default(),
        relay_endpoints: Vec::new(),
        relay_shared_secret: None,
        relay_volunteer: cascade_backend_p2p::RelayVolunteer::Off,
        max_relay_sessions: cascade_backend_p2p::DEFAULT_MAX_RELAY_SESSIONS,
        max_relay_bandwidth: cascade_backend_p2p::DEFAULT_MAX_RELAY_BANDWIDTH_BYTES_PER_SEC,
    };
    let node_backend = P2pBackend::open(node_cfg).unwrap();

    // Read the OS-assigned listener port once the background bind completes.
    let node_addr = {
        let mut found = None;
        for _ in 0..200 {
            if let Some(addr) = node_backend.sync().local_listen_addr().await {
                found = Some(addr);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        found.expect("node listener never bound")
    };

    // ── Engine around the node backend, with a grant for the manager ──
    let engine_db = node_data.path().join("state.db");
    let backends: Vec<Arc<dyn Backend>> = vec![Arc::new(node_backend)];
    let engine = Arc::new(
        Engine::new(EngineConfig {
            db_path: engine_db,
            mount_point: node_data.path().join("mnt"),
            backends,
            cache_dir: None,
            enable_p2p: false,
            p2p_data_dir: None,
            p2p_posture: None,
            p2p_relay_endpoints: Vec::new(),
            p2p_relay_shared_secret: None,
            backend_factory: None,
        })
        .unwrap(),
    );
    // The node owner grants the manager status:read node-wide — exactly what
    // `cascade grant add <manager> status:read` persists.
    engine
        .db()
        .insert_grant(&grant(
            &manager_id,
            Capability::StatusRead,
            Scope::Node,
            &node_id,
        ))
        .unwrap();

    // The production seam: thread the engine's ManageDispatch into the backend.
    engine.wire_manage_dispatch().await;

    // ── Manager dials the node directly and sends the request ──
    manager.trust(node_id.clone()).await;
    manager
        .connect_to(Peer {
            device_id: node_id.clone(),
            address: node_addr,
        })
        .await
        .unwrap();

    for _ in 0..200 {
        if manager.has_verified_peer(&node_id).await {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        manager.has_verified_peer(&node_id).await,
        "manager never established a TLS-verified session with the node",
    );

    // ── Authorised StatusRead — must succeed, proving the dispatcher ran ──
    let status = manager
        .send_manage_request(&node_id, ManageCommand::StatusRead, ManageScope::Node, None)
        .await
        .expect("status round-trip should not fail at the transport");
    assert!(
        matches!(status, ManageResult::Ok { .. }),
        "a wired node must authorise + run status:read, got {status:?}",
    );

    // ── Unwired command (no grant) — the real grant check refuses it ──
    // CacheEvict needs cache:manage, which the manager was never granted, so a
    // genuine authorisation denial (not the "not accepting management" refusal)
    // confirms the request reached the engine's grant logic.
    let denied = manager
        .send_manage_request(&node_id, ManageCommand::CacheEvict, ManageScope::Node, None)
        .await
        .expect("an unauthorised command still returns a typed reply");
    match denied {
        ManageResult::Err {
            kind: ManageErrorKind::Unauthorised,
            message,
        } => assert_ne!(
            message, "node is not accepting remote management",
            "the node must be accepting management; the denial must come from the grant check",
        ),
        other => panic!("expected an authorisation denial, got {other:?}"),
    }
}
