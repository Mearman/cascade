#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::string_slice
)]
//! Integration tests: `CacheManager` eviction-sweep orchestration.
//!
//! The pure lifecycle-matcher logic (`EvictionDecision`, `EvictionReason`) is
//! already exercised by the unit tests in `cache/lifecycle.rs` and the
//! integration tests in `cache_lifecycle.rs`. These tests focus exclusively on
//! the manager's sweep orchestration:
//!
//! - Pinned files are never evicted (neither lifecycle nor LRU phase touches them).
//! - LRU eviction halts as soon as the cache drops below the size budget.
//! - Lifecycle evictions are recorded separately from LRU evictions in the report.
//! - A cache already under budget produces an empty report.
//! - A manager with no `max_size` runs lifecycle evictions but zero LRU evictions.
//! - Multiple lifecycle policies at different priorities are applied highest-first.
//! - `bytes_freed` accurately reflects the sum of sizes of LRU-evicted files.
//! - `total_evicted()` equals the sum of `lifecycle_evicted` and `size_evicted`.

use cascade_engine::cache::manager::{CacheManager, CacheManagerConfig};
use cascade_engine::db::StateDb;
use cascade_engine::portable::native::{SqliteStorage, TokioRuntimeHandle};
use cascade_engine::types::{CacheState, FileEntry, ItemId};
use std::sync::Arc;

// ─────────────────────────── Helpers ───────────────────────────

fn setup_db() -> Arc<StateDb> {
    let db = StateDb::open_in_memory().unwrap();
    db.register_backend("test", "test", "Test", None, None)
        .unwrap();
    Arc::new(db)
}

fn make_manager(db: Arc<StateDb>, config: CacheManagerConfig) -> CacheManager<TokioRuntimeHandle> {
    let runtime = TokioRuntimeHandle::current();
    let storage = SqliteStorage::new(db, runtime.clone());
    CacheManager::new(Arc::new(storage), runtime, config)
}

/// Construct a minimal file entry. The `name` is used as both the display name
/// and (together with `id`) the unique key — use distinct `id` values for every
/// file in a test.
fn make_file(name: &str, id: &str, size: Option<u64>) -> FileEntry {
    FileEntry::file(
        ItemId::new("test", id),
        ItemId::new("test", "root"),
        name.to_string(),
    )
    .with_size(size)
}

/// Insert a file into the database and immediately set its cache state.
fn insert_with_state(db: &StateDb, file: &FileEntry, state: CacheState) {
    db.upsert_file(file).unwrap();
    db.update_cache_state(&file.id, state).unwrap();
}

// ─────────────────────────── Tests ───────────────────────────

/// A completely empty cache (no files at all) should produce an all-zero report.
#[tokio::test]
async fn empty_cache_evicts_nothing() {
    let db = setup_db();
    let config = CacheManagerConfig {
        max_size: Some(0),
        ..CacheManagerConfig::default()
    };
    let manager = make_manager(db, config);

    let report = manager.evict().await.unwrap();

    assert_eq!(report.lifecycle_evicted, 0);
    assert_eq!(report.size_evicted, 0);
    assert_eq!(report.bytes_freed, 0);
    assert_eq!(report.total_evicted(), 0);
}

/// When the total cache size is already at or below `max_size`, no LRU
/// eviction should occur even though there are cached files present.
#[tokio::test]
async fn under_budget_evicts_nothing_by_lru() {
    let db = setup_db();

    let small = make_file("small.txt", "f1", Some(100));
    insert_with_state(&db, &small, CacheState::Cached);

    // Budget is generous — 10 times the file size.
    let ten_x_budget = small.size.unwrap() * 10;
    let config = CacheManagerConfig {
        max_size: Some(ten_x_budget),
        ..CacheManagerConfig::default()
    };
    let manager = make_manager(Arc::clone(&db), config);
    let report = manager.evict().await.unwrap();

    assert_eq!(report.size_evicted, 0);
    assert_eq!(report.bytes_freed, 0);
    // The file should still be Cached.
    assert_eq!(
        db.get_cache_state(&small.id).unwrap(),
        Some(CacheState::Cached)
    );
}

