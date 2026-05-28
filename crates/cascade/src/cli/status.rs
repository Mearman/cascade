use anyhow::{Context as _, Result};
use cascade_engine::db::StateDb;

use super::{CliContext, is_process_alive};

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
    let pid: u32 = raw.trim().parse().with_context(|| {
        format!(
            "invalid PID in {}: {:?}",
            ctx.pid_path.display(),
            raw.trim()
        )
    })?;

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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_ctx(dir: &TempDir) -> CliContext {
        let config_dir = dir.path().to_path_buf();
        CliContext {
            db_path: config_dir.join("state.db"),
            pid_path: config_dir.join("cascade.pid"),
            config_dir,
        }
    }

    #[test]
    fn show_reports_not_running_when_no_pid_file() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);

        // Should succeed — prints "not running" rather than erroring.
        show(&ctx).unwrap();
    }

    #[test]
    fn show_reports_running_with_valid_pid_file() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);

        // Write the current process PID — it is definitely alive.
        std::fs::write(&ctx.pid_path, std::process::id().to_string()).unwrap();

        // Seed a backend in the DB so the output lists it.
        let db = StateDb::open(&ctx.db_path).unwrap();
        db.register_backend(
            "test-gdrive",
            "gdrive",
            "Google Drive (test)",
            Some("/mnt/cloud"),
            None,
        )
        .unwrap();

        show(&ctx).unwrap();
    }

    #[test]
    fn show_reports_stale_pid_when_process_dead() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);

        // PID 999999999 is extremely unlikely to be a real process.
        std::fs::write(&ctx.pid_path, "999999999").unwrap();

        show(&ctx).unwrap();
    }

    #[test]
    fn show_lists_registered_backends() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);

        std::fs::write(&ctx.pid_path, std::process::id().to_string()).unwrap();

        let db = StateDb::open(&ctx.db_path).unwrap();
        db.register_backend("s3-backup", "s3", "S3 (backup)", None, None)
            .unwrap();
        db.register_backend(
            "gdrive-work",
            "gdrive",
            "Google Drive (work)",
            Some("Work"),
            None,
        )
        .unwrap();

        show(&ctx).unwrap();
    }

    #[test]
    fn show_handles_corrupt_pid_file() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);

        std::fs::write(&ctx.pid_path, "not-a-number").unwrap();

        // Should return an error — the PID is unparseable.
        assert!(show(&ctx).is_err());
    }

    #[test]
    fn backend_list_with_no_db() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);

        // No state.db file exists yet.
        backend_list(&ctx).unwrap();
    }

    #[test]
    fn backend_list_with_empty_db() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);

        // Create the DB but don't register any backends.
        let _db = StateDb::open(&ctx.db_path).unwrap();

        backend_list(&ctx).unwrap();
    }

    #[test]
    fn backend_list_shows_registered_backends() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);

        let db = StateDb::open(&ctx.db_path).unwrap();
        db.register_backend(
            "personal",
            "gdrive",
            "Google Drive (personal)",
            Some("/Personal"),
            None,
        )
        .unwrap();
        db.register_backend("backup", "s3", "S3 (backup)", None, None)
            .unwrap();

        backend_list(&ctx).unwrap();
    }
}
