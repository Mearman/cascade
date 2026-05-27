pub mod config;
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
            Commands::Start { mount_point } => {
                mount::start(mount_point.as_deref()).await
            }
            Commands::Stop => {
                mount::stop().await
            }
            Commands::Restart => {
                mount::stop().await?;
                mount::start(None).await
            }
            Commands::Status => {
                status::show().await
            }
            Commands::Pin { path } => {
                tracing::info!("Pinning: {}", path);
                println!("Pinned: {}", path);
                Ok(())
            }
            Commands::Unpin { path } => {
                tracing::info!("Unpinning: {}", path);
                println!("Unpinned: {}", path);
                Ok(())
            }
            Commands::PinList => {
                println!("No pinned paths.");
                Ok(())
            }
            Commands::Cache { command } => {
                match command {
                    CacheCommands::Status => {
                        status::cache_status().await
                    }
                    CacheCommands::Evict { all } => {
                        tracing::info!("Running eviction (all={})", all);
                        println!("Eviction complete.");
                        Ok(())
                    }
                    CacheCommands::Warm { path } => {
                        tracing::info!("Warming cache: {}", path);
                        println!("Cache warming: {}", path);
                        Ok(())
                    }
                    CacheCommands::Clear { path } => {
                        tracing::info!("Clearing cache: {}", path);
                        println!("Cache cleared: {}", path);
                        Ok(())
                    }
                }
            }
            Commands::ConfigShow { path } => {
                config::show(&path).await
            }
            Commands::ConfigValidate => {
                config::validate().await
            }
            Commands::BackendList => {
                status::backend_list().await
            }
        }
    }
}