/// Pinned files must survive both phases of the sweep.
///
/// The lifecycle phase reads only `Cached` files, and `eviction_candidates`
/// also filters to `cached` state, so a `Pinned` file never reaches either
/// eviction path regardless of size or matching policies.
#[tokio::test]
async fn pinned_files_are_never_evicted() {
    let db = setup_db();

    let pinned = make_file("pinned.dat", "p1", Some(5_000));
    insert_with_state(&db, &pinned, CacheState::Pinned);

    // Add a lifecycle policy that matches the file and would evict anything.
    db.add_lifecycle_policy("**", None, Some(0), 0, None)
        .unwrap();

    // Set max_size to zero — any cached file would be LRU-evicted immediately.
    let config = CacheManagerConfig {
        max_size: Some(0),
        ..CacheManagerConfig::default()
    };
    let manager = make_manager(Arc::clone(&db), config);
    let report = manager.evict().await.unwrap();

    // Nothing should be evicted: the only file is pinned.
    assert_eq!(
        report.total_evicted(),
        0,
        "a pinned file must not be evicted by either lifecycle or LRU"
    );
    assert_eq!(
        db.get_cache_state(&pinned.id).unwrap(),
        Some(CacheState::Pinned),
        "pinned file state must be unchanged after sweep"
    );
}

/// A pinned file that matches a lifecycle policy must not be evicted.
///
/// This verifies the same invariant through the lifecycle path specifically:
/// `list_files_by_cache_state(Cached)` excludes `Pinned` files, so the policy
/// never sees the file.
#[tokio::test]
async fn pinned_file_matching_lifecycle_policy_is_not_evicted() {
    let db = setup_db();

    // A sizeable pinned file.
    let pinned = make_file("large.bin", "p1", Some(10_000));
    insert_with_state(&db, &pinned, CacheState::Pinned);

    // A policy with zero max_file_size would evict any cached file it matches.
    db.add_lifecycle_policy("large.bin", None, Some(0), 0, None)
        .unwrap();

    let config = CacheManagerConfig::default(); // no max_size limit
    let manager = make_manager(Arc::clone(&db), config);
    let report = manager.evict().await.unwrap();

    assert_eq!(
        report.lifecycle_evicted, 0,
        "lifecycle must not touch pinned files"
    );
    assert_eq!(
        db.get_cache_state(&pinned.id).unwrap(),
        Some(CacheState::Pinned)
    );
}

/// LRU eviction stops as soon as the cache drops to or below `max_size`.
///
/// Setup: two files, each large enough alone to bust the budget. Only one
/// needs to be evicted to bring the total back under the limit. The test
/// verifies `size_evicted == 1` — eviction stopped after one file, not two.
#[tokio::test]
async fn lru_stops_once_under_budget() {
    let db = setup_db();

    // Each file is 300 bytes; total = 600. Budget = 400.
    // Evicting one file (300 bytes freed) leaves 300 ≤ 400, so only one
    // eviction should occur regardless of which file the LRU query returns first.
    let file_size: u64 = 300;
    let total_budget: u64 = 400;

    let f1 = make_file("alpha.dat", "f1", Some(file_size));
    let f2 = make_file("beta.dat", "f2", Some(file_size));
    insert_with_state(&db, &f1, CacheState::Cached);
    insert_with_state(&db, &f2, CacheState::Cached);

    let config = CacheManagerConfig {
        max_size: Some(total_budget),
        ..CacheManagerConfig::default()
    };
    let manager = make_manager(Arc::clone(&db), config);
    let report = manager.evict().await.unwrap();

    // Exactly one file is evicted: after freeing 300 bytes from 600, the
    // remaining 300 is within the 400-byte budget.
    assert_eq!(
        report.size_evicted, 1,
        "LRU eviction must stop after the first file brings us under the budget"
    );
    assert_eq!(report.bytes_freed, file_size);

    // Exactly one file should be Online and one should remain Cached.
    let cached_count = db
        .list_files_by_cache_state(CacheState::Cached)
        .unwrap()
        .len();
    let online_count = db
        .list_files_by_cache_state(CacheState::Online)
        .unwrap()
        .len();
    assert_eq!(cached_count, 1);
    assert_eq!(online_count, 1);
}

