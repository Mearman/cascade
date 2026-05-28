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
///
/// On non-Unix platforms, a reliable cross-process liveness check is not
/// available without an OS-specific crate, so the presence of the PID file is
/// treated as sufficient and this function always returns `true`.
pub fn is_process_alive(pid: u32) -> bool {
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
        let _ = pid;
        true
    }
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
    Init,

    /// Start the daemon and mount all configured backends
    Start {
        /// Mount point override
        #[arg(long)]
        mount_point: Option<String>,
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
        /// Backend type (gdrive, s3, webdav, dropbox, onedrive, local)
        backend_type: String,

        /// Name for this backend instance
        #[arg(long)]
        name: Option<String>,

        /// Mount path (relative path in VFS, e.g. Work/Projects)
        #[arg(long)]
        mount_path: Option<String>,
    },

    /// Remove a backend
    #[command(name = "backend-remove")]
    BackendRemove {
        /// Backend name
        name: String,
    },

    /// Authenticate a backend (runs `OAuth2` device-code flow)
    #[command(name = "backend-auth")]
    BackendAuth {
        /// Backend name
        name: String,
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
            Commands::Init => init::run(ctx),
            Commands::Start { mount_point } => mount::start(ctx, mount_point.as_deref()).await,
            Commands::Stop => mount::stop(ctx),
            Commands::Restart => {
                mount::stop(ctx)?;
                mount::start(ctx, None).await
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
            } => cache::backend_add(ctx, &backend_type, name.as_deref(), mount_path.as_deref()),
            Commands::BackendRemove { name } => cache::backend_remove(ctx, &name),
            Commands::BackendAuth { name } => auth::authenticate(ctx, &name).await,
        }
    }
}
