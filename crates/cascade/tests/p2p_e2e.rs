//! End-to-end P2P data path test.
//!
//! Exercises the full path: create two P2P engines, index a file on one,
//! share block data between their block stores (simulating P2P transfer),
//! and verify the second engine can reassemble the data.

use cascade_engine::db::StateDb;
use cascade_engine::p2p_bridge::P2pBridge;
use cascade_engine::types::{FileEntry, ItemId};
use cascade_p2p::P2pEngine;
use std::sync::Arc;

/// 128 KB block size — matches the P2P engine's default for files under 250 MB.
const BLOCK_128KB: usize = 128 * 1024;

/// Full P2P data path: index → share → reassemble.
///
/// This test verifies the data path works end-to-end when blocks are
/// available in the local store, without real networking.
#[tokio::test]
async fn e2e_p2p_data_path() {
    // ── Setup: two engines with separate temp dirs ──
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();

    let engine_a = P2pEngine::new(dir_a.path()).await.unwrap();
    let engine_b = P2pEngine::new(dir_b.path()).await.unwrap();

    let db_a = Arc::new(StateDb::open_in_memory().unwrap());
    let db_b = Arc::new(StateDb::open_in_memory().unwrap());

    // Register backends so we can create file entries.
    db_a.register_backend("p2p", "p2p", "P2P-A", None, None)
        .unwrap();
    db_b.register_backend("p2p", "p2p", "P2P-B", None, None)
        .unwrap();

    let bridge_a = P2pBridge::new(engine_a, db_a);
    let bridge_b = P2pBridge::new(engine_b, db_b);

    // Verify both engines have distinct device IDs.
    assert_ne!(bridge_a.device_id(), bridge_b.device_id());

    // ── Step 1: Index a test file on engine A ──
    let test_data = build_test_data();
    let file_id = ItemId::new("p2p", "e2e-test.bin");
    let blocks = bridge_a
        .index_file_by_id(&file_id, &test_data)
        .await
        .unwrap();

    assert_eq!(blocks.size, test_data.len() as u64);
    assert!(
        blocks.block_count() >= 3,
        "test data should span multiple blocks"
    );

    // ── Step 2: Share blocks from A's store to B's store ──
    // Simulate a peer transfer by copying each block from A's store to B's.
    let store_a = bridge_a.block_store();
    let store_b = bridge_b.block_store();

    for block_hash in &blocks.blocks {
        let block_data = store_a
            .get_block(block_hash)
            .await
            .unwrap()
            .expect("block must exist in engine A's store");
        store_b.store_block(block_hash, &block_data).await.unwrap();
    }

    // ── Step 3: Record the block index in engine B's DB ──
    // (In production, this would happen via BEP Index exchange.)
    let file_id_b = ItemId::new("p2p", "e2e-test.bin");
    // Re-index in B's store (store_block is idempotent, blocks already present).
    let blocks_b = bridge_b
        .index_file_by_id(&file_id_b, &test_data)
        .await
        .unwrap();
    assert_eq!(blocks_b.block_count(), blocks.block_count());

    // ── Step 4: Fetch from engine B ──
    let file = FileEntry::file(
        file_id_b.clone(),
        ItemId::new("p2p", "root"),
        "e2e-test.bin".into(),
    )
    .with_size(Some(test_data.len() as u64));

    let result = bridge_b.try_fetch_from_peers(&file).await.unwrap();
    let fetched = result.expect("engine B should be able to reassemble the file");

    // ── Step 5: Verify data integrity ──
    assert_eq!(fetched.len(), test_data.len());
    assert_eq!(fetched, test_data);
}

/// Verify that a zero-length file can be indexed and queried.
#[tokio::test]
async fn e2e_empty_file() {
    let dir = tempfile::tempdir().unwrap();
    let engine = P2pEngine::new(dir.path()).await.unwrap();
    let db = Arc::new(StateDb::open_in_memory().unwrap());
    db.register_backend("p2p", "p2p", "P2P", None, None)
        .unwrap();
    let bridge = P2pBridge::new(engine, db);

    let file_id = ItemId::new("p2p", "empty.txt");
    let blocks = bridge.index_file_by_id(&file_id, b"").await.unwrap();
    assert_eq!(blocks.block_count(), 0);
    assert_eq!(blocks.size, 0);
}

/// Verify that multiple files can be indexed independently and their
/// blocks don't interfere.
#[tokio::test]
async fn e2e_multiple_files_independent_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let engine = P2pEngine::new(dir.path()).await.unwrap();
    let db = Arc::new(StateDb::open_in_memory().unwrap());
    db.register_backend("p2p", "p2p", "P2P", None, None)
        .unwrap();
    let bridge = P2pBridge::new(engine, db);

    let data_a = vec![0x11; BLOCK_128KB + 100];
    let data_b = vec![0x22; BLOCK_128KB * 2 + 200];

    let id_a = ItemId::new("p2p", "file_a.bin");
    let id_b = ItemId::new("p2p", "file_b.bin");

    let blocks_a = bridge.index_file_by_id(&id_a, &data_a).await.unwrap();
    let blocks_b = bridge.index_file_by_id(&id_b, &data_b).await.unwrap();

    // Different block counts.
    assert_ne!(blocks_a.block_count(), blocks_b.block_count());

    // Fetch each independently.
    let file_a = FileEntry::file(
        id_a.clone(),
        ItemId::new("p2p", "root"),
        "file_a.bin".into(),
    )
    .with_size(Some(data_a.len() as u64));
    let file_b = FileEntry::file(
        id_b.clone(),
        ItemId::new("p2p", "root"),
        "file_b.bin".into(),
    )
    .with_size(Some(data_b.len() as u64));

    let fetched_a = bridge.try_fetch_from_peers(&file_a).await.unwrap().unwrap();
    let fetched_b = bridge.try_fetch_from_peers(&file_b).await.unwrap().unwrap();

    assert_eq!(fetched_a, data_a);
    assert_eq!(fetched_b, data_b);
}

/// Build test data spanning several blocks (~384 KB).
fn build_test_data() -> Vec<u8> {
    // Three full blocks + a partial.
    let size = BLOCK_128KB * 3 + 7890;
    let mut data = Vec::with_capacity(size);
    for i in 0..size {
        data.push((i % 251) as u8); // PRIME_MOD to get varied byte values.
    }
    data
}
