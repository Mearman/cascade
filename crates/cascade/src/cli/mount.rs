use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use cascade_engine::engine::{Engine, EngineConfig};
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
use cascade_engine::presenter::VfsPresenter;
use cascade_presenter_nfs::nfs::server::{NfsCacheMode, NfsServer, NfsServerConfig};
use cascade_presenter_webdav::WebDavPresenter;

use super::init::{BackendConfig, CascadeConfig, P2pConfig, WebConfig};
use super::{CliContext, is_process_alive};

// ---------------------------------------------------------------------------
// HTTP API (PWA) wiring
// ---------------------------------------------------------------------------

/// Default HTTP API bind — loopback only.
const WEB_DEFAULT_BIND: &str = "127.0.0.1:7842";
/// Default HTTP API request timeout in seconds (one hour).
const WEB_DEFAULT_TIMEOUT_SECS: u64 = 3600;
/// Default HTTP API maximum request body size in bytes (1 GiB).
const WEB_DEFAULT_MAX_BODY_BYTES: usize = 1024 * 1024 * 1024;

/// The HTTP API flags supplied on the `cascade start` command line.
#[derive(Debug, Clone, Default)]
pub struct WebFlags {
    /// `--web`: serve the HTTP API (overrides `[web].enabled`).
    pub enable: bool,
    /// `--web-bind`: bind address override.
    pub bind: Option<String>,
    /// `--web-bundle-url`: advertised PWA bundle URL.
    pub bundle_url: Option<String>,
    /// `--web-cors-origin`: additional CORS origins (repeatable).
    pub cors_origins: Vec<String>,
}

/// The resolved HTTP API configuration, merging `config.toml` `[web]` with the
/// CLI flags. Always present so the `web` parameter is used in both feature
/// builds; the actual server is feature-gated.
///
/// All fields but `enabled` are read only by the `web`-feature server; without
/// the feature the struct still carries them (so `resolve_web_options` and the
/// CLI surface are identical across builds), hence the feature-conditional
/// dead-code allowance.
#[derive(Debug, Clone)]
#[cfg_attr(not(feature = "web"), allow(dead_code))]
struct WebOptions {
    enabled: bool,
    bind: String,
    bundle_url: Option<String>,
    cors_origins: Vec<String>,
    request_timeout_secs: u64,
    max_body_bytes: usize,
}

/// Merge the `[web]` config table with the CLI flags. A flag overrides the
/// config value; `--web` and `[web].enabled` are OR-ed; CORS origins from both
/// sources accumulate.
fn resolve_web_options(cfg: &WebConfig, flags: &WebFlags) -> WebOptions {
    let mut cors_origins = cfg.cors_origins.clone();
    cors_origins.extend(flags.cors_origins.iter().cloned());
    WebOptions {
        enabled: flags.enable || cfg.enabled,
        bind: flags
            .bind
            .clone()
            .or_else(|| cfg.bind.clone())
            .unwrap_or_else(|| WEB_DEFAULT_BIND.to_owned()),
        bundle_url: flags.bundle_url.clone().or_else(|| cfg.bundle_url.clone()),
        cors_origins,
        request_timeout_secs: cfg.request_timeout_secs.unwrap_or(WEB_DEFAULT_TIMEOUT_SECS),
        max_body_bytes: cfg.max_body_bytes.unwrap_or(WEB_DEFAULT_MAX_BODY_BYTES),
    }
}

/// A running HTTP API server plus the readiness handle the daemon flips once the
/// data plane is up.
#[cfg(feature = "web")]
struct WebRuntime {
    handle: cascade_web_api::RouterHandle,
    readiness: cascade_web_api::Readiness,
}

/// What [`start_web`] returns: a running server (or `None` when disabled) in the
/// `web` build, or a unit placeholder otherwise. The alias keeps the presenter
/// call sites identical across feature builds.
#[cfg(feature = "web")]
type WebRuntimeOpt = Option<WebRuntime>;
/// See [`WebRuntimeOpt`] — the placeholder form when the `web` feature is off.
#[cfg(not(feature = "web"))]
type WebRuntimeOpt = Option<()>;

/// Validate the resolved web options and build the bind configuration without
/// touching the network, so a misconfigured `[web]` fails fast at startup —
/// before any mount is attempted — rather than after a presenter has come up.
///
/// Catches an unparseable bind address, the `0.0.0.0`-without-`bundle_url`
/// footgun, and a wildcard CORS origin (refused by `BindConfig::new`).
#[cfg(feature = "web")]
fn validated_web_config(
    web: &WebOptions,
) -> Result<(std::net::SocketAddr, cascade_web_api::state::BindConfig)> {
    let bind: std::net::SocketAddr = web
        .bind
        .parse()
        .with_context(|| format!("[web] bind `{}` is not a valid socket address", web.bind))?;
    if bind.ip().is_unspecified() && web.bundle_url.is_none() {
        anyhow::bail!(
            "[web] bind {bind} exposes the API on all interfaces but no bundle_url is set; \
             refusing to start. A bearer-auth API on a public interface without TLS leaks \
             credentials — put a TLS-terminating reverse proxy in front and set bundle_url."
        );
    }
    let bind_config = cascade_web_api::state::BindConfig::new(
        bind,
        web.bundle_url.clone(),
        web.cors_origins.clone(),
        web.request_timeout_secs,
        web.max_body_bytes,
        env!("CARGO_PKG_VERSION").to_owned(),
        option_env!("CASCADE_BUILD_SHA").map(ToOwned::to_owned),
    )
    .context("invalid [web] configuration")?;
    Ok((bind, bind_config))
}

/// Build the HTTP API state from a live engine and spawn the server, when the
/// `web` feature is compiled in and the operator enabled it.
#[cfg(feature = "web")]
async fn start_web(engine: &Arc<Engine>, web: &WebOptions) -> Result<WebRuntimeOpt> {
    use cascade_web_api::state::{AppState, NodeIdentity, Readiness};

    if !web.enabled {
        return Ok(None);
    }

    let identity = engine.device_identity().cloned().context(
        "the HTTP API requires a device identity; configure a P2P backend or omit --web",
    )?;
    let (bind, bind_config) = validated_web_config(web)?;
    if bind.ip().is_unspecified() {
        tracing::warn!(
            %bind,
            "serving the HTTP API on a public interface — ensure a TLS-terminating reverse \
             proxy fronts it; bearer tokens must never cross the wire in clear"
        );
    }

    let readiness = Readiness::new(chrono::Utc::now());
    let state = AppState::new(
        engine.clone(),
        NodeIdentity::new(identity),
        bind_config,
        readiness.clone(),
    );
    let handle = cascade_web_api::serve(state)
        .await
        .context("could not bind the HTTP API socket")?;
    tracing::info!(addr = %handle.local_addr(), "HTTP API serving");
    println!("  HTTP API: http://{}", handle.local_addr());
    Ok(Some(WebRuntime { handle, readiness }))
}

/// Stub form when the `web` feature is off: refuse loudly if the operator asked
/// for the API, otherwise do nothing.
#[cfg(not(feature = "web"))]
#[allow(clippy::unused_async)]
async fn start_web(_engine: &Arc<Engine>, web: &WebOptions) -> Result<WebRuntimeOpt> {
    if web.enabled {
        anyhow::bail!(
            "the HTTP API was requested (--web or [web].enabled) but this binary was built \
             without the `web` feature; rebuild with `cargo build --features web`"
        );
    }
    Ok(None)
}

/// Flip the data-plane readiness bit once the presenter is up, so the F3-gated
/// data routes begin serving. A no-op without the `web` feature.
#[cfg(feature = "web")]
fn mark_web_ready(runtime: &WebRuntimeOpt) {
    if let Some(runtime) = runtime {
        runtime.readiness.set_data_plane_ready(true);
    }
}

/// See [`mark_web_ready`] — the no-op form when the `web` feature is off.
#[cfg(not(feature = "web"))]
#[allow(clippy::trivially_copy_pass_by_ref)] // must match the #[cfg(web)] signature
const fn mark_web_ready(_runtime: &WebRuntimeOpt) {}

/// Gracefully stop the HTTP API server on daemon shutdown, draining in-flight
/// requests. A no-op without the `web` feature.
#[cfg(feature = "web")]
async fn stop_web(runtime: WebRuntimeOpt) {
    if let Some(runtime) = runtime {
        runtime.handle.stop().await;
    }
}

/// See [`stop_web`] — the no-op form when the `web` feature is off.
#[cfg(not(feature = "web"))]
#[allow(clippy::unused_async)]
async fn stop_web(_runtime: WebRuntimeOpt) {}

/// Resolved optimisation-layer P2P configuration threaded into every
/// `EngineConfig` at daemon startup.
///
/// Bundles the three new `EngineConfig` fields so they can be passed
/// to each `try_*` presenter constructor without repeating the same three
/// arguments on every call.
#[derive(Debug, Clone)]
struct ResolvedP2pConfig {
    enable_p2p: bool,
    posture: Option<cascade_p2p::DiscoveryReach>,
    relay_endpoints: Vec<std::net::SocketAddr>,
    relay_shared_secret: Option<[u8; 32]>,
}

/// Parse the `[p2p]` table from `config.toml` into the fields that the engine's
/// P2P bridge needs.
///
/// Fails loudly on a malformed `posture` value (typos should surface at startup,
/// not silently default to a less-exposed posture the operator did not intend)
/// and on a malformed relay secret (wrong length or non-hex characters).
fn resolve_p2p_bridge_config(cfg: &P2pConfig) -> Result<ResolvedP2pConfig> {
    let posture = cfg.posture.as_deref().map(parse_posture_str).transpose()?;

    let relay_endpoints = cfg
        .relay_endpoint
        .as_deref()
        .map(|ep| {
            // Accept a DNS hostname or a Docker service name, not only a literal
            // IP:port — resolved here at config load so compose service names
            // like `relay:9999` work. The relay is dialled by the resolved
            // address; restart the daemon if the relay's address later changes.
            use std::net::ToSocketAddrs as _;
            ep.to_socket_addrs()
                .with_context(|| {
                    format!("[p2p] relay_endpoint `{ep}` is not a resolvable host:port")
                })?
                .next()
                .with_context(|| format!("[p2p] relay_endpoint `{ep}` resolved to no addresses"))
        })
        .transpose()?
        .map(|addr| vec![addr])
        .unwrap_or_default();

    let relay_shared_secret = cfg
        .relay_shared_secret
        .as_deref()
        .map(parse_relay_secret_hex)
        .transpose()?;

    Ok(ResolvedP2pConfig {
        enable_p2p: cfg.enabled,
        posture,
        relay_endpoints,
        relay_shared_secret,
    })
}

