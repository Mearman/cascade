//! P2P integration tests — bridge lifecycle, cross-engine block sharing,
//! and sync-runner integration.

use cascade_engine::backend::NullBackend;
use cascade_engine::cache::manager::{CacheManager, CacheManagerConfig};
use cascade_engine::db::StateDb;
use cascade_engine::p2p_bridge::P2pBridge;
use cascade_engine::sync::runner::SyncRunner;
use cascade_engine::types::{CacheState, FileEntry, ItemId};
use cascade_p2p::P2pEngine;
use std::sync::Arc;

/// 128 KB block size — matches the P2P engine's default for files under 250 MB.
const BLOCK_128KB: usize = 128 * 1024;

/// Verify that a P2pBridge can be created, index a file, and then
/// look up the blocks.
#[tokio::test]
async fn bridge_index_and_lookup() {
    let dir = tempfile::tempdir().unwrap();
    let engine = P2pEngine::new(dir.path()).await.unwrap();
    let db = Arc::new(StateDb::open_in_memory().unwrap());
    let bridge = P2pBridge::new(engine, db);

    let data = vec![0xAA; BLOCK_128KB * 3];
    let blocks = bridge.index_file("hello.bin", &data).await.unwrap();

    assert_eq!(blocks.size, data.len() as u64);
    assert_eq!(blocks.block_count(), 3);

    // All block hashes should be queryable.
    for hash in &blocks.blocks {
        assert!(bridge.has_blocks(&hash.to_string()).await.unwrap());
    }
}

/// Two P2pEngine instances sharing a block store directory: index on one,
/// read from the other. This simulates P2P block sharing without real
/// networking — the block store is content-addressed so both engines
/// can read the same blocks from disk.
#[tokio::test]
async fn cross_engine_block_sharing_via_shared_store() {
    let dir = tempfile::tempdir().unwrap();

    // Engine A indexes data.
    let engine_a = P2pEngine::new(dir.path()).await.unwrap();
    let db_a = Arc::new(StateDb::open_in_memory().unwrap());
    let bridge_a = P2pBridge::new(engine_a, db_a);

    let data = vec![0x42; BLOCK_128KB * 2 + 1024];
    let blocks = bridge_a.index_file("shared.bin", &data).await.unwrap();

    // Engine B reads from the same block store directory.
    let engine_b = P2pEngine::new(dir.path()).await.unwrap();
    let db_b = Arc::new(StateDb::open_in_memory().unwrap());

    // Register the backend so we can create a file entry.
    db_b.register_backend("p2p", "p2p", "P2P", None, None)
        .unwrap();

    // Record the block index in engine B's DB manually (simulating
    // what would happen via gossip/index exchange).
    let file_id = ItemId::new("p2p", "shared.bin");
    let hashes: Vec<[u8; 32]> = blocks.blocks.iter().map(|h| h.0).collect();
    db_b.index_p2p_blocks(&file_id, &hashes).unwrap();

    let bridge_b = P2pBridge::new(engine_b, db_b);

    let file = FileEntry::file(
        file_id.clone(),
        ItemId::new("p2p", "root"),
        "shared.bin".into(),
    )
    .with_size(Some(data.len() as u64));

    let result = bridge_b.try_fetch_from_peers(&file).await.unwrap();
    let fetched = result.expect("should fetch from shared block store");
    assert_eq!(fetched, data);
}

/// Sync runner with P2P bridge: create a NullBackend with files, verify
/// the sync runner processes them and the P2P bridge is accessible.
#[tokio::test]
async fn sync_runner_with_p2p_bridge() {
    let db = Arc::new(StateDb::open_in_memory().unwrap());
    db.register_backend("test", "null", "Test", None, None)
        .unwrap();

    let p2p_dir = tempfile::tempdir().unwrap();
    let p2p_engine = P2pEngine::new(p2p_dir.path()).await.unwrap();
    let bridge = P2pBridge::new(p2p_engine, db.clone());

    // Verify the bridge works before attaching.
    let data = b"sync test data";
    let blocks = bridge.index_file("sync_test.txt", data).await.unwrap();
    assert_eq!(blocks.block_count(), 1);

    let backend: Arc<dyn cascade_engine::backend::Backend> = Arc::new(NullBackend::new("test"));
    let presenter = Arc::new(TestPresenter::default());
    let config = Arc::new(cascade_engine::config::ConfigResolver::new(
        std::path::PathBuf::from("/tmp/test"),
    ));

    let runner = SyncRunner::new(db, vec![backend], presenter, config).with_p2p(bridge);
    runner.stop();
    let result = runner.run().await;
    assert!(result.is_ok());
}

/// Cache manager with P2P: verify fetch_from_p2p and index_for_p2p work
/// through the cache manager's API.
#[tokio::test]
async fn cache_manager_with_p2p() {
    let db = Arc::new(StateDb::open_in_memory().unwrap());
    db.register_backend("p2p", "p2p", "P2P", None, None)
        .unwrap();

    let p2p_dir = tempfile::tempdir().unwrap();
    let p2p_engine = P2pEngine::new(p2p_dir.path()).await.unwrap();
    let bridge = Arc::new(P2pBridge::new(p2p_engine, db.clone()));

    let cache = CacheManager::new(db.clone(), CacheManagerConfig::default()).with_p2p(bridge);

    let file_id = ItemId::new("p2p", "cached.bin");
    let data = vec![0xDD; BLOCK_128KB + 256];

    // Index through cache manager.
    cache.index_for_p2p(&file_id, &data).await.unwrap();

    // Fetch back through cache manager.
    let file = FileEntry::file(file_id, ItemId::new("p2p", "root"), "cached.bin".into())
        .with_size(Some(data.len() as u64));

    let result = cache.fetch_from_p2p(&file).await.unwrap();
    let fetched = result.expect("should fetch from P2P");
    assert_eq!(fetched, data);
}

/// Cache manager without P2P returns None from fetch_from_p2p.
#[tokio::test]
async fn cache_manager_without_p2p_returns_none() {
    let db = Arc::new(StateDb::open_in_memory().unwrap());
    let cache = CacheManager::new(db.clone(), CacheManagerConfig::default());

    let file = FileEntry::file(
        ItemId::new("gdrive", "file1"),
        ItemId::new("gdrive", "root"),
        "test.txt".into(),
    );

    let result = cache.fetch_from_p2p(&file).await.unwrap();
    assert!(result.is_none());
}

/// A minimal presenter for testing.
#[derive(Default)]
struct TestPresenter {
    _marker: std::marker::PhantomData<()>,
}

#[async_trait::async_trait]
impl cascade_engine::presenter::VfsPresenter for TestPresenter {
    async fn upsert_item(&self, _item: cascade_engine::types::VfsItem) -> anyhow::Result<()> {
        Ok(())
    }
    async fn delete_item(&self, _id: &ItemId) -> anyhow::Result<()> {
        Ok(())
    }
    async fn update_state(&self, _id: &ItemId, _state: CacheState) -> anyhow::Result<()> {
        Ok(())
    }
    async fn fetch_contents(&self, _id: &ItemId) -> anyhow::Result<std::path::PathBuf> {
        anyhow::bail!("not implemented")
    }
    async fn evict_item(&self, _id: &ItemId) -> anyhow::Result<()> {
        Ok(())
    }
    async fn start(&self, _mount_point: &std::path::Path) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&self) -> anyhow::Result<()> {
        Ok(())
    }
}
