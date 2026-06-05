pub mod auth;
pub mod cache;
pub mod config;
pub mod grant;
pub mod init;
pub mod mount;
/// Engine-backed `ContentProvider` for the Windows `ProjFS` presenter.
/// Only the Windows daemon wires it up in production, so the module is
/// Windows-gated — but it is plain cross-platform Rust, so it is also
/// compiled under `test` to keep its unit tests running on every host.
#[cfg(any(target_os = "windows", test))]
pub mod projfs_provider;
pub mod remote;
pub mod service;
pub mod share;
pub mod status;
pub mod token;

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

/// Arguments for the `cascade init` sub-command.
///
/// Extracted into a separate Args struct so the `Commands::Init` variant
/// does not become a large-size outlier in the `Commands` enum, which triggers
/// `clippy::large_enum_variant`. Clap's `#[command(flatten)]` inlines these
/// fields into the `Init` variant for argument parsing.
#[derive(Debug, clap::Args)]
pub struct InitArgs {
    /// Backend type (gdrive, s3, local, p2p)
    #[arg(long)]
    pub backend_type: Option<String>,

    /// Name for this backend instance
    #[arg(long)]
    pub name: Option<String>,

    /// Mount point path
    #[arg(long)]
    pub mount_point: Option<String>,

    /// S3 endpoint URL (for --backend-type s3)
    #[arg(long)]
    pub endpoint: Option<String>,

    /// S3 bucket name (for --backend-type s3)
    #[arg(long)]
    pub bucket: Option<String>,

    /// S3 region (for --backend-type s3)
    #[arg(long)]
    pub region: Option<String>,

    /// S3 access key ID (for --backend-type s3)
    #[arg(long)]
    pub access_key_id: Option<String>,

    /// S3 secret access key (for --backend-type s3)
    #[arg(long)]
    pub secret_access_key: Option<String>,

    /// Google Drive `OAuth2` client ID (for --backend-type gdrive)
    #[arg(long)]
    pub client_id: Option<String>,

    /// Google Drive `OAuth2` client secret (for --backend-type gdrive)
    #[arg(long)]
    pub client_secret: Option<String>,

    /// Root directory for the local backend (for --backend-type local)
    #[arg(long)]
    pub local_root: Option<String>,

    /// P2P block-store data directory (for --backend-type p2p)
    #[arg(long)]
    pub p2p_data_dir: Option<String>,

    /// P2P discovery posture: lan-only, private, or public (for --backend-type p2p)
    #[arg(long)]
    pub p2p_exposure: Option<String>,

    /// P2P BEP listener address, e.g. 0.0.0.0:22000 (for --backend-type p2p)
    #[arg(long)]
    pub p2p_listen_addr: Option<String>,

    /// P2P relay endpoint host:port for WAN NAT traversal (for --backend-type p2p)
    #[arg(long)]
    pub p2p_relay_endpoint: Option<String>,

    /// 64-char hex HMAC secret for the relay server (for --backend-type p2p)
    #[arg(long)]
    pub p2p_relay_secret: Option<String>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Guided initial setup
    Init {
        #[command(flatten)]
        args: Box<InitArgs>,
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

        /// Serve the macOS File Provider RPC bridge instead of `FSKit`.
        ///
        /// Stands up the File Provider Unix-socket server that the Swift
        /// extension connects to (`~/.config/cascade/fileprovider.sock`) and
        /// drives the sync runner to populate the state DB and change feed.
        /// The Swift host app must be built, codesigned, and have registered
        /// its File Provider domain separately — see
        /// `docs/fileprovider-smoke-test.md`. No effect on Linux or Windows.
        #[arg(long)]
        file_provider: bool,

        /// Serve the typed HTTP JSON API for the PWA alongside the mount.
        /// Overrides `[web].enabled` in config.toml.
        #[arg(long)]
        web: bool,

        /// Bind address for the HTTP API (default 127.0.0.1:7842, loopback only).
        #[arg(long)]
        web_bind: Option<String>,

        /// PWA bundle URL the daemon advertises in /v1/bundle.
        #[arg(long)]
        web_bundle_url: Option<String>,

        /// Additional CORS origin to allow for the HTTP API (repeatable).
        /// Loopback origins are always allowed; a wildcard `*` is refused.
        #[arg(long)]
        web_cors_origin: Vec<String>,
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

    /// Configure per-peer, per-folder directional data sharing
    Share {
        #[command(subcommand)]
        command: ShareCommands,
    },

    /// Administer the capability grants this node confers on remote managers
    Grant {
        #[command(subcommand)]
        command: GrantCommands,
    },

    /// Administer a remote node by its device ID
    Remote {
        /// Device ID of the node to administer
        device_id: String,

        /// Path to a signed capability token (JSON) to present, authorising the
        /// command without a live grant row on the node. Issue one with
        /// `cascade token issue`.
        #[arg(long, global = true)]
        token: Option<PathBuf>,

        #[command(subcommand)]
        command: RemoteCommands,
    },

    /// Issue, revoke, and list signed capability tokens this node confers
    Token {
        #[command(subcommand)]
        command: TokenCommands,
    },

    /// Manage the Cascade daemon as an OS background service
    Service {
        #[command(subcommand)]
        command: ServiceCommands,

        /// Install into the per-user scope (no administrator rights). This is
        /// the default; the flag is accepted for explicitness.
        #[arg(long, global = true, conflicts_with = "system")]
        user: bool,

        /// Install into the machine-wide scope (requires elevation). Scaffolded
        /// but not yet implemented — selecting it errors with guidance.
        #[arg(long, global = true)]
        system: bool,
    },

    /// PWA authentication: pairing codes, device codes, and shared secrets
    Auth {
        #[command(subcommand)]
        command: AuthCommands,
    },
}

#[derive(Subcommand)]
pub enum ServiceCommands {
    /// Write the service definition and register it with the OS
    Install,