/// Parse a posture string from the config file into a `DiscoveryReach`.
///
/// Fails loudly rather than silently defaulting: an operator who typed `publik`
/// deserves a clear error rather than discovering the node quietly confined to
/// the LAN when they meant WAN, or the reverse.
fn parse_posture_str(s: &str) -> Result<cascade_p2p::DiscoveryReach> {
    match s {
        "lan-only" => Ok(cascade_p2p::DiscoveryReach::LanOnly),
        "private" => Ok(cascade_p2p::DiscoveryReach::Private),
        "public" => Ok(cascade_p2p::DiscoveryReach::Public),
        other => anyhow::bail!(
            "[p2p] posture `{other}` is not valid; expected one of: lan-only, private, public"
        ),
    }
}

/// Parse a 64-character hex relay shared secret into a 32-byte array.
///
/// The relay HMAC key is exactly 32 bytes (64 hex chars). Catching a
/// malformed value at startup is better than a confusing runtime 401.
fn parse_relay_secret_hex(hex: &str) -> Result<[u8; 32]> {
    if hex.len() != 64 {
        anyhow::bail!(
            "[p2p] relay_shared_secret must be exactly 64 hex characters (32 bytes), got {}",
            hex.len()
        );
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let pair_start = i
            .checked_mul(2)
            .ok_or_else(|| anyhow::anyhow!("relay_shared_secret index overflow"))?;
        let pair_end = pair_start
            .checked_add(2)
            .ok_or_else(|| anyhow::anyhow!("relay_shared_secret index overflow"))?;
        let pair = hex.get(pair_start..pair_end).ok_or_else(|| {
            anyhow::anyhow!("relay_shared_secret hex slice out of range at position {pair_start}")
        })?;
        *byte = u8::from_str_radix(pair, 16)
            .with_context(|| format!("invalid hex pair `{pair}` in relay_shared_secret"))?;
    }
    Ok(out)
}

/// Park until the process is asked to shut down.
///
/// On Unix this resolves on `SIGINT` (Ctrl+C) **or** `SIGTERM`. `SIGTERM` is
/// what `cascade stop`, launchd, and systemd send to stop the daemon; catching
/// it lets the caller run its graceful unmount-and-cleanup path rather than
/// being terminated abruptly with the mount left stale. On non-Unix platforms
/// `SIGTERM` has no equivalent, so only Ctrl+C is awaited.
#[cfg(unix)]
async fn wait_for_shutdown_signal() -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = interrupt.recv() => {}
        _ = terminate.recv() => {}
    }
    Ok(())
}

/// Park until the process is asked to shut down (Ctrl+C on non-Unix platforms,
/// which have no `SIGTERM` equivalent).
#[cfg(not(unix))]
async fn wait_for_shutdown_signal() -> std::io::Result<()> {
    tokio::signal::ctrl_c().await
}

/// Resources held by a running presenter attempt.
///
/// When a mount strategy fails, [`PresenterResources::shutdown`] must be
/// called to stop every component that was started during the attempt
/// (sync task, presenter, engine background tasks).  This prevents leaked
/// tasks from an earlier attempt surviving into the next fallback.
struct PresenterResources {
    /// The engine instance (dropped last so cancel signals reach tasks). Held as
    /// an `Arc` so the management-plane dispatcher wired into the P2P backend at
    /// startup ([`Engine::wire_manage_dispatch`]) can share ownership.
    engine: Arc<Engine>,
    /// Join handle for the cache-manager background task.
    engine_handle: cascade_engine::engine::EngineHandle,
    /// Join handle for the sync runner task.
    sync_handle: tokio::task::JoinHandle<()>,
}

impl PresenterResources {
    /// Shut down all components in reverse start order: abort the sync runner,
    /// signal the engine to cancel, then abort the cache-manager task as a safety net.
    fn shutdown(self) {
        self.sync_handle.abort();
        self.engine.shutdown();
        self.engine_handle.cache_handle.abort();
        // `engine` is dropped here, releasing the VFS tree and cancel token.
    }
}

/// Upper bound on the daemon's own unmount-and-teardown after a stop signal,
/// before it forces an exit.
///
/// Sits comfortably inside the 5 s grace `cascade stop` allows after `SIGTERM`,
/// so in a wedged shutdown the daemon self-terminates before the external
/// `SIGKILL` escalation fires, while still guaranteeing it never hangs — for
/// example on a macOS `webdavfs` unmount that stalls because the in-process
/// `WebDAV` server it is talking to is itself tearing down.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// Run the post-signal shutdown sequence, bounded by [`SHUTDOWN_GRACE`].
///
/// The OS unmount is a blocking subprocess (`diskutil`/`umount`/`net use`), so
/// it runs on a blocking thread rather than a runtime worker: unmounting a
/// self-hosted mount makes the kernel client issue its final requests to the
/// in-process presenter server, which must stay responsive to answer them, and
/// occupying a worker with the blocking call can wedge that exchange.
/// `teardown` then stops the presenter and engine. If the whole sequence does
/// not finish within the grace period the daemon removes its PID file and
/// forces an exit rather than hanging with an unkillable process and a stale
/// mount — which the next `cascade start` reclaims, and which `cascade stop`
/// also clears once the process is gone.
async fn finish_shutdown(
    pid_path: &Path,
    mount_path: Option<PathBuf>,
    teardown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let sequence = async move {
        if let Some(mount_path) = mount_path {
            let _ = tokio::task::spawn_blocking(move || unmount_path(&mount_path)).await;
        }
        teardown.await;
    };

    if tokio::time::timeout(SHUTDOWN_GRACE, sequence)
        .await
        .is_err()
    {
        tracing::warn!(
            grace_secs = SHUTDOWN_GRACE.as_secs(),
            "graceful shutdown timed out; forcing exit"
        );
        let _ = std::fs::remove_file(pid_path);
        std::process::exit(0);
    }

    let _ = std::fs::remove_file(pid_path);
    tracing::info!("Cascade stopped.");
    Ok(())
}

