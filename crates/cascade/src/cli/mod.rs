pub mod auth;
pub mod cache;
pub mod config;
pub mod init;
pub mod mount;
pub mod status;

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};

// ---------------------------------------------------------------------------
// CliContext — shared context for all CLI commands
// ---------------------------------------------------------------------------

/// Shared context derived once from the parsed CLI arguments.
///
/// Every command receives `&CliContext` instead of calling `dirs::config_dir()`
/// independently. This makes `--config` functional and makes commands testable
/// with a temporary directory.
#[derive(Debug)]
pub struct CliContext {
    /// Root config directory (e.g. `~/.config/cascade/`).
    pub config_dir: PathBuf,
    /// Path to the `SQLite` state database.
    pub db_path: PathBuf,
    /// Path to the PID file.
    pub pid_path: PathBuf,
}

impl CliContext {
    /// Resolve paths from the `--config` flag value.
    ///
    /// The `config_flag` is the raw string from `--config` (may contain `~`).
    /// If it points to a file, its parent directory is used as the config dir.
    /// If it points to a directory, that directory is used directly.
    pub fn resolve(config_flag: &str) -> Result<Self> {
        let expanded = shellexpand::tilde(config_flag).to_string();
        let path = PathBuf::from(expanded);

        let config_dir = if path.is_file() {
            path.parent()
                .context("--config path has no parent directory")?
                .to_path_buf()
        } else {
            path
        };

        Ok(Self {
            db_path: config_dir.join("state.db"),
            pid_path: config_dir.join("cascade.pid"),
            config_dir,
        })
    }
}

// ---------------------------------------------------------------------------
// is_process_alive — shared liveness check
// ---------------------------------------------------------------------------

/// Check whether the process with the given PID is alive.
///
/// On Unix, sends signal 0 (no-op) via `kill(2)` — returns `true` if the call
/// succeeds (process exists and we have permission to signal it), `false` if
/// `ESRCH` is returned (no such process), and `false` for any other error.
#[cfg(unix)]
pub fn is_process_alive(pid: u32) -> bool {
    let Ok(pid_signed) = i32::try_from(pid) else {
        return false;
    };
    let nix_pid = nix::unistd::Pid::from_raw(pid_signed);
    match nix::sys::signal::kill(nix_pid, None) {
        Ok(()) => true,
        Err(_) => false,
    }
}

/// Check whether the process with the given PID is alive.
///
/// On non-Unix platforms, a reliable cross-process liveness check is not
/// available without an OS-specific crate, so the presence of the PID file is
/// treated as sufficient and this function always returns `true`.
#[cfg(not(unix))]
pub const fn is_process_alive(_pid: u32) -> bool {
    true
}

// ---------------------------------------------------------------------------
// Clap definitions
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "cascade")]
#[command(about = "Cross-platform cloud storage filesystem client")]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Config directory path (or path to config.toml)
    #[arg(long, global = true, default_value = "~/.config/cascade")]
    pub config: String,

    /// Increase verbosity (-v = debug, -vv = trace)
    #[arg(long, short, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Suppress non-error output
    #[arg(long, short, global = true)]
    pub quiet: bool,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Guided initial setup
    Init {
        /// Backend type (gdrive, s3)
        #[arg(long)]
        backend_type: Option<String>,

        /// Name for this backend instance
        #[arg(long)]
        name: Option<String>,

        /// Mount point path
        #[arg(long)]
        mount_point: Option<String>,

        /// S3 endpoint URL (for --backend-type s3)
        #[arg(long)]
        endpoint: Option<String>,

        /// S3 bucket name (for --backend-type s3)
        #[arg(long)]
        bucket: Option<String>,

        /// S3 region (for --backend-type s3)
        #[arg(long)]
        region: Option<String>,

        /// S3 access key ID (for --backend-type s3)
        #[arg(long)]
        access_key_id: Option<String>,

        /// S3 secret access key (for --backend-type s3)
        #[arg(long)]
        secret_access_key: Option<String>,

        /// Google Drive `OAuth2` client ID (for --backend-type gdrive)
        #[arg(long)]
        client_id: Option<String>,

        /// Google Drive `OAuth2` client secret (for --backend-type gdrive)
        #[arg(long)]
        client_secret: Option<String>,
    },

    /// Start the daemon and mount all configured backends
    Start {
        /// Mount point override
        #[arg(long)]
        mount_point: Option<String>,

        /// Start the server without mounting the filesystem — useful for e2e testing against localhost
        #[arg(long)]
        no_mount: bool,

        /// Enable the P2P optimisation layer (overrides config.toml `[p2p].enabled`).
        /// When set, the daemon checks LAN peers for file blocks before falling
        /// back to the cloud backend.
        #[arg(long)]
        p2p: bool,

        /// Disable the P2P optimisation layer even if config.toml enables it.
        #[arg(long, conflicts_with = "p2p")]
        no_p2p: bool,
    },

    /// Stop the daemon and unmount
    Stop,

    /// Restart the daemon
    Restart,

    /// Show mount status, cache usage, backend health
    Status,

    /// Pin a file or directory (always available offline)
    Pin {
        /// Path to pin
        path: String,
    },

    /// Unpin a file or directory
    Unpin {
        /// Path to unpin
        path: String,
    },

    /// List all pinned paths
    #[command(name = "pin-list")]
    PinList,

    /// Cache management commands
    Cache {
        #[command(subcommand)]
        command: CacheCommands,
    },

    /// Show the resolved .cascade config for a directory
    #[command(name = "config-show")]
    ConfigShow {
        /// Directory path
        path: String,
    },

    /// Validate all .cascade files in the tree
    #[command(name = "config-validate")]
    ConfigValidate,

    /// List configured backends
    #[command(name = "backend-list")]
    BackendList,

    /// Add a backend
    #[command(name = "backend-add")]
    BackendAdd {
        /// Backend type (gdrive, s3, p2p)
        backend_type: String,

        /// Name for this backend instance
        #[arg(long)]
        name: Option<String>,

        /// Mount path (relative path in VFS, e.g. Work/Projects)
        #[arg(long)]
        mount_path: Option<String>,

        /// OAuth client ID (gdrive only)
        #[arg(long)]
        client_id: Option<String>,

        /// OAuth client secret (gdrive only)
        #[arg(long)]
        client_secret: Option<String>,
    },

    /// Remove a backend
    #[command(name = "backend-remove")]
    BackendRemove {
        /// Backend name
        name: String,
    },

    /// Print (or generate) the P2P backend's device identity for a data dir.
    ///
    /// Used to bootstrap multi-node P2P clusters: read each node's device
    /// ID once, then write peer configs that reference them.
    #[command(name = "p2p-identity")]
    P2pIdentity {
        /// Directory holding the P2P identity files. Created if missing.
        #[arg(long)]
        data_dir: PathBuf,
    },

    /// Authenticate a backend (runs `OAuth2` flow)
    #[command(name = "backend-auth")]
    BackendAuth {
        /// Backend name
        name: String,

        /// Override the OAuth client ID (takes priority over config and built-in)
        #[arg(long)]
        client_id: Option<String>,

        /// Override the OAuth client secret (takes priority over config and built-in)
        #[arg(long)]
        client_secret: Option<String>,

        /// Use the device code flow instead of the localhost redirect.
        /// Device code grants per-file access (drive.file scope) only.
        #[arg(long)]
        device_code: bool,
    },
}

