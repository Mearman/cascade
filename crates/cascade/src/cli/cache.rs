//! CLI implementations for pin/unpin/pin-list and cache commands.

use anyhow::Result;
use cascade_engine::cache::manager::CacheManager;
use cascade_engine::cache::manager::CacheManagerConfig;
use cascade_engine::db::StateDb;
use cascade_engine::portable::native::{SqliteStorage, TokioRuntimeHandle};
use std::sync::Arc;

use super::CliContext;
use super::init::{BackendConfig, CascadeConfig};

/// Open the state database.
fn open_db(ctx: &CliContext) -> Result<StateDb> {
    StateDb::open(&ctx.db_path)
}

/// Build a `CacheManager` over a freshly opened state database.
fn make_manager(db: Arc<StateDb>) -> CacheManager<TokioRuntimeHandle> {
    let runtime = TokioRuntimeHandle::current();
    let storage = SqliteStorage::new(db, runtime.clone());
    CacheManager::new(Arc::new(storage), runtime, CacheManagerConfig::default())
}

/// Build a `CacheManager` with custom config.
fn make_manager_with_config(
    db: Arc<StateDb>,
    config: CacheManagerConfig,
) -> CacheManager<TokioRuntimeHandle> {
    let runtime = TokioRuntimeHandle::current();
    let storage = SqliteStorage::new(db, runtime.clone());
    CacheManager::new(Arc::new(storage), runtime, config)
}

/// Pin a path.
pub async fn pin(ctx: &CliContext, path: &str) -> Result<()> {
    let db = open_db(ctx)?;
    let db = Arc::new(db);
    let manager = make_manager(db);

    let recursive = true; // Default to recursive pin.
    manager.pin(path, recursive).await?;
    println!("Pinned: {path}");
    Ok(())
}

/// Unpin a path.
pub async fn unpin(ctx: &CliContext, path: &str) -> Result<()> {
    let db = open_db(ctx)?;
    let db = Arc::new(db);
    let manager = make_manager(db);

    if manager.unpin(path).await? {
        println!("Unpinned: {path}");
    } else {
        println!("Not pinned: {path}");
    }
    Ok(())
}