/// Start the Cascade daemon.
///
/// `p2p_override` lets the CLI flag (`--p2p` / `--no-p2p`) override the
/// `[p2p].enabled` value from `config.toml`. `None` falls through to
/// whatever the config file says.
pub async fn start(
    ctx: &CliContext,
    mount_point: Option<&str>,
    no_mount: bool,
    p2p_override: Option<bool>,
    file_provider: bool,
    web_flags: WebFlags,
) -> Result<()> {
    tracing::info!("Starting Cascade daemon");

    ensure_directory(&ctx.config_dir, "config directory")?;

    // Bail early if the daemon is already running.
    if ctx.pid_path.exists() {
        let raw = read_text_file(&ctx.pid_path, "PID file")?;
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

    // Resolve P2P config; misconfigured posture or relay values surface here before any mount attempt.
    let mut p2p_config = resolve_p2p_bridge_config(&main_config.p2p)?;

    // --p2p / --no-p2p overrides the [p2p].enabled value from config.toml.
    if let Some(override_val) = p2p_override {
        p2p_config.enable_p2p = override_val;
    }

    if p2p_config.enable_p2p {
        tracing::info!("P2P optimisation layer enabled");
    }

    // Resolve the HTTP API config from the [web] table and the CLI flags. The
    // server is spawned inside whichever presenter strategy succeeds.
    let web_options = resolve_web_options(&main_config.web, &web_flags);
    if web_options.enabled {
        tracing::info!("HTTP API (PWA) enabled");
        // Fail fast on a misconfigured API before mounting anything.
        #[cfg(feature = "web")]
        validated_web_config(&web_options).context("[web] configuration is invalid")?;
        #[cfg(not(feature = "web"))]
        anyhow::bail!(
            "the HTTP API was requested (--web or [web].enabled) but this binary was built \
             without the `web` feature; rebuild with `cargo build --features web`"
        );
    }

    // Create backends from config.
    //
    // A daemon with nothing to mount has no work to do, so it exits cleanly
    // rather than bailing with an error. This matters when `cascade start` is
    // run by an OS background service (`cascade service`): a non-zero exit
    // would make launchd / systemd treat the start as a crash and restart it
    // in a tight loop. Exiting 0 lets the service stay idle until a backend is
    // added and the service is restarted. The advice to run `cascade init` is
    // surfaced for the interactive operator who started the daemon by hand.
    if main_config.backends.is_empty() {
        tracing::info!("No backends configured; nothing to mount.");
        println!("No backends configured. Run `cascade init` to set one up, then start again.");
        return Ok(());
    }

    // Build the single long-lived pooled HTTP client on the daemon's stable
    // main runtime (the Drive TLS deadlock fix). All Drive backends share it.
    let shared_http: Arc<dyn cascade_engine::portable::HttpClient> =
        Arc::new(cascade_engine::portable::native::ReqwestClient::new());

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

    // Clean up any stale mount left behind by a previous run that
    // exited without unmounting (e.g. kill -9, tmux kill, panic).
    #[cfg(target_os = "macos")]
    evict_stale_mount(&mount_path);

    ensure_directory(&mount_path, "mount point")?;

    // Override knob for containers and tests: CASCADE_PRESENTER=webdav
    // forces the WebDAV presenter on any platform. Useful for Linux
    // containers where /dev/fuse may be unavailable and NFS needs root.
    if std::env::var("CASCADE_PRESENTER").as_deref() == Ok("webdav") {
        let backends = rebuild_backends(&main_config, &ctx.config_dir, shared_http.clone())?;
        tracing::info!(
            strategy = "webdav-forced",
            "honouring CASCADE_PRESENTER=webdav"
        );
        return try_webdav(
            ctx,
            &mount_path,
            backends,
            no_mount,
            &p2p_config,
            &web_options,
        )
        .await;
    }

    // `--file-provider` is an explicit opt-in to the macOS File Provider
    // bridge. It is an alternative to FSKit, not a fallback in the FSKit
    // chain — both are macOS-native and self-mounting, so trying both would
    // be meaningless. When requested, it is the only strategy attempted.
    #[cfg(not(target_os = "macos"))]
    if file_provider {
        anyhow::bail!("--file-provider is only supported on macOS");
    }

    #[cfg(target_os = "macos")]
    if file_provider {
        let backends = rebuild_backends(&main_config, &ctx.config_dir, shared_http.clone())?;
        tracing::info!(strategy = "fileprovider", "attempting File Provider mount");
        return try_fileprovider(
            ctx,
            &mount_path,
            backends,
            no_mount,
            &p2p_config,
            &web_options,
        )
        .await;
    }

    // On macOS: FSKit first (native, kext-free, POSIX), then WebDAV (no
    // root needed), then NFSv4/v3 fallback.  NFS already has the v4 → v3 →
    // escalation chain inside mount_nfs().
    //
    // FSKit self-mounts via the Swift extension and cannot be used without
    // mounting, so it is skipped when --no-mount is set.
    #[cfg(target_os = "macos")]
    {
        // Attempt 1: FSKit (skipped if --no-mount since FSKit always self-mounts).
        if !no_mount {
            let backends = rebuild_backends(&main_config, &ctx.config_dir, shared_http.clone())?;
            tracing::info!(strategy = "fskit", "attempting FSKit mount");
            match try_fskit(ctx, &mount_path, backends, &p2p_config, &web_options).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    tracing::warn!(error = %e, "FSKit mount failed, falling back to WebDAV");
                    drop(e);
                }
            }
        }

        // Attempt 2: WebDAV.
        let backends = rebuild_backends(&main_config, &ctx.config_dir, shared_http.clone())?;
        tracing::info!(strategy = "webdav", "attempting WebDAV mount");
        match try_webdav(
            ctx,
            &mount_path,
            backends,
            no_mount,
            &p2p_config,
            &web_options,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!(error = %e, "WebDAV mount failed, falling back to NFS");
                drop(e);
            }
        }

        // Attempt 3: NFS (v4 → v3 escalation inside mount_nfs).
        let backends = rebuild_backends(&main_config, &ctx.config_dir, shared_http.clone())?;
        try_nfs(
            ctx,
            &mount_path,
            backends,
            no_mount,
            &p2p_config,
            &web_options,
        )
        .await
    }

    #[cfg(target_os = "linux")]
    {
        // When --no-mount is set, skip FUSE entirely (it always mounts and
        // the entrypoint's seed mode relies on this gate to stay unprivileged).
        // Fall straight through to WebDAV, which honours no_mount and runs
        // without /dev/fuse or SYS_ADMIN. Otherwise (the operator wants a real
        // mount, and CASCADE_PRESENTER=webdav was not forced above) try FUSE
        // first, then NFS.
        if no_mount {
            // Headless seed mode used by the Docker entrypoint: run the WebDAV
            // server without mounting.
            let backends = rebuild_backends(&main_config, &ctx.config_dir, shared_http.clone())?;
            tracing::info!(
                strategy = "webdav-seed",
                "--no-mount set on Linux; starting WebDAV server in seed mode"
            );
            try_webdav(
                ctx,
                &mount_path,
                backends,
                no_mount,
                &p2p_config,
                &web_options,
            )
            .await
        } else {
            let backends = rebuild_backends(&main_config, &ctx.config_dir, shared_http.clone())?;
            tracing::info!(strategy = "fuse", "attempting FUSE mount");
            match try_fuse(ctx, &mount_path, backends, &p2p_config, &web_options).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    tracing::warn!(error = %e, "FUSE mount failed, falling back to NFS");
                    drop(e);
                }
            }

            let backends = rebuild_backends(&main_config, &ctx.config_dir, shared_http.clone())?;
            try_nfs(
                ctx,
                &mount_path,
                backends,
                no_mount,
                &p2p_config,
                &web_options,
            )
            .await
        }
    }

    #[cfg(target_os = "windows")]
    {
        // Attempt 1: ProjFS (native, no admin required).
        //
        // The presenter implements the full callback table and serves
        // on-demand reads through the engine-backed content provider.
        // ProjFS self-mounts via the Win32 ProjectedFileSystem API and
        // cannot be used without mounting, so it is skipped when
        // --no-mount is set.
        if !no_mount {
            let backends = rebuild_backends(&main_config, &ctx.config_dir, shared_http.clone())?;
            tracing::info!(strategy = "projfs", "attempting ProjFS mount");
            match try_projfs(ctx, &mount_path, backends, &p2p_config, &web_options).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    tracing::warn!(error = %e, "ProjFS mount failed, falling back to WebDAV");
                    drop(e);
                }
            }
        }

        // Attempt 2: WebDAV via the built-in WebClient service.
        let backends = rebuild_backends(&main_config, &ctx.config_dir, shared_http.clone())?;
        tracing::info!(strategy = "webdav", "attempting WebDAV mount");
        try_webdav(
            ctx,
            &mount_path,
            backends,
            no_mount,
            &p2p_config,
            &web_options,
        )
        .await
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let backends = rebuild_backends(&main_config, &ctx.config_dir, shared_http.clone())?;
        try_nfs(
            ctx,
            &mount_path,
            backends,
            no_mount,
            &p2p_config,
            &web_options,
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// macOS presenters
// ---------------------------------------------------------------------------

/// Try to mount via the `FSKit` presenter.
///
/// On success, blocks until Ctrl+C and then shuts down.  On failure at any
/// stage (engine start, presenter start, mount command) every resource that
/// was started is stopped before the error is returned, so the caller can
/// safely attempt the next fallback.
#[cfg(target_os = "macos")]
async fn try_fskit(
    ctx: &CliContext,
    mount_path: &Path,
    backends: Vec<cascade_engine::backend::MountedBackend>,
    p2p: &ResolvedP2pConfig,
    web: &WebOptions,
) -> Result<()> {
    let engine_config = EngineConfig {
        db_path: ctx.db_path.clone(),
        mount_point: mount_path.to_path_buf(),
        backends,
        cache_dir: None,
        enable_p2p: p2p.enable_p2p,
        p2p_data_dir: None,
        p2p_posture: p2p.posture,
        p2p_relay_endpoints: p2p.relay_endpoints.clone(),
        p2p_relay_shared_secret: p2p.relay_shared_secret,
        backend_factory: Some(cli_backend_factory()),
    };
    let engine = Arc::new(Engine::new(engine_config)?);
    // Wire the management-plane dispatcher into every backend that serves
    // remote management (the P2P backend). Done before the presenter starts so
    // an inbound ManageRequest is authorised and executed rather than refused.
    engine.wire_manage_dispatch().await;

    // Clone the engine handle for the HTTP API before the engine is moved into
    // the presenter resources; the API server is started only once the winning
    // presenter is confirmed up, so a failed attempt never binds the port.
    let engine_for_web = engine.clone();

    let presenter = Arc::new(
        cascade_presenter_fskit::FSKitPresenter::from_default_socket()?
            .with_mount_point(mount_path),
    );

    let sync_runner = engine.create_sync_runner(presenter.clone());
    let cancel = engine.cancel_flag();
    let sync_handle = tokio::spawn(async move {
        if let Err(e) = sync_runner.run(cancel).await {
            tracing::error!(error = %e, "sync runner exited with error");
        }
    });

    let engine_handle = engine.start()?;

    let resources = PresenterResources {
        engine,
        engine_handle,
        sync_handle,
    };

    if let Err(e) = presenter.start(mount_path).await {
        resources.shutdown();
        return Err(e).with_context(|| {
            format!(
                "failed to start FSKit presenter at {}",
                mount_path.display()
            )
        });
    }

    // FSKit presenter.start() triggers the mount internally via the Swift
    // extension; no separate OS mount command is needed.

    if let Err(e) = write_pid_file(&ctx.pid_path) {
        resources.shutdown();
        return Err(e);
    }

    println!("Cascade started (FSKit).");
    println!("  Mount point: {}", mount_path.display());
    println!("  PID: {}", std::process::id());
    println!();
    println!("Press Ctrl+C to stop.");

    // The presenter is up: spawn the HTTP API (feature-gated, when enabled) and
    // flip the F3 data-plane readiness bit so its data routes begin serving.
    let web_runtime = start_web(&engine_for_web, web).await?;
    mark_web_ready(&web_runtime);

    wait_for_shutdown_signal().await?;

    // Stop the HTTP API before tearing the rest down.
    stop_web(web_runtime).await;

    tracing::info!("Shutting down...");

    finish_shutdown(&ctx.pid_path, Some(mount_path.to_path_buf()), async move {
        if let Err(e) = presenter.stop().await {
            tracing::warn!(error = %e, "FSKit presenter stop returned an error");
        }
        resources.shutdown();
    })
    .await
}

/// A presenter that consumes nothing.
///
/// The File Provider model is *pull-based*: Finder, via the
/// `NSFileProviderReplicatedExtension`, drives the daemon by calling
/// `enumerateItems` / `enumerateChanges` / `fetchContents` over the RPC
/// socket and tracking the sync anchor. Nothing in the extension listens
/// for engine-initiated push notifications, so the sync runner — which
/// exists to poll backends, apply changes to the state DB, and feed the
/// change index — has no presenter to push to. This no-op satisfies the
/// `VfsPresenter` contract while discarding the push direction the File
/// Provider path does not use.
#[cfg(target_os = "macos")]
#[derive(Debug)]
struct NoopPresenter;

#[cfg(target_os = "macos")]
#[async_trait::async_trait]
impl VfsPresenter for NoopPresenter {
    async fn upsert_item(&self, _item: cascade_engine::types::VfsItem) -> Result<()> {
        Ok(())
    }

    async fn delete_item(&self, _id: &cascade_engine::types::ItemId) -> Result<()> {
        Ok(())
    }

    async fn update_state(
        &self,
        _id: &cascade_engine::types::ItemId,
        _state: cascade_engine::types::CacheState,
    ) -> Result<()> {
        Ok(())
    }

    async fn fetch_contents(&self, _id: &cascade_engine::types::ItemId) -> Result<PathBuf> {
        // Content is fetched on demand through the File Provider RPC
        // server's `fetchContents` handler, never through the sync
        // runner's presenter. Reaching here means the runner tried to
        // pull content itself, which it does not do for File Provider.
        anyhow::bail!("no-op presenter does not serve content")
    }

    async fn evict_item(&self, _id: &cascade_engine::types::ItemId) -> Result<()> {
        Ok(())
    }

    async fn start(&self, _mount_point: &Path) -> Result<()> {
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        Ok(())
    }
}

/// Try to serve the macOS File Provider RPC bridge.
///
/// Unlike `FSKit`, the daemon does not mount anything here. It stands up the
/// Unix-socket server (`~/.config/cascade/fileprovider.sock`) that the
/// Swift `CascadeFileProvider` extension connects to per request, and runs
/// the sync runner so the state database and the engine's change feed stay
/// populated. The extension surfaces the tree in Finder once its File
/// Provider domain is registered — a step performed by the Swift host app
/// (`NSFileProviderManager`), not by this daemon. See
/// `docs/fileprovider-smoke-test.md` for the end-to-end bring-up.
///
/// `no_mount` is accepted for signature parity with the other strategies
/// but has no effect: there is no OS-level mount command to skip, because
/// the extension owns the mount.
#[cfg(target_os = "macos")]
async fn try_fileprovider(
    ctx: &CliContext,
    mount_path: &Path,
    backends: Vec<cascade_engine::backend::MountedBackend>,
    _no_mount: bool,
    p2p: &ResolvedP2pConfig,
    web: &WebOptions,
) -> Result<()> {
    use cascade_presenter_fileprovider::engine_handlers::EngineHandlers;
    use cascade_presenter_fileprovider::server::FileProviderServer;

    let engine_config = EngineConfig {
        db_path: ctx.db_path.clone(),
        mount_point: mount_path.to_path_buf(),
        backends,
        cache_dir: None,
        enable_p2p: p2p.enable_p2p,
        p2p_data_dir: None,
        p2p_posture: p2p.posture,
        p2p_relay_endpoints: p2p.relay_endpoints.clone(),
        p2p_relay_shared_secret: p2p.relay_shared_secret,
        backend_factory: Some(cli_backend_factory()),
    };
    let engine = Arc::new(Engine::new(engine_config)?);
    // Wire the management-plane dispatcher into every backend that serves
    // remote management (the P2P backend). Done before the presenter starts so
    // an inbound ManageRequest is authorised and executed rather than refused.
    engine.wire_manage_dispatch().await;

    // Clone the engine handle for the HTTP API before the engine is moved into
    // the presenter resources; the API server is started only once the winning
    // presenter is confirmed up, so a failed attempt never binds the port.
    let engine_for_web = engine.clone();

    // The File Provider RPC server answers inbound queries from the Swift
    // extension against the engine's live VFS, state DB, cache directory,
    // and change feed. The cache directory is a sibling of the state DB
    // under the config root.
    let cache_dir = ctx.config_dir.join("cache");
    ensure_directory(&cache_dir, "File Provider cache directory")?;
    let handlers = Arc::new(EngineHandlers::new(
        engine.vfs().clone(),
        engine.db().clone(),
        cache_dir,
        engine.change_feed(),
    ));

    let socket_path = cascade_presenter_fileprovider::bridge::default_socket_path()
        .context("resolve File Provider socket path")?;
    let server = Arc::new(FileProviderServer::new(socket_path.clone(), handlers));

    // Cancellation channel for the server task; signalled on Ctrl+C below.
    let (server_cancel_tx, server_cancel_rx) = tokio::sync::watch::channel(false);
    let server_for_task = server.clone();
    let server_handle = tokio::spawn(async move {
        if let Err(e) = server_for_task.serve(server_cancel_rx).await {
            tracing::error!(error = %e, "File Provider server exited with error");
        }
    });

    // The sync runner drives backend polling: it populates the state DB
    // and the change feed (attached by `create_sync_runner`). Its push
    // direction has no consumer in the pull-based File Provider model, so
    // it pushes to a no-op presenter.
    let sync_runner = engine.create_sync_runner(Arc::new(NoopPresenter));
    let cancel = engine.cancel_flag();
    let sync_handle = tokio::spawn(async move {
        if let Err(e) = sync_runner.run(cancel).await {
            tracing::error!(error = %e, "sync runner exited with error");
        }
    });

    let engine_handle = engine.start()?;

    let resources = PresenterResources {
        engine,
        engine_handle,
        sync_handle,
    };

    if let Err(e) = write_pid_file(&ctx.pid_path) {
        let _ = server_cancel_tx.send(true);
        server_handle.abort();
        resources.shutdown();
        return Err(e);
    }

    println!("Cascade started (File Provider bridge).");
    println!("  RPC socket: {}", socket_path.display());
    println!("  PID: {}", std::process::id());
    println!();
    println!("The Swift File Provider extension surfaces the tree in Finder once its");
    println!(
        "domain is registered via the Cascade host app — see docs/fileprovider-smoke-test.md."
    );
    println!();
    println!("Press Ctrl+C to stop.");

    // The presenter is up: spawn the HTTP API (feature-gated, when enabled) and
    // flip the F3 data-plane readiness bit so its data routes begin serving.
    let web_runtime = start_web(&engine_for_web, web).await?;
    mark_web_ready(&web_runtime);

    wait_for_shutdown_signal().await?;

    // Stop the HTTP API before tearing the rest down.
    stop_web(web_runtime).await;

    tracing::info!("Shutting down...");

    let _ = server_cancel_tx.send(true);
    // Give the server task a moment to unbind the socket cleanly, then
    // abort if it has not returned.
    if tokio::time::timeout(std::time::Duration::from_secs(2), server_handle)
        .await
        .is_err()
    {
        tracing::warn!("File Provider server did not stop within 2s");
    }
    let _ = std::fs::remove_file(&socket_path);
    resources.shutdown();

    let _ = std::fs::remove_file(&ctx.pid_path);

    tracing::info!("Cascade stopped.");
    Ok(())
}

/// Try to mount via the `ProjFS` presenter (Windows only).
///
/// `ProjFS` (Projected File System) is the native Windows equivalent of
/// `FSKit` on macOS and FUSE on Linux. The presenter self-mounts via
/// `PrjStartVirtualizing` — there is no separate OS mount command — so
/// this function does not need a `no_mount` parameter and is skipped at
/// the dispatch level when `--no-mount` is set.
///
/// Same shutdown guarantees as the other presenter functions: on any
/// failure during engine, presenter, or mount setup, every started
/// resource is stopped before the error is returned.
#[cfg(target_os = "windows")]
async fn try_projfs(
    ctx: &CliContext,
    mount_path: &Path,
    backends: Vec<cascade_engine::backend::MountedBackend>,
    p2p: &ResolvedP2pConfig,
    web: &WebOptions,
) -> Result<()> {
    // Extract the raw backend Arc list for the content provider before the
    // MountedBackend list is consumed by EngineConfig. Cloning is cheap —
    // just Arc reference-count bumps.
    let backends_for_provider: Vec<Arc<dyn cascade_engine::backend::Backend>> =
        backends.iter().map(|mb| mb.backend.clone()).collect();

    let engine_config = EngineConfig {
        db_path: ctx.db_path.clone(),
        mount_point: mount_path.to_path_buf(),
        backends,
        cache_dir: None,
        enable_p2p: p2p.enable_p2p,
        p2p_data_dir: None,
        p2p_posture: p2p.posture,
        p2p_relay_endpoints: p2p.relay_endpoints.clone(),
        p2p_relay_shared_secret: p2p.relay_shared_secret,
        backend_factory: Some(cli_backend_factory()),
    };
    let engine = Arc::new(Engine::new(engine_config)?);
    // Wire the management-plane dispatcher into every backend that serves
    // remote management (the P2P backend). Done before the presenter starts so
    // an inbound ManageRequest is authorised and executed rather than refused.
    engine.wire_manage_dispatch().await;

    // Clone the engine handle for the HTTP API before the engine is moved into
    // the presenter resources; the API server is started only once the winning
    // presenter is confirmed up, so a failed attempt never binds the port.
    let engine_for_web = engine.clone();

    // Capture the daemon's runtime handle while still on it. The ProjFS
    // GetFileData callback runs on a kernel thread outside any runtime; the
    // provider uses this handle to `block_on` the cold-path download.
    //
    // The daemon's runtime is built explicitly with
    // `tokio::runtime::Runtime::new()` in `main.rs` and owned by `main`'s
    // stack for the whole life of `block_on(args.run(..))`, so it provably
    // outlives every ProjFS callback: the daemon does not return from this
    // function (it parks on `ctrl_c` below) until shutdown, by which point
    // the presenter has been stopped and no further callbacks can fire.
    //
    // `Runtime::new()` builds a multi-thread runtime. That flavour matters:
    // the provider's cold-path bridge falls back to `block_in_place` when it
    // detects it is already on a runtime worker, and `block_in_place` is only
    // valid on the multi-thread scheduler. A current-thread runtime would
    // make that fallback panic, so assert the flavour here where the daemon
    // wiring is in view rather than discovering it at a callback.
    let handle = tokio::runtime::Handle::current();
    debug_assert_eq!(
        handle.runtime_flavor(),
        tokio::runtime::RuntimeFlavor::MultiThread,
        "ProjFS provider requires the daemon's multi-thread runtime; \
         see crates/cascade/src/main.rs (Runtime::new)"
    );

    // The content provider serves on-demand reads. Each read fetches only
    // the requested byte range straight from the owning backend, so there
    // is no whole-file cache directory to provision.
    let provider = Arc::new(super::projfs_provider::EngineContentProvider::new(
        engine.vfs().clone(),
        engine.db().clone(),
        backends_for_provider,
        handle,
    ));

    // Do not set `with_root`: the sync runner labels each top-level item
    // with its backend's own root id (e.g. `gdrive:root`), not a single
    // shared root. Pinning one root id would filter out every backend but
    // one. Leaving the root unset uses the presenter's loose-root
    // fallback, treating every item in the map as eligible — which is
    // exactly what the multi-backend daemon populates.
    let presenter = Arc::new(
        cascade_presenter_projfs::ProjFsPresenter::new(mount_path).with_content_provider(provider),
    );

    let sync_runner = engine.create_sync_runner(presenter.clone());
    let cancel = engine.cancel_flag();
    let sync_handle = tokio::spawn(async move {
        if let Err(e) = sync_runner.run(cancel).await {
            tracing::error!(error = %e, "sync runner exited with error");
        }
    });

    let engine_handle = engine.start()?;

    let resources = PresenterResources {
        engine,
        engine_handle,
        sync_handle,
    };

    if let Err(e) = presenter.start(mount_path).await {
        resources.shutdown();
        return Err(e).with_context(|| {
            format!(
                "failed to start ProjFS presenter at {}",
                mount_path.display()
            )
        });
    }

    // ProjFS self-mounts via PrjStartVirtualizing; no separate OS mount
    // command is needed.

    if let Err(e) = write_pid_file(&ctx.pid_path) {
        resources.shutdown();
        return Err(e);
    }

    println!("Cascade started (ProjFS).");
    println!("  Mount point: {}", mount_path.display());
    println!("  PID: {}", std::process::id());
    println!();
    println!("Press Ctrl+C to stop.");

    // The presenter is up: spawn the HTTP API (feature-gated, when enabled) and
    // flip the F3 data-plane readiness bit so its data routes begin serving.
    let web_runtime = start_web(&engine_for_web, web).await?;
    mark_web_ready(&web_runtime);

    wait_for_shutdown_signal().await?;

    // Stop the HTTP API before tearing the rest down.
    stop_web(web_runtime).await;

    tracing::info!("Shutting down...");

    finish_shutdown(&ctx.pid_path, None, async move {
        if let Err(e) = presenter.stop().await {
            tracing::warn!(error = %e, "ProjFS presenter stop returned an error");
        }
        resources.shutdown();
    })
    .await
}

/// Try to mount via the `WebDAV` presenter.
///
/// Same shutdown guarantees as the FSKit/FUSE presenter functions: on
/// any failure during engine, presenter, or mount setup, every started
/// resource is stopped before the error is returned. The `WebDAV`
/// server runs on every platform; only the OS-level mount command
/// varies. macOS uses `mount_webdav`, Windows uses `net use` against
/// the built-in `WebClient` service, and Linux falls through to
/// FUSE/NFS rather than using `mount.davfs` (which requires root).
async fn try_webdav(
    ctx: &CliContext,
    mount_path: &Path,
    backends: Vec<cascade_engine::backend::MountedBackend>,
    no_mount: bool,
    p2p: &ResolvedP2pConfig,
    web: &WebOptions,
) -> Result<()> {
    let engine_config = EngineConfig {
        db_path: ctx.db_path.clone(),
        mount_point: mount_path.to_path_buf(),
        backends,
        cache_dir: None,
        enable_p2p: p2p.enable_p2p,
        p2p_data_dir: None,
        p2p_posture: p2p.posture,
        p2p_relay_endpoints: p2p.relay_endpoints.clone(),
        p2p_relay_shared_secret: p2p.relay_shared_secret,
        backend_factory: Some(cli_backend_factory()),
    };
    let engine = Arc::new(Engine::new(engine_config)?);
    // Wire the management-plane dispatcher into every backend that serves
    // remote management (the P2P backend). Done before the presenter starts so
    // an inbound ManageRequest is authorised and executed rather than refused.
    engine.wire_manage_dispatch().await;

    // Clone the engine handle for the HTTP API before the engine is moved into
    // the presenter resources; the API server is started only once the winning
    // presenter is confirmed up, so a failed attempt never binds the port.
    let engine_for_web = engine.clone();

    let mut presenter = WebDavPresenter::new(mount_path);
    if let Ok(bind) = std::env::var("CASCADE_WEBDAV_BIND") {
        presenter = presenter.with_bind_addr(bind);
    }

    // Pass the mounted child backends and DB for on-demand directory expansion;
    // the neutral root is synthetic and owns no content.
    let all_backends: Vec<Arc<dyn cascade_engine::backend::Backend>> = {
        let vfs = engine
            .vfs()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        vfs.children()
            .iter()
            .map(|(_, backend)| backend.clone())
            .collect()
    };
    presenter.with_backends(all_backends).await;
    presenter.with_db(engine.db().clone());

    let presenter = Arc::new(presenter);
    let items = presenter.items().clone();

    let sync_runner = engine.create_sync_runner(presenter.clone());
    let cancel = engine.cancel_flag();
    let sync_handle = tokio::spawn(async move {
        if let Err(e) = sync_runner.run(cancel).await {
            tracing::error!(error = %e, "sync runner exited with error");
        }
    });

    let engine_handle = engine.start()?;

    let resources = PresenterResources {
        engine,
        engine_handle,
        sync_handle,
    };

    if let Err(e) = presenter.start(mount_path).await {
        resources.shutdown();
        return Err(e).with_context(|| {
            format!(
                "failed to start WebDAV presenter at {}",
                mount_path.display()
            )
        });
    }

    let webdav_port = {
        let guard = presenter.server.lock().await;
        guard
            .as_ref()
            .map(cascade_presenter_webdav::server::WebDavServer::port)
            .context("WebDAV server not started")?
    };

    if no_mount {
        println!("Cascade started (WebDAV, no-mount).");
        println!("  WebDAV URL: http://localhost:{webdav_port}");
    } else {
        if let Err(e) = mount_webdav(mount_path, webdav_port) {
            resources.shutdown();
            return Err(e);
        }
        println!("Cascade started (WebDAV).");
        println!("  Mount point: {}", mount_path.display());
        println!("  WebDAV port: {webdav_port}");
    }
    println!("  PID: {}", std::process::id());

    if let Err(e) = write_pid_file(&ctx.pid_path) {
        resources.shutdown();
        return Err(e);
    }

    println!();
    println!("Press Ctrl+C to stop.");

    // The presenter is up: spawn the HTTP API (feature-gated, when enabled) and
    // flip the F3 data-plane readiness bit so its data routes begin serving.
    let web_runtime = start_web(&engine_for_web, web).await?;
    mark_web_ready(&web_runtime);

    wait_for_shutdown_signal().await?;

    // Stop the HTTP API before tearing the rest down.
    stop_web(web_runtime).await;

    tracing::info!("Shutting down...");

    let mount = (!no_mount).then(|| mount_path.to_path_buf());
    finish_shutdown(&ctx.pid_path, mount, async move {
        if let Err(e) = presenter.stop().await {
            tracing::warn!(error = %e, "WebDAV presenter stop returned an error");
        }
        drop(items);
        resources.shutdown();
    })
    .await
}

/// Try to mount via the FUSE presenter (Linux only).
///
/// Same shutdown guarantees as the macOS presenter functions: on any
/// failure during engine, presenter, or mount setup, every started
/// resource is stopped before the error is returned.
///
/// This function is only called when `--no-mount` is not set. The `start()`
/// caller gates the FUSE attempt behind `!no_mount` so that seed mode (the
/// Docker container default) is a genuine unprivileged path: FUSE always mounts
/// and cannot honour `--no-mount` internally, so the gate lives at the call
/// site rather than inside this function.
#[cfg(target_os = "linux")]
async fn try_fuse(
    ctx: &CliContext,
    mount_path: &Path,
    backends: Vec<cascade_engine::backend::MountedBackend>,
    p2p: &ResolvedP2pConfig,
    web: &WebOptions,
) -> Result<()> {
    let engine_config = EngineConfig {
        db_path: ctx.db_path.clone(),
        mount_point: mount_path.to_path_buf(),
        backends,
        cache_dir: None,
        enable_p2p: p2p.enable_p2p,
        p2p_data_dir: None,
        p2p_posture: p2p.posture,
        p2p_relay_endpoints: p2p.relay_endpoints.clone(),
        p2p_relay_shared_secret: p2p.relay_shared_secret,
        backend_factory: Some(cli_backend_factory()),
    };
    let engine = Arc::new(Engine::new(engine_config)?);
    // Wire the management-plane dispatcher into every backend that serves
    // remote management (the P2P backend). Done before the presenter starts so
    // an inbound ManageRequest is authorised and executed rather than refused.
    engine.wire_manage_dispatch().await;

    // Clone the engine handle for the HTTP API before the engine is moved into
    // the presenter resources; the API server is started only once the winning
    // presenter is confirmed up, so a failed attempt never binds the port.
    let engine_for_web = engine.clone();

    let root_id = cascade_engine::types::ItemId::new("vfs", "root");
    let presenter = Arc::new(
        cascade_presenter_fuse::FusePresenter::with_vfs(root_id, engine.vfs().clone())
            .with_mount_point(mount_path),
    );

    let sync_runner = engine.create_sync_runner(presenter.clone());
    let cancel = engine.cancel_flag();
    let sync_handle = tokio::spawn(async move {
        if let Err(e) = sync_runner.run(cancel).await {
            tracing::error!(error = %e, "sync runner exited with error");
        }
    });

    let engine_handle = engine.start()?;
    let resources = PresenterResources {
        engine,
        engine_handle,
        sync_handle,
    };

    if let Err(e) = presenter.start(mount_path).await {
        resources.shutdown();
        return Err(e).with_context(|| {
            format!("failed to start FUSE presenter at {}", mount_path.display())
        });
    }

    if let Err(e) = write_pid_file(&ctx.pid_path) {
        resources.shutdown();
        return Err(e);
    }

    println!("Cascade started (FUSE).");
    println!("  Mount point: {}", mount_path.display());
    println!("  PID: {}", std::process::id());
    println!();
    println!("Press Ctrl+C to stop.");

    // The presenter is up: spawn the HTTP API (feature-gated, when enabled) and
    // flip the F3 data-plane readiness bit so its data routes begin serving.
    let web_runtime = start_web(&engine_for_web, web).await?;
    mark_web_ready(&web_runtime);

    wait_for_shutdown_signal().await?;

    // Stop the HTTP API before tearing the rest down.
    stop_web(web_runtime).await;

    tracing::info!("Shutting down...");

    finish_shutdown(&ctx.pid_path, None, async move {
        if let Err(e) = presenter.stop().await {
            tracing::warn!(error = %e, "FUSE presenter stop returned an error");
        }
        resources.shutdown();
    })
    .await
}

/// Try to mount via the NFS presenter.
///
/// Works on all platforms (Linux/macOS).  Same shutdown guarantees as
/// the macOS presenter functions. On Windows the chain doesn't fall
/// back to NFS (no built-in NFS client we can talk to from cascade),
/// so this function would be unused there — silence the lint.
#[cfg_attr(target_os = "windows", allow(dead_code))]
async fn try_nfs(
    ctx: &CliContext,
    mount_path: &Path,
    backends: Vec<cascade_engine::backend::MountedBackend>,
    no_mount: bool,
    p2p: &ResolvedP2pConfig,
    web: &WebOptions,
) -> Result<()> {
    let engine_config = EngineConfig {
        db_path: ctx.db_path.clone(),
        mount_point: mount_path.to_path_buf(),
        backends,
        cache_dir: None,
        enable_p2p: p2p.enable_p2p,
        p2p_data_dir: None,
        p2p_posture: p2p.posture,
        p2p_relay_endpoints: p2p.relay_endpoints.clone(),
        p2p_relay_shared_secret: p2p.relay_shared_secret,
        backend_factory: Some(cli_backend_factory()),
    };
    let engine = Arc::new(Engine::new(engine_config)?);
    // Wire the management-plane dispatcher into every backend that serves
    // remote management (the P2P backend). Done before the presenter starts so
    // an inbound ManageRequest is authorised and executed rather than refused.
    engine.wire_manage_dispatch().await;

    // Clone the engine handle for the HTTP API before the engine is moved into
    // the presenter resources; the API server is started only once the winning
    // presenter is confirmed up, so a failed attempt never binds the port.
    let engine_for_web = engine.clone();

    let presenter = Arc::new(cascade_presenter_nfs::NfsPresenter::with_vfs(
        mount_path,
        engine.vfs().clone(),
    ));
    let nfs_ctx = presenter.context().clone();

    let sync_runner = engine.create_sync_runner(presenter);
    let cancel = engine.cancel_flag();
    let sync_handle = tokio::spawn(async move {
        if let Err(e) = sync_runner.run(cancel).await {
            tracing::error!(error = %e, "sync runner exited with error");
        }
    });

    let engine_handle = engine.start()?;

    let resources = PresenterResources {
        engine,
        engine_handle,
        sync_handle,
    };

    let server_config = NfsServerConfig {
        bind_addr: "127.0.0.1:0".parse()?,
        export_path: "/".to_string(),
        cache_mode: NfsCacheMode::default(),
    };
    let server = match NfsServer::start(server_config, nfs_ctx).await {
        Ok(s) => s,
        Err(e) => {
            resources.shutdown();
            return Err(e).context("failed to start NFS server");
        }
    };

    let nfs_port = server.local_addr.port();
    tracing::info!(port = nfs_port, "NFS server started");

    if !no_mount && let Err(e) = mount_nfs(mount_path, nfs_port) {
        resources.shutdown();
        return Err(e);
    }

    if let Err(e) = write_pid_file(&ctx.pid_path) {
        resources.shutdown();
        return Err(e);
    }

    if no_mount {
        println!("Cascade started (NFS, no-mount).");
    } else {
        println!("Cascade started (NFS).");
        println!("  Mount point: {}", mount_path.display());
    }
    println!("  NFS port: {nfs_port}");
    println!("  PID: {}", std::process::id());

    println!();
    println!("Press Ctrl+C to stop.");

    // The presenter is up: spawn the HTTP API (feature-gated, when enabled) and
    // flip the F3 data-plane readiness bit so its data routes begin serving.
    let web_runtime = start_web(&engine_for_web, web).await?;
    mark_web_ready(&web_runtime);

    wait_for_shutdown_signal().await?;

    // Stop the HTTP API before tearing the rest down.
    stop_web(web_runtime).await;

    tracing::info!("Shutting down...");

    let mount = (!no_mount).then(|| mount_path.to_path_buf());
    finish_shutdown(&ctx.pid_path, mount, async move {
        if let Err(e) = server.stop() {
            tracing::warn!(error = %e, "NFS server stop returned an error");
        }
        resources.shutdown();
    })
    .await
}

// ---------------------------------------------------------------------------
// Stop
// ---------------------------------------------------------------------------

/// Poll until the process with `pid` is gone or `grace` elapses.
///
/// Returns `true` if the process exited within the grace period. The number of
/// polls is derived from `grace / poll` so the timing is expressed once, in the
/// caller's units, rather than as a magic iteration count.
#[cfg(unix)]
fn wait_for_pid_exit(pid: u32, grace: std::time::Duration, poll: std::time::Duration) -> bool {
    let polls = grace.as_millis().div_ceil(poll.as_millis().max(1));
    for _ in 0..polls {
        if !is_process_alive(pid) {
            return true;
        }
        std::thread::sleep(poll);
    }
    !is_process_alive(pid)
}

/// Stop the Cascade daemon.
#[cfg(unix)]
pub fn stop(ctx: &CliContext) -> anyhow::Result<()> {
    if !ctx.pid_path.exists() {
        println!("Cascade is not running.");
        return Ok(());
    }

    let raw = read_text_file(&ctx.pid_path, "PID file")?;
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

    // Wait for the daemon to exit after SIGTERM. It runs a bounded graceful
    // shutdown of its own (see `SHUTDOWN_GRACE`), so it normally exits well
    // inside this window.
    let poll = std::time::Duration::from_millis(500);
    let term_grace = std::time::Duration::from_secs(5);
    let mut exited = wait_for_pid_exit(pid_u32, term_grace, poll);

    if !exited {
        // The SIGTERM grace elapsed — escalate to SIGKILL so `stop` actually
        // stops the daemon rather than reporting success and leaving an orphan
        // (the Windows path force-kills here too). SIGKILL cannot be caught, so
        // all that remains is to wait for the kernel to reap the process.
        eprintln!(
            "Warning: PID {pid_u32} did not exit within {}s of SIGTERM; sending SIGKILL.",
            term_grace.as_secs()
        );
        match nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
            Err(e) => {
                return Err(anyhow::anyhow!("failed to SIGKILL PID {pid_u32}: {e}"));
            }
        }
        let kill_grace = std::time::Duration::from_secs(2);
        exited = wait_for_pid_exit(pid_u32, kill_grace, poll);
        if !exited {
            eprintln!("Warning: PID {pid_u32} still present after SIGKILL.");
        }
    }

    let _ = std::fs::remove_file(&ctx.pid_path);

    // Clean up a stale mount only now the daemon is gone, so this never races
    // the daemon's own unmount. A graceful exit has already unmounted; this
    // covers the SIGKILL case, where it could not.
    let mount_path = resolve_mount_path_from_config(&ctx.config_dir);
    if mount_path.is_dir()
        && let Err(e) = unmount_path(&mount_path)
    {
        tracing::debug!(error = %e, "unmount after stop failed (may already be unmounted)");
    }

    println!("Cascade stopped.");
    Ok(())
}

