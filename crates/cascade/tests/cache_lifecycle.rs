#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::string_slice
)]
//! Integration tests: cache manager — pinning, eviction, lifecycle policies.

use cascade_engine::cache::lifecycle::{EvictionDecision, EvictionReason, LifecycleEvaluator};
use cascade_engine::cache::manager::{CacheManager, CacheManagerConfig};
use cascade_engine::cache::pin::PinMatcher;
use cascade_engine::db::StateDb;
use cascade_engine::portable::native::{SqliteStorage, TokioRuntimeHandle};
use cascade_engine::types::{CacheState, FileEntry, ItemId};
use std::path::Path;
use std::sync::Arc;

fn setup_db() -> Arc<StateDb> {
    let db = StateDb::open_in_memory().unwrap();
    db.register_backend("test", "test", "Test", None, None)
        .unwrap();
    Arc::new(db)
}

fn make_manager(db: Arc<StateDb>) -> CacheManager<TokioRuntimeHandle> {
    let runtime = TokioRuntimeHandle::current();
    let storage = SqliteStorage::new(db, runtime.clone());
    CacheManager::new(Arc::new(storage), runtime, CacheManagerConfig::default())
}

fn make_manager_with_config(
    db: Arc<StateDb>,
    config: CacheManagerConfig,
) -> CacheManager<TokioRuntimeHandle> {
    let runtime = TokioRuntimeHandle::current();
    let storage = SqliteStorage::new(db, runtime.clone());
    CacheManager::new(Arc::new(storage), runtime, config)
}

fn make_file(name: &str, id: &str, size: Option<u64>) -> FileEntry {
    FileEntry::file(
        ItemId::new("test", id),
        ItemId::new("test", "root"),
        name.to_string(),
    )
    .with_size(size)
}

#[tokio::test]
async fn pin_rule_adds_to_database() {
    let db = setup_db();
    let manager = make_manager(db);

    manager.pin("Documents", true).await.unwrap();

    let rules = manager.list_pins().await.unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].path_glob, "Documents");
    assert!(rules[0].recursive);
}

#[tokio::test]
async fn unpin_removes_rule() {
    let db = setup_db();
    let manager = make_manager(db);

    manager.pin("Documents", true).await.unwrap();
    assert!(manager.unpin("Documents").await.unwrap());
    assert!(!manager.unpin("Documents").await.unwrap()); // Already removed.

    let rules = manager.list_pins().await.unwrap();
    assert!(rules.is_empty());
}

#[test]
fn pin_matcher_matches_paths() {
    let db = setup_db();
    db.add_pin_rule("Documents", true, None).unwrap();
    db.add_pin_rule("Photos/img.jpg", false, None).unwrap();

    let matcher = PinMatcher::load_native(&db).unwrap();

    // Recursive rule.
    assert!(matcher.is_pinned(Path::new("Documents")));
    assert!(matcher.is_pinned(Path::new("Documents/report.pdf")));
    assert!(matcher.is_pinned(Path::new("Documents/Projects/code.rs")));

    // Non-recursive rule.
    assert!(matcher.is_pinned(Path::new("Photos/img.jpg")));
    assert!(!matcher.is_pinned(Path::new("Photos/other.jpg")));

    // Unrelated path.
    assert!(!matcher.is_pinned(Path::new("Music/song.mp3")));
}

#[test]
fn lifecycle_max_size_evicts_large_files() {
    let db = setup_db();
    db.add_lifecycle_policy("Documents/**", None, Some(1024), 0, None)
        .unwrap();

    let evaluator = LifecycleEvaluator::load_native(&db).unwrap();

    // Small file: keep.
    let small = make_file("tiny.txt", "f1", Some(512));
    assert_eq!(
        evaluator.should_evict(&small, Path::new("Documents/tiny.txt")),
        EvictionDecision::Keep
    );

    // Large file: evict.
    let large = make_file("big.bin", "f2", Some(2048));
    let decision = evaluator.should_evict(&large, Path::new("Documents/big.bin"));
    assert!(matches!(
        decision,
        EvictionDecision::Evict {
            reason: EvictionReason::MaxSize { .. }
        }
    ));
}

#[test]
fn lifecycle_no_matching_policy_means_keep() {
    let db = setup_db();
    db.add_lifecycle_policy("Temp/**", None, Some(0), 0, None)
        .unwrap();

    let evaluator = LifecycleEvaluator::load_native(&db).unwrap();
    let file = make_file("report.pdf", "f1", Some(2048));
    assert_eq!(
        evaluator.should_evict(&file, Path::new("Documents/report.pdf")),
        EvictionDecision::Keep
    );
}

#[tokio::test]
async fn cache_eviction_sweep_removes_cached_files() {
    let db = setup_db();

    // Insert files.
    let f1 = make_file("old.txt", "f1", Some(100));
    let f2 = make_file("kept.txt", "f2", Some(200));
    db.upsert_file(&f1).unwrap();
    db.upsert_file(&f2).unwrap();
    db.update_cache_state(&f1.id, CacheState::Cached).unwrap();
    db.update_cache_state(&f2.id, CacheState::Pinned).unwrap();

    // Add lifecycle policy that evicts everything in old/**.
    db.add_lifecycle_policy("old.*", None, Some(0), 0, None)
        .unwrap();

    let manager = make_manager(db.clone());
    let report = manager.evict().await.unwrap();

    assert_eq!(report.lifecycle_evicted, 1);
    assert_eq!(report.total_evicted(), 1);

    // old.txt should be back to online, kept.txt still pinned.
    assert_eq!(
        db.get_cache_state(&f1.id).unwrap(),
        Some(CacheState::Online)
    );
    assert_eq!(
        db.get_cache_state(&f2.id).unwrap(),
        Some(CacheState::Pinned)
    );
}

#[tokio::test]
async fn cache_stats_count_by_state() {
    let db = setup_db();

    let f1 = make_file("a.txt", "f1", Some(100));
    let f2 = make_file("b.txt", "f2", Some(200));
    let f3 = make_file("c.txt", "f3", Some(300));
    db.upsert_file(&f1).unwrap();
    db.upsert_file(&f2).unwrap();
    db.upsert_file(&f3).unwrap();
    db.update_cache_state(&f1.id, CacheState::Online).unwrap();
    db.update_cache_state(&f2.id, CacheState::Cached).unwrap();
    db.update_cache_state(&f3.id, CacheState::Pinned).unwrap();

    let manager = make_manager(db);
    let stats = manager.stats().await.unwrap();

    assert_eq!(stats.online_count, 1);
    assert_eq!(stats.cached_count, 1);
    assert_eq!(stats.pinned_count, 1);
    assert_eq!(stats.total_bytes, 500); // cached (200) + pinned (300)
}

#[tokio::test]
async fn size_based_eviction_frees_space() {
    let db = setup_db();

    let f1 = make_file("big.bin", "f1", Some(2000));
    let f2 = make_file("small.txt", "f2", Some(100));
    db.upsert_file(&f1).unwrap();
    db.upsert_file(&f2).unwrap();
    db.update_cache_state(&f1.id, CacheState::Cached).unwrap();
    db.update_cache_state(&f2.id, CacheState::Cached).unwrap();

    // Config: max 500 bytes. Total cache is 2100 bytes.
    let config = CacheManagerConfig {
        max_size: Some(500),
        ..CacheManagerConfig::default()
    };
    let manager = make_manager_with_config(db, config);
    let report = manager.evict().await.unwrap();

    // Should evict LRU files until under 500 bytes.
    assert!(report.size_evicted > 0);
    assert!(report.bytes_freed > 0);
}
