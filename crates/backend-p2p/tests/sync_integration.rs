//! Multi-engine integration tests for the P2P backend.
//!
//! These tests exercise the public [`Backend`] surface against two or
//! three [`P2pBackend`] instances communicating over loopback TCP +
//! mutual TLS — exactly the wire path the daemon uses, just without
//! Docker. The Docker compose tests in `test/e2e/p2p/` cover the same
//! scenarios against multiple real OS network stacks.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use cascade_backend_p2p::sync::Peer;
use cascade_backend_p2p::{P2pBackend, P2pBackendConfig};
use cascade_engine::backend::Backend;
use cascade_engine::types::FileId;
use tempfile::TempDir;

/// One backend instance with its own tempdir, ready to be used in a
/// multi-peer scenario.
struct Node {
    _dir: TempDir,
    backend: Arc<P2pBackend>,
}

impl Node {
    async fn new(name: &str, folder_id: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cfg = P2pBackendConfig {
            instance_id: format!("p2p-{name}"),
            folder_id: folder_id.to_string(),
            display_name: name.to_string(),
            index_path: dir.path().join("index.db"),
            block_store_root: dir.path().join("blocks"),
            identity_dir: dir.path().join("identity"),
            ..Default::default()
        };
        let backend = Arc::new(P2pBackend::open(cfg).unwrap());
        Self { _dir: dir, backend }
    }

    fn device_id(&self) -> String {
        self.backend.sync().device_id().to_string()
    }
}

/// Trust both ways between two nodes.
async fn mutual_trust(a: &Node, b: &Node) {
    a.backend.sync().trust(b.device_id()).await;
    b.backend.sync().trust(a.device_id()).await;
}

