use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use cascade_engine::engine::{Engine, EngineConfig};
use cascade_presenter_nfs::nfs::server::{NfsServer, NfsServerConfig};

use super::init::{BackendConfig, CascadeConfig};

/// Start the Cascade daemon.
pub async fn start(mount_point: Option<&str>) -> Result<()> {
    tracing::info!("Starting Cascade daemon");

    // Resolve config directory and database path.
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".cascade"))
        .join("cascade");
    std::fs::create_dir_all(&config_dir)?;
    let db_path = config_dir.join("state.db");

    // Read main config.toml written by `cascade init`.
    let main_config = load_main_config(&config_dir)?;

    // Create backends from config.
    if main_config.backends.is_empty() {
        anyhow::bail!(
            "No backends configured. Run `cascade init` to set up."
        );
    }

    let mut backends: Vec<Arc<dyn cascade_engine::backend::Backend>> = Vec::new();
    for (name, value) in &main_config.backends {
        let backend_cfg: BackendConfig = value
            .clone()
            .try_into()
            .with_context(|| format!("invalid config for backend `{name}`"))?;
        let per_backend_config = load_backend_config(&config_dir, name)?;
        let backend = create_backend(name, &backend_cfg.backend_type, &per_backend_config)?;
        backends.push(Arc::from(backend));
    }

    // Resolve mount point: CLI arg > config.toml [mount].point > ~/Cloud.
    let mount_path = if let Some(p) = mount_point {
        resolve_mount_path(p)
    } else {
        let configured = main_config.mount.point.trim().to_string();
        if configured.is_empty() {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join("Cloud")
        } else {
            resolve_mount_path(&configured)
        }
    };

    std::fs::create_dir_all(&mount_path)?;

    // Create and start the engine.
    let engine_config = EngineConfig {
        db_path,
        mount_point: mount_path.clone(),
        backends,
        cache_dir: None,
        enable_p2p: false,
        p2p_data_dir: None,
    };
    let engine = Engine::new(engine_config).await?;
    let handle = engine.start()?;

    // Create presenter using engine's VFS.
    let presenter = Arc::new(cascade_presenter_nfs::NfsPresenter::with_vfs(
        &mount_path,
        engine.vfs().clone(),
    ));

    // Start NFS server on loopback, sharing the presenter's context.
    let server_config = NfsServerConfig {
        bind_addr: "127.0.0.1:0".parse()?,
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
    server.stop()?;
    engine.shutdown();
    handle.sync_handle.abort();
    handle.cache_handle.abort();

    tracing::info!("Cascade stopped.");
    Ok(())
}

/// Stop the Cascade daemon.
pub fn stop() {
    // Phase 1: find the running process and signal it.
    // For now, the user runs Ctrl+C on the foreground process.
    println!("Cascade stopped.");
}

/// Instantiate a backend by type, using its per-backend TOML config.
fn create_backend(
    name: &str,
    backend_type: &str,
    config: &toml::Value,
) -> Result<Box<dyn cascade_engine::backend::Backend>> {
    match backend_type {
        "gdrive" => cascade_backend_gdrive::create_backend(config)
            .with_context(|| format!("failed to create gdrive backend `{name}`")),
        "s3" => cascade_backend_s3::create_backend(config)
            .with_context(|| format!("failed to create s3 backend `{name}`")),
        other => anyhow::bail!("unsupported backend type: {other}"),
    }
}

/// Load the top-level `config.toml` from the config directory.
fn load_main_config(config_dir: &Path) -> Result<CascadeConfig> {
    let config_path = config_dir.join("config.toml");
    if config_path.exists() {
        let contents = std::fs::read_to_string(&config_path)
            .with_context(|| format!("failed to read {}", config_path.display()))?;
        let config: CascadeConfig = toml::from_str(&contents)
            .with_context(|| format!("failed to parse {}", config_path.display()))?;
        tracing::info!(path = ?config_path, "Loaded main config");
        Ok(config)
    } else {
        anyhow::bail!(
            "Config file not found at {}. Run `cascade init` to create it.",
            config_path.display()
        );
    }
}

/// Load a backend config from the config directory.
fn load_backend_config(config_dir: &Path, name: &str) -> Result<toml::Value> {
    let config_path = config_dir.join(format!("{name}.toml"));
    if !config_path.exists() {
        anyhow::bail!(
            "Backend config not found: {}. Re-run `cascade init` or create it manually.",
            config_path.display()
        );
    }
    let contents = std::fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let value: toml::Value = toml::from_str(&contents)
        .with_context(|| format!("failed to parse {}", config_path.display()))?;
    tracing::info!(path = ?config_path, "Loaded backend config");
    Ok(value)
}

/// Resolve a mount point path, expanding ~ and environment variables.
fn resolve_mount_path(path: &str) -> PathBuf {
    let expanded = shellexpand::tilde(path).to_string();
    PathBuf::from(expanded)
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