/// Stop the Cascade daemon (Windows).
///
/// Reads the PID file, sends `taskkill /PID`, polls up to 5 s, then
/// force-kills with `taskkill /F`. Removes the PID file and any Cascade
/// `WebDAV` mounts. A stale PID file (process already gone) is success.
#[cfg(windows)]
pub fn stop(ctx: &CliContext) -> anyhow::Result<()> {
    if !ctx.pid_path.exists() {
        println!("Cascade is not running.");
        return Ok(());
    }

    let raw = read_text_file(&ctx.pid_path, "PID file")?;
    let pid: u32 = raw.trim().parse().with_context(|| {
        format!(
            "invalid PID in {}: {:?}",
            ctx.pid_path.display(),
            raw.trim()
        )
    })?;

    if !is_process_alive(pid) {
        // Process already gone — clean up the stale PID file and exit.
        let _ = std::fs::remove_file(&ctx.pid_path);
        println!("Cascade is not running.");
        return Ok(());
    }

    // Graceful first: `taskkill` without `/F` posts WM_CLOSE which Cascade
    // can react to like a Ctrl+C. Ignore non-zero exit — the process may
    // have died between our liveness check and this call, or the OS may
    // report "not found".
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string()])
        .output();

    // Poll for up to 5 seconds for the process to exit (10 × 500 ms).
    let mut exited = false;
    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if !is_process_alive(pid) {
            exited = true;
            break;
        }
    }

    if !exited {
        // Force-kill: `taskkill /F` issues `TerminateProcess`. This wins
        // unless the PID is somehow unkillable. "Process not found"
        // stderr is treated as success because the goal — process gone —
        // is already met.
        let output = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .output()
            .with_context(|| format!("failed to force-kill PID {pid} via taskkill"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let combined = format!("{stdout}{stderr}");
            if !is_windows_process_not_found(&combined) {
                return Err(anyhow::anyhow!(
                    "taskkill /F /PID {pid} failed: {}",
                    combined.trim()
                ));
            }
            tracing::debug!(
                pid,
                "taskkill reported process not found; treating as already exited"
            );
        }
    }

    let _ = std::fs::remove_file(&ctx.pid_path);

    // Detach any localhost WebDAV mounts that look like ours.
    let mount_path = resolve_mount_path_from_config(&ctx.config_dir);
    if let Err(e) = unmount_path(&mount_path) {
        tracing::debug!(error = %e, "unmount after stop failed (may already be unmounted)");
    }

    println!("Cascade stopped.");
    Ok(())
}

