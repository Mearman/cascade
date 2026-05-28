pub mod auth;
pub mod cache;
pub mod config;
pub mod init;
pub mod mount;
pub mod status;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "cascade")]
#[command(about = "Cross-platform cloud storage filesystem client")]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Config file path
    #[arg(long, global = true, default_value = "~/.config/cascade/config.toml")]
    pub config: String,

    /// Increase verbosity
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
    pub async fn run(self) -> Result<()> {
        match self.command {
            Commands::Init => init::run(),
            Commands::Start { mount_point } => mount::start(mount_point.as_deref()).await,
            Commands::Stop => mount::stop(),
            Commands::Restart => {
                mount::stop()?;
                mount::start(None).await
            }
            Commands::Status => status::show(),
            Commands::Pin { path } => cache::pin(&path),
            Commands::Unpin { path } => cache::unpin(&path),
            Commands::PinList => cache::pin_list(),
            Commands::Cache { command } => match command {
                CacheCommands::Status => cache::cache_status(),
                CacheCommands::Evict { all } => cache::evict(all),
                CacheCommands::Warm { path } => {
                    println!("Cache warming: {path}");
                    Ok(())
                }
                CacheCommands::Clear { path } => {
                    println!("Cache cleared: {path}");
                    Ok(())
                }
            },
            Commands::ConfigShow { path } => config::show(&path),
            Commands::ConfigValidate => config::validate(),
            Commands::BackendList => status::backend_list(),
            Commands::BackendAdd {
                backend_type,
                name,
                mount_path,
            } => cache::backend_add(&backend_type, name.as_deref(), mount_path.as_deref()),
            Commands::BackendRemove { name } => cache::backend_remove(&name),
            Commands::BackendAuth { name } => auth::authenticate(&name).await,
        }
    }
}
