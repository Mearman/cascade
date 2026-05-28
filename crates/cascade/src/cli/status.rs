use anyhow::{Context as _, Result};
use cascade_engine::db::StateDb;

use super::{is_process_alive, CliContext};

/// Show overall mount status.
pub fn show(ctx: &CliContext) -> Result<()> {
    // If the PID file does not exist, the daemon has never started or already
    // cleaned up after itself on exit.
    if !ctx.pid_path.exists() {
        println!("Cascade is not running.");
        return Ok(());
    }

    // Read and parse the PID.
    let raw = std::fs::read_to_string(&ctx.pid_path)
        .with_context(|| format!("failed to read {}", ctx.pid_path.display()))?;
    let pid: u32 = raw
        .trim()
        .parse()
        .with_context(|| format!("invalid PID in {}: {:?}", ctx.pid_path.display(), raw.trim()))?;

    // Verify the process is actually alive.
    if !is_process_alive(pid) {
        println!("Cascade is not running (stale PID file).");
        return Ok(());
    }

    let db = StateDb::open(&ctx.db_path)?;
    let backends = db.list_backends()?;

    println!("Cascade Status");
    println!("  Running: true");
    println!("  PID: {pid}");
    println!("  State DB: {}", ctx.db_path.display());

    if backends.is_empty() {
        println!("  No backends registered.");
    } else {
        println!("  Backends:");
        for b in &backends {
            let cursor_info = db
                .get_cursor(&b.id)
                .ok()
                .flatten()
                .map_or_else(|| "no cursor".to_string(), |c| format!("cursor: {}", c.0));
            println!(
                "    {} ({}) — {} [{}]",
                b.display_name, b.backend_type, b.id, cursor_info
            );
            if let Some(mp) = &b.mount_path {
                println!("      Mount: {mp}");
            }
        }
    }

    Ok(())
}

/// List configured backends.
pub fn backend_list(ctx: &CliContext) -> Result<()> {
    if !ctx.db_path.exists() {
        println!("No backends configured. State database not found.");
        return Ok(());
    }

    let db = StateDb::open(&ctx.db_path)?;
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
