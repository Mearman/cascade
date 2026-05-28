use anyhow::{Context as _, Result};
use cascade_engine::db::StateDb;
use std::path::PathBuf;

/// Check whether the process with the given PID is alive.
///
/// On Unix, sends signal 0 (no-op) via `kill(2)` — returns `true` if the call
/// succeeds (process exists and we have permission to signal it), `false` if
/// `ESRCH` is returned (no such process), and `false` for any other error.
///
/// On non-Unix platforms, a reliable cross-process liveness check is not
/// available without an OS-specific crate, so the presence of the PID file is
/// treated as sufficient and this function always returns `true`.
fn is_process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid_signed) = i32::try_from(pid) else {
            return false;
        };
        let nix_pid = nix::unistd::Pid::from_raw(pid_signed);
        match nix::sys::signal::kill(nix_pid, None) {
            Ok(()) => true,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        // No reliable cross-platform liveness check without an OS crate.
        // Treat presence of PID file as sufficient.
        let _ = pid;
        true
    }
}

/// Show overall mount status.
pub fn show() -> Result<()> {
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".cascade"))
        .join("cascade");
    let pid_path = config_dir.join("cascade.pid");

    // If the PID file does not exist, the daemon has never started or already
    // cleaned up after itself on exit.
    if !pid_path.exists() {
        println!("Cascade is not running.");
        return Ok(());
    }

    // Read and parse the PID.
    let raw = std::fs::read_to_string(&pid_path)
        .with_context(|| format!("failed to read {}", pid_path.display()))?;
    let pid: u32 = raw
        .trim()
        .parse()
        .with_context(|| format!("invalid PID in {}: {:?}", pid_path.display(), raw.trim()))?;

    // Verify the process is actually alive.
    if !is_process_alive(pid) {
        println!("Cascade is not running (stale PID file).");
        return Ok(());
    }

    let db_path = config_dir.join("state.db");
    let db = StateDb::open(&db_path)?;
    let backends = db.list_backends()?;

    println!("Cascade Status");
    println!("  Running: true");
    println!("  PID: {pid}");
    println!("  State DB: {}", db_path.display());

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
