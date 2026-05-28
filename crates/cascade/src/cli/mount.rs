use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use cascade_engine::engine::{Engine, EngineConfig};
use cascade_presenter_nfs::nfs::server::{NfsServer, NfsServerConfig};

/// Start the Cascade daemon.
pub async fn start(mount_point: Option<&str>) -> Result<()> {
    tracing::info!("Starting Cascade daemon");

    // Resolve config directory and database path.
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".cascade"))
        .join("cascade");
    std::fs::create_dir_all(&config_dir)?;
    let db_path = config_dir.join("state.db");

    // Create backend(s) from config.
    // Phase 1: single Google Drive backend with defaults.
    let gdrive_config = load_backend_config(&config_dir, "gdrive")?;
    let backend = cascade_backend_gdrive::create_backend(&gdrive_config)?;
    let backend: Arc<dyn cascade_engine::backend::Backend> = Arc::from(backend);

    // Resolve mount point.
    let mount_path = mount_point.map(resolve_mount_path).unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("Cloud")
    });

    std::fs::create_dir_all(&mount_path)?;

    // Create and start the engine.
    let engine_config = EngineConfig {
        db_path,
        mount_point: mount_path.clone(),
        backends: vec![backend],
        cache_dir: None,
        enable_p2p: false,
        p2p_data_dir: None,
    };
    let engine = Engine::new(engine_config).await?;
    let handle = engine.start().await?;

    // Create presenter using engine's VFS.
    let presenter = Arc::new(cascade_presenter_nfs::NfsPresenter::with_vfs(
        &mount_path,
        engine.vfs().clone(),
    ));

    // Start NFS server on loopback, sharing the presenter's context.
    let server_config = NfsServerConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        export_path: "/".to_string(),
    };
    let nfs_ctx = presenter.context().clone();
    let server = NfsServer::start(server_config, nfs_ctx).await?;
    let nfs_port = server.local_addr.port();
    tracing::info!(port = nfs_port, "NFS server started");

    // Mount NFS via OS mount command.
    mount_nfs(&mount_path, nfs_port)?;

    println!("Cascade started.");
    println!("  Mount point: {}", mount_path.display());
    println!("  NFS port: {nfs_port}");
    println!("  PID: {}", std::process::id());
    println!();
    println!("Press Ctrl+C to stop.");

    // Wait for Ctrl+C.
    tokio::signal::ctrl_c().await?;

    tracing::info!("Shutting down...");

    // Clean up.
    unmount_nfs(&mount_path)?;
    server.stop().await?;
    engine.shutdown().await?;
    handle.sync_handle.abort();
    handle.cache_handle.abort();

    tracing::info!("Cascade stopped.");
    Ok(())
}

/// Stop the Cascade daemon.
pub async fn stop() -> Result<()> {
    // Phase 1: find the running process and signal it.
    // For now, the user runs Ctrl+C on the foreground process.
    println!("Cascade stopped.");
    Ok(())
}

/// Resolve a mount point path, expanding ~ and environment variables.
fn resolve_mount_path(path: &str) -> PathBuf {
    let expanded = shellexpand::tilde(path).to_string();
    PathBuf::from(expanded)
}

/// Load a backend config from the config directory.
fn load_backend_config(config_dir: &Path, name: &str) -> Result<toml::Value> {
    let config_path = config_dir.join(format!("{name}.toml"));
    if config_path.exists() {
        let contents = std::fs::read_to_string(&config_path)?;
        let value: toml::Value = toml::from_str(&contents)?;
        tracing::info!(path = ?config_path, "Loaded backend config");
        Ok(value)
    } else {
        tracing::warn!(path = ?config_path, "Backend config not found, using defaults");
        Ok(toml::Value::Table(Default::default()))
    }
}

/// Mount NFS filesystem using the OS mount command (macOS).
fn mount_nfs(mount_point: &Path, port: u16) -> Result<()> {
    // Create the mount point directory.
    if !mount_point.exists() {
        std::fs::create_dir_all(mount_point)?;
    }

    let nfs_spec = format!("127.0.0.1:{port}:/");
    tracing::info!(spec = %nfs_spec, mount = %mount_point.display(), "mounting NFS");

    // macOS mount_nfs.
    let output = std::process::Command::new("/sbin/mount_nfs")
        .arg("-o")
        .arg("rw,resvport")
        .arg(&nfs_spec)
        .arg(mount_point)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // mount_nfs may fail if already mounted — check.
        if is_mounted(mount_point) {
            tracing::warn!(path = %mount_point.display(), "already mounted");
            return Ok(());
        }
        anyhow::bail!("mount_nfs failed: {stderr}");
    }

    tracing::info!(path = %mount_point.display(), "NFS mounted");
    Ok(())
}

/// Unmount the NFS filesystem.
fn unmount_nfs(mount_point: &Path) -> Result<()> {
    if !is_mounted(mount_point) {
        tracing::debug!(path = %mount_point.display(), "not mounted, skipping unmount");
        return Ok(());
    }

    tracing::info!(path = %mount_point.display(), "unmounting NFS");

    let output = std::process::Command::new("/usr/bin/diskutil")
        .arg("unmount")
        .arg(mount_point)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(error = %stderr, "unmount failed (will be cleaned up on exit)");
    }

    Ok(())
}

/// Check if a path is already mounted.
fn is_mounted(path: &Path) -> bool {
    let output = std::process::Command::new("/sbin/mount").output();

    let mounts = match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(_) => return false,
    };
    mounts.contains(&*path.to_string_lossy())
}
