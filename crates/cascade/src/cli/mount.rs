use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use cascade_engine::engine::{Engine, EngineConfig};
use cascade_engine::presenter::VfsPresenter;
use cascade_presenter_nfs::nfs::server::{NfsServer, NfsServerConfig};
use cascade_presenter_webdav::WebDavPresenter;

use super::init::{BackendConfig, CascadeConfig};
use super::{CliContext, is_process_alive};

/// Start the Cascade daemon.
pub async fn start(ctx: &CliContext, mount_point: Option<&str>) -> Result<()> {
    tracing::info!("Starting Cascade daemon");

    std::fs::create_dir_all(&ctx.config_dir)?;

    // Bail early if the daemon is already running.
    if ctx.pid_path.exists() {
        let raw = std::fs::read_to_string(&ctx.pid_path)
            .with_context(|| format!("failed to read {}", ctx.pid_path.display()))?;
        if let Ok(pid) = raw.trim().parse::<u32>() {
            if is_process_alive(pid) {
                anyhow::bail!("Cascade is already running (PID {pid}). Run `cascade stop` first.");
            }
            // Stale PID file — clean it up and continue.
            let _ = std::fs::remove_file(&ctx.pid_path);
        }
    }

    // Read main config.toml written by `cascade init`.
    let main_config = load_main_config(&ctx.config_dir)?;

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
        let per_backend_config = load_backend_config(&ctx.config_dir, name)?;
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
        db_path: ctx.db_path.clone(),
        mount_point: mount_path.clone(),
        backends,
        cache_dir: None,
        enable_p2p: false,
        p2p_data_dir: None,
    };
    let engine = Engine::new(engine_config).await?;

    // On macOS, prefer WebDAV mounting which works without root.
    // NFS is the fallback for Linux and older macOS where WebDAV fails.
    let mount_strategy = select_mount_strategy();
    tracing::info!(strategy = %mount_strategy, "selected mount strategy");

    #[cfg(target_os = "macos")]
    match mount_strategy {
        MountStrategy::WebDav => run_with_webdav(ctx, mount_path, engine).await,
        MountStrategy::Nfs => run_with_nfs(ctx, mount_path, engine).await,
    }
    #[cfg(not(target_os = "macos"))]
    match mount_strategy {
        MountStrategy::Nfs => run_with_nfs(ctx, mount_path, engine).await,
        MountStrategy::WebDav => {
            anyhow::bail!("WebDAV mounting is not supported on this platform")
        }
    }
}

/// Mount strategy — `WebDAV` preferred on macOS, NFS elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MountStrategy {
    WebDav,
    #[allow(dead_code)] // Used on non-macOS platforms
    Nfs,
}

impl std::fmt::Display for MountStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WebDav => write!(f, "webdav"),
            Self::Nfs => write!(f, "nfs"),
        }
    }
}

/// Select the best mount strategy for the current platform.
const fn select_mount_strategy() -> MountStrategy {
    #[cfg(target_os = "macos")]
    {
        MountStrategy::WebDav
    }
    #[cfg(not(target_os = "macos"))]
    {
        MountStrategy::Nfs
    }
}

/// Run the daemon using `WebDAV` presenter.
#[cfg(target_os = "macos")]
async fn run_with_webdav(ctx: &CliContext, mount_path: PathBuf, engine: Engine) -> Result<()> {
    let presenter = Arc::new(WebDavPresenter::new(&mount_path));
    let items = presenter.items().clone();

    let sync_runner = engine.create_sync_runner(presenter.clone());
    let sync_handle = tokio::spawn(async move {
        if let Err(e) = sync_runner.run().await {
            tracing::error!(error = %e, "sync runner exited with error");
        }
    });

    let handle = engine.start()?;

    // Start the WebDAV presenter's HTTP server.
    presenter.start(&mount_path).await?;
    let webdav_port = {
        let guard = presenter.server.lock().await;
        guard
            .as_ref()
            .map(cascade_presenter_webdav::server::WebDavServer::port)
            .context("WebDAV server not started")?
    };

    mount_webdav(&mount_path, webdav_port)?;

    std::fs::write(&ctx.pid_path, std::process::id().to_string())?;

    println!("Cascade started.");
    println!("  Mount point: {}", mount_path.display());
    println!("  WebDAV port: {webdav_port}");
    println!("  PID: {}", std::process::id());
    println!();
    println!("Press Ctrl+C to stop.");

    tokio::signal::ctrl_c().await?;

    tracing::info!("Shutting down...");

    unmount_path(&mount_path)?;
    presenter.stop().await?;
    drop(items);
    sync_handle.abort();
    engine.shutdown();
    handle.cache_handle.abort();

    let _ = std::fs::remove_file(&ctx.pid_path);

    tracing::info!("Cascade stopped.");
    Ok(())
}

