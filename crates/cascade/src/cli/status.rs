use anyhow::Result;
use cascade_engine::protocol::StatusInfo;

/// Show overall mount status.
pub async fn show() -> Result<()> {
    let status = StatusInfo {
        running: false,
        mount_point: None,
        backends: vec![],
        cache_stats: Default::default(),
    };

    println!("Cascade Status");
    println!("  Running: {}", status.running);
    if let Some(mp) = &status.mount_point {
        println!("  Mount point: {}", mp);
    }
    if status.backends.is_empty() {
        println!("  No backends configured.");
    } else {
        for backend in &status.backends {
            println!(
                "  Backend: {} ({}) — {}",
                backend.display_name, backend.backend_type, backend.id
            );
        }
    }

    Ok(())
}

/// Show cache status.
pub async fn cache_status() -> Result<()> {
    println!("Cache Status");
    println!("  No cache data available.");
    Ok(())
}

/// List configured backends.
pub async fn backend_list() -> Result<()> {
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from(".cascade"))
        .join("cascade");
    let db_path = config_dir.join("state.db");

    if !db_path.exists() {
        println!("No backends configured. State database not found.");
        return Ok(());
    }

    let db = cascade_engine::db::StateDb::open(&db_path)?;
    let backends = db.list_backends()?;

    if backends.is_empty() {
        println!("No backends configured.");
    } else {
        for b in &backends {
            println!("{} ({}): {}", b.display_name, b.backend_type, b.id);
            if let Some(mp) = &b.mount_path {
                println!("  Mount path: {}", mp);
            }
        }
    }

    Ok(())
}