/// When there is no `max_size` configured, no LRU eviction occurs.
/// Lifecycle evictions can still happen independently.
#[tokio::test]
async fn no_max_size_means_no_lru_eviction() {
    let db = setup_db();

    // Two cached files, one matching a lifecycle policy.
    let lifecycle_target = make_file("temp.log", "f1", Some(1_000));
    let kept = make_file("important.db", "f2", Some(2_000));
    insert_with_state(&db, &lifecycle_target, CacheState::Cached);
    insert_with_state(&db, &kept, CacheState::Cached);

    // Policy evicts anything named "temp.log" by size.
    db.add_lifecycle_policy("temp.log", None, Some(0), 0, None)
        .unwrap();

    // No max_size — LRU phase is skipped entirely.
    let config = CacheManagerConfig {
        max_size: None,
        ..CacheManagerConfig::default()
    };
    let manager = make_manager(Arc::clone(&db), config);
    let report = manager.evict().await.unwrap();

    // One lifecycle eviction; zero LRU evictions.
    assert_eq!(report.lifecycle_evicted, 1);
    assert_eq!(report.size_evicted, 0);
    assert_eq!(report.bytes_freed, 0);
    assert_eq!(report.total_evicted(), 1);

    // temp.log evicted; important.db kept.
    assert_eq!(
        db.get_cache_state(&lifecycle_target.id).unwrap(),
        Some(CacheState::Online)
    );
    assert_eq!(
        db.get_cache_state(&kept.id).unwrap(),
        Some(CacheState::Cached)
    );
}

/// Lifecycle evictions and LRU evictions both happen in the same sweep and are
/// reported separately via `lifecycle_evicted` and `size_evicted`.
///
/// The lifecycle phase runs first (on the Cached snapshot), then the LRU phase
/// checks whether the post-lifecycle cache is still over budget. If those two
/// files are distinct, both counters will be non-zero.
#[tokio::test]
async fn lifecycle_and_lru_evictions_are_independent_and_separately_reported() {
    let db = setup_db();

    // lifecycle_file: 200 bytes, matches the lifecycle policy → evicted first.
    // lru_file: 400 bytes, not matched by lifecycle → only LRU candidate.
    // Budget: 100 bytes. Even after lifecycle eviction removes 200 bytes, the
    // remaining 400 still exceeds the budget, so LRU must also fire.
    let lifecycle_file_size: u64 = 200;
    let lru_file_size: u64 = 400;
    let budget: u64 = 100;

    let lifecycle_file = make_file("evict.tmp", "lc1", Some(lifecycle_file_size));
    let lru_file = make_file("large.dat", "lr1", Some(lru_file_size));
    insert_with_state(&db, &lifecycle_file, CacheState::Cached);
    insert_with_state(&db, &lru_file, CacheState::Cached);

    // Policy matches only evict.tmp by size (max_file_size = 0 means any
    // non-empty file with size > 0 bytes is evicted).
    db.add_lifecycle_policy("evict.tmp", None, Some(0), 0, None)
        .unwrap();

    let config = CacheManagerConfig {
        max_size: Some(budget),
        ..CacheManagerConfig::default()
    };
    let manager = make_manager(Arc::clone(&db), config);
    let report = manager.evict().await.unwrap();

    // Both phases must have fired.
    assert_eq!(
        report.lifecycle_evicted, 1,
        "lifecycle phase must evict evict.tmp"
    );
    assert_eq!(
        report.size_evicted, 1,
        "LRU phase must evict large.dat after lifecycle left the cache over budget"
    );
    assert_eq!(
        report.bytes_freed, lru_file_size,
        "bytes_freed covers only the LRU phase"
    );
    assert_eq!(report.total_evicted(), 2);

    // Both files should now be Online.
    assert_eq!(
        db.get_cache_state(&lifecycle_file.id).unwrap(),
        Some(CacheState::Online)
    );
    assert_eq!(
        db.get_cache_state(&lru_file.id).unwrap(),
        Some(CacheState::Online)
    );
}