/// List all pinned paths.
pub async fn pin_list(ctx: &CliContext) -> Result<()> {
    let db = open_db(ctx)?;
    let db = Arc::new(db);
    let manager = make_manager(db);

    let rules = manager.list_pins().await?;
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
pub async fn warm(ctx: &CliContext, path: &str) -> Result<()> {
    let db = open_db(ctx)?;
    let db = Arc::new(db);
    let manager = make_manager(db);

    manager.pin(path, true).await?;
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
pub async fn clear(ctx: &CliContext, path: &str) -> Result<()> {
    use cascade_engine::types::CacheState;

    let db = open_db(ctx)?;
    let db = Arc::new(db);
    let manager = make_manager(Arc::clone(&db));

    // Remove the pin rule if one exists — ignore "not pinned" case.
    let _ = manager.unpin(path).await?;

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
pub async fn cache_status(ctx: &CliContext) -> Result<()> {
    let db = open_db(ctx)?;
    let db = Arc::new(db);
    let manager = make_manager(db);

    let stats = manager.stats().await?;
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
pub async fn evict(ctx: &CliContext, all: bool) -> Result<()> {
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
    let manager = make_manager_with_config(db, config);

    let report = manager.evict().await?;
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
        "p2p" => {
            println!("\nP2P configuration:");

            let listen_addr = read_input("Listen address (host:port, blank to disable)")?;
            let display_name = read_input("Display name (blank for backend name)")?;
            let device_name = read_input(
                "Device name (friendly name for THIS device, blank to use the 8-char device ID)",
            )?;
            let data_dir =
                read_input("Data directory (blank for default ~/.config/cascade/p2p-<name>)")?;
            let exposure = prompt_exposure()?;

            let mut peers: Vec<(String, String, Option<String>)> = Vec::new();
            loop {
                let add_peer = read_input("Add a peer? (y/N)")?;
                if !matches!(add_peer.to_ascii_lowercase().as_str(), "y" | "yes") {
                    break;
                }
                let device_id = loop {
                    let id = read_input("  Peer device_id")?;
                    if id.is_empty() {
                        eprintln!("  device_id must not be empty");
                        continue;
                    }
                    break id;
                };
                let address = loop {
                    let addr = read_input("  Peer address (host:port)")?;
                    if addr.is_empty() {
                        eprintln!("  address must not be empty");
                        continue;
                    }
                    break addr;
                };
                let peer_name_input = read_input("  Friendly name for this peer (blank for none)")?;
                let peer_name = if peer_name_input.is_empty() {
                    None
                } else {
                    Some(peer_name_input)
                };
                peers.push((device_id, address, peer_name));
            }

            let mut full_config = toml::Table::new();
            full_config.insert("type".to_string(), toml::Value::String("p2p".to_string()));
            full_config.insert(
                "name".to_string(),
                toml::Value::String(backend_name.to_string()),
            );
            if let Some(mp) = mount_path {
                full_config.insert(
                    "mount_path".to_string(),
                    toml::Value::String(mp.to_string()),
                );
            }
            if !display_name.is_empty() {
                full_config.insert(
                    "display_name".to_string(),
                    toml::Value::String(display_name),
                );
            }
            if !device_name.is_empty() {
                full_config.insert("device_name".to_string(), toml::Value::String(device_name));
            }
            if !data_dir.is_empty() {
                full_config.insert("data_dir".to_string(), toml::Value::String(data_dir));
            }
            if !listen_addr.is_empty() {
                full_config.insert("listen_addr".to_string(), toml::Value::String(listen_addr));
            }
            // Only emit `exposure` when it differs from the Private default,
            // so a default-posture config stays clean.
            if let Some(value) = exposure {
                full_config.insert(
                    "exposure".to_string(),
                    toml::Value::String(value.to_string()),
                );
            }
            let config_str = render_p2p_config(&full_config, &peers)?;
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
    // Use the explicit mount_path when given; otherwise default to the
    // backend name (the same default the engine applies at startup).
    let resolved_mount = mount_path.map_or_else(|| backend_name.to_string(), ToOwned::to_owned);
    let backend_entry = BackendConfig {
        backend_type: backend_type.to_string(),
        mount: Some(resolved_mount),
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

/// Prompt for the p2p exposure posture, returning the TOML value to emit, or
/// `None` to leave the key out (the Private default).
///
/// The posture decides how far the device reaches out for peers: `lan-only`
/// confines it to the segment, `private` (the default) adds gossip, hole
/// punch, and peer relay among trusted peers, and `public` additionally
/// publishes to the DHT and announce servers. An unrecognised answer falls
/// back to the default rather than rejecting input mid-wizard. `None` is
/// returned for the default so a default-posture config omits the key.
fn prompt_exposure() -> Result<Option<&'static str>> {
    let raw =
        read_input("Exposure posture — lan-only / private / public (blank for private default)")?;
    let lowered = raw.to_ascii_lowercase();
    if exposure_from_input(&lowered).is_none() && !lowered.is_empty() && lowered != "private" {
        eprintln!("Unrecognised posture `{raw}`; defaulting to private.");
    }
    Ok(exposure_from_input(&lowered))
}

/// Map a lowercased posture answer to the TOML value to emit, or `None` for
/// the Private default (blank, `private`, or anything unrecognised).
///
/// Pure mapping split out from [`prompt_exposure`] so the wizard's
/// posture-to-config decision is unit-testable without driving stdin.
fn exposure_from_input(lowered: &str) -> Option<&'static str> {
    match lowered {
        "lan-only" => Some("lan-only"),
        "public" => Some("public"),
        _ => None,
    }
}

/// Render the p2p backend config with peers as a proper `[[peers]]`
/// array-of-tables.
///
/// `toml::to_string_pretty` serialises an array of tables as a flat inline
/// array (`peers = [{...}]`). The parser accepts both forms but the
/// documented hand-written form uses array-of-tables, so the wizard's
/// output should match for visual parity with the e2e fixtures and the
/// docs. The scalar config is serialised through `toml::to_string_pretty`
/// (which handles string escaping correctly); each peer is then appended
/// as its own `[[peers]]` table, with the per-peer body serialised through
/// `toml::to_string` so string values are escaped according to the TOML
/// grammar.
fn render_p2p_config(
    scalar_config: &toml::Table,
    peers: &[(String, String, Option<String>)],
) -> Result<String> {
    let mut out = toml::to_string_pretty(scalar_config)?;
    for (device_id, address, name) in peers {
        if !out.ends_with("\n\n") {
            if out.ends_with('\n') {
                out.push('\n');
            } else {
                out.push_str("\n\n");
            }
        }
        out.push_str("[[peers]]\n");
        let mut peer = toml::Table::new();
        peer.insert(
            "device_id".to_string(),
            toml::Value::String(device_id.clone()),
        );
        peer.insert("address".to_string(), toml::Value::String(address.clone()));
        if let Some(name) = name {
            peer.insert("name".to_string(), toml::Value::String(name.clone()));
        }
        out.push_str(&toml::to_string(&peer)?);
    }
    Ok(out)
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
        "p2p" => "P2P",
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

    #[tokio::test]
    async fn pin_adds_rule_to_db() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_backend(&ctx);

        pin(&ctx, "Documents/Accounts").await.unwrap();

        let db = StateDb::open(&ctx.db_path).unwrap();
        let rules = db.list_pin_rules().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].path_glob, "Documents/Accounts");
        assert!(rules[0].recursive);
    }

    #[tokio::test]
    async fn unpin_removes_existing_rule() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_backend(&ctx);

        pin(&ctx, "Photos").await.unwrap();
        unpin(&ctx, "Photos").await.unwrap();

        let db = StateDb::open(&ctx.db_path).unwrap();
        assert!(db.list_pin_rules().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unpin_nonexistent_path_succeeds() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_backend(&ctx);

        // Unpinning a path that was never pinned should succeed (prints
        // "Not pinned: ...").
        unpin(&ctx, "no/such/path").await.unwrap();
    }

    #[tokio::test]
    async fn pin_list_empty() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_backend(&ctx);

        pin_list(&ctx).await.unwrap();
    }

    #[tokio::test]
    async fn pin_list_shows_rules() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_backend(&ctx);

        pin(&ctx, "Docs").await.unwrap();
        pin(&ctx, "Photos").await.unwrap();

        pin_list(&ctx).await.unwrap();
    }

    // ── cache_status ──

    #[tokio::test]
    async fn cache_status_empty_db() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_backend(&ctx);

        cache_status(&ctx).await.unwrap();
    }

    #[tokio::test]
    async fn cache_status_with_files() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        let db = seed_backend(&ctx);

        seed_file(&db, "file1.txt", CacheState::Online);
        seed_file(&db, "file2.txt", CacheState::Cached);
        seed_file(&db, "file3.txt", CacheState::Pinned);

        cache_status(&ctx).await.unwrap();
    }

    // ── evict ──

    #[tokio::test]
    async fn evict_with_no_files() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_backend(&ctx);

        evict(&ctx, false).await.unwrap();
    }

    #[tokio::test]
    async fn evict_all_with_cached_files() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        let db = seed_backend(&ctx);

        seed_file(&db, "cached1.txt", CacheState::Cached);
        seed_file(&db, "cached2.txt", CacheState::Cached);
        seed_file(&db, "pinned.txt", CacheState::Pinned);

        evict(&ctx, true).await.unwrap();
    }

    // ── clear ──

    #[tokio::test]
    async fn clear_reverts_cached_files_to_online() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        let db = seed_backend(&ctx);

        seed_file(&db, "dir/file1.txt", CacheState::Cached);
        seed_file(&db, "dir/file2.txt", CacheState::Cached);
        seed_file(&db, "other.txt", CacheState::Cached);

        drop(db);
        clear(&ctx, "dir").await.unwrap();

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

    #[tokio::test]
    async fn clear_with_no_matching_files() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        let db = seed_backend(&ctx);

        seed_file(&db, "unrelated.txt", CacheState::Cached);

        drop(db);
        clear(&ctx, "no/match").await.unwrap();
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

    // ── backend_add (p2p) ──

    /// The TOML the p2p arm of `backend_add` writes for a representative
    /// input must round-trip through `cascade_backend_p2p::create_backend`.
    /// This is the public contract between the CLI wizard and the backend
    /// factory — if the wizard ever produces a key the factory does not
    /// recognise (or omits one it requires), this test fails.
    ///
    /// The wizard now emits peers as `[[peers]]` array-of-tables (matching
    /// the documented hand-written form), so this test also asserts the
    /// serialised text contains that marker.
    #[tokio::test]
    async fn p2p_backend_add_produces_loadable_config() {
        let dir = TempDir::new().unwrap();

        let mut config = toml::Table::new();
        config.insert("type".to_string(), toml::Value::String("p2p".to_string()));
        config.insert(
            "name".to_string(),
            toml::Value::String("shared".to_string()),
        );
        config.insert(
            "device_name".to_string(),
            toml::Value::String("work-laptop".to_string()),
        );
        config.insert(
            "listen_addr".to_string(),
            toml::Value::String("0.0.0.0:22000".to_string()),
        );
        config.insert(
            "data_dir".to_string(),
            toml::Value::String(dir.path().to_string_lossy().into_owned()),
        );

        let peers = vec![(
            "PEER-DEVICE-ID".to_string(),
            "192.0.2.1:22000".to_string(),
            Some("home-laptop".to_string()),
        )];
        let rendered = render_p2p_config(&config, &peers).unwrap();

        assert!(
            rendered.contains("[[peers]]"),
            "wizard output must use array-of-tables for peers, got:\n{rendered}",
        );
        assert!(
            rendered.contains("device_name = \"work-laptop\""),
            "wizard output must emit the local device_name, got:\n{rendered}",
        );
        assert!(
            rendered.contains("name = \"home-laptop\""),
            "wizard output must emit the per-peer friendly name, got:\n{rendered}",
        );

        let parsed: toml::Value = toml::from_str(&rendered).unwrap();
        if let Err(err) = cascade_backend_p2p::create_backend(&parsed) {
            panic!("create_backend rejected wizard output: {err:#}\nrendered:\n{rendered}");
        }
    }

    /// `render_p2p_config` must serialise the local `device_name` as a scalar
    /// and each peer's friendly `name` inside its `[[peers]]` table when set.
    /// Smaller than the round-trip test above — exercises just the rendering
    /// shape without touching `create_backend`.
    #[test]
    fn render_p2p_config_with_device_and_peer_names() {
        let mut scalar = toml::Table::new();
        scalar.insert("type".to_string(), toml::Value::String("p2p".to_string()));
        scalar.insert(
            "name".to_string(),
            toml::Value::String("shared".to_string()),
        );
        scalar.insert(
            "device_name".to_string(),
            toml::Value::String("work-laptop".to_string()),
        );
        let peers = vec![(
            "AAAA".to_string(),
            "node-b:22000".to_string(),
            Some("home-laptop".to_string()),
        )];
        let rendered = render_p2p_config(&scalar, &peers).unwrap();
        assert!(rendered.contains("device_name = \"work-laptop\""));
        assert!(rendered.contains("[[peers]]"));
        assert!(rendered.contains("name = \"home-laptop\""));
    }

    /// `render_p2p_config` with no peers must emit nothing peer-related — no
    /// stray `[[peers]]` headers, no empty `peers = []` array.
    #[test]
    fn render_p2p_config_with_no_peers_omits_peers_section() {
        let mut config = toml::Table::new();
        config.insert("type".to_string(), toml::Value::String("p2p".to_string()));
        config.insert("name".to_string(), toml::Value::String("solo".to_string()));

        let peers: Vec<(String, String, Option<String>)> = Vec::new();
        let rendered = render_p2p_config(&config, &peers).unwrap();
        assert!(
            !rendered.contains("peers"),
            "unexpected peers content: {rendered}"
        );
    }

    /// Strings containing TOML metacharacters must be escaped properly when
    /// rendered as part of the `[[peers]]` block — `toml::to_string` handles
    /// this, this test guards against accidental hand-formatting that would
    /// break round-tripping.
    #[test]
    fn render_p2p_config_escapes_peer_strings() {
        let mut config = toml::Table::new();
        config.insert("type".to_string(), toml::Value::String("p2p".to_string()));

        let peers = vec![(
            "dev with \"quotes\" and \\backslash".to_string(),
            "[::1]:22000".to_string(),
            None,
        )];
        let rendered = render_p2p_config(&config, &peers).unwrap();
        let parsed: toml::Value = toml::from_str(&rendered).unwrap();
        let peers_value = parsed.get("peers").unwrap().as_array().unwrap();
        assert_eq!(peers_value.len(), 1);
        let peer = peers_value[0].as_table().unwrap();
        assert_eq!(
            peer.get("device_id").unwrap().as_str().unwrap(),
            "dev with \"quotes\" and \\backslash",
        );
        assert_eq!(
            peer.get("address").unwrap().as_str().unwrap(),
            "[::1]:22000"
        );
    }

    // ── exposure_from_input ──

    /// Blank or `private` → the default posture, emitted as no key.
    #[test]
    fn exposure_from_input_blank_or_private_omits_key() {
        assert_eq!(exposure_from_input(""), None);
        assert_eq!(exposure_from_input("private"), None);
    }

    /// The non-default postures map to their kebab-case TOML values.
    #[test]
    fn exposure_from_input_maps_non_default_postures() {
        assert_eq!(exposure_from_input("lan-only"), Some("lan-only"));
        assert_eq!(exposure_from_input("public"), Some("public"));
    }

    /// An unrecognised answer falls back to the default rather than emitting a
    /// bogus key — the wizard separately warns the user.
    #[test]
    fn exposure_from_input_unknown_falls_back_to_default() {
        assert_eq!(exposure_from_input("publik"), None);
        assert_eq!(exposure_from_input("lanonly"), None);
    }
}