#[derive(Subcommand)]
pub enum CacheCommands {
    /// Show cache usage: pinned vs cached vs online
    Status,

    /// Manually run lifecycle eviction
    Evict {
        /// Evict all non-pinned files
        #[arg(long)]
        all: bool,
    },

    /// Pre-download a directory tree
    Warm {
        /// Path to warm
        path: String,
    },

    /// Evict specific files from cache
    Clear {
        /// Path to clear
        path: String,
    },
}

impl Cli {
    pub async fn run(self, ctx: &CliContext) -> Result<()> {
        match self.command {
            Commands::Init {
                backend_type,
                name,
                mount_point,
                endpoint,
                bucket,
                region,
                access_key_id,
                secret_access_key,
                client_id,
                client_secret,
            } => init::run(
                ctx,
                init::InitFlags {
                    backend_type,
                    name,
                    mount_point,
                    endpoint,
                    bucket,
                    region,
                    access_key_id,
                    secret_access_key,
                    client_id,
                    client_secret,
                },
            ),
            Commands::Start {
                mount_point,
                no_mount,
                p2p,
                no_p2p,
            } => {
                // Tri-state override: --p2p forces on, --no-p2p forces off,
                // neither falls through to the config-file default.
                let p2p_override = if p2p {
                    Some(true)
                } else if no_p2p {
                    Some(false)
                } else {
                    None
                };
                mount::start(ctx, mount_point.as_deref(), no_mount, p2p_override).await
            }
            Commands::Stop => mount::stop(ctx),
            Commands::Restart => {
                mount::stop(ctx)?;
                mount::start(ctx, None, false, None).await
            }
            Commands::Status => status::show(ctx),
            Commands::Pin { path } => cache::pin(ctx, &path),
            Commands::Unpin { path } => cache::unpin(ctx, &path),
            Commands::PinList => cache::pin_list(ctx),
            Commands::Cache { command } => match command {
                CacheCommands::Status => cache::cache_status(ctx),
                CacheCommands::Evict { all } => cache::evict(ctx, all),
                CacheCommands::Warm { path } => cache::warm(ctx, &path),
                CacheCommands::Clear { path } => cache::clear(ctx, &path),
            },
            Commands::ConfigShow { path } => config::show(&path),
            Commands::ConfigValidate => config::validate(),
            Commands::BackendList => status::backend_list(ctx),
            Commands::BackendAdd {
                backend_type,
                name,
                mount_path,
                client_id,
                client_secret,
            } => cache::backend_add(
                ctx,
                &backend_type,
                name.as_deref(),
                mount_path.as_deref(),
                client_id.as_deref(),
                client_secret.as_deref(),
            ),
            Commands::BackendRemove { name } => cache::backend_remove(ctx, &name),
            Commands::P2pIdentity { data_dir } => {
                let identity_dir = data_dir.join("identity");
                let identity =
                    cascade_p2p::identity::DeviceIdentity::load_or_generate(&identity_dir)
                        .context("loading P2P identity")?;
                println!("{}", identity.device_id);
                Ok(())
            }
            Commands::BackendAuth {
                name,
                client_id,
                client_secret,
                device_code,
            } => {
                auth::authenticate(
                    ctx,
                    &name,
                    client_id.as_deref(),
                    client_secret.as_deref(),
                    device_code,
                )
                .await
            }
        }
    }
}