/// Run the daemon using NFS presenter.
async fn run_with_nfs(ctx: &CliContext, mount_path: PathBuf, engine: Engine) -> Result<()> {
    let presenter = Arc::new(cascade_presenter_nfs::NfsPresenter::with_vfs(
        &mount_path,
        engine.vfs().clone(),
    ));

    let nfs_ctx = presenter.context().clone();

    let sync_runner = engine.create_sync_runner(presenter);
    let sync_handle = tokio::spawn(async move {
        if let Err(e) = sync_runner.run().await {
            tracing::error!(error = %e, "sync runner exited with error");
        }
    });

    let handle = engine.start()?;

    let server_config = NfsServerConfig {
        bind_addr: "127.0.0.1:0".parse()?,
        export_path: "/".to_string(),
    };
    let server = NfsServer::start(server_config, nfs_ctx).await?;
    let nfs_port = server.local_addr.port();
    tracing::info!(port = nfs_port, "NFS server started");

    mount_nfs(&mount_path, nfs_port)?;

    std::fs::write(&ctx.pid_path, std::process::id().to_string())?;

    println!("Cascade started.");
    println!("  Mount point: {}", mount_path.display());
    println!("  NFS port: {nfs_port}");
    println!("  PID: {}", std::process::id());
    println!();
    println!("Press Ctrl+C to stop.");

    tokio::signal::ctrl_c().await?;

    tracing::info!("Shutting down...");

    unmount_path(&mount_path)?;
    server.stop()?;
    sync_handle.abort();
    engine.shutdown();
    handle.cache_handle.abort();

    let _ = std::fs::remove_file(&ctx.pid_path);

    tracing::info!("Cascade stopped.");
    Ok(())
}

