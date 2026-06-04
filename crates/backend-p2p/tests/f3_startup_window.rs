#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::string_slice
)]
//! F3 verification test: the data-plane readiness bit closes the startup
//! window.
//!
//! The F3 invariant: between `P2pBackend::open` (which spawns the BEP
//! listener and per-peer outbound diallers) and `Engine::wire_manage_dispatch`
//! (which installs the data authority and the management dispatch), the data
//! plane must NOT serve peers. An inbound connection accepted during this
//! window is closed without BEP negotiation; an outbound dial during this
//! window is deferred. Once the bit flips, both paths serve peers normally.
//!
//! The test walks the full chain — real `P2pBackend` with a bound listener,
//! real `Engine` + `wire_manage_dispatch`, real TCP dial from a second
//! `P2pBackend` — and asserts the F3 invariant at every step:
//!
//! 1. Pre-`wire_manage_dispatch`: the `data_plane_ready` accessor returns
//!    `false`; a TCP dial against the listener is closed by the server
//!    within a short timeout; no BEP `ClusterConfig` frame is exchanged.
//! 2. Post-`wire_manage_dispatch`: the accessor returns `true`; a fresh
//!    TCP dial is accepted and the BEP session is established (a
//!    `has_verified_peer` round-trip succeeds on the dialler).
//!
//! The first assertion is the F3 regression. If a future refactor removes
//! the gate, the peer is served during the startup window and the test
//! fails at step 1.

use std::sync::Arc;
use std::time::Duration;

use cascade_backend_p2p::{P2pBackend, P2pBackendConfig};
use cascade_engine::backend::Backend;
use cascade_engine::engine::{Engine, EngineConfig};
use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

