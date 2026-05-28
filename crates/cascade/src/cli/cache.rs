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

/// Resolve the config directory.
fn config_dir() -> std::path::PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from(".cascade"))
        .join("cascade")
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
    print!("  Total:   {total_mb:.1} MB");
    if let Some(max) = stats.max_bytes {
        let max_mb = max as f64 / (1024.0 * 1024.0);
        println!(" / {max_mb:.1} MB limit");
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

/// Add a backend configuration.
pub fn backend_add(backend_type: &str, name: Option<&str>, mount_path: Option<&str>) -> Result<()> {
    let config_dir = config_dir();
    std::fs::create_dir_all(&config_dir)?;

    let backend_name = name.unwrap_or(backend_type);
    let config_path = config_dir.join(format!("{backend_name}.toml"));

    if config_path.exists() {
        anyhow::bail!("Backend '{backend_name}' already exists. Remove it first.");
    }

    // Write minimal config with the backend type.
    let mut config = toml::Table::new();
    config.insert(
        "type".to_string(),
        toml::Value::String(backend_type.to_string()),
    );
    if let Some(mp) = mount_path {
        config.insert(
            "mount_path".to_string(),
            toml::Value::String(mp.to_string()),
        );
    }

    let config_str = toml::to_string_pretty(&config)?;
    std::fs::write(&config_path, &config_str)?;

    // Register in the state DB.
    let db = open_db()?;
    db.register_backend(
        backend_name,
        backend_type,
        &format!("{} ({backend_name})", backend_type_display(backend_type)),
        mount_path,
        None,
    )?;

    println!("Added backend: {backend_name} ({backend_type})");
    if let Some(mp) = mount_path {
        println!("  Mount path: {mp}");
    }
    println!("  Config: {}", config_path.display());

    // Type-specific instructions.
    match backend_type {
        "gdrive" => {
            println!("\nRun `cascade backend auth {backend_name}` to authenticate.");
        }
        _ => {
            println!("\nEdit {} to add credentials.", config_path.display());
        }
    }

    Ok(())
}

/// Remove a backend configuration.
pub fn backend_remove(name: &str) -> Result<()> {
    let config_dir = config_dir();
    let config_path = config_dir.join(format!("{name}.toml"));

    if !config_path.exists() {
        anyhow::bail!("Backend '{name}' not found.");
    }

    std::fs::remove_file(&config_path)?;

    println!("Removed backend: {name}");
    println!("  Config deleted: {}", config_path.display());
    Ok(())
}

/// Display name for a backend type.
fn backend_type_display(backend_type: &str) -> &'static str {
    match backend_type {
        "gdrive" => "Google Drive",
        "s3" => "S3 Compatible",
        "webdav" => "WebDAV",
        "dropbox" => "Dropbox",
        "onedrive" => "OneDrive",
        "local" => "Local Filesystem",
        _ => "Unknown",
    }
}
