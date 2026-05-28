//! CLI implementations for pin/unpin/pin-list and cache commands.

use anyhow::Result;
use cascade_engine::cache::manager::CacheManager;
use cascade_engine::cache::manager::CacheManagerConfig;
use cascade_engine::db::StateDb;
use std::sync::Arc;

use super::init::{BackendConfig, CascadeConfig};

/// Open the state database from the default path.
fn open_db() -> Result<StateDb> {
    let db_path = config_dir().join("state.db");
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

/// Pre-warm a path by pinning it so files are downloaded on the next sync.
pub fn warm(path: &str) -> Result<()> {
    let db = open_db()?;
    let db = Arc::new(db);
    let manager = CacheManager::new(db, CacheManagerConfig::default());

    manager.pin(path, true)?;
    println!("Warmed: {path}");
    println!("Files will be downloaded on next sync.");
    Ok(())
}

/// Clear a path from the local cache, reverting matching files to online-only.
///
/// There is no per-path eviction method on `CacheManager`, so this function
/// works directly at the `StateDb` level: it unpins the path (if pinned), then
/// transitions every cached file whose path matches `path` as a prefix to the
/// `Online` state. The background worker will then skip those files until they
/// are accessed or re-pinned.
pub fn clear(path: &str) -> Result<()> {
    use cascade_engine::types::CacheState;

    let db = open_db()?;
    let db = Arc::new(db);
    let manager = CacheManager::new(Arc::clone(&db), CacheManagerConfig::default());

    // Remove the pin rule if one exists — ignore "not pinned" case.
    let _ = manager.unpin(path)?;

    // Normalise the prefix for matching: "foo/bar" should match "foo/bar" and
    // "foo/bar/baz" but not "foo/barbaz".
    let prefix_slash = format!("{}/", path.trim_end_matches('/'));

    let cached = db.list_files_by_cache_state(CacheState::Cached)?;
    let pinned = db.list_files_by_cache_state(CacheState::Pinned)?;

    let mut cleared: usize = 0;
    for file in cached.iter().chain(pinned.iter()) {
        if file.name == path || file.name.starts_with(&prefix_slash) {
            db.update_cache_state(&file.id, CacheState::Online)?;
            cleared += 1;
        }
    }

    if cleared == 0 {
        println!("Cleared: {path} (no locally cached files matched)");
    } else {
        println!("Cleared: {path} ({cleared} file(s) reverted to online-only)");
    }
    Ok(())
}

/// Show cache status.
#[allow(clippy::cast_precision_loss)]
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

    // Collect type-specific credentials and write the full config file.
    match backend_type {
        "s3" => {
            println!("\nS3 configuration:");

            let endpoint = read_input("Endpoint URL (e.g. https://s3.amazonaws.com)")?;
            if endpoint.is_empty() {
                anyhow::bail!("endpoint is required");
            }

            let bucket = read_input("Bucket name")?;
            if bucket.is_empty() {
                anyhow::bail!("bucket is required");
            }

            let region_input = read_input("Region (e.g. us-east-1)")?;
            let region = if region_input.is_empty() {
                "us-east-1".to_string()
            } else {
                region_input
            };

            let access_key_id = read_input("Access key ID")?;
            if access_key_id.is_empty() {
                anyhow::bail!("access_key_id is required");
            }

            let secret_access_key = read_input("Secret access key")?;
            if secret_access_key.is_empty() {
                anyhow::bail!("secret_access_key is required");
            }

            let mut full_config = toml::Table::new();
            full_config.insert(
                "type".to_string(),
                toml::Value::String(backend_type.to_string()),
            );
            if let Some(mp) = mount_path {
                full_config.insert(
                    "mount_path".to_string(),
                    toml::Value::String(mp.to_string()),
                );
            }
            full_config.insert("endpoint".to_string(), toml::Value::String(endpoint));
            full_config.insert("bucket".to_string(), toml::Value::String(bucket));
            full_config.insert("region".to_string(), toml::Value::String(region));
            full_config.insert(
                "access_key_id".to_string(),
                toml::Value::String(access_key_id),
            );
            full_config.insert(
                "secret_access_key".to_string(),
                toml::Value::String(secret_access_key),
            );
            let config_str = toml::to_string_pretty(&full_config)?;
            std::fs::write(&config_path, &config_str)?;
        }
        "gdrive" => {
            println!("\nGoogle Drive requires OAuth credentials (client_id and client_secret).");
            println!("You can set these up at https://console.cloud.google.com/");

            let client_id = read_input("Client ID")?;
            if client_id.is_empty() {
                anyhow::bail!("client_id is required");
            }

            let client_secret = read_input("Client secret")?;
            if client_secret.is_empty() {
                anyhow::bail!("client_secret is required");
            }

            let mut full_config = toml::Table::new();
            full_config.insert(
                "type".to_string(),
                toml::Value::String("gdrive".to_string()),
            );
            if let Some(mp) = mount_path {
                full_config.insert(
                    "mount_path".to_string(),
                    toml::Value::String(mp.to_string()),
                );
            }
            full_config.insert("client_id".to_string(), toml::Value::String(client_id));
            full_config.insert(
                "client_secret".to_string(),
                toml::Value::String(client_secret),
            );
            let config_str = toml::to_string_pretty(&full_config)?;
            std::fs::write(&config_path, &config_str)?;
        }
        _ => {
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
        }
    }

    // Register in the state DB.
    let db = open_db()?;
    db.register_backend(
        backend_name,
        backend_type,
        &format!("{} ({backend_name})", backend_type_display(backend_type)),
        mount_path,
        None,
    )?;

    // Update config.toml so `cascade start` can discover this backend.
    let main_config_path = config_dir.join("config.toml");
    let mut main_config: CascadeConfig = if main_config_path.exists() {
        let raw = std::fs::read_to_string(&main_config_path)?;
        toml::from_str(&raw)?
    } else {
        CascadeConfig::default()
    };
    let backend_entry = BackendConfig {
        backend_type: backend_type.to_string(),
        account: None,
    };
    main_config
        .backends
        .insert(backend_name.to_string(), toml::Value::try_from(&backend_entry)?);
    let main_config_str = toml::to_string_pretty(&main_config)?;
    std::fs::write(&main_config_path, &main_config_str)?;

    println!("Added backend: {backend_name} ({backend_type})");
    if let Some(mp) = mount_path {
        println!("  Mount path: {mp}");
    }
    println!("  Config: {}", config_path.display());

    // Type-specific follow-up instructions.
    match backend_type {
        "gdrive" => {
            println!("\nRun `cascade backend auth {backend_name}` to authenticate.");
        }
        "s3" => {}
        _ => {
            println!("\nEdit {} to add credentials.", config_path.display());
        }
    }

    Ok(())
}

fn read_input(prompt: &str) -> Result<String> {
    use std::io::Write as _;
    print!("{prompt}: ");
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
}

/// Remove a backend configuration.
pub fn backend_remove(name: &str) -> Result<()> {
    let config_dir = config_dir();
    let config_path = config_dir.join(format!("{name}.toml"));

    if !config_path.exists() {
        anyhow::bail!("Backend '{name}' not found.");
    }

    std::fs::remove_file(&config_path)?;

    // Remove from config.toml so `cascade start` no longer tries to load it.
    let main_config_path = config_dir.join("config.toml");
    if main_config_path.exists() {
        let raw = std::fs::read_to_string(&main_config_path)?;
        let mut main_config: CascadeConfig = toml::from_str(&raw)?;
        main_config.backends.remove(name);
        let main_config_str = toml::to_string_pretty(&main_config)?;
        std::fs::write(&main_config_path, &main_config_str)?;
    }

    // Remove from the state DB.
    let db = open_db()?;
    db.remove_backend(name)?;

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