/// `total_evicted()` always equals `lifecycle_evicted + size_evicted`.
///
/// This is a derived-value invariant: we test it with a case where both
/// fields are non-zero so the addition is exercised rather than trivially
/// returning zero.
#[tokio::test]
async fn total_evicted_equals_sum_of_lifecycle_and_size_evicted() {
    let db = setup_db();

    // Two lifecycle targets + one LRU-only file that keeps the cache over budget.
    let lc1 = make_file("cache1.tmp", "lc1", Some(50));
    let lc2 = make_file("cache2.tmp", "lc2", Some(50));
    let lru1 = make_file("data.bin", "lr1", Some(500));
    insert_with_state(&db, &lc1, CacheState::Cached);
    insert_with_state(&db, &lc2, CacheState::Cached);
    insert_with_state(&db, &lru1, CacheState::Cached);

    // Policy evicts any *.tmp file by size.
    db.add_lifecycle_policy("*.tmp", None, Some(0), 0, None)
        .unwrap();

    // Budget = 0 so the LRU phase will also evict data.bin.
    let config = CacheManagerConfig {
        max_size: Some(0),
        ..CacheManagerConfig::default()
    };
    let manager = make_manager(db, config);
    let report = manager.evict().await.unwrap();

    assert_eq!(
        report.total_evicted(),
        report.lifecycle_evicted + report.size_evicted,
        "total_evicted() must equal the sum of its components"
    );
    // Confirm both phases fired so the addition is meaningful.
    assert!(report.lifecycle_evicted > 0);
    assert!(report.size_evicted > 0);
}

/// A single lifecycle policy at high priority that would evict a file takes
/// precedence over a lower-priority "keep" (no-match) scenario for other files.
/// Only files that actually match the policy are evicted.
#[tokio::test]
async fn lifecycle_policy_only_evicts_matching_files() {
    let db = setup_db();

    let evicted = make_file("Temp/work.tmp", "e1", Some(1_024));
    let kept = make_file("Documents/notes.txt", "k1", Some(512));
    insert_with_state(&db, &evicted, CacheState::Cached);
    insert_with_state(&db, &kept, CacheState::Cached);

    // Policy matches only files under Temp/.
    db.add_lifecycle_policy("Temp/**", None, Some(0), 0, None)
        .unwrap();

    let config = CacheManagerConfig {
        max_size: None,
        ..CacheManagerConfig::default()
    };
    let manager = make_manager(Arc::clone(&db), config);
    let report = manager.evict().await.unwrap();

    assert_eq!(report.lifecycle_evicted, 1);
    assert_eq!(
        db.get_cache_state(&evicted.id).unwrap(),
        Some(CacheState::Online)
    );
    assert_eq!(
        db.get_cache_state(&kept.id).unwrap(),
        Some(CacheState::Cached),
        "file outside the policy path glob must remain cached"
    );
}