    /// Deregister the service and remove its definition
    Uninstall,

    /// Start the registered service
    Start,

    /// Stop the registered service
    Stop,

    /// Show whether the service is registered and running
    Status,
}

impl ServiceCommands {
    /// Map the parsed clap subcommand to the [`service::ServiceAction`] the
    /// handler runs.
    #[must_use]
    pub const fn into_action(self) -> service::ServiceAction {
        match self {
            Self::Install => service::ServiceAction::Install,
            Self::Uninstall => service::ServiceAction::Uninstall,
            Self::Start => service::ServiceAction::Start,
            Self::Stop => service::ServiceAction::Stop,
            Self::Status => service::ServiceAction::Status,
        }
    }
}

#[derive(Subcommand)]
pub enum AuthCommands {
    /// Generate a pairing code for the web UI
    Pair,

    /// Authorise a device code from the web UI
    Authorize {
        /// The device code shown in the web UI
        code: String,
    },

    /// Show or generate the daemon shared secret
    Secret,
}

#[derive(Subcommand)]
pub enum TokenCommands {
    /// Mint a token for a bearer device and print its JSON
    Issue {
        /// Device ID of the bearer the token authorises
        bearer: String,

        /// Capability conferred (e.g. status:read, pin:write)
        #[arg(long)]
        cap: String,

        /// Scope: a path prefix, or `*` for node-wide
        #[arg(long)]
        scope: String,

        /// RFC 3339 expiry timestamp (a token always expires)
        #[arg(long)]
        expires: String,
    },

    /// Revoke a token by its id
    Revoke {
        /// Token ID (as printed by `token issue` / `token list`)
        token_id: String,
    },

    /// List every token this node has issued
    List,
}

impl TokenCommands {
    /// Map the parsed clap subcommand to the [`token::TokenCommand`] the handler
    /// runs.
    #[must_use]
    pub fn into_token_command(self) -> token::TokenCommand {
        match self {
            Self::Issue {
                bearer,
                cap,
                scope,
                expires,
            } => token::TokenCommand::Issue {
                bearer,
                capability: cap,
                scope,
                expires,
            },
            Self::Revoke { token_id } => token::TokenCommand::Revoke { token_id },
            Self::List => token::TokenCommand::List,
        }
    }
}

#[derive(Subcommand)]
pub enum GrantCommands {
    /// Grant capabilities to a device over a scope
    Add {
        /// Device ID of the grantee
        device_id: String,

        /// Comma-separated capabilities (e.g. status:read,pin:write)
        #[arg(long)]
        cap: String,

        /// Scope: a path prefix, or `*` for node-wide
        #[arg(long)]
        scope: String,

        /// Optional RFC 3339 expiry timestamp
        #[arg(long)]
        expires: Option<String>,
    },

    /// List every grant held on this node
    List,

    /// Revoke a grant by its ID
    Revoke {
        /// Grant ID (as shown by `grant list`)
        grant_id: i64,
    },

    /// Print the management audit log
    Audit,
}

#[derive(Subcommand)]
pub enum ShareCommands {
    /// Grant a peer directional access to a folder
    Add {
        /// Device ID of the peer to share with
        peer_device_id: String,

        /// Folder path to share (must be an explicit folder, not `*`)
        folder: String,

        /// Sharing direction: read-only, write-only, or read-write
        #[arg(long)]
        direction: String,

        /// Optional RFC 3339 expiry timestamp
        #[arg(long)]
        expires: Option<String>,
    },

    /// List directional shares per peer
    List {
        /// Restrict listing to a specific folder
        folder: Option<String>,
    },