/// Start a listener on `node` and connect `peer` to it.
async fn connect_via_listener(server: &Node, client: &Node) {
    let (addr, _) = server
        .backend
        .sync()
        .start_listener("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    client
        .backend
        .sync()
        .connect_to(Peer {
            device_id: server.device_id(),
            address: addr,
        })
        .await
        .unwrap();
}

/// Spin until the given index has an entry for `name` or the deadline
/// passes. Returns the entry's size on success.
async fn wait_for_file(node: &Node, name: &str) -> Option<u64> {
    let id = format!("{}:{name}", node.backend.id());
    for _ in 0..60 {
        if let Ok(entry) = node
            .backend
            .metadata(Path::new(name))
            .await
            .map(|e| e.size.unwrap_or(0))
        {
            return Some(entry);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = id; // suppress unused-warn on slow paths
    }
    None
}

/// Three-peer star: A is the hub, B and C connect to it. An upload on
/// A should appear in both B and C's indexes via IndexUpdate.
#[tokio::test]
async fn three_peer_index_propagation() {
    let a = Node::new("a", "shared").await;
    let b = Node::new("b", "shared").await;
    let c = Node::new("c", "shared").await;

    mutual_trust(&a, &b).await;
    mutual_trust(&a, &c).await;

    // A listens; B and C dial in.
    let (addr_a, _task) = a
        .backend
        .sync()
        .start_listener("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    b.backend
        .sync()
        .connect_to(Peer {
            device_id: a.device_id(),
            address: addr_a,
        })
        .await
        .unwrap();
    c.backend
        .sync()
        .connect_to(Peer {
            device_id: a.device_id(),
            address: addr_a,
        })
        .await
        .unwrap();

    // Let handshakes settle.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Upload on A.
    let payload = b"three-peer test".repeat(20);
    let mut reader = Cursor::new(payload.clone());
    a.backend
        .upload(
            Path::new("hub.txt"),
            &mut reader,
            &FileId(format!("{}:root", a.backend.id())),
        )
        .await
        .unwrap();

    // Both B and C should observe it via the IndexUpdate broadcast.
    assert_eq!(
        wait_for_file(&b, "hub.txt").await,
        Some(payload.len() as u64),
        "B never received hub.txt",
    );
    assert_eq!(
        wait_for_file(&c, "hub.txt").await,
        Some(payload.len() as u64),
        "C never received hub.txt",
    );

    // Both B and C should be able to download by pulling blocks from A.
    let entry_b = b.backend.metadata(Path::new("hub.txt")).await.unwrap();
    let mut out_b = Vec::new();
    b.backend.download(&entry_b, &mut out_b).await.unwrap();
    assert_eq!(out_b, payload);

    let entry_c = c.backend.metadata(Path::new("hub.txt")).await.unwrap();
    let mut out_c = Vec::new();
    c.backend.download(&entry_c, &mut out_c).await.unwrap();
    assert_eq!(out_c, payload);
}

/// Two peers upload the same path. The later upload (wall-clock
/// `modified`) wins on both sides.
#[tokio::test]
async fn last_write_wins_conflict() {
    let a = Node::new("a", "shared").await;
    let b = Node::new("b", "shared").await;
    mutual_trust(&a, &b).await;
    connect_via_listener(&a, &b).await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    // A uploads first.
    let early = b"early".to_vec();
    let mut r = Cursor::new(early.clone());
    a.backend
        .upload(
            Path::new("doc.txt"),
            &mut r,
            &FileId(format!("{}:root", a.backend.id())),
        )
        .await
        .unwrap();

    // Make sure B sees A's version (so we know B's later upload is
    // unambiguously newer in wall-clock order).
    assert!(wait_for_file(&b, "doc.txt").await.is_some());

    // Bump wall clock at least one second so the LWW comparison is
    // unambiguous — Index stores `modified` as Unix seconds.
    tokio::time::sleep(Duration::from_millis(1100)).await;

    let late = b"late and longer".to_vec();
    let mut r = Cursor::new(late.clone());
    b.backend
        .upload(
            Path::new("doc.txt"),
            &mut r,
            &FileId(format!("{}:root", b.backend.id())),
        )
        .await
        .unwrap();

    // Both ends should converge on the later content.
    for _ in 0..60 {
        let on_a = a.backend.metadata(Path::new("doc.txt")).await.unwrap();
        if on_a.size == Some(late.len() as u64) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let on_a = a.backend.metadata(Path::new("doc.txt")).await.unwrap();
    assert_eq!(on_a.size, Some(late.len() as u64));
    let on_b = b.backend.metadata(Path::new("doc.txt")).await.unwrap();
    assert_eq!(on_b.size, Some(late.len() as u64));
}

/// Documented behaviour: a delete on one device is not propagated to
/// peers in this phase (BEP v1 has no tombstone field). The deleted
/// row stays as an "invisible" tombstone locally; the remote peer
/// still sees the file.
#[tokio::test]
async fn deletes_do_not_propagate() {
    let a = Node::new("a", "shared").await;
    let b = Node::new("b", "shared").await;
    mutual_trust(&a, &b).await;
    connect_via_listener(&a, &b).await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    let payload = b"to be deleted".to_vec();
    let mut r = Cursor::new(payload.clone());
    let entry = a
        .backend
        .upload(
            Path::new("ephemeral.txt"),
            &mut r,
            &FileId(format!("{}:root", a.backend.id())),
        )
        .await
        .unwrap();
    assert!(wait_for_file(&b, "ephemeral.txt").await.is_some());

    a.backend.delete(&entry).await.unwrap();

    // A's local listing should hide it.
    let kids_a = a.backend.list_children("root").await.unwrap();
    assert!(!kids_a.iter().any(|e| e.name == "ephemeral.txt"));

    // B should still see it — tombstones don't cross the wire (yet).
    tokio::time::sleep(Duration::from_millis(300)).await;
    let on_b = b.backend.metadata(Path::new("ephemeral.txt")).await;
    assert!(
        on_b.is_ok(),
        "B should still see the file (deletes don't propagate in phase 3)",
    );
}

/// Block-fetch fallback: A has a block, B does not. B's `download`
/// must satisfy the read by pulling the block from A.
#[tokio::test]
async fn download_pulls_missing_blocks_from_peer() {
    let a = Node::new("a", "shared").await;
    let b = Node::new("b", "shared").await;
    mutual_trust(&a, &b).await;
    connect_via_listener(&a, &b).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let payload = vec![0xCDu8; 200 * 1024]; // > one 128KB block
    let mut r = Cursor::new(payload.clone());
    a.backend
        .upload(
            Path::new("big.bin"),
            &mut r,
            &FileId(format!("{}:root", a.backend.id())),
        )
        .await
        .unwrap();

    // Wait for B to learn about the file via IndexUpdate.
    assert_eq!(
        wait_for_file(&b, "big.bin").await,
        Some(payload.len() as u64),
    );

    // B has the index entry but no blocks. Download must pull them
    // from A.
    let entry = b.backend.metadata(Path::new("big.bin")).await.unwrap();
    let mut out = Vec::new();
    b.backend.download(&entry, &mut out).await.unwrap();
    assert_eq!(out, payload);
}
