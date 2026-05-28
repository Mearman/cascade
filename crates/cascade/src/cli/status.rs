use anyhow::Result;
use cascade_engine::db::StateDb;
use std::path::PathBuf;

/// Show overall mount status.
pub fn show() -> Result<()> {
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".cascade"))
        .join("cascade");
    let db_path = config_dir.join("state.db");

    // Check if running by looking for the state database.
    let running = db_path.exists();

    if !running {
        println!("Cascade is not running.");
        println!("  State database not found at {}", db_path.display());
        return Ok(());
    }

    let db = StateDb::open(&db_path)?;
    let backends = db.list_backends()?;

    println!("Cascade Status");
    println!("  Running: true");
    println!("  State DB: {}", db_path.display());

    if backends.is_empty() {
        println!("  No backends registered.");
    } else {
        println!("  Backends:");
        for b in &backends {
            let cursor_info = db
                .get_cursor(&b.id)
                .ok()
                .flatten().map_or_else(|| "no cursor".to_string(), |c| format!("cursor: {}", c.0));
            println!(
                "    {} ({}) — {} [{}]",
                b.display_name, b.backend_type, b.id, cursor_info
            );
            if let Some(mp) = &b.mount_path {
                println!("      Mount: {mp}");
            }
        }
    }

    println!();
    println!("  PID file: check `cascade start` process");

    Ok(())
}

/// List configured backends.
pub fn backend_list() -> Result<()> {
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
                println!("    Mount path: {mp}");
            }
        }
    }

    Ok(())
}