    /// Revoke a peer's directional access to a folder
    Revoke {
        /// Device ID of the peer
        peer_device_id: String,

        /// Folder path
        folder: String,

        /// Restrict revocation to a specific direction (read-only, write-only, read-write)
        #[arg(long)]
        direction: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum RemoteCommands {
    /// Read the remote node's status
    Status,

    /// Pin a path on the remote node
    Pin {
        /// Path to pin
        path: String,
    },

    /// Unpin a path on the remote node
    Unpin {
        /// Path to unpin
        path: String,
    },

    /// Cache management on the remote node
    Cache {
        #[command(subcommand)]
        command: RemoteCacheCommands,
    },

    /// Push a .cascade config fragment to the remote node
    Config {
        #[command(subcommand)]
        command: RemoteConfigCommands,
    },

    /// Lifecycle policy management on the remote node
    Policy {
        #[command(subcommand)]
        command: RemotePolicyCommands,
    },

    /// Backend management on the remote node
    Backend {
        #[command(subcommand)]
        command: RemoteBackendCommands,
    },

    /// Restart the remote node's background workers
    Restart {
        /// Folder scope the lifecycle:control grant covers (no wildcard — a
        /// dangerous capability needs an explicit folder)
        #[arg(long)]
        scope: String,
    },

    /// Stop the remote node's background workers
    Stop {
        /// Folder scope the lifecycle:control grant covers (no wildcard — a
        /// dangerous capability needs an explicit folder)
        #[arg(long)]
        scope: String,
    },

    /// Delegate or revoke grants on the remote node
    Grant {
        #[command(subcommand)]
        command: RemoteGrantCommands,
    },
}

#[derive(Subcommand)]
pub enum RemoteCacheCommands {
    /// Run cache eviction on the remote node
    Evict,

    /// Warm a path on the remote node (pins it so files download)
    Warm {
        /// Path to warm
        path: String,
    },
}

#[derive(Subcommand)]
pub enum RemoteConfigCommands {
    /// Push a .cascade fragment file, merging it into the node's rule set
    Push {
        /// Path to the local .cascade fragment file (its extension selects the
        /// format: .toml/.yaml/.json, else gitignore-style)
        file: PathBuf,

        /// Folder the fragment applies to (defaults to the node root)
        #[arg(long)]
        scope: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum RemotePolicyCommands {
    /// Set a lifecycle policy over a path glob on the remote node
    Set {
        /// Path glob the policy applies to
        path: String,

        /// Maximum file age before eviction, in seconds
        #[arg(long)]
        max_age_secs: Option<i64>,

        /// Maximum file size before eviction, in bytes
        #[arg(long)]
        max_file_size: Option<i64>,

        /// Priority — higher wins when policies overlap
        #[arg(long, default_value_t = 0)]
        priority: i32,
    },
}

#[derive(Subcommand)]
pub enum RemoteBackendCommands {
    /// Register a backend on the remote node
    Add {
        /// Backend name (its identifier and config file stem)
        name: String,

        /// Backend type (gdrive, s3, p2p, …)
        backend_type: String,

        /// VFS mount path the backend is mounted at (the authorised scope)
        #[arg(long)]
        mount_path: String,

        /// Path to the local TOML config fragment for the backend
        #[arg(long = "config-file")]
        config_file: PathBuf,
    },

    /// Remove a registered backend from the remote node
    Remove {
        /// Backend name to remove
        name: String,

        /// VFS mount path the backend occupied (the authorised scope)
        #[arg(long)]
        mount_path: String,
    },
}

#[derive(Subcommand)]
pub enum RemoteGrantCommands {
    /// Delegate a grant to another device on the remote node
    Add {
        /// Device ID of the grantee
        grantee: String,

        /// Capability to confer, in its wire form (e.g. status:read)
        #[arg(long)]
        cap: String,

        /// Scope: a folder path, or `*` for node-wide
        #[arg(long)]
        scope: String,

        /// Optional RFC 3339 expiry timestamp
        #[arg(long)]
        expires: Option<String>,
    },

    /// Revoke a grant on the remote node by its row id
    Revoke {
        /// Grant row id (as shown by the node's `grant list`)
        grant_id: i64,

        /// Folder scope the caller's grant:admin grant covers
        #[arg(long)]
        scope: String,
    },
}

impl RemoteCommands {
    /// Map a parsed remote subcommand into the transport-side
    /// [`remote::RemoteCommand`].
    ///
    /// The file-bearing variants (`config push`, `backend add`) read their
    /// fragment from disk here, so a missing or unreadable file fails before
    /// any transport is opened.
    ///
    /// # Errors
    ///
    /// Returns an error when a referenced fragment file cannot be read.
    pub fn into_remote_command(self) -> Result<remote::RemoteCommand> {
        let command = match self {
            Self::Status => remote::RemoteCommand::Status,
            Self::Pin { path } => remote::RemoteCommand::Pin { path },
            Self::Unpin { path } => remote::RemoteCommand::Unpin { path },
            Self::Cache { command } => match command {
                RemoteCacheCommands::Evict => remote::RemoteCommand::CacheEvict,
                RemoteCacheCommands::Warm { path } => remote::RemoteCommand::CacheWarm { path },
            },
            Self::Config { command } => match command {
                RemoteConfigCommands::Push { file, scope } => {
                    remote::config_push(&file, scope.as_deref())?
                }
            },
            Self::Policy { command } => match command {
                RemotePolicyCommands::Set {
                    path,
                    max_age_secs,
                    max_file_size,
                    priority,
                } => remote::RemoteCommand::PolicySet {
                    path_glob: path,
                    max_age_secs,
                    max_file_size,
                    priority,
                },
            },
            Self::Backend { command } => match command {
                RemoteBackendCommands::Add {
                    name,
                    backend_type,
                    mount_path,
                    config_file,
                } => remote::backend_add(name, backend_type, mount_path, &config_file)?,
                RemoteBackendCommands::Remove { name, mount_path } => {
                    remote::RemoteCommand::BackendRemove { name, mount_path }
                }
            },
            Self::Restart { scope } => remote::RemoteCommand::Restart { scope },
            Self::Stop { scope } => remote::RemoteCommand::Stop { scope },
            Self::Grant { command } => match command {
                RemoteGrantCommands::Add {
                    grantee,
                    cap,
                    scope,
                    expires,
                } => remote::RemoteCommand::GrantAdd {
                    grantee,
                    capability: cap,
                    scope,
                    expires,
                },
                RemoteGrantCommands::Revoke { grant_id, scope } => {
                    remote::RemoteCommand::GrantRevoke { grant_id, scope }
                }
            },
        };
        Ok(command)
    }
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
            Commands::Init { args } => init::run(
                ctx,
                init::InitFlags {
                    backend_type: args.backend_type,
                    name: args.name,
                    mount_point: args.mount_point,
                    endpoint: args.endpoint,
                    bucket: args.bucket,
                    region: args.region,
                    access_key_id: args.access_key_id,
                    secret_access_key: args.secret_access_key,
                    client_id: args.client_id,
                    client_secret: args.client_secret,
                    local_root: args.local_root,
                    p2p_data_dir: args.p2p_data_dir,
                    p2p_exposure: args.p2p_exposure,
                    p2p_listen_addr: args.p2p_listen_addr,
                    p2p_relay_endpoint: args.p2p_relay_endpoint,
                    p2p_relay_secret: args.p2p_relay_secret,
                },
            ),
            Commands::Start {
                mount_point,
                no_mount,
                p2p,
                no_p2p,
                file_provider,
                web,
                web_bind,
                web_bundle_url,
                web_cors_origin,
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
                let web_flags = mount::WebFlags {
                    enable: web,
                    bind: web_bind,
                    bundle_url: web_bundle_url,
                    cors_origins: web_cors_origin,
                };
                mount::start(
                    ctx,
                    mount_point.as_deref(),
                    no_mount,
                    p2p_override,
                    file_provider,
                    web_flags,
                )
                .await
            }
            Commands::Stop => mount::stop(ctx),
            Commands::Restart => {
                mount::stop(ctx)?;
                mount::start(ctx, None, false, None, false, mount::WebFlags::default()).await
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
            Commands::Share { command } => match command {
                ShareCommands::Add {
                    peer_device_id,
                    folder,
                    direction,
                    expires,
                } => share::add(
                    ctx,
                    &peer_device_id,
                    &folder,
                    &direction,
                    expires.as_deref(),
                ),
                ShareCommands::List { folder } => share::list(ctx, folder.as_deref()),
                ShareCommands::Revoke {
                    peer_device_id,
                    folder,
                    direction,
                } => share::revoke(ctx, &peer_device_id, &folder, direction.as_deref()),
            },
            Commands::Grant { command } => match command {
                GrantCommands::Add {
                    device_id,
                    cap,
                    scope,
                    expires,
                } => grant::add(ctx, &device_id, &cap, &scope, expires.as_deref()),
                GrantCommands::List => grant::list(ctx),
                GrantCommands::Revoke { grant_id } => grant::revoke(ctx, grant_id),
                GrantCommands::Audit => grant::audit(ctx),
            },
            Commands::Remote {
                device_id,
                token,
                command,
            } => {
                let remote_command = command.into_remote_command()?;
                let token_json = token.as_deref().map(remote::read_token_file).transpose()?;
                remote::run(ctx, &device_id, remote_command, token_json).await
            }
            Commands::Token { command } => token::run(ctx, command.into_token_command()),
            Commands::Service {
                command,
                user,
                system,
            } => {
                // Scope resolution is owned by the service module: an explicit
                // flag wins, otherwise the session is inferred, with a
                // TTY-gated prompt only at the interactive-desktop boundary.
                let request = service::ScopeRequest::from_flags(user, system);
                service::run(ctx, command.into_action(), request).await
            }
            Commands::Auth { command } => auth::pwa_auth(ctx, command).await,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use cascade_p2p::protocol::{ManageCommand, ManageConfigFormat, ManageGrant, ManageScope};
    use clap::Parser as _;

    use super::*;

    /// Parse a full `cascade remote <device-id> …` invocation and return the
    /// device id plus the mapped transport-side command. Fails the test if the
    /// arguments do not parse or do not resolve to a `Remote` subcommand.
    fn parse_remote(args: &[&str]) -> (String, remote::RemoteCommand) {
        let cli = Cli::try_parse_from(args).expect("arguments should parse");
        match cli.command {
            Commands::Remote {
                device_id, command, ..
            } => (device_id, command.into_remote_command().unwrap()),
            _ => panic!("expected a remote subcommand"),
        }
    }

    #[test]
    fn parse_service_install_defaults_to_user_scope() {
        let cli = Cli::try_parse_from(["cascade", "service", "install"])
            .expect("service install should parse");
        let Commands::Service {
            command,
            user,
            system,
        } = cli.command
        else {
            panic!("expected a service subcommand");
        };
        assert!(!user, "no scope flag given");
        assert!(!system, "no scope flag given");
        assert_eq!(command.into_action(), service::ServiceAction::Install);
    }

    #[test]
    fn parse_service_subcommands_map_to_actions() {
        let cases = [
            ("install", service::ServiceAction::Install),
            ("uninstall", service::ServiceAction::Uninstall),
            ("start", service::ServiceAction::Start),
            ("stop", service::ServiceAction::Stop),
            ("status", service::ServiceAction::Status),
        ];
        for (sub, expected) in cases {
            let cli = Cli::try_parse_from(["cascade", "service", sub])
                .expect("service subcommand should parse");
            let Commands::Service { command, .. } = cli.command else {
                panic!("expected a service subcommand");
            };
            assert_eq!(command.into_action(), expected);
        }
    }

    #[test]
    fn parse_service_scope_flags_are_mutually_exclusive() {
        // --user and --system cannot both be given.
        assert!(
            Cli::try_parse_from(["cascade", "service", "install", "--user", "--system"]).is_err()
        );
    }

    #[test]
    fn parse_service_system_flag_selects_system_scope() {
        let cli = Cli::try_parse_from(["cascade", "service", "install", "--system"])
            .expect("service install --system should parse");
        let Commands::Service { system, .. } = cli.command else {
            panic!("expected a service subcommand");
        };
        assert!(system);
    }

    #[test]
    fn parse_remote_status() {
        let (device, cmd) = parse_remote(&["cascade", "remote", "PEER", "status"]);
        assert_eq!(device, "PEER");
        assert_eq!(cmd, remote::RemoteCommand::Status);
        assert_eq!(cmd.to_wire(), ManageCommand::StatusRead);
        assert_eq!(cmd.wire_scope(), ManageScope::Node);
    }

    #[test]
    fn parse_remote_cache_warm() {
        let (_device, cmd) =
            parse_remote(&["cascade", "remote", "PEER", "cache", "warm", "/media"]);
        assert_eq!(
            cmd,
            remote::RemoteCommand::CacheWarm {
                path: "/media".to_owned(),
            }
        );
        // `cache warm` rides a recursive pin, the same wire command the local
        // `cache warm` produces.
        assert_eq!(
            cmd.to_wire(),
            ManageCommand::Pin {
                path_glob: "/media".to_owned(),
                recursive: true,
            }
        );
        assert_eq!(
            cmd.wire_scope(),
            ManageScope::Folder {
                path: "/media".to_owned(),
            }
        );
    }

    #[test]
    fn parse_remote_config_push_infers_format_and_default_scope() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("rules.toml");
        std::fs::write(&file, "ignore = [\"*.tmp\"]\n").unwrap();
        let file_arg = file.to_str().unwrap();

        let (_device, cmd) =
            parse_remote(&["cascade", "remote", "PEER", "config", "push", file_arg]);
        assert_eq!(
            cmd,
            remote::RemoteCommand::ConfigPush {
                folder: "/".to_owned(),
                format: ManageConfigFormat::Toml,
                body: "ignore = [\"*.tmp\"]\n".to_owned(),
            }
        );
        assert_eq!(
            cmd.to_wire(),
            ManageCommand::ConfigPush {
                format: ManageConfigFormat::Toml,
                folder: "/".to_owned(),
                body: "ignore = [\"*.tmp\"]\n".to_owned(),
            }
        );
        // An unscoped push targets the node root.
        assert_eq!(
            cmd.wire_scope(),
            ManageScope::Folder {
                path: "/".to_owned(),
            }
        );
    }

    #[test]
    fn parse_remote_config_push_with_scope_and_gitignore_format() {
        let dir = tempfile::tempdir().unwrap();
        // An extensionless `.cascade` file is gitignore-style.
        let file = dir.path().join(".cascade");
        std::fs::write(&file, "*.log\n").unwrap();
        let file_arg = file.to_str().unwrap();

        let (_device, cmd) = parse_remote(&[
            "cascade", "remote", "PEER", "config", "push", file_arg, "--scope", "/work",
        ]);
        assert_eq!(
            cmd,
            remote::RemoteCommand::ConfigPush {
                folder: "/work".to_owned(),
                format: ManageConfigFormat::Gitignore,
                body: "*.log\n".to_owned(),
            }
        );
        assert_eq!(
            cmd.wire_scope(),
            ManageScope::Folder {
                path: "/work".to_owned(),
            }
        );
    }

    #[test]
    fn parse_remote_policy_set() {
        let (_device, cmd) = parse_remote(&[
            "cascade",
            "remote",
            "PEER",
            "policy",
            "set",
            "/work/*.iso",
            "--max-age-secs",
            "86400",
            "--max-file-size",
            "1048576",
            "--priority",
            "5",
        ]);
        assert_eq!(
            cmd,
            remote::RemoteCommand::PolicySet {
                path_glob: "/work/*.iso".to_owned(),
                max_age_secs: Some(86_400),
                max_file_size: Some(1_048_576),
                priority: 5,
            }
        );
        assert_eq!(
            cmd.to_wire(),
            ManageCommand::PolicySet {
                path_glob: "/work/*.iso".to_owned(),
                max_age_secs: Some(86_400),
                max_file_size: Some(1_048_576),
                priority: 5,
            }
        );
        assert_eq!(
            cmd.wire_scope(),
            ManageScope::Folder {
                path: "/work/*.iso".to_owned(),
            }
        );
    }

    #[test]
    fn parse_remote_policy_set_defaults_priority_and_unbounded_dimensions() {
        let (_device, cmd) = parse_remote(&["cascade", "remote", "PEER", "policy", "set", "/work"]);
        assert_eq!(
            cmd,
            remote::RemoteCommand::PolicySet {
                path_glob: "/work".to_owned(),
                max_age_secs: None,
                max_file_size: None,
                priority: 0,
            }
        );
    }

    #[test]
    fn parse_remote_backend_add_reads_config_fragment() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("gdrive.toml");
        std::fs::write(&config, "type = \"gdrive\"\nname = \"work\"\n").unwrap();
        let config_arg = config.to_str().unwrap();

        let (_device, cmd) = parse_remote(&[
            "cascade",
            "remote",
            "PEER",
            "backend",
            "add",
            "work",
            "gdrive",
            "--mount-path",
            "/Work",
            "--config-file",
            config_arg,
        ]);
        assert_eq!(
            cmd,
            remote::RemoteCommand::BackendAdd {
                name: "work".to_owned(),
                backend_type: "gdrive".to_owned(),
                mount_path: "/Work".to_owned(),
                config_toml: "type = \"gdrive\"\nname = \"work\"\n".to_owned(),
            }
        );
        assert_eq!(
            cmd.to_wire(),
            ManageCommand::BackendAdd {
                name: "work".to_owned(),
                backend_type: "gdrive".to_owned(),
                mount_path: "/Work".to_owned(),
                config_toml: "type = \"gdrive\"\nname = \"work\"\n".to_owned(),
            }
        );
        assert_eq!(
            cmd.wire_scope(),
            ManageScope::Folder {
                path: "/Work".to_owned(),
            }
        );
    }

    #[test]
    fn parse_remote_backend_remove() {
        let (_device, cmd) = parse_remote(&[
            "cascade",
            "remote",
            "PEER",
            "backend",
            "remove",
            "work",
            "--mount-path",
            "/Work",
        ]);
        assert_eq!(
            cmd,
            remote::RemoteCommand::BackendRemove {
                name: "work".to_owned(),
                mount_path: "/Work".to_owned(),
            }
        );
        assert_eq!(
            cmd.wire_scope(),
            ManageScope::Folder {
                path: "/Work".to_owned(),
            }
        );
    }

    #[test]
    fn parse_remote_restart_and_stop_carry_their_scope() {
        let (_device, restart) =
            parse_remote(&["cascade", "remote", "PEER", "restart", "--scope", "/work"]);
        assert_eq!(
            restart,
            remote::RemoteCommand::Restart {
                scope: "/work".to_owned(),
            }
        );
        assert_eq!(restart.to_wire(), ManageCommand::Restart);
        // A dangerous capability is never node-wide, so the advertised scope is
        // the explicit folder the operator named.
        assert_eq!(
            restart.wire_scope(),
            ManageScope::Folder {
                path: "/work".to_owned(),
            }
        );

        let (_device, stop) =
            parse_remote(&["cascade", "remote", "PEER", "stop", "--scope", "/work"]);
        assert_eq!(stop.to_wire(), ManageCommand::Stop);
        assert_eq!(
            stop.wire_scope(),
            ManageScope::Folder {
                path: "/work".to_owned(),
            }
        );
    }

    #[test]
    fn parse_remote_grant_add_advertises_grant_admin_scope() {
        let (_device, cmd) = parse_remote(&[
            "cascade",
            "remote",
            "PEER",
            "grant",
            "add",
            "GRANTEE",
            "--cap",
            "status:read",
            "--scope",
            "/work",
            "--expires",
            "2026-12-31T00:00:00Z",
        ]);
        assert_eq!(
            cmd,
            remote::RemoteCommand::GrantAdd {
                grantee: "GRANTEE".to_owned(),
                capability: "status:read".to_owned(),
                scope: "/work".to_owned(),
                expires: Some("2026-12-31T00:00:00Z".to_owned()),
            }
        );
        assert_eq!(
            cmd.to_wire(),
            ManageCommand::GrantAdd {
                grant: ManageGrant {
                    grantee: "GRANTEE".to_owned(),
                    capability: "status:read".to_owned(),
                    scope: ManageScope::Folder {
                        path: "/work".to_owned(),
                    },
                    expires: Some("2026-12-31T00:00:00Z".to_owned()),
                },
            }
        );
        // The grant command itself is authorised over the grant's scope.
        assert_eq!(
            cmd.wire_scope(),
            ManageScope::Folder {
                path: "/work".to_owned(),
            }
        );
    }

    #[test]
    fn parse_remote_grant_add_wildcard_scope_maps_to_node() {
        let (_device, cmd) = parse_remote(&[
            "cascade",
            "remote",
            "PEER",
            "grant",
            "add",
            "GRANTEE",
            "--cap",
            "status:read",
            "--scope",
            "*",
        ]);
        assert_eq!(
            cmd.to_wire(),
            ManageCommand::GrantAdd {
                grant: ManageGrant {
                    grantee: "GRANTEE".to_owned(),
                    capability: "status:read".to_owned(),
                    scope: ManageScope::Node,
                    expires: None,
                },
            }
        );
        assert_eq!(cmd.wire_scope(), ManageScope::Node);
    }

    #[test]
    fn parse_remote_grant_revoke() {
        let (_device, cmd) = parse_remote(&[
            "cascade", "remote", "PEER", "grant", "revoke", "7", "--scope", "/work",
        ]);
        assert_eq!(
            cmd,
            remote::RemoteCommand::GrantRevoke {
                grant_id: 7,
                scope: "/work".to_owned(),
            }
        );
        assert_eq!(
            cmd.to_wire(),
            ManageCommand::GrantRevoke {
                grant_id: 7,
                scope: ManageScope::Folder {
                    path: "/work".to_owned(),
                },
            }
        );
    }

    #[test]
    fn parse_remote_config_push_missing_file_fails() {
        let cli = Cli::try_parse_from([
            "cascade",
            "remote",
            "PEER",
            "config",
            "push",
            "/nonexistent/rules.toml",
        ])
        .expect("arguments should parse");
        let Commands::Remote { command, .. } = cli.command else {
            panic!("expected a remote subcommand");
        };
        // A fragment file that cannot be read fails before any transport opens.
        assert!(command.into_remote_command().is_err());
    }

    #[test]
    fn parse_remote_restart_requires_scope() {
        // The dangerous restart command has no default scope; omitting --scope
        // is a parse error rather than a silent node-wide attempt.
        assert!(Cli::try_parse_from(["cascade", "remote", "PEER", "restart"]).is_err());
    }

    // ── In-process loopback round-trip ──────────────────────────────────────
    //
    // A manager-side `SyncEngine` drives a managed node through a newly-wired
    // command (`policy set`) end to end over loopback TCP + mutual TLS — the
    // same wire path the daemon uses, with no real network. The manager builds
    // the wire frame exactly as `cascade remote <id> policy set …` does: parse
    // the CLI, map it to `RemoteCommand`, and send `to_wire()` + `wire_scope()`.
    // The managed node authorises + audits + runs it through the real dispatch
    // core, and the typed reply is correlated back to the caller.

    mod loopback {
        use std::sync::{Arc, Mutex};

        use async_trait::async_trait;
        use cascade_backend_p2p::index::FolderIndex;
        use cascade_backend_p2p::sync::{Peer, SyncEngine};
        use cascade_engine::db::AuditEntry;
        use cascade_engine::manage::{
            Capability, DeviceId, Grant, ManageCommandExecutor, ManageDispatch, ManageGrantStore,
            Scope, run_dispatch,
        };
        use cascade_p2p::identity::DeviceIdentity;
        use cascade_p2p::protocol::{
            ManageCommand, ManageConfigFormat, ManageErrorKind, ManageResult, ManageScope,
        };
        use cascade_p2p::store::BlockStore;
        use chrono::{DateTime, Utc};
        use clap::Parser as _;
        use tempfile::TempDir;

        use crate::cli::{Cli, Commands, remote::RemoteCommand};

        /// In-memory grant store + audit sink + recording executor: the
        /// in-process double standing in for the daemon's `Engine`, so the
        /// round-trip exercises the real authorise → audit → execute core
        /// without a database or live filesystem.
        struct TestNode {
            grants: Vec<Grant>,
            audit: Mutex<Vec<AuditEntry>>,
            calls: Mutex<Vec<String>>,
            node_device_id: DeviceId,
        }

        impl TestNode {
            fn new(grants: Vec<Grant>) -> Self {
                Self {
                    grants,
                    audit: Mutex::new(Vec::new()),
                    calls: Mutex::new(Vec::new()),
                    node_device_id: DeviceId::new("CLI-TEST-NODE"),
                }
            }

            fn calls(&self) -> Vec<String> {
                self.calls.lock().map(|c| c.clone()).unwrap_or_default()
            }

            fn audit_outcomes(&self) -> Vec<String> {
                self.audit
                    .lock()
                    .map(|rows| rows.iter().map(|r| r.outcome.clone()).collect())
                    .unwrap_or_default()
            }

            fn record(&self, call: &str) {
                if let Ok(mut calls) = self.calls.lock() {
                    calls.push(call.to_owned());
                }
            }
        }

        impl ManageGrantStore for TestNode {
            fn manage_grants(&self) -> anyhow::Result<Vec<Grant>> {
                Ok(self.grants.clone())
            }

            fn manage_grant_scope(&self, _grant_id: i64) -> anyhow::Result<Option<Scope>> {
                Ok(None)
            }

            fn manage_append_audit(&self, entry: &AuditEntry) -> anyhow::Result<()> {
                self.audit
                    .lock()
                    .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?
                    .push(entry.clone());
                Ok(())
            }

            fn manage_node_device_id(&self) -> anyhow::Result<DeviceId> {
                Ok(self.node_device_id.clone())
            }

            fn manage_revoked_token_ids(
                &self,
            ) -> anyhow::Result<std::collections::HashSet<String>> {
                Ok(std::collections::HashSet::new())
            }
        }

        #[async_trait]
        impl ManageCommandExecutor for TestNode {
            async fn manage_status(&self) -> anyhow::Result<String> {
                self.record("status");
                Ok("ok".to_owned())
            }

            async fn manage_pin(&self, path_glob: &str, recursive: bool) -> anyhow::Result<String> {
                self.record(&format!("pin {path_glob} {recursive}"));
                Ok(format!("pinned {path_glob}"))
            }

            async fn manage_unpin(&self, path_glob: &str) -> anyhow::Result<String> {
                self.record(&format!("unpin {path_glob}"));
                Ok(format!("unpinned {path_glob}"))
            }

            async fn manage_cache_evict(&self) -> anyhow::Result<String> {
                self.record("evict");
                Ok("evicted".to_owned())
            }

            async fn manage_cache_warm(&self, path_glob: &str) -> anyhow::Result<String> {
                self.record(&format!("warm {path_glob}"));
                Ok(format!("warmed {path_glob}"))
            }

            async fn manage_config_push(
                &self,
                format: ManageConfigFormat,
                folder: &str,
                _body: &str,
            ) -> anyhow::Result<String> {
                self.record(&format!("config_push {format:?} {folder}"));
                Ok(format!("pushed into {folder}"))
            }

            async fn manage_policy_set(
                &self,
                path_glob: &str,
                max_age_secs: Option<i64>,
                max_file_size: Option<i64>,
                priority: i32,
            ) -> anyhow::Result<String> {
                self.record(&format!(
                    "policy_set {path_glob} {max_age_secs:?} {max_file_size:?} {priority}"
                ));
                Ok(format!("policy set for {path_glob}"))
            }

            async fn manage_backend_add(
                &self,
                name: &str,
                backend_type: &str,
                mount_path: &str,
                _config_toml: &str,
            ) -> anyhow::Result<String> {
                self.record(&format!("backend_add {name} {backend_type} {mount_path}"));
                Ok(format!("backend {name} added"))
            }

            async fn manage_backend_remove(
                &self,
                name: &str,
                mount_path: &str,
            ) -> anyhow::Result<String> {
                self.record(&format!("backend_remove {name} {mount_path}"));
                Ok(format!("backend {name} removed"))
            }

            async fn manage_restart(&self) -> anyhow::Result<String> {
                self.record("restart");
                Ok("restarted".to_owned())
            }

            async fn manage_stop(&self) -> anyhow::Result<String> {
                self.record("stop");
                Ok("stopped".to_owned())
            }

            async fn manage_grant_add(&self, grant: &Grant) -> anyhow::Result<String> {
                self.record(&format!(
                    "grant_add {} {}",
                    grant.grantee,
                    grant.capability.as_wire()
                ));
                Ok("grant added".to_owned())
            }

            async fn manage_grant_revoke(&self, grant_id: i64) -> anyhow::Result<String> {
                self.record(&format!("grant_revoke {grant_id}"));
                Ok(format!("grant {grant_id} revoked"))
            }
        }

        #[async_trait]
        impl ManageDispatch for TestNode {
            async fn dispatch(
                &self,
                caller: &DeviceId,
                command: ManageCommand,
                scope: ManageScope,
                token: Option<String>,
                now: DateTime<Utc>,
            ) -> ManageResult {
                run_dispatch(self, self, caller, command, scope, token, now).await
            }
        }

        /// A bare `SyncEngine` backed by a fresh tempdir index + block store and
        /// a freshly generated device identity. The tempdir outlives the engine.
        fn make_engine(folder_id: &str) -> (TempDir, SyncEngine) {
            let dir = tempfile::tempdir().unwrap();
            let index = Arc::new(FolderIndex::open(&dir.path().join("index.db")).unwrap());
            let blocks = Arc::new(BlockStore::new(&dir.path().join("blocks")).unwrap());
            let identity = DeviceIdentity::load_or_generate(&dir.path().join("identity")).unwrap();
            let engine = SyncEngine::new(folder_id.to_owned(), index, blocks, identity);
            (dir, engine)
        }

        fn grant(grantee: &str, capability: Capability, scope: Scope) -> Grant {
            Grant {
                grantee: DeviceId::new(grantee.to_owned()),
                capability,
                scope,
                granted_by: DeviceId::new("OWNER"),
                expires: None,
            }
        }

        #[tokio::test]
        async fn manager_cli_drives_policy_set_over_loopback() {
            // The manager builds the wire frame exactly as the CLI does: parse
            // `cascade remote <id> policy set …`, map to RemoteCommand, send its
            // to_wire()/wire_scope().
            let cli = Cli::try_parse_from([
                "cascade",
                "remote",
                "TARGET",
                "policy",
                "set",
                "/work/reports",
                "--max-age-secs",
                "3600",
                "--priority",
                "2",
            ])
            .unwrap();
            let Commands::Remote { command, .. } = cli.command else {
                panic!("expected remote subcommand");
            };
            let remote_command = command.into_remote_command().unwrap();
            assert_eq!(
                remote_command,
                RemoteCommand::PolicySet {
                    path_glob: "/work/reports".to_owned(),
                    max_age_secs: Some(3600),
                    max_file_size: None,
                    priority: 2,
                }
            );

            let (_manager_dir, manager) = make_engine("shared");
            let manager_id = manager.device_id().to_owned();

            // The node grants the manager policy:set over /work — exactly what
            // `cascade grant add MANAGER --cap policy:set --scope /work` persists.
            let node = Arc::new(TestNode::new(vec![grant(
                &manager_id,
                Capability::PolicySet,
                Scope::folder("/work"),
            )]));

            let (_target_dir, target) = make_engine("shared");
            let dispatch: Arc<dyn ManageDispatch> = node.clone();
            let target = target.with_manage_dispatch(dispatch);
            let target_id = target.device_id().to_owned();

            target.trust(manager_id.clone()).await;
            manager.trust(target_id.clone()).await;

            let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
            let (addr, _task) = target
                .start_listener("127.0.0.1:0".parse().unwrap(), cancel_rx)
                .await
                .unwrap();
            manager
                .connect_to(Peer {
                    device_id: target_id.clone(),
                    address: addr,
                })
                .await
                .unwrap();

            for _ in 0..100 {
                if manager.has_peer(&target_id).await {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            assert!(
                manager.has_peer(&target_id).await,
                "manager never established a session with the target node",
            );

            // Drive the newly-wired policy set end to end.
            let result = manager
                .send_manage_request(
                    &target_id,
                    remote_command.to_wire(),
                    remote_command.wire_scope(),
                    None,
                )
                .await
                .expect("policy set round-trip should not fail at the transport");
            assert!(
                matches!(result, ManageResult::Ok { .. }),
                "authorised policy:set should succeed, got {result:?}",
            );
            assert_eq!(
                node.calls(),
                vec!["policy_set /work/reports Some(3600) None 2".to_owned()],
                "the node must have run the policy set with the wired arguments",
            );
            assert_eq!(node.audit_outcomes(), vec!["allowed".to_owned()]);

            // A policy set OUTSIDE the granted scope is refused by the node —
            // the same CLI mapping, a different path.
            let outside = Cli::try_parse_from([
                "cascade",
                "remote",
                "TARGET",
                "policy",
                "set",
                "/personal/secret",
            ])
            .unwrap();
            let Commands::Remote { command, .. } = outside.command else {
                panic!("expected remote subcommand");
            };
            let outside = command.into_remote_command().unwrap();
            let denied = manager
                .send_manage_request(&target_id, outside.to_wire(), outside.wire_scope(), None)
                .await
                .expect("an unauthorised policy set still returns a typed reply");
            assert!(
                matches!(
                    denied,
                    ManageResult::Err {
                        kind: ManageErrorKind::Unauthorised,
                        ..
                    }
                ),
                "a policy set outside the granted scope must be refused, got {denied:?}",
            );
            assert_eq!(
                node.calls().len(),
                1,
                "the denied policy set must not have run a side effect",
            );
        }
    }
}