/// Detect whether `taskkill` output indicates the target PID no longer
/// exists. `taskkill` writes localised messages but the English token
/// "not found" (with various surrounding text) is consistent enough to
/// match against, and matches the format documented by Microsoft.
#[cfg(windows)]
fn is_windows_process_not_found(output: &str) -> bool {
    let lower = output.to_lowercase();
    lower.contains("not found")
        || lower.contains("no running instance")
        || lower.contains("no tasks running")
}

/// Stop the Cascade daemon on platforms that genuinely lack an implementation.
#[cfg(not(any(unix, windows)))]
pub fn stop(_ctx: &CliContext) -> anyhow::Result<()> {
    anyhow::bail!("cascade stop is not supported on this platform yet");
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The daemon's [`BackendFactory`](cascade_engine::backend::BackendFactory)
/// implementation — the composition-edge adapter that lets the engine build a
/// backend at runtime (the management-plane `BackendAdd` path) without depending
/// on any concrete backend crate. It parses the supplied TOML fragment and
/// routes to the same per-type [`create_backend`] factories the daemon uses at
/// startup.
struct CliBackendFactory;

impl cascade_engine::backend::BackendFactory for CliBackendFactory {
    fn create(
        &self,
        name: &str,
        backend_type: &str,
        config_toml: &str,
    ) -> Result<Arc<dyn cascade_engine::backend::Backend>> {
        let config: toml::Value = toml::from_str(config_toml)
            .with_context(|| format!("parsing config for backend `{name}`"))?;
        // The management-plane hot-reload path does not carry the daemon's
        // shared HTTP client; pass None so the gdrive backend builds its own
        // default pooled client (still on the daemon's runtime, so no stranding).
        let backend = create_backend(name, backend_type, &config, None)?;
        Ok(Arc::from(backend))
    }
}

/// A shared [`CliBackendFactory`] for injection into an [`EngineConfig`].
fn cli_backend_factory() -> Arc<dyn cascade_engine::backend::BackendFactory> {
    Arc::new(CliBackendFactory)
}

/// Instantiate a backend by type, using its per-backend TOML config.
///
/// When `shared_http` is `Some`, the gdrive backend uses the daemon's shared
/// pooled client; when `None` (the management-plane hot-reload path), it builds
/// its own default pooled client. All other backend types ignore it.
fn create_backend(
    name: &str,
    backend_type: &str,
    config: &toml::Value,
    shared_http: Option<Arc<dyn cascade_engine::portable::HttpClient>>,
) -> Result<Box<dyn cascade_engine::backend::Backend>> {
    match backend_type {
        "gdrive" => {
            let store = Arc::new(cascade_backend_gdrive::token_store::PlatformTokenStore);
            match shared_http {
                Some(http) => {
                    cascade_backend_gdrive::create_backend_with_store_and_http(config, store, http)
                }
                None => cascade_backend_gdrive::create_backend_with_store(config, store),
            }
            .with_context(|| format!("failed to create gdrive backend `{name}`"))
        }
        "s3" => cascade_backend_s3::create_backend(config)
            .with_context(|| format!("failed to create s3 backend `{name}`")),
        "local" => cascade_backend_local::create_backend(config)
            .with_context(|| format!("failed to create local backend `{name}`")),
        "p2p" => cascade_backend_p2p::create_backend(config)
            .with_context(|| format!("failed to create p2p backend `{name}`")),
        other => anyhow::bail!("unsupported backend type: {other}"),
    }
}

/// Rebuild backends from the main config in deterministic alphabetical order.
/// Fails loudly on an empty or duplicate mount path.
fn rebuild_backends(
    main_config: &CascadeConfig,
    config_dir: &Path,
    shared_http: Arc<dyn cascade_engine::portable::HttpClient>,
) -> Result<Vec<cascade_engine::backend::MountedBackend>> {
    let mut names: Vec<&String> = main_config.backends.keys().collect();
    names.sort_unstable();

    // Validate all mounts before creating any backends so config errors surface immediately.
    let mut resolved: Vec<(&String, BackendConfig, String)> = Vec::with_capacity(names.len());
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for name in &names {
        let cfg: BackendConfig = main_config
            .backends
            .get(name.as_str())
            .ok_or_else(|| anyhow::anyhow!("backend `{name}` disappeared during iteration"))?
            .clone()
            .try_into()
            .with_context(|| format!("invalid config for backend `{name}`"))?;
        let mount_str = cfg
            .mount
            .as_deref()
            .unwrap_or(name.as_str())
            .trim()
            .to_string();
        if mount_str.is_empty() {
            anyhow::bail!(
                "backend `{name}` has an empty mount path; remove the `mount` key or set a non-empty value"
            );
        }
        let dedup = if mount_str == "/" {
            String::new()
        } else {
            mount_str.clone()
        };
        if !seen.insert(dedup) {
            anyhow::bail!(
                "duplicate mount path for backend `{name}`: \"{mount_str}\" is already used"
            );
        }
        resolved.push((name, cfg, mount_str));
    }

    let mut mounted: Vec<cascade_engine::backend::MountedBackend> =
        Vec::with_capacity(resolved.len());
    for (name, cfg, mount_str) in resolved {
        let per_backend_config = load_backend_config(config_dir, name)?;
        let backend = create_backend(
            name,
            &cfg.backend_type,
            &per_backend_config,
            Some(shared_http.clone()),
        )?;
        mounted.push(cascade_engine::backend::MountedBackend::new(
            Some(mount_str),
            Arc::from(backend),
        ));
    }
    Ok(mounted)
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

/// Resolve the mount path from config.toml, falling back to ~/Cloud.
#[cfg_attr(not(any(unix, windows)), allow(dead_code))]
fn resolve_mount_path_from_config(config_dir: &Path) -> PathBuf {
    let main_config = load_main_config(config_dir).ok();
    let configured = main_config.as_ref().and_then(|c| {
        let p = c.mount.point.trim().to_string();
        (!p.is_empty()).then_some(p)
    });
    configured.map_or_else(
        || {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join("Cloud")
        },
        |p| resolve_mount_path(&p),
    )
}

/// Ensure a path exists as a directory, with a message that names the path.
///
/// On macOS a stale `WebDAV` mount can leave the path in a state where
/// `stat` and other syscalls hang against the dead server. We attempt
/// `create_dir_all` first and only fall back to `stat`-style checks
/// after confirming the path is not a live mount entry — that way a
/// broken mount produces a clear actionable error instead of hanging.
fn ensure_directory(path: &Path, label: &str) -> Result<()> {
    match std::fs::create_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // The path exists. If it's still a live mount entry the
            // eviction earlier in startup didn't take, and any `stat`
            // call would hang against the dead server. Surface that
            // explicitly with recovery instructions.
            #[cfg(target_os = "macos")]
            if path_is_in_mount_table(path) {
                anyhow::bail!(
                    "{label} {} is still listed as an active mount — please run `sudo umount -f {}` and try again",
                    path.display(),
                    path.display()
                );
            }
            // Safe to `stat` now: not a mount, so the syscall returns
            // immediately. Reject the case where the path is a regular
            // file rather than a directory.
            if path.is_dir() {
                Ok(())
            } else {
                anyhow::bail!("{label} {} exists but is not a directory", path.display());
            }
        }
        Err(e) => Err(e).with_context(|| format!("failed to create {label} at {}", path.display())),
    }
}

