//! CLI implementations for pin/unpin/pin-list and cache commands.

use anyhow::Result;
use cascade_engine::cache::manager::CacheManager;
use cascade_engine::cache::manager::CacheManagerConfig;
use cascade_engine::db::StateDb;
use std::sync::Arc;

use super::CliContext;
use super::init::{BackendConfig, CascadeConfig};

/// Open the state database.
fn open_db(ctx: &CliContext) -> Result<StateDb> {
    StateDb::open(&ctx.db_path)
}

/// Pin a path.
pub fn pin(ctx: &CliContext, path: &str) -> Result<()> {
    let db = open_db(ctx)?;
    let db = Arc::new(db);
    let manager = CacheManager::new(db, CacheManagerConfig::default());

    let recursive = true; // Default to recursive pin.
    manager.pin(path, recursive)?;
    println!("Pinned: {path}");
    Ok(())
}

/// Unpin a path.
pub fn unpin(ctx: &CliContext, path: &str) -> Result<()> {
    let db = open_db(ctx)?;
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
pub fn pin_list(ctx: &CliContext) -> Result<()> {
    let db = open_db(ctx)?;
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
pub fn warm(ctx: &CliContext, path: &str) -> Result<()> {
    let db = open_db(ctx)?;
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
pub fn clear(ctx: &CliContext, path: &str) -> Result<()> {
    use cascade_engine::types::CacheState;

    let db = open_db(ctx)?;
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
pub fn cache_status(ctx: &CliContext) -> Result<()> {
    let db = open_db(ctx)?;
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
pub fn evict(ctx: &CliContext, all: bool) -> Result<()> {
    let db = open_db(ctx)?;
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
pub fn backend_add(
    ctx: &CliContext,
    backend_type: &str,
    name: Option<&str>,
    mount_path: Option<&str>,
    cli_client_id: Option<&str>,
    cli_client_secret: Option<&str>,
) -> Result<()> {
    std::fs::create_dir_all(&ctx.config_dir)?;

    let backend_name = name.unwrap_or(backend_type);
    let config_path = ctx.config_dir.join(format!("{backend_name}.toml"));

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
            // Use CLI flags if provided, otherwise prompt interactively.
            let client_id = if let Some(cid) = cli_client_id {
                if cid.is_empty() {
                    anyhow::bail!("--client-id must not be empty");
                }
                cid.to_string()
            } else {
                println!(
                    "\nGoogle Drive requires OAuth credentials (client_id and client_secret)."
                );
                println!("Create a Desktop app OAuth client at https://console.cloud.google.com/");
                let cid = read_input("Client ID")?;
                if cid.is_empty() {
                    anyhow::bail!("client_id is required");
                }
                cid
            };

            let client_secret = if let Some(csec) = cli_client_secret {
                if csec.is_empty() {
                    anyhow::bail!("--client-secret must not be empty");
                }
                csec.to_string()
            } else if cli_client_id.is_some() {
                // --client-id was given but not --client-secret — error.
                anyhow::bail!("--client-secret is required when --client-id is provided");
            } else {
                let csec = read_input("Client secret")?;
                if csec.is_empty() {
                    anyhow::bail!("client_secret is required");
                }
                csec
            };

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
            eprintln!(
                "Warning: '{backend_type}' is not a supported backend type. \
                 Config written but `cascade start` will reject it."
            );
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
    let db = open_db(ctx)?;
    db.register_backend(
        backend_name,
        backend_type,
        &format!("{} ({backend_name})", backend_type_display(backend_type)),
        mount_path,
        None,
    )?;

    // Update config.toml so `cascade start` can discover this backend.
    let main_config_path = ctx.config_dir.join("config.toml");
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
    main_config.backends.insert(
        backend_name.to_string(),
        toml::Value::try_from(&backend_entry)?,
    );
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
pub fn backend_remove(ctx: &CliContext, name: &str) -> Result<()> {
    let config_path = ctx.config_dir.join(format!("{name}.toml"));

    if !config_path.exists() {
        anyhow::bail!("Backend '{name}' not found.");
    }

    std::fs::remove_file(&config_path)?;

    // Remove from config.toml so `cascade start` no longer tries to load it.
    let main_config_path = ctx.config_dir.join("config.toml");
    if main_config_path.exists() {
        let raw = std::fs::read_to_string(&main_config_path)?;
        let mut main_config: CascadeConfig = toml::from_str(&raw)?;
        main_config.backends.remove(name);
        let main_config_str = toml::to_string_pretty(&main_config)?;
        std::fs::write(&main_config_path, &main_config_str)?;
    }

    // Remove from the state DB.
    let db = open_db(ctx)?;
    db.remove_backend(name)?;

    println!("Removed backend: {name}");
    println!("  Config deleted: {}", config_path.display());
    Ok(())
}

/// Display name for a backend type.
///
/// Returns the human-readable name for known types, or the raw type string
/// for unrecognised types (forward-compatible with future backends).
fn backend_type_display(backend_type: &str) -> &str {
    match backend_type {
        "gdrive" => "Google Drive",
        "s3" => "S3 Compatible",
        _ => backend_type,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use cascade_engine::types::{CacheState, FileEntry, ItemId};
    use tempfile::TempDir;

    fn make_ctx(dir: &TempDir) -> CliContext {
        let config_dir = dir.path().to_path_buf();
        CliContext {
            db_path: config_dir.join("state.db"),
            pid_path: config_dir.join("cascade.pid"),
            config_dir,
        }
    }

    fn seed_backend(ctx: &CliContext) -> StateDb {
        let db = StateDb::open(&ctx.db_path).unwrap();
        db.register_backend("test", "gdrive", "Test Backend", None, None)
            .unwrap();
        db
    }

    fn seed_file(db: &StateDb, name: &str, state: CacheState) {
        let file_id = ItemId::new("test", name);
        let parent_id = ItemId::new("test", "root");
        let entry = FileEntry::file(file_id.clone(), parent_id, name.to_string());
        db.upsert_file(&entry).unwrap();
        db.update_cache_state(&file_id, state).unwrap();
    }

    // ── pin / unpin / pin_list ──

    #[test]
    fn pin_adds_rule_to_db() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_backend(&ctx);

        pin(&ctx, "Documents/Accounts").unwrap();

        let db = StateDb::open(&ctx.db_path).unwrap();
        let rules = db.list_pin_rules().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].path_glob, "Documents/Accounts");
        assert!(rules[0].recursive);
    }

    #[test]
    fn unpin_removes_existing_rule() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_backend(&ctx);

        pin(&ctx, "Photos").unwrap();
        unpin(&ctx, "Photos").unwrap();

        let db = StateDb::open(&ctx.db_path).unwrap();
        assert!(db.list_pin_rules().unwrap().is_empty());
    }

    #[test]
    fn unpin_nonexistent_path_succeeds() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_backend(&ctx);

        // Unpinning a path that was never pinned should succeed (prints
        // "Not pinned: ...").
        unpin(&ctx, "no/such/path").unwrap();
    }

    #[test]
    fn pin_list_empty() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_backend(&ctx);

        pin_list(&ctx).unwrap();
    }

    #[test]
    fn pin_list_shows_rules() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_backend(&ctx);

        pin(&ctx, "Docs").unwrap();
        pin(&ctx, "Photos").unwrap();

        pin_list(&ctx).unwrap();
    }

    // ── cache_status ──

    #[test]
    fn cache_status_empty_db() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_backend(&ctx);

        cache_status(&ctx).unwrap();
    }

    #[test]
    fn cache_status_with_files() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        let db = seed_backend(&ctx);

        seed_file(&db, "file1.txt", CacheState::Online);
        seed_file(&db, "file2.txt", CacheState::Cached);
        seed_file(&db, "file3.txt", CacheState::Pinned);

        cache_status(&ctx).unwrap();
    }

    // ── evict ──

    #[test]
    fn evict_with_no_files() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_backend(&ctx);

        evict(&ctx, false).unwrap();
    }

    #[test]
    fn evict_all_with_cached_files() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        let db = seed_backend(&ctx);

        seed_file(&db, "cached1.txt", CacheState::Cached);
        seed_file(&db, "cached2.txt", CacheState::Cached);
        seed_file(&db, "pinned.txt", CacheState::Pinned);

        evict(&ctx, true).unwrap();
    }

    // ── clear ──

    #[test]
    fn clear_reverts_cached_files_to_online() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        let db = seed_backend(&ctx);

        seed_file(&db, "dir/file1.txt", CacheState::Cached);
        seed_file(&db, "dir/file2.txt", CacheState::Cached);
        seed_file(&db, "other.txt", CacheState::Cached);

        drop(db);
        clear(&ctx, "dir").unwrap();

        let db = StateDb::open(&ctx.db_path).unwrap();
        let file1_id = ItemId::new("test", "dir/file1.txt");
        let file2_id = ItemId::new("test", "dir/file2.txt");
        let other_id = ItemId::new("test", "other.txt");

        assert_eq!(
            db.get_cache_state(&file1_id).unwrap(),
            Some(CacheState::Online)
        );
        assert_eq!(
            db.get_cache_state(&file2_id).unwrap(),
            Some(CacheState::Online)
        );
        // "other.txt" should remain cached — it doesn't match the prefix.
        assert_eq!(
            db.get_cache_state(&other_id).unwrap(),
            Some(CacheState::Cached)
        );
    }

    #[test]
    fn clear_with_no_matching_files() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        let db = seed_backend(&ctx);

        seed_file(&db, "unrelated.txt", CacheState::Cached);

        drop(db);
        clear(&ctx, "no/match").unwrap();
    }

    // ── backend_remove ──

    #[test]
    fn backend_remove_errors_when_config_missing() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);

        let result = backend_remove(&ctx, "nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn backend_remove_deletes_config_and_db_entry() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);

        let db = seed_backend(&ctx);

        let config_path = ctx.config_dir.join("test.toml");
        std::fs::write(&config_path, "type = \"gdrive\"\n").unwrap();

        let main_config_path = ctx.config_dir.join("config.toml");
        std::fs::write(&main_config_path, "[mount]\npoint = \"/tmp/cloud\"\n").unwrap();

        drop(db);
        backend_remove(&ctx, "test").unwrap();

        assert!(!config_path.exists());

        let db = StateDb::open(&ctx.db_path).unwrap();
        let backends = db.list_backends().unwrap();
        assert!(backends.is_empty());
    }
}