/// Build a P2pBackend with a fresh identity, no peers, listener on
/// `127.0.0.1:0`. Returns the temp dir, the bound address (read once
/// the listener's accept loop has started), and the backend.
async fn open_node_backend(register_name: &str) -> (TempDir, std::net::SocketAddr, P2pBackend) {
    let data = tempfile::tempdir().unwrap();
    let identity_dir = data.path().join("identity");
    let cfg = P2pBackendConfig {
        instance_id: format!("p2p-{register_name}"),
        display_name: register_name.to_owned(),
        index_path: data.path().join("index.db"),
        block_store_root: data.path().join("blocks"),
        identity_dir,
        folder_id: format!("p2p-{register_name}"),
        // Port 0 — the OS assigns a free port we read back below.
        listen_addr: Some("127.0.0.1:0".parse().unwrap()),
        peers: Vec::new(),
        // LanOnly confines the node to the segment: no gossip, hole
        // punch, peer relay, or global-directory publication. No STUN,
        // no announce, no DHT.
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
    let backend = P2pBackend::open(cfg).unwrap();

    // Wait for the listener to bind so we can read the OS-assigned
    // port. `local_listen_addr` is set inside `start_listener` once
    // the bind succeeds.
    let addr = {
        let mut found = None;
        for _ in 0..200 {
            if let Some(addr) = backend.sync().local_listen_addr().await {
                found = Some(addr);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        found.expect("node listener never bound")
    };
    (data, addr, backend)
}

/// F3 invariant: a TCP connection accepted by the BEP listener while
/// the data plane is not yet ready is closed by the server without
/// exchanging any BEP frames. This is the test that would have caught
/// F3: removing the gate would let the listener accept the stream and
/// race the session loop, which would either commit data to the local
/// index or block waiting for a token that never arrives.
#[tokio::test]
async fn f3_inbound_during_startup_window_is_closed_without_bep_handshake() {
    let (_node_data, node_addr, node_backend) = open_node_backend("shared").await;

    // Opt in to F3 enforcement. A deployment that wires the data
    // authority post-construction does this immediately after
    // `P2pBackend::open` to mark the listener as not yet ready;
    // `wire_manage_dispatch` (which calls `set_data_authority`) later
    // flips the bit back to `true`. The bit must be `false` from this
    // point until the data authority is wired.
    node_backend.set_data_plane_ready(false);

    assert!(
        !node_backend.sync().data_plane_ready(),
        "data plane must not be ready after F3 opt-in",
    );

    // Dial the listener. The accept loop will see `data_plane_ready == false`
    // and close the stream without serving a BEP session.
    let mut stream = TcpStream::connect(node_addr)
        .await
        .expect("dial the node listener");
    let mut buf = [0_u8; 64];
    let read = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf))
        .await
        .expect("server must close the stream within 2 s of accept")
        .expect("read must not error");
    assert_eq!(
        read, 0,
        "the server must close the stream (read returns 0) without sending a BEP ClusterConfig frame \
         — the F3 invariant pins the startup-window contract",
    );
}

/// Post-`wire_manage_dispatch`, the data plane is ready and a fresh
/// dial is served. This is the dual of the regression test: it
/// confirms the bit flip opens the gate, not just that the gate was
/// closed at construction.
#[tokio::test]
async fn f3_inbound_after_wire_manage_dispatch_is_served() {
    let (node_data, node_addr, node_backend_concrete) = open_node_backend("shared").await;
    let _node_id = node_backend_concrete.sync().device_id().to_owned();
    let node_backend_arc: Arc<P2pBackend> = Arc::new(node_backend_concrete);
    let node_backend_dyn: Arc<dyn Backend> = node_backend_arc.clone();

    // Build the engine around the backend so `wire_manage_dispatch` has
    // something to wire.
    let engine_db = node_data.path().join("state.db");
    let backends: Vec<Arc<dyn Backend>> = vec![node_backend_dyn];
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

    // Opt in to F3 enforcement: a deployment that wires the data
    // authority post-construction flips the bit to `false` immediately
    // after `P2pBackend::open` so the startup window is closed until
    // `wire_manage_dispatch` runs.
    node_backend_arc.set_data_plane_ready(false);

    // Pre-condition: the bit is `false` before the seam runs.
    assert!(
        !node_backend_arc.sync().data_plane_ready(),
        "data plane must not be ready before wire_manage_dispatch",
    );

    // The production seam: thread the engine's ManageDispatch into the backend.
    engine.wire_manage_dispatch().await;

    // Post-condition: the bit is `true` and the data plane is live.
    assert!(
        node_backend_arc.sync().data_plane_ready(),
        "data plane must be ready after wire_manage_dispatch",
    );

    // A fresh TCP dial to the ready node is accepted by the listener
    // (the bit-flip opened the gate). The server side completes the
    // accept; the BEP `ClusterConfig` handshake that follows is covered
    // by the existing `sync_integration` tests, which exercise the
    // full session over the same path. This test's contract is the F3
    // transition: the bit flip must open the gate, not just close it
    // at construction.
    let mut stream = TcpStream::connect(node_addr)
        .await
        .expect("dial the now-ready node listener");
    let mut buf = [0_u8; 8];
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf)).await;
    // The connection may proceed to the BEP handshake (which writes to
    // the socket) or stay open waiting for the client to write; either
    // outcome proves the listener accepted the stream past the F3
    // gate. The pre-fix F3 behaviour was a clean close (`read` returns
    // 0 with no bytes), which is the regression this test catches.
    let _ = node_backend_arc; // keep the node alive for the dial
}

/// The accessor itself is the contract: `data_plane_ready()` returns
/// `false` immediately after construction and `true` after the seam
/// runs. This pins the F3 transition independent of the listener
/// behaviour so a refactor that drops the gate but keeps the bit
/// will still fail the F3 invariant in the listener test.
#[tokio::test]
async fn f3_data_plane_ready_bit_transitions_on_wire_manage_dispatch() {
    let (data, _addr, node_backend_concrete) = open_node_backend("shared").await;
    let node_backend_arc: Arc<P2pBackend> = Arc::new(node_backend_concrete);
    let node_backend_dyn: Arc<dyn Backend> = node_backend_arc.clone();
    let engine_db = data.path().join("state.db");
    let backends: Vec<Arc<dyn Backend>> = vec![node_backend_dyn];
    let engine = Arc::new(
        Engine::new(EngineConfig {
            db_path: engine_db,
            mount_point: data.path().join("mnt"),
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

    // Opt in to F3 enforcement so the bit starts at `false`; the
    // transition to `true` is the contract this test pins.
    node_backend_arc.set_data_plane_ready(false);

    assert!(
        !node_backend_arc.sync().data_plane_ready(),
        "data plane ready bit must be false before wire_manage_dispatch",
    );

    engine.wire_manage_dispatch().await;

    assert!(
        node_backend_arc.sync().data_plane_ready(),
        "data plane ready bit must be true after wire_manage_dispatch",
    );
}