/// Return true if `path` appears in the `mount` table output. Used to
/// distinguish "EEXIST because of a stale mount we couldn't evict" from
/// "EEXIST because the directory already exists as expected".
#[cfg(target_os = "macos")]
fn path_is_in_mount_table(path: &Path) -> bool {
    let path_str = path.to_string_lossy();
    let Ok(output) = std::process::Command::new("/sbin/mount").output() else {
        return false;
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().any(|line| line.contains(&*path_str))
}

/// Read a text file with the file path included in the error message.
fn read_text_file(path: &Path, label: &str) -> Result<String> {
    std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {label} at {}", path.display()))
}

/// Write the current process ID to the PID file with a path-aware error.
fn write_pid_file(path: &Path) -> Result<()> {
    std::fs::write(path, std::process::id().to_string())
        .with_context(|| format!("failed to write PID file at {}", path.display()))
}

// ---------------------------------------------------------------------------
// macOS mount helpers
// ---------------------------------------------------------------------------

/// Run a subprocess with a hard timeout. If the child does not exit
/// within `timeout`, it is killed and `None` is returned. Used to keep
/// daemon startup snappy even when unmount commands stall on a dead
/// `WebDAV` server (which can otherwise hang for minutes).
#[cfg(target_os = "macos")]
fn run_with_timeout(
    cmd: &mut std::process::Command,
    timeout: std::time::Duration,
) -> Option<std::process::Output> {
    use std::process::Stdio;

    let Ok(mut child) = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() else {
        return None;
    };

    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                return child.wait_with_output().ok();
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

/// Remove stale `WebDAV` or NFS mounts on the given path left behind by a
/// previous Cascade run that exited without clean shutdown.
///
/// Detects mounts whose remote endpoint is `http://127.0.0.1:` (`WebDAV`) or
/// `127.0.0.1:/` (NFS) — Cascade-only patterns — so it never touches mounts
/// belonging to other programs.
///
/// Attempts, in order: `umount -f`, then `diskutil unmount force`. Each
/// command runs with a hard timeout because dead `WebDAV` mounts can make
/// kernel syscalls (including the unmount path itself) hang indefinitely
/// while the VFS waits for a server that will never respond.
#[cfg(target_os = "macos")]
fn evict_stale_mount(mount_point: &Path) {
    let path_str = mount_point.to_string_lossy();
    let cmd_timeout = std::time::Duration::from_secs(5);

    let output = match std::process::Command::new("/sbin/mount").output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(_) => return,
    };

    // Collect lines that mention our mount point *and* look like a Cascade
    // WebDAV or NFS mount (localhost endpoint).
    let stale: Vec<&str> = output
        .lines()
        .filter(|line| line.contains(&*path_str))
        .filter(|line| {
            line.contains("http://127.0.0.1:")
                || line.contains("http://localhost:")
                || line.contains("127.0.0.1:/")
        })
        .collect();

    if stale.is_empty() {
        return;
    }

    for entry in &stale {
        tracing::warn!(path = %path_str, entry, "evicting stale mount");
    }

    // `umount -f` (force) skips the "flush dirty buffers" path, which is
    // what hangs on a dead WebDAV server. Loop because stacked mounts on
    // the same path need one call per layer.
    for _ in 0..=stale.len() {
        let mut cmd = std::process::Command::new("/sbin/umount");
        cmd.arg("-f").arg(mount_point);
        match run_with_timeout(&mut cmd, cmd_timeout) {
            Some(o) if o.status.success() => {
                tracing::info!(path = %path_str, "stale mount evicted via umount -f");
            }
            Some(_) => break, // umount returned non-zero: nothing more to remove
            None => {
                tracing::warn!(path = %path_str, "umount -f timed out; falling back to diskutil");
                break;
            }
        }
    }

    // Verify by re-reading the mount table — if anything matching is left
    // (stacked or stuck) try the heavier `diskutil unmount force`.
    if path_is_in_mount_table(mount_point) {
        let mut cmd = std::process::Command::new("/usr/sbin/diskutil");
        cmd.args(["unmount", "force"]).arg(mount_point);
        match run_with_timeout(&mut cmd, cmd_timeout) {
            Some(o) if o.status.success() => {
                tracing::info!(path = %path_str, "stale mount force-evicted via diskutil");
            }
            Some(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                tracing::warn!(path = %path_str, stderr = %stderr, "diskutil force unmount failed");
            }
            None => {
                tracing::warn!(path = %path_str, "diskutil unmount force timed out");
            }
        }
    }
}

/// Mount `WebDAV` filesystem using the OS mount command (macOS).
/// Uses `/sbin/mount_webdav` which does not require root.
#[cfg(target_os = "macos")]
fn mount_webdav(mount_point: &Path, port: u16) -> Result<()> {
    ensure_directory(mount_point, "WebDAV mount point")?;

    let url = format!("http://localhost:{port}/");
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
    let url = format!("http://localhost:{port}/");
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
    ensure_directory(mount_point, "NFS mount point")?;

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

// ---------------------------------------------------------------------------
// Non-macOS stubs
// ---------------------------------------------------------------------------

/// Unmount the NFS filesystem on Linux using `umount`.
///
/// Tries the plain `umount <path>` first; if the kernel reports the
/// device as busy we retry with `-l` (lazy detach) so the daemon can
/// always exit cleanly even when a shell still has the mount as `cwd`.
/// Permission and "not mounted" failures are logged at debug rather
/// than propagated — shutdown should not block on an already-detached
/// mount.
#[cfg(target_os = "linux")]
#[allow(clippy::unnecessary_wraps)]
// Returns `Result<()>` to share the signature with the macOS/Windows impls;
// shutdown is best-effort and never propagates errors on Linux.
fn unmount_path(mount_point: &Path) -> Result<()> {
    tracing::info!(path = %mount_point.display(), "unmounting NFS");

    let output = std::process::Command::new("umount")
        .arg(mount_point)
        .output();

    match output {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            if stderr.contains("not mounted") || stderr.contains("not found") {
                tracing::debug!(path = %mount_point.display(), "not mounted, skipping unmount");
                return Ok(());
            }
            // Try a lazy unmount on busy/permission failures so the
            // daemon doesn't get stuck waiting for shells to leave.
            let lazy = std::process::Command::new("umount")
                .arg("-l")
                .arg(mount_point)
                .output();
            match lazy {
                Ok(l) if l.status.success() => {
                    tracing::info!(path = %mount_point.display(), "NFS lazily unmounted");
                    Ok(())
                }
                Ok(l) => {
                    let lazy_err = String::from_utf8_lossy(&l.stderr);
                    tracing::warn!(
                        error = %stderr.trim(),
                        lazy_error = %lazy_err.trim(),
                        "umount failed (will be cleaned up on exit)"
                    );
                    Ok(())
                }
                Err(e) => {
                    tracing::warn!(error = %e, "lazy umount failed to spawn");
                    Ok(())
                }
            }
        }
        Err(e) => {
            tracing::debug!(error = %e, "umount failed to spawn (may already be unmounted)");
            Ok(())
        }
    }
}

/// Unmount stub for platforms with neither macOS, Linux, nor Windows
/// implementations.
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
#[allow(clippy::unnecessary_wraps, clippy::missing_const_for_fn)]
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

/// Mount the cascade NFS server on Linux using `mount -t nfs`.
///
/// The cascade NFS server speaks `NFSv3`, so we always request v3 and
/// connect over TCP. `mount(8)` on Linux requires root for NFS mounts;
/// when we hit `EACCES` or a "must be root" / "permission denied" stderr
/// we surface a clear hint to retry with `sudo` rather than letting the
/// raw `mount` error reach the user.
#[cfg(target_os = "linux")]
fn mount_nfs(mount_point: &Path, port: u16) -> Result<()> {
    ensure_directory(mount_point, "NFS mount point")?;

    let mut cmd = linux_nfs_mount_command(mount_point, port);
    tracing::info!(
        mount = %mount_point.display(),
        port,
        "mounting NFS (Linux, v3 over TCP)"
    );

    let output = cmd
        .output()
        .context("failed to invoke /bin/mount (is it installed?)")?;

    if output.status.success() {
        tracing::info!(path = %mount_point.display(), "NFS mounted");
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if is_linux_permission_error(&stderr, output.status.code()) {
        anyhow::bail!(
            "mount: NFS mounts on Linux require root. Re-run with sudo (e.g. `sudo cascade start`) or grant CAP_SYS_ADMIN. Original error: {}",
            stderr.trim()
        );
    }
    anyhow::bail!("mount -t nfs failed: {}", stderr.trim());
}

/// Build the Linux `mount -t nfs` command for the cascade server.
///
/// Extracted from `mount_nfs` so tests can assert argument construction
/// without invoking the kernel (which would need root and a live server).
#[cfg(target_os = "linux")]
fn linux_nfs_mount_command(mount_point: &Path, port: u16) -> std::process::Command {
    let options = format!("port={port},mountport={port},proto=tcp,vers=3");
    let mut cmd = std::process::Command::new("mount");
    cmd.arg("-t")
        .arg("nfs")
        .arg("-o")
        .arg(&options)
        .arg("127.0.0.1:/")
        .arg(mount_point);
    cmd
}

/// Detect whether a Linux `mount` invocation failed because we lacked
/// root. Linux `mount` writes "must be root", "permission denied", or
/// "only root" depending on distro and version; we check all three plus
/// the EACCES exit code (32 on util-linux ≥ 2.34).
#[cfg(target_os = "linux")]
fn is_linux_permission_error(stderr: &str, exit_code: Option<i32>) -> bool {
    let lower = stderr.to_lowercase();
    if lower.contains("must be root")
        || lower.contains("permission denied")
        || lower.contains("only root")
        || lower.contains("operation not permitted")
    {
        return true;
    }
    // util-linux convention: exit code 32 means "mount failure" with
    // permission as the most common cause when running unprivileged.
    matches!(exit_code, Some(32))
}

/// NFS mount stub for platforms with neither macOS nor Linux support.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
#[allow(clippy::missing_const_for_fn, clippy::unused_self, dead_code)]
fn mount_nfs(_mount_point: &Path, _port: u16) -> Result<()> {
    anyhow::bail!("NFS mounting is not supported on this platform yet");
}

/// Mount the `WebDAV` server as a network drive on Windows via the
/// built-in `WebClient` service (`net use`).
///
/// `mount_point` is used as a display string; the actual mount target
/// is whichever drive letter Windows assigns when `*` is passed. The
/// drive letter is reported in the log.
#[cfg(target_os = "windows")]
fn mount_webdav(mount_point: &Path, port: u16) -> Result<()> {
    let url = format!("http://localhost:{port}/");
    tracing::info!(url = %url, "mounting WebDAV via `net use`");

    let output = std::process::Command::new("net")
        .args(["use", "*", &url])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::bail!("net use failed (is the WebClient service running?): {stdout}{stderr}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    tracing::info!(
        path = %mount_point.display(),
        output = %stdout.trim(),
        "WebDAV mounted on Windows"
    );
    Ok(())
}

/// Unmount the most recently assigned `WebDAV` drive on Windows.
///
/// `net use <url> /delete` removes by URL rather than by drive letter,
/// which matches how we created the mount with `*`.
#[cfg(target_os = "windows")]
fn unmount_path(_mount_point: &Path) -> Result<()> {
    // Best-effort: enumerate all drives mapped to a localhost WebDAV URL
    // and detach each. The mount point passed here is the cascade-logical
    // path, not the assigned drive letter, so we can't address it directly.
    let output = std::process::Command::new("net").arg("use").output()?;
    if !output.status.success() {
        return Ok(());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if line.contains("http://localhost:") || line.contains("http://127.0.0.1:") {
            // Extract the URL (last whitespace-separated token).
            if let Some(url) = line.split_whitespace().last() {
                let _ = std::process::Command::new("net")
                    .args(["use", url, "/delete", "/y"])
                    .output();
            }
        }
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
#[allow(clippy::missing_const_for_fn, clippy::unnecessary_wraps, dead_code)]
fn mount_webdav(_mount_point: &Path, _port: u16) -> Result<()> {
    anyhow::bail!("WebDAV mounting is not supported on this platform yet");
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::missing_const_for_fn, dead_code)]
fn is_mounted(_path: &Path) -> bool {
    false
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
#[path = "mount_tests.rs"]
mod tests;
