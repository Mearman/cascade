use anyhow::Result;
use cascade_engine::db::StateDb;
use std::path::PathBuf;

/// Show overall mount status.
pub async fn show() -> Result<()> {
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".cascade"))
        .join("cascade");
    let db_path = config_dir.join("state.db");

    // Check if running by looking for the state database.
    let running = db_path.exists();

    if !running {
        println!("Cascade is not running.");
        println!("  State database not found at {:?}", db_path);
        return Ok(());
    }

    let db = StateDb::open(&db_path)?;
    let backends = db.list_backends()?;

    println!("Cascade Status");
    println!("  Running: true");
    println!("  State DB: {:?}", db_path);

    if backends.is_empty() {
        println!("  No backends registered.");
    } else {
        println!("  Backends:");
        for b in &backends {
            let cursor_info = db
                .get_cursor(&b.id)
                .ok()
                .flatten()
                .map(|c| format!("cursor: {}", c.0))
                .unwrap_or_else(|| "no cursor".to_string());
            println!(
                "    {} ({}) — {} [{}]",
                b.display_name, b.backend_type, b.id, cursor_info
            );
            if let Some(mp) = &b.mount_path {
                println!("      Mount: {}", mp);
            }
        }
    }

    println!();
    println!("  PID file: check `cascade start` process");

    Ok(())
}

/// Show cache status.
pub async fn cache_status() -> Result<()> {
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".cascade"))
        .join("cascade");
    let db_path = config_dir.join("state.db");

    if !db_path.exists() {
        println!("Cache Status");
        println!("  No cache data available (not running).");
        return Ok(());
    }

    let _db = StateDb::open(&db_path)?;

    // Cache stats require scanning the files table — provide summary.
    println!("Cache Status");
    println!("  Use `cascade status` for backend info.");
    println!("  File-level cache stats coming in Phase 2.");

    Ok(())
}

/// List configured backends.
pub async fn backend_list() -> Result<()> {
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".cascade"))
        .join("cascade");
    let db_path = config_dir.join("state.db");

    if !db_path.exists() {
        println!("No backends configured. State database not found.");
        return Ok(());
    }

    let db = StateDb::open(&db_path)?;
    let backends = db.list_backends()?;

    if backends.is_empty() {
        println!("No backends configured.");
    } else {
        println!("Configured backends:");
        for b in &backends {
            println!("  {} ({}) — {}", b.display_name, b.backend_type, b.id);
            if let Some(mp) = &b.mount_path {
                println!("    Mount path: {}", mp);
            }
        }
    }

    Ok(())
}
