//! CLI implementations for pin/unpin/pin-list and cache commands.

use anyhow::Result;
use cascade_engine::cache::manager::CacheManager;
use cascade_engine::cache::manager::CacheManagerConfig;
use cascade_engine::db::StateDb;
use std::path::Path;
use std::sync::Arc;

/// Open the state database from the default path.
fn open_db() -> Result<StateDb> {
    let db_path = dirs::data_dir()
        .unwrap_or_else(|| Path::new("/tmp").to_path_buf())
        .join("cascade")
        .join("state.db");
    StateDb::open(&db_path)
}

/// Pin a path.
pub fn pin(path: &str) -> Result<()> {
    let db = open_db()?;
    let db = Arc::new(db);
    let manager = CacheManager::new(db, CacheManagerConfig::default());

    let recursive = true; // Default to recursive pin.
    manager.pin(path, recursive)?;
    println!("Pinned: {path}");
    Ok(())
}

/// Unpin a path.
pub fn unpin(path: &str) -> Result<()> {
    let db = open_db()?;
    let db = Arc::new(db);
    let manager = CacheManager::new(db, CacheManagerConfig::default());

    if manager.unpin(path)? {
        println!("Unpinned: {path}");
    } else {
        println!("Not pinned: {path}");
    }
    Ok(())
}

/// List all pinned paths.
pub fn pin_list() -> Result<()> {
    let db = open_db()?;
    let db = Arc::new(db);
    let manager = CacheManager::new(db, CacheManagerConfig::default());

    let rules = manager.list_pins()?;
    if rules.is_empty() {
        println!("No pinned paths.");
    } else {
        println!("Pinned paths:");
        for rule in &rules {
            let scope = if rule.recursive { "recursive" } else { "exact" };
            println!("  {} ({})", rule.path_glob, scope);
        }
    }
    Ok(())
}

/// Show cache status.
pub fn cache_status() -> Result<()> {
    let db = open_db()?;
    let db = Arc::new(db);
    let manager = CacheManager::new(db, CacheManagerConfig::default());

    let stats = manager.stats()?;
    println!("Cache Status:");
    println!("  Online:  {} files", stats.online_count);
    println!("  Cached:  {} files", stats.cached_count);
    println!("  Pinned:  {} files", stats.pinned_count);

    let total_mb = stats.total_bytes as f64 / (1024.0 * 1024.0);
    print!("  Total:   {:.1} MB", total_mb);
    if let Some(max) = stats.max_bytes {
        let max_mb = max as f64 / (1024.0 * 1024.0);
        println!(" / {:.1} MB limit", max_mb);
    } else {
        println!();
    }

    Ok(())
}

/// Run eviction.
pub fn evict(all: bool) -> Result<()> {
    let db = open_db()?;
    let db = Arc::new(db);
    let config = if all {
        CacheManagerConfig {
            max_size: Some(0), // Evict everything non-pinned.
            ..CacheManagerConfig::default()
        }
    } else {
        CacheManagerConfig::default()
    };
    let manager = CacheManager::new(db, config);

    let report = manager.evict()?;
    if report.total_evicted() == 0 {
        println!("No files to evict.");
    } else {
        println!(
            "Evicted {} files ({} lifecycle, {} size, {} bytes freed)",
            report.total_evicted(),
            report.lifecycle_evicted,
            report.size_evicted,
            report.bytes_freed,
        );
    }
    Ok(())
}
