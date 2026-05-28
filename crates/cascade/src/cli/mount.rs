use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use cascade_engine::engine::{Engine, EngineConfig};
use cascade_presenter_nfs::nfs::server::{NfsServer, NfsServerConfig};

use super::init::{BackendConfig, CascadeConfig};

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

/// Start the Cascade daemon.
pub async fn start(mount_point: Option<&str>) -> Result<()> {
    tracing::info!("Starting Cascade daemon");

    // Resolve config directory and database path.
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".cascade"))
        .join("cascade");
    std::fs::create_dir_all(&config_dir)?;

    // Bail early if the daemon is already running.
    let pid_path = config_dir.join("cascade.pid");
    if pid_path.exists() {
        let raw = std::fs::read_to_string(&pid_path)
            .with_context(|| format!("failed to read {}", pid_path.display()))?;
        if let Ok(pid) = raw.trim().parse::<u32>() {
            if is_process_alive(pid) {
                anyhow::bail!(
                    "Cascade is already running (PID {pid}). Run `cascade stop` first."
                );
            }
            // Stale PID file — clean it up and continue.
            let _ = std::fs::remove_file(&pid_path);
        }
    }
    let db_path = config_dir.join("state.db");

    // Read main config.toml written by `cascade init`.
    let main_config = load_main_config(&config_dir)?;

    // Create backends from config.
    if main_config.backends.is_empty() {
        anyhow::bail!("No backends configured. Run `cascade init` to set up.");
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

    std::fs::write(&pid_path, std::process::id().to_string())?;

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

    let _ = std::fs::remove_file(&pid_path);

    tracing::info!("Cascade stopped.");
    Ok(())
}

/// Stop the Cascade daemon.
#[cfg(unix)]
pub fn stop() -> anyhow::Result<()> {
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from(".cascade"))
        .join("cascade");
    let pid_path = config_dir.join("cascade.pid");

    if !pid_path.exists() {
        println!("Cascade is not running.");
        return Ok(());
    }

    let raw = std::fs::read_to_string(&pid_path)
        .with_context(|| format!("failed to read {}", pid_path.display()))?;
    let pid_u32: u32 = raw
        .trim()
        .parse()
        .with_context(|| format!("invalid PID in {}: {:?}", pid_path.display(), raw.trim()))?;
    let pid_signed =
        i32::try_from(pid_u32).with_context(|| format!("PID {pid_u32} overflows i32"))?;
    let pid = nix::unistd::Pid::from_raw(pid_signed);

    match nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM) {
        Ok(()) => {}
        Err(nix::errno::Errno::ESRCH) => {
            // Process no longer exists — clean up stale PID file.
            let _ = std::fs::remove_file(&pid_path);
            println!("Cascade is not running.");
            return Ok(());
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "failed to send SIGTERM to PID {pid_u32}: {e}"
            ));
        }
    }

    // Wait up to 5 seconds for the process to exit (10 × 500 ms polls).
    let mut exited = false;
    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if nix::sys::signal::kill(pid, None) == Err(nix::errno::Errno::ESRCH) {
            exited = true;
            break;
        }
    }

    if !exited {
        eprintln!("Warning: process {pid_u32} did not exit within 5 seconds after SIGTERM.");
    }

    let _ = std::fs::remove_file(&pid_path);
    println!("Cascade stopped.");
    Ok(())
}

/// Stop the Cascade daemon.
#[cfg(not(unix))]
pub fn stop() -> anyhow::Result<()> {
    anyhow::bail!("cascade stop is not supported on this platform yet");
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
#[cfg(target_os = "macos")]
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
#[cfg(target_os = "macos")]
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
#[cfg(target_os = "macos")]
fn is_mounted(path: &Path) -> bool {
    let output = std::process::Command::new("/sbin/mount").output();

    let mounts = match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(_) => return false,
    };
    mounts.contains(&*path.to_string_lossy())
}

#[cfg(not(target_os = "macos"))]
fn mount_nfs(_mount_point: &Path, _port: u16) -> Result<()> {
    anyhow::bail!("NFS mounting is not supported on this platform yet");
}

#[cfg(not(target_os = "macos"))]
fn unmount_nfs(_mount_point: &Path) -> Result<()> {
    Ok(()) // no-op: nothing was mounted
}

#[cfg(not(target_os = "macos"))]
fn is_mounted(_path: &Path) -> bool {
    false
}