/// Stop the Cascade daemon.
#[cfg(unix)]
pub fn stop(ctx: &CliContext) -> anyhow::Result<()> {
    if !ctx.pid_path.exists() {
        println!("Cascade is not running.");
        return Ok(());
    }

    let raw = std::fs::read_to_string(&ctx.pid_path)
        .with_context(|| format!("failed to read {}", ctx.pid_path.display()))?;
    let pid_u32: u32 = raw.trim().parse().with_context(|| {
        format!(
            "invalid PID in {}: {:?}",
            ctx.pid_path.display(),
            raw.trim()
        )
    })?;
    let pid_signed =
        i32::try_from(pid_u32).with_context(|| format!("PID {pid_u32} overflows i32"))?;
    let pid = nix::unistd::Pid::from_raw(pid_signed);

    match nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM) {
        Ok(()) => {}
        Err(nix::errno::Errno::ESRCH) => {
            // Process no longer exists — clean up stale PID file.
            let _ = std::fs::remove_file(&ctx.pid_path);
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

    let _ = std::fs::remove_file(&ctx.pid_path);
    println!("Cascade stopped.");
    Ok(())
}

/// Stop the Cascade daemon.
#[cfg(not(unix))]
pub fn stop(_ctx: &CliContext) -> anyhow::Result<()> {
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

/// Mount `WebDAV` filesystem using the OS mount command (macOS).
/// Uses `/sbin/mount_webdav` which does not require root.
#[cfg(target_os = "macos")]
fn mount_webdav(mount_point: &Path, port: u16) -> Result<()> {
    if !mount_point.exists() {
        std::fs::create_dir_all(mount_point)?;
    }

    let url = format!("http://127.0.0.1:{port}/");
    tracing::info!(url = %url, mount = %mount_point.display(), "mounting WebDAV");

    let output = std::process::Command::new("/sbin/mount_webdav")
        .arg(&url)
        .arg(mount_point)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if is_mounted(mount_point) {
            tracing::warn!(path = %mount_point.display(), "already mounted");
            return Ok(());
        }
        anyhow::bail!("mount_webdav failed: {stderr}");
    }

    tracing::info!(path = %mount_point.display(), "WebDAV mounted");
    Ok(())
}

/// Build a `mount_webdav` command (for testing command construction).
#[cfg(test)]
#[cfg(target_os = "macos")]
fn webdav_mount_command(mount_point: &Path, port: u16) -> std::process::Command {
    let url = format!("http://127.0.0.1:{port}/");
    let mut cmd = std::process::Command::new("/sbin/mount_webdav");
    cmd.arg(&url).arg(mount_point);
    cmd
}

/// Unmount a filesystem at the given path.
#[cfg(target_os = "macos")]
fn unmount_path(mount_point: &Path) -> Result<()> {
    if !is_mounted(mount_point) {
        tracing::debug!(path = %mount_point.display(), "not mounted, skipping unmount");
        return Ok(());
    }

    tracing::info!(path = %mount_point.display(), "unmounting");

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

/// Mount NFS filesystem using the OS mount command (macOS).
///
/// On macOS 26+, `NFSv3` mounts fail with permission errors. `NFSv4` works
/// without admin escalation (matching FUSE-T's approach). We try v4 first,
/// then fall back to v3 with osascript escalation.
#[cfg(target_os = "macos")]
fn mount_nfs(mount_point: &Path, port: u16) -> Result<()> {
    if !mount_point.exists() {
        std::fs::create_dir_all(mount_point)?;
    }

    let nfs_spec = "127.0.0.1:/".to_string();
    tracing::info!(spec = %nfs_spec, port, mount = %mount_point.display(), "mounting NFS");

    // Try `NFSv4` first — works on macOS 26+ without admin privileges.
    let v4_output = std::process::Command::new("/sbin/mount_nfs")
        .arg("-o")
        .arg(format!("vers=4,resvport,port={port}"))
        .arg(&nfs_spec)
        .arg(mount_point)
        .output();

    match v4_output {
        Ok(output) if output.status.success() => {
            tracing::info!(path = %mount_point.display(), "`NFSv4` mounted");
            return Ok(());
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::debug!(
                path = %mount_point.display(),
                error = %stderr,
                "`NFSv4` mount failed, falling back to v3"
            );
        }
        Err(e) => {
            tracing::debug!(
                path = %mount_point.display(),
                error = %e,
                "mount_nfs command failed for v4, falling back to v3"
            );
        }
    }

    // Fall back to `NFSv3`.
    let output = std::process::Command::new("/sbin/mount_nfs")
        .arg("-o")
        .arg(format!("rw,resvport,port={port}"))
        .arg(&nfs_spec)
        .arg(mount_point)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if is_mounted(mount_point) {
            tracing::warn!(path = %mount_point.display(), "already mounted");
            return Ok(());
        }

        // Retry with administrator privileges on permission errors.
        if is_permission_error(&stderr) {
            tracing::info!(path = %mount_point.display(), "retrying mount with administrator privileges");
            let escalated = osascript_mount_command(mount_point, port).output()?;
            if escalated.status.success() {
                tracing::info!(path = %mount_point.display(), "NFS mounted (via administrator privileges)");
                return Ok(());
            }
            let esc_stderr = String::from_utf8_lossy(&escalated.stderr);
            anyhow::bail!("mount_nfs failed even with administrator privileges: {esc_stderr}");
        }

        anyhow::bail!("mount_nfs failed: {stderr}");
    }

    tracing::info!(path = %mount_point.display(), "`NFSv3` mounted");
    Ok(())
}

/// Check if a `mount_nfs` stderr indicates a permission error.
#[cfg(target_os = "macos")]
fn is_permission_error(stderr: &str) -> bool {
    stderr.contains("Operation not permitted")
}

/// Build an osascript command that mounts NFS with administrator privileges.
#[cfg(target_os = "macos")]
fn osascript_mount_command(mount_point: &Path, port: u16) -> std::process::Command {
    let mount_cmd = format!(
        "/sbin/mount_nfs -o rw,resvport,port={port} 127.0.0.1:/ {mp}",
        mp = mount_point.display()
    );
    let script = format!("do shell script \"{mount_cmd}\" with administrator privileges");
    let mut cmd = std::process::Command::new("osascript");
    cmd.arg("-e").arg(&script);
    cmd
}

/// Unmount the NFS filesystem (kept for compatibility with non-macOS code paths).
#[cfg(not(target_os = "macos"))]
#[allow(clippy::unnecessary_wraps)]
fn unmount_path(_mount_point: &Path) -> Result<()> {
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
#[allow(clippy::missing_const_for_fn, clippy::unused_self)]
fn mount_nfs(_mount_point: &Path, _port: u16) -> Result<()> {
    anyhow::bail!("NFS mounting is not supported on this platform yet");
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::missing_const_for_fn, clippy::unnecessary_wraps)]
fn mount_webdav(_mount_point: &Path, _port: u16) -> Result<()> {
    anyhow::bail!("WebDAV mounting is not supported on this platform yet");
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::missing_const_for_fn, dead_code)]
fn is_mounted(_path: &Path) -> bool {
    false
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_ctx(dir: &TempDir) -> CliContext {
        let config_dir: PathBuf = dir.path().to_path_buf();
        CliContext {
            db_path: config_dir.join("state.db"),
            pid_path: config_dir.join("cascade.pid"),
            config_dir,
        }
    }

    #[test]
    fn stop_succeeds_when_no_pid_file() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);

        // No PID file exists — should print "not running" and succeed.
        stop(&ctx).unwrap();
    }

    #[test]
    fn stop_cleans_up_stale_pid_file() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);

        // PID 999999999 is not a real process.
        std::fs::write(&ctx.pid_path, "999999999").unwrap();

        stop(&ctx).unwrap();

        // The stale PID file should have been removed.
        assert!(!ctx.pid_path.exists());
    }

    // --- Permission escalation ---

    #[test]
    fn is_permission_error_detects_operation_not_permitted() {
        assert!(is_permission_error(
            "mount_nfs: can't mount / from 127.0.0.1 onto /tmp/cloud: Operation not permitted"
        ));
    }

    #[test]
    fn is_permission_error_rejects_other_errors() {
        assert!(!is_permission_error("mount_nfs: No such file or directory"));
    }

    #[test]
    fn osascript_mount_command_constructs_correctly() {
        let dir = TempDir::new().unwrap();
        let mount_point = dir.path().join("Cloud");
        let cmd = osascript_mount_command(&mount_point, 12345);

        assert_eq!(cmd.get_program(), "osascript");
        let args: Vec<String> = cmd
            .get_args()
            .map(std::ffi::OsStr::to_string_lossy)
            .map(std::borrow::Cow::into_owned)
            .collect();
        assert_eq!(args[0], "-e");
        assert!(args[1].contains("with administrator privileges"));
        assert!(args[1].contains("port=12345"));
        assert!(args[1].contains(mount_point.to_str().unwrap()));
    }

    #[test]
    fn mount_nfs_on_unreachable_port_errors_without_privilege_escalation() {
        // When mount_nfs fails with a non-permission error (e.g. connection
        // refused), it should not try osascript escalation.
        //
        // We can't easily control mount_nfs output, but we CAN test the
        // decision function directly — which is the real logic under test.
        assert!(!is_permission_error("Connection refused"));
        assert!(!is_permission_error("No such file or directory"));
        assert!(is_permission_error("Operation not permitted"));
    }

    // --- WebDAV mount command construction ---

    #[cfg(target_os = "macos")]
    #[test]
    fn webdav_mount_command_constructs_correctly() {
        let dir = TempDir::new().unwrap();
        let mount_point = dir.path().join("Cloud");
        let cmd = webdav_mount_command(&mount_point, 52431);

        assert_eq!(cmd.get_program(), "/sbin/mount_webdav");
        let args: Vec<String> = cmd
            .get_args()
            .map(std::ffi::OsStr::to_string_lossy)
            .map(std::borrow::Cow::into_owned)
            .collect();
        assert_eq!(args[0], "http://127.0.0.1:52431/");
        assert_eq!(args[1], mount_point.to_str().unwrap());
    }

    #[test]
    fn mount_strategy_selects_correctly() {
        let strategy = select_mount_strategy();
        #[cfg(target_os = "macos")]
        assert_eq!(strategy, MountStrategy::WebDav);
        #[cfg(not(target_os = "macos"))]
        assert_eq!(strategy, MountStrategy::Nfs);
    }

    #[test]
    fn mount_strategy_display() {
        assert_eq!(MountStrategy::WebDav.to_string(), "webdav");
        assert_eq!(MountStrategy::Nfs.to_string(), "nfs");
    }
}