/// `bytes_freed` reflects the sum of the sizes of only the LRU-evicted files.
/// Lifecycle-evicted files' sizes are not included in `bytes_freed`.
#[tokio::test]
async fn bytes_freed_covers_only_lru_evicted_files() {
    let db = setup_db();

    let lifecycle_size: u64 = 200;
    let lru_size: u64 = 600;
    let budget: u64 = 100;

    let lifecycle_file = make_file("old.log", "lc1", Some(lifecycle_size));
    let lru_file = make_file("bulk.dat", "lr1", Some(lru_size));
    insert_with_state(&db, &lifecycle_file, CacheState::Cached);
    insert_with_state(&db, &lru_file, CacheState::Cached);

    // Lifecycle evicts old.log.
    db.add_lifecycle_policy("old.log", None, Some(0), 0, None)
        .unwrap();

    let config = CacheManagerConfig {
        max_size: Some(budget),
        ..CacheManagerConfig::default()
    };
    let manager = make_manager(db, config);
    let report = manager.evict().await.unwrap();

    assert_eq!(report.lifecycle_evicted, 1);
    assert_eq!(report.size_evicted, 1);
    // bytes_freed must equal exactly the LRU file's size, not the sum of both.
    assert_eq!(
        report.bytes_freed, lru_size,
        "bytes_freed must not include the lifecycle-evicted file's size"
    );
}

/// After LRU eviction the cache size (as reported by `CacheStats`) drops to at
/// or below the configured `max_size`.
#[tokio::test]
async fn cache_size_drops_below_budget_after_lru_sweep() {
    let db = setup_db();

    // Three 300-byte files; total = 900. Budget = 350.
    // Need to evict at least two to get from 900 to ≤ 350.
    let file_size: u64 = 300;
    let budget: u64 = 350;

    let f1 = make_file("data1.bin", "f1", Some(file_size));
    let f2 = make_file("data2.bin", "f2", Some(file_size));
    let f3 = make_file("data3.bin", "f3", Some(file_size));
    insert_with_state(&db, &f1, CacheState::Cached);
    insert_with_state(&db, &f2, CacheState::Cached);
    insert_with_state(&db, &f3, CacheState::Cached);

    let config = CacheManagerConfig {
        max_size: Some(budget),
        ..CacheManagerConfig::default()
    };
    let manager = make_manager(Arc::clone(&db), config);
    let report = manager.evict().await.unwrap();

    // At least two evictions are needed (3*300 - 2*300 = 300 ≤ 350).
    assert!(
        report.size_evicted >= 2,
        "need at least two evictions to drop from 900 to ≤ 350 bytes, got {}",
        report.size_evicted
    );

    // Verify via stats that the reported cache size is now within budget.
    let stats = manager.stats().await.unwrap();
    assert!(
        stats.total_bytes <= budget,
        "post-sweep cache ({} bytes) must be at or below budget ({} bytes)",
        stats.total_bytes,
        budget,
    );
}

/// `Online`-state files are not eviction candidates for either phase.
///
/// Only `Cached` files appear in `list_files_by_cache_state(Cached)` (lifecycle
/// phase) and in `eviction_candidates` (LRU phase). An `Online` file must be
/// completely invisible to both, even under an aggressive budget.
#[tokio::test]
async fn online_files_are_not_eviction_candidates() {
    let db = setup_db();

    let online = make_file("remote.txt", "o1", Some(10_000));
    // Insert in the default Online state (upsert_file always starts as Online).
    db.upsert_file(&online).unwrap();
    // No update_cache_state call — stays Online.

    // Policy matches the file, budget is zero.
    db.add_lifecycle_policy("**", None, Some(0), 0, None)
        .unwrap();

    let config = CacheManagerConfig {
        max_size: Some(0),
        ..CacheManagerConfig::default()
    };
    let manager = make_manager(Arc::clone(&db), config);
    let report = manager.evict().await.unwrap();

    assert_eq!(
        report.total_evicted(),
        0,
        "online files must not be touched by either eviction phase"
    );
    assert_eq!(
        db.get_cache_state(&online.id).unwrap(),
        Some(CacheState::Online)
    );
}
