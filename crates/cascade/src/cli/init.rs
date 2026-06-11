//! `cascade init` — guided initial setup command.
//!
//! Walks the user through:
//! 1. Choosing a cloud provider
//! 2. Naming the backend
//! 3. Authenticating (provider-specific)
//! 4. Setting a mount point
//! 5. Writing `~/.config/cascade/config.toml`

use std::io::{self, Write as IoWrite};
use std::path::PathBuf;

use anyhow::Result;
use cascade_engine::db::StateDb;
use serde::{Deserialize, Serialize};

/// Flags accepted by `cascade init` for non-interactive mode.
///
/// When `backend_type` is `Some` and all required fields for that backend are
/// present, `run()` skips the interactive wizard entirely. When no flags are
/// provided it falls through to the existing interactive path.
#[derive(Debug, Default)]
pub struct InitFlags {
    pub backend_type: Option<String>,
    pub name: Option<String>,
    pub mount_point: Option<String>,
    // S3 flags.
    pub endpoint: Option<String>,
    pub bucket: Option<String>,
    pub region: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    // Google Drive flags.
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    // Local backend flags.
    pub local_root: Option<String>,
    // P2P backend flags.
    pub p2p_data_dir: Option<String>,
    pub p2p_exposure: Option<String>,
    pub p2p_listen_addr: Option<String>,
    pub p2p_relay_endpoint: Option<String>,
    pub p2p_relay_secret: Option<String>,
}

use super::CliContext;

/// Top-level configuration persisted to `~/.config/cascade/config.toml`.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct CascadeConfig {
    #[serde(default)]
    pub backends: toml::Table,
    #[serde(default)]
    pub mount: MountConfig,
    #[serde(default)]
    pub p2p: P2pConfig,
    #[serde(default)]
    pub web: WebConfig,
}

/// HTTP JSON API configuration — the front door the PWA drives.
///
/// Off by default; the daemon serves the API only when `[web].enabled = true`
/// or `cascade start --web` is passed. Mirrors the CLI flags so an operator can
/// configure the bind, the advertised bundle URL, and the CORS allowlist from
/// `config.toml` without repeating them on every start.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct WebConfig {
    /// Whether to serve the HTTP API. Overridden on by `--web`.
    #[serde(default)]
    pub enabled: bool,
    /// The socket to bind. Absent means the loopback default `127.0.0.1:7842`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
    /// The hosted PWA bundle URL the daemon advertises in `/v1/bundle`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_url: Option<String>,
    /// Operator-configured CORS origins, in addition to loopback (always
    /// allowed). A wildcard `*` is refused at startup.
    #[serde(default)]
    pub cors_origins: Vec<String>,
    /// Server-side request timeout in seconds. Absent means the default 3600.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_timeout_secs: Option<u64>,
    /// Maximum request body size in bytes. Absent means the default 1 GiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_body_bytes: Option<usize>,
}

/// Mount configuration.
///
/// The `point` field is the physical filesystem path where the neutral virtual
/// root is exposed to the OS — the top-level directory the OS mount command
/// binds to. Individual backends appear as named subdirectories under it
/// (e.g. `~/Cloud/personal/`, `~/Cloud/work/`), unless a backend is explicitly
/// mounted at `"/"`, in which case it occupies the root directly.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct MountConfig {
    pub point: String,
}

/// P2P engine configuration.
///
/// The P2P layer sits between the VFS and each cloud backend as an
/// optimisation: when a file isn't in the local cache, the engine
/// checks LAN peers for the blocks first before falling back to the
/// cloud. Default off — opt-in via `[p2p] enabled = true` in
/// `config.toml`, or `--p2p` on the CLI.
///
/// The `posture`, `relay_endpoint`, and `relay_shared_secret` fields extend the
/// optimisation-layer P2P (i.e. a cloud-backed node that also shares blocks) so
/// it can express a `DiscoveryReach` posture and a WAN relay endpoint rather than
/// always running with built-in defaults. A pure-P2P backend (type = "p2p") has
/// its own per-backend TOML for these; these fields serve the case where a node
/// is backed by gdrive/s3 but also wants `posture = public` with a relay for NAT
/// traversal of the optimisation-layer P2P.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct P2pConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Discovery reach for the optimisation-layer P2P engine.
    ///
    /// Accepted values: `lan-only`, `private`, `public`. Absent means the
    /// engine default (`private`) applies. Stored as a free-form string so the
    /// config file can be round-tripped without importing the backend crate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub posture: Option<String>,
    /// `host:port` of the cascade-relay server used for WAN NAT traversal.
    ///
    /// Required when `posture = "public"` and the node is behind NAT. Absent
    /// means no relay strategy is provisioned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_endpoint: Option<String>,
    /// 64-character hex HMAC shared secret for authenticating this node to the
    /// relay server. Required when `relay_endpoint` is set. Never placed on
    /// argv — stays in the config file with mode 0600.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_shared_secret: Option<String>,
}

/// A single backend configuration entry stored in `config.toml`.
///
/// Each entry in the `[backends]` table deserialises to one of these.
/// The `mount` field controls where the backend appears in the VFS tree:
///
/// - `None` (field absent) — defaults to the backend name at startup.
/// - `Some(name)` — mounted at that name under the neutral root.
/// - `Some("/")` — mounted at the root prefix (single-backend, at-root mode).
#[derive(Debug, Serialize, Deserialize)]
pub struct BackendConfig {
    #[serde(rename = "type")]
    pub backend_type: String,
    /// Optional VFS mount path for this backend.
    ///
    /// When absent the backend's name is used as the mount path.
    /// Use `"/"` to mount a backend at the neutral root (single-backend mode).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mount: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

/// Supported backend types for the init wizard.
const BACKEND_TYPES: &[(&str, &str)] = &[
    ("gdrive", "Google Drive"),
    ("s3", "S3-compatible"),
    ("local", "Local filesystem"),
    ("p2p", "P2P content-addressed store"),
];

/// Run the init command.
///
/// When `flags.backend_type` is `Some` and all required fields are present
/// the command runs non-interactively. Otherwise the interactive wizard is
/// invoked (which reads from stdin).
pub fn run(ctx: &CliContext, flags: InitFlags) -> Result<()> {
    if flags.backend_type.is_some() {
        return run_noninteractive(ctx, &flags);
    }
    run_interactive(ctx)
}

/// Non-interactive path — validates flags and writes config without stdin.
fn run_noninteractive(ctx: &CliContext, flags: &InitFlags) -> Result<()> {
    let backend_type = flags
        .backend_type
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--backend-type is required for non-interactive mode"))?;

    if !BACKEND_TYPES.iter().any(|&(key, _)| key == backend_type) {
        let valid = BACKEND_TYPES
            .iter()
            .map(|&(key, _)| key)
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!("unsupported backend type '{backend_type}'; expected one of: {valid}");
    }

    let name = flags.name.as_deref().unwrap_or(backend_type);

    let mount_point = flags.mount_point.as_deref().map_or_else(
        || {
            dirs::home_dir()
                .map_or_else(|| PathBuf::from("/tmp/Cloud"), |h| h.join("Cloud"))
                .to_string_lossy()
                .to_string()
        },
        |mp| shellexpand::tilde(mp).to_string(),
    );

    std::fs::create_dir_all(&ctx.config_dir)?;

    write_provider_config(ctx, backend_type, name, flags)?;
    write_main_config(ctx, backend_type, name, &mount_point)?;

    // Create the mount point directory.
    std::fs::create_dir_all(&mount_point)?;

    println!("\u{2713} Backend \"{name}\" configured successfully!");
    println!(
        "\u{2713} Config written to {}",
        ctx.config_dir.join("config.toml").display()
    );
    println!("\u{2713} Mount point: {mount_point}");
    println!();
    println!("Run `cascade start` to begin.");

    Ok(())
}

/// Write the per-backend credentials TOML file.
fn write_provider_config(
    ctx: &CliContext,
    backend_type: &str,
    name: &str,
    flags: &InitFlags,
) -> Result<()> {
    let mut backend_table = toml::Table::new();
    backend_table.insert(
        "type".to_string(),
        toml::Value::String(backend_type.to_string()),
    );

    match backend_type {
        "gdrive" => {
            // `account` is the gdrive-specific identifier for token persistence.
            backend_table.insert("account".to_string(), toml::Value::String(name.to_string()));

            let client_id = flags.client_id.as_deref().ok_or_else(|| {
                anyhow::anyhow!("--client-id is required for backend type 'gdrive'")
            })?;
            if client_id.is_empty() {
                anyhow::bail!("--client-id must not be empty");
            }

            let client_secret = flags.client_secret.as_deref().ok_or_else(|| {
                anyhow::anyhow!("--client-secret is required for backend type 'gdrive'")
            })?;
            if client_secret.is_empty() {
                anyhow::bail!("--client-secret must not be empty");
            }

            backend_table.insert(
                "client_id".to_string(),
                toml::Value::String(client_id.to_string()),
            );
            backend_table.insert(
                "client_secret".to_string(),
                toml::Value::String(client_secret.to_string()),
            );
        }
        "s3" => {
            let endpoint = flags
                .endpoint
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("--endpoint is required for backend type 's3'"))?;
            if endpoint.is_empty() {
                anyhow::bail!("--endpoint must not be empty");
            }

            let bucket = flags
                .bucket
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("--bucket is required for backend type 's3'"))?;
            if bucket.is_empty() {
                anyhow::bail!("--bucket must not be empty");
            }

            let region = flags.region.as_deref().unwrap_or("us-east-1");

            let access_key_id = flags.access_key_id.as_deref().ok_or_else(|| {
                anyhow::anyhow!("--access-key-id is required for backend type 's3'")
            })?;
            if access_key_id.is_empty() {
                anyhow::bail!("--access-key-id must not be empty");
            }

            let secret_access_key = flags.secret_access_key.as_deref().ok_or_else(|| {
                anyhow::anyhow!("--secret-access-key is required for backend type 's3'")
            })?;
            if secret_access_key.is_empty() {
                anyhow::bail!("--secret-access-key must not be empty");
            }

            backend_table.insert(
                "endpoint".to_string(),
                toml::Value::String(endpoint.to_string()),
            );
            backend_table.insert(
                "bucket".to_string(),
                toml::Value::String(bucket.to_string()),
            );
            backend_table.insert(
                "region".to_string(),
                toml::Value::String(region.to_string()),
            );
            backend_table.insert(
                "access_key_id".to_string(),
                toml::Value::String(access_key_id.to_string()),
            );
            backend_table.insert(
                "secret_access_key".to_string(),
                toml::Value::String(secret_access_key.to_string()),
            );
        }
        "local" => {
            let root = flags.local_root.as_deref().ok_or_else(|| {
                anyhow::anyhow!("--local-root is required for backend type 'local'")
            })?;
            if root.is_empty() {
                anyhow::bail!("--local-root must not be empty");
            }
            backend_table.insert("root".to_string(), toml::Value::String(root.to_string()));
        }
        "p2p" => {
            // The `name` key is required by backend-p2p's open_from_config.
            backend_table.insert("name".to_string(), toml::Value::String(name.to_string()));

            if let Some(data_dir) = flags.p2p_data_dir.as_deref()
                && !data_dir.is_empty()
            {
                backend_table.insert(
                    "data_dir".to_string(),
                    toml::Value::String(data_dir.to_string()),
                );
            }

            if let Some(exposure) = flags.p2p_exposure.as_deref()
                && !exposure.is_empty()
            {
                validate_posture(exposure)?;
                backend_table.insert(
                    "exposure".to_string(),
                    toml::Value::String(exposure.to_string()),
                );
            }

            if let Some(listen_addr) = flags.p2p_listen_addr.as_deref()
                && !listen_addr.is_empty()
            {
                listen_addr.parse::<std::net::SocketAddr>().map_err(|e| {
                    anyhow::anyhow!("invalid --p2p-listen-addr `{listen_addr}`: {e}")
                })?;
                backend_table.insert(
                    "listen_addr".to_string(),
                    toml::Value::String(listen_addr.to_string()),
                );
            }

            if let Some(relay_endpoint) = flags.p2p_relay_endpoint.as_deref()
                && !relay_endpoint.is_empty()
            {
                relay_endpoint
                    .parse::<std::net::SocketAddr>()
                    .map_err(|e| {
                        anyhow::anyhow!("invalid --p2p-relay-endpoint `{relay_endpoint}`: {e}")
                    })?;
                // relay_endpoints is an array in the TOML.
                backend_table.insert(
                    "relay_endpoints".to_string(),
                    toml::Value::Array(vec![toml::Value::String(relay_endpoint.to_string())]),
                );
            }

            if let Some(relay_secret) = flags.p2p_relay_secret.as_deref()
                && !relay_secret.is_empty()
            {
                validate_relay_shared_secret_hex(relay_secret)?;
                backend_table.insert(
                    "relay_shared_secret".to_string(),
                    toml::Value::String(relay_secret.to_string()),
                );
            }
        }
        other => anyhow::bail!("unsupported backend type '{other}'"),
    }

    let backend_toml = toml::to_string_pretty(&backend_table)?;
    let backend_config_path = ctx.config_dir.join(format!("{name}.toml"));
    std::fs::write(&backend_config_path, &backend_toml)?;
    Ok(())
}

/// Validate that a posture string is one of the accepted values.
///
/// Fails loudly with a clear message rather than silently defaulting — an
/// operator who typed `publik` deserves to be told.
fn validate_posture(posture: &str) -> Result<()> {
    match posture {
        "lan-only" | "private" | "public" => Ok(()),
        other => {
            anyhow::bail!("posture must be one of `lan-only`, `private`, `public`, got `{other}`")
        }
    }
}

/// Validate that a relay shared secret is exactly 64 hex characters.
///
/// The relay authenticates writes with a 32-byte HMAC key expressed as 64 hex
/// characters. Catching a malformed value at config-write time avoids a
/// confusing runtime authentication failure.
fn validate_relay_shared_secret_hex(secret: &str) -> Result<()> {
    if secret.len() != 64 {
        anyhow::bail!(
            "relay shared secret must be exactly 64 hex characters (32 bytes), got {}",
            secret.len()
        );
    }
    if !secret.chars().all(|c| c.is_ascii_hexdigit()) {
        anyhow::bail!("relay shared secret must contain only hex characters (0-9, a-f, A-F)");
    }
    Ok(())
}

/// Write the main `config.toml` and register the backend in the state DB.
fn write_main_config(
    ctx: &CliContext,
    backend_type: &str,
    name: &str,
    mount_point: &str,
) -> Result<()> {
    let config_path = ctx.config_dir.join("config.toml");

    let backend_config = BackendConfig {
        backend_type: backend_type.to_string(),
        // Default mount is the backend name — write it explicitly so
        // the config file documents where the backend appears in the tree.
        mount: Some(name.to_string()),
        account: None,
    };

    let mut config = CascadeConfig::default();
    config
        .backends
        .insert(name.to_string(), toml::Value::try_from(&backend_config)?);
    config.mount = MountConfig {
        point: mount_point.to_string(),
    };

    let config_str = toml::to_string_pretty(&config)?;
    std::fs::write(&config_path, &config_str)?;

    // Register the backend in the state DB with its mount path.
    let db = StateDb::open(&ctx.db_path)?;
    let display_name = BACKEND_TYPES
        .iter()
        .find(|&&(key, _)| key == backend_type)
        .map_or(backend_type, |&(_, name)| name);
    db.register_backend(
        name,
        backend_type,
        &format!("{display_name} ({name})"),
        Some(name),
        None,
    )?;

    Ok(())
}

/// Run the interactive init wizard.
fn run_interactive(ctx: &CliContext) -> Result<()> {
    println!("Welcome to Cascade! Let's set up your first cloud storage backend.");
    println!();

    // Step 1: Choose provider.
    println!("Which cloud provider?");
    for (i, (_, name)) in BACKEND_TYPES.iter().enumerate() {
        println!("  {}) {}", i + 1, name);
    }
    println!();

    let choice = read_input("Enter number")?;
    let choice_idx: usize = choice
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid selection"))?;
    if choice_idx == 0 || choice_idx > BACKEND_TYPES.len() {
        anyhow::bail!("selection out of range");
    }
    let &(backend_type, backend_display_name) = BACKEND_TYPES
        .get(choice_idx - 1)
        .ok_or_else(|| anyhow::anyhow!("selection out of range"))?;
    println!();

    // Step 2: Name the backend.
    println!("Enter a name for this backend (e.g. \"personal\", \"work\"):");
    let name = read_input("Name")?;
    let name = if name.is_empty() {
        backend_type.to_string()
    } else {
        name
    };
    println!();

    // Prepare config directory.
    std::fs::create_dir_all(&ctx.config_dir)?;

    // Step 3: Provider-specific setup.
    // Writes ~/.config/cascade/{name}.toml with credentials for the chosen provider.
    match backend_type {
        "gdrive" => {
            // Collect OAuth2 client credentials.
            println!("Google Drive setup:");
            println!("  You'll need an OAuth2 client ID and secret from the Google Cloud Console.");
            println!(
                "  Create a project at https://console.cloud.google.com/ and enable the Drive API."
            );
            println!();

            let client_id = read_input("Client ID")?;
            if client_id.is_empty() {
                anyhow::bail!("client ID must not be empty");
            }

            let client_secret = read_input("Client secret")?;
            if client_secret.is_empty() {
                anyhow::bail!("client secret must not be empty");
            }

            println!();

            // Write per-backend credentials file: ~/.config/cascade/{name}.toml
            let mut backend_table = toml::Table::new();
            backend_table.insert(
                "type".to_string(),
                toml::Value::String("gdrive".to_string()),
            );
            backend_table.insert("client_id".to_string(), toml::Value::String(client_id));
            backend_table.insert(
                "client_secret".to_string(),
                toml::Value::String(client_secret),
            );
            backend_table.insert("account".to_string(), toml::Value::String(name.clone()));

            let backend_toml = toml::to_string_pretty(&backend_table)?;
            let backend_config_path = ctx.config_dir.join(format!("{name}.toml"));
            std::fs::write(&backend_config_path, &backend_toml)?;

            println!(
                "\u{2713} Credentials written to {}",
                backend_config_path.display()
            );
            println!("Run `cascade backend auth {name}` to complete OAuth setup.");
            println!();
        }
        "s3" => {
            // S3-compatible backend: collect credentials interactively.
            println!("S3 configuration:");
            println!();

            let endpoint = read_input("Endpoint URL (e.g. https://s3.amazonaws.com)")?;
            if endpoint.is_empty() {
                anyhow::bail!("endpoint URL must not be empty");
            }

            let bucket = read_input("Bucket name")?;
            if bucket.is_empty() {
                anyhow::bail!("bucket name must not be empty");
            }

            let region_input = read_input("Region (e.g. us-east-1)")?;
            let region = if region_input.is_empty() {
                "us-east-1".to_string()
            } else {
                region_input
            };

            let access_key_id = read_input("Access key ID")?;
            if access_key_id.is_empty() {
                anyhow::bail!("access key ID must not be empty");
            }

            let secret_access_key = read_input("Secret access key")?;
            if secret_access_key.is_empty() {
                anyhow::bail!("secret access key must not be empty");
            }

            println!();

            // Write per-backend credentials file: ~/.config/cascade/{name}.toml
            let mut backend_table = toml::Table::new();
            backend_table.insert("type".to_string(), toml::Value::String("s3".to_string()));
            backend_table.insert("endpoint".to_string(), toml::Value::String(endpoint));
            backend_table.insert("bucket".to_string(), toml::Value::String(bucket));
            backend_table.insert("region".to_string(), toml::Value::String(region));
            backend_table.insert(
                "access_key_id".to_string(),
                toml::Value::String(access_key_id),
            );
            backend_table.insert(
                "secret_access_key".to_string(),
                toml::Value::String(secret_access_key),
            );

            let backend_toml = toml::to_string_pretty(&backend_table)?;
            let backend_config_path = ctx.config_dir.join(format!("{name}.toml"));
            std::fs::write(&backend_config_path, &backend_toml)?;
        }
        "local" => {
            println!("Local filesystem backend:");
            println!("  Adopts an existing directory and syncs its contents into Cascade.");
            println!();

            let root = read_input("Root directory path")?;
            if root.is_empty() {
                anyhow::bail!("root directory must not be empty");
            }
            let root = shellexpand::tilde(&root).to_string();

            let mut backend_table = toml::Table::new();
            backend_table.insert("type".to_string(), toml::Value::String("local".to_string()));
            backend_table.insert("root".to_string(), toml::Value::String(root));

            let backend_toml = toml::to_string_pretty(&backend_table)?;
            let backend_config_path = ctx.config_dir.join(format!("{name}.toml"));
            std::fs::write(&backend_config_path, &backend_toml)?;
        }
        "p2p" => {
            println!("P2P content-addressed store:");
            println!("  Stores files as content-addressed blocks shared with trusted peers.");
            println!();

            let listen_addr_input =
                read_input("BEP listener address (e.g. 0.0.0.0:22000, or empty for any port)")?;

            let mut backend_table = toml::Table::new();
            backend_table.insert("type".to_string(), toml::Value::String("p2p".to_string()));
            // The `name` key is required by backend-p2p's open_from_config.
            backend_table.insert("name".to_string(), toml::Value::String(name.clone()));

            if !listen_addr_input.is_empty() {
                listen_addr_input
                    .parse::<std::net::SocketAddr>()
                    .map_err(|e| {
                        anyhow::anyhow!("invalid BEP listen address `{listen_addr_input}`: {e}")
                    })?;
                backend_table.insert(
                    "listen_addr".to_string(),
                    toml::Value::String(listen_addr_input),
                );
            }

            let backend_toml = toml::to_string_pretty(&backend_table)?;
            let backend_config_path = ctx.config_dir.join(format!("{name}.toml"));
            std::fs::write(&backend_config_path, &backend_toml)?;
        }
        other => anyhow::bail!("unsupported backend type '{other}'"),
    }

    // Step 4: Mount point.
    let default_mount = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("Cloud");
    let default_mount_str = default_mount.to_string_lossy().to_string();
    println!("Mount point [{default_mount_str}]:");
    let mount_input = read_input("Mount point")?;
    let mount_point = if mount_input.is_empty() {
        default_mount_str
    } else {
        shellexpand::tilde(&mount_input).to_string()
    };

    // Step 5: Write config.
    let config_path = ctx.config_dir.join("config.toml");

    // Build TOML config.
    // The `account` field in the main config.toml is not used for routing —
    // it was a legacy field kept for backwards compatibility. The per-backend
    // TOML carries the actual credential configuration.
    let backend_config = BackendConfig {
        backend_type: backend_type.to_string(),
        // Default mount is the backend name — write it explicitly so
        // the config file documents where the backend appears in the tree.
        mount: Some(name.clone()),
        account: None,
    };

    let mut config = CascadeConfig::default();
    config
        .backends
        .insert(name.clone(), toml::Value::try_from(&backend_config)?);
    config.mount = MountConfig {
        point: mount_point.clone(),
    };

    let config_str = toml::to_string_pretty(&config)?;
    std::fs::write(&config_path, &config_str)?;

    // Register the backend in the state DB so `cascade status` and
    // `cascade backend-list` can discover it.
    let db_path = ctx.db_path.clone();
    let db = StateDb::open(&db_path)?;
    db.register_backend(
        &name,
        backend_type,
        &format!("{backend_display_name} ({name})"),
        Some(&name),
        None,
    )?;

    // Create the mount point directory.
    std::fs::create_dir_all(&mount_point)?;

    println!();
    println!("\u{2713} Backend \"{name}\" configured successfully!");
    println!("\u{2713} Config written to {}", config_path.display());
    println!("\u{2713} Mount point: {mount_point}");
    println!();
    println!("Run `cascade start` to begin.");

    Ok(())
}

/// Read a line from stdin, trimming whitespace.
fn read_input(prompt: &str) -> Result<String> {
    print!("{prompt}: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn cascade_config_serialises_to_toml() {
        let mut config = CascadeConfig::default();
        let backend = BackendConfig {
            backend_type: "gdrive".to_string(),
            mount: Some("personal".to_string()),
            account: Some("personal".to_string()),
        };
        config
            .backends
            .insert("personal".into(), toml::Value::try_from(&backend).unwrap());
        config.mount = MountConfig {
            point: "/Users/joe/Cloud".to_string(),
        };

        let toml_str = toml::to_string_pretty(&config).unwrap();
        assert!(toml_str.contains("[backends.personal]"));
        assert!(toml_str.contains("type = \"gdrive\""));
        assert!(toml_str.contains("mount = \"personal\""));
        assert!(toml_str.contains("[mount]"));
        assert!(toml_str.contains("point = \"/Users/joe/Cloud\""));
    }

    #[test]
    fn cascade_config_round_trip() {
        let mut config = CascadeConfig::default();
        let backend = BackendConfig {
            backend_type: "gdrive".to_string(),
            mount: Some("personal".to_string()),
            account: Some("personal".to_string()),
        };
        config
            .backends
            .insert("personal".into(), toml::Value::try_from(&backend).unwrap());
        config.mount = MountConfig {
            point: "/home/user/Cloud".to_string(),
        };

        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: CascadeConfig = toml::from_str(&toml_str).unwrap();

        assert_eq!(parsed.mount.point, "/home/user/Cloud");
        assert!(parsed.backends.contains_key("personal"));
        // Verify the mount round-trips.
        let parsed_backend: BackendConfig = parsed
            .backends
            .get("personal")
            .unwrap()
            .clone()
            .try_into()
            .unwrap();
        assert_eq!(parsed_backend.mount.as_deref(), Some("personal"));
    }

    #[test]
    fn backend_config_without_account_or_mount() {
        let backend = BackendConfig {
            backend_type: "local".to_string(),
            mount: None,
            account: None,
        };
        let toml_str = toml::to_string_pretty(&backend).unwrap();
        assert!(toml_str.contains("type = \"local\""));
        assert!(!toml_str.contains("account"));
        assert!(!toml_str.contains("mount"));
    }

    #[test]
    fn backend_config_with_mount_serialises() {
        let backend = BackendConfig {
            backend_type: "local".to_string(),
            mount: Some("files".to_string()),
            account: None,
        };
        let toml_str = toml::to_string_pretty(&backend).unwrap();
        assert!(toml_str.contains("type = \"local\""));
        assert!(toml_str.contains("mount = \"files\""));
        assert!(!toml_str.contains("account"));
    }

    #[test]
    fn backend_config_root_mount_serialises() {
        // "/" mount means "at-root mode" — single-backend path shape.
        let backend = BackendConfig {
            backend_type: "gdrive".to_string(),
            mount: Some("/".to_string()),
            account: None,
        };
        let toml_str = toml::to_string_pretty(&backend).unwrap();
        assert!(toml_str.contains("mount = \"/\""));
    }

    #[test]
    fn empty_config_round_trip() {
        let config = CascadeConfig::default();
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: CascadeConfig = toml::from_str(&toml_str).unwrap();
        assert!(parsed.backends.is_empty());
        assert!(parsed.mount.point.is_empty());
    }

    // -- Non-interactive init tests ------------------------------------------

    fn make_context(dir: &std::path::Path) -> CliContext {
        CliContext {
            config_dir: dir.to_path_buf(),
            db_path: dir.join("state.db"),
            pid_path: dir.join("cascade.pid"),
        }
    }

    #[test]
    fn noninteractive_s3_writes_config_files() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = make_context(dir.path());
        let mount = dir.path().join("mnt");

        let flags = InitFlags {
            backend_type: Some("s3".to_string()),
            name: Some("mybucket".to_string()),
            mount_point: Some(mount.to_string_lossy().to_string()),
            endpoint: Some("https://s3.example.com".to_string()),
            bucket: Some("test-bucket".to_string()),
            region: Some("eu-west-1".to_string()),
            access_key_id: Some("AKID".to_string()),
            secret_access_key: Some("SECRET".to_string()),
            ..Default::default()
        };

        run(&ctx, flags).unwrap();

        // Per-backend credentials file.
        let backend_toml = std::fs::read_to_string(dir.path().join("mybucket.toml")).unwrap();
        assert!(backend_toml.contains("type = \"s3\""));
        assert!(backend_toml.contains("endpoint = \"https://s3.example.com\""));
        assert!(backend_toml.contains("bucket = \"test-bucket\""));
        assert!(backend_toml.contains("region = \"eu-west-1\""));
        assert!(backend_toml.contains("access_key_id = \"AKID\""));
        assert!(backend_toml.contains("secret_access_key = \"SECRET\""));

        // Main config.
        let config_toml = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(config_toml.contains("[backends.mybucket]"));
        assert!(config_toml.contains("type = \"s3\""));
        // The init command writes the mount path defaulting to the backend name.
        assert!(config_toml.contains("mount = \"mybucket\""));
        assert!(config_toml.contains(&*mount.to_string_lossy()));

        // Mount point directory was created.
        assert!(mount.is_dir());
    }

    #[test]
    fn noninteractive_gdrive_writes_config_files() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = make_context(dir.path());
        let mount = dir.path().join("cloud");

        let flags = InitFlags {
            backend_type: Some("gdrive".to_string()),
            name: Some("personal".to_string()),
            mount_point: Some(mount.to_string_lossy().to_string()),
            client_id: Some("cid-123".to_string()),
            client_secret: Some("csec-456".to_string()),
            ..Default::default()
        };

        run(&ctx, flags).unwrap();

        // Per-backend credentials file.
        let backend_toml = std::fs::read_to_string(dir.path().join("personal.toml")).unwrap();
        assert!(backend_toml.contains("type = \"gdrive\""));
        assert!(backend_toml.contains("client_id = \"cid-123\""));
        assert!(backend_toml.contains("client_secret = \"csec-456\""));

        // Main config.
        let config_toml = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(config_toml.contains("[backends.personal]"));
        assert!(config_toml.contains("type = \"gdrive\""));
        // The init command writes the mount path defaulting to the backend name.
        assert!(config_toml.contains("mount = \"personal\""));
        assert!(config_toml.contains(&*mount.to_string_lossy()));

        // Mount point directory was created.
        assert!(mount.is_dir());
    }

    #[test]
    fn noninteractive_s3_missing_required_field_errors() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = make_context(dir.path());

        // Provide backend_type=s3 but omit --bucket.
        let flags = InitFlags {
            backend_type: Some("s3".to_string()),
            endpoint: Some("https://s3.example.com".to_string()),
            access_key_id: Some("AKID".to_string()),
            secret_access_key: Some("SECRET".to_string()),
            ..Default::default()
        };

        let err = run(&ctx, flags).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("--bucket is required"), "error was: {msg}");
    }

    #[test]
    fn noninteractive_gdrive_missing_required_field_errors() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = make_context(dir.path());

        // Provide backend_type=gdrive but omit --client-secret.
        let flags = InitFlags {
            backend_type: Some("gdrive".to_string()),
            client_id: Some("cid".to_string()),
            ..Default::default()
        };

        let err = run(&ctx, flags).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("--client-secret is required"),
            "error was: {msg}"
        );
    }

    #[test]
    fn noninteractive_unsupported_backend_type_errors() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = make_context(dir.path());

        let flags = InitFlags {
            backend_type: Some("ftp".to_string()),
            ..Default::default()
        };

        let err = run(&ctx, flags).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unsupported backend type"), "error was: {msg}");
    }

    #[test]
    fn noninteractive_s3_defaults_name_and_region() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = make_context(dir.path());

        let flags = InitFlags {
            backend_type: Some("s3".to_string()),
            // No --name: should default to "s3"
            // No --region: should default to "us-east-1"
            mount_point: Some(dir.path().join("mnt").to_string_lossy().to_string()),
            endpoint: Some("https://s3.example.com".to_string()),
            bucket: Some("test-bucket".to_string()),
            access_key_id: Some("AKID".to_string()),
            secret_access_key: Some("SECRET".to_string()),
            ..Default::default()
        };

        run(&ctx, flags).unwrap();

        // Name defaults to backend_type → backend config is "s3.toml".
        let backend_toml = std::fs::read_to_string(dir.path().join("s3.toml")).unwrap();
        assert!(backend_toml.contains("region = \"us-east-1\""));

        // Main config references "s3" as the backend name, with mount defaulting to name.
        let config_toml = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(config_toml.contains("[backends.s3]"));
        assert!(config_toml.contains("mount = \"s3\""));
    }

    // -- P2pConfig serialisation tests ---------------------------------------

    #[test]
    fn p2p_config_defaults_are_absent_in_toml() {
        let config = CascadeConfig::default();
        let toml_str = toml::to_string_pretty(&config).unwrap();
        // None fields should be omitted entirely.
        assert!(!toml_str.contains("posture"));
        assert!(!toml_str.contains("relay_endpoint"));
        assert!(!toml_str.contains("relay_shared_secret"));
    }

    #[test]
    fn p2p_config_posture_round_trips() {
        let toml_str = r#"
[p2p]
enabled = true
posture = "public"
relay_endpoint = "10.0.0.1:22067"
relay_shared_secret = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
"#;
        let config: CascadeConfig = toml::from_str(toml_str).unwrap();
        assert!(config.p2p.enabled);
        assert_eq!(config.p2p.posture.as_deref(), Some("public"));
        assert_eq!(config.p2p.relay_endpoint.as_deref(), Some("10.0.0.1:22067"));
        assert!(config.p2p.relay_shared_secret.is_some());
    }

    #[test]
    fn p2p_config_absent_posture_is_none() {
        let toml_str = "[p2p]\nenabled = true\n";
        let config: CascadeConfig = toml::from_str(toml_str).unwrap();
        assert!(config.p2p.enabled);
        assert!(config.p2p.posture.is_none());
        assert!(config.p2p.relay_endpoint.is_none());
        assert!(config.p2p.relay_shared_secret.is_none());
    }

    // -- Non-interactive local backend tests ---------------------------------

    #[test]
    fn noninteractive_local_writes_config_files() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = make_context(dir.path());
        let mount = dir.path().join("mnt");
        let root = dir.path().join("data");

        let flags = InitFlags {
            backend_type: Some("local".to_string()),
            name: Some("mylocal".to_string()),
            mount_point: Some(mount.to_string_lossy().to_string()),
            local_root: Some(root.to_string_lossy().to_string()),
            ..Default::default()
        };

        run(&ctx, flags).unwrap();

        let backend_toml = std::fs::read_to_string(dir.path().join("mylocal.toml")).unwrap();
        assert!(backend_toml.contains("type = \"local\""));
        assert!(backend_toml.contains("root = "));

        let config_toml = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(config_toml.contains("[backends.mylocal]"));
        assert!(config_toml.contains("type = \"local\""));
        // The init command writes the mount path defaulting to the backend name.
        assert!(config_toml.contains("mount = \"mylocal\""));

        // The local TOML should NOT contain an `account` key.
        assert!(!backend_toml.contains("account"));
    }

    #[test]
    fn noninteractive_local_missing_root_errors() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = make_context(dir.path());

        let flags = InitFlags {
            backend_type: Some("local".to_string()),
            ..Default::default()
        };

        let err = run(&ctx, flags).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("--local-root is required"), "error was: {msg}");
    }

    // -- Non-interactive P2P backend tests -----------------------------------

    #[test]
    fn noninteractive_p2p_writes_config_with_required_name_key() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = make_context(dir.path());
        let mount = dir.path().join("mnt");

        let flags = InitFlags {
            backend_type: Some("p2p".to_string()),
            name: Some("myp2p".to_string()),
            mount_point: Some(mount.to_string_lossy().to_string()),
            ..Default::default()
        };

        run(&ctx, flags).unwrap();

        let backend_toml = std::fs::read_to_string(dir.path().join("myp2p.toml")).unwrap();
        assert!(backend_toml.contains("type = \"p2p\""));
        // The `name` key is required by backend-p2p's open_from_config.
        assert!(backend_toml.contains("name = \"myp2p\""));
        // No account key for p2p backends.
        assert!(!backend_toml.contains("account"));

        let config_toml = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(config_toml.contains("[backends.myp2p]"));
        assert!(config_toml.contains("type = \"p2p\""));
        // The init command writes the mount path defaulting to the backend name.
        assert!(config_toml.contains("mount = \"myp2p\""));
    }

    #[test]
    fn noninteractive_p2p_writes_exposure_and_relay() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = make_context(dir.path());
        let mount = dir.path().join("mnt");
        let secret = "a".repeat(64);

        let flags = InitFlags {
            backend_type: Some("p2p".to_string()),
            name: Some("seeder".to_string()),
            mount_point: Some(mount.to_string_lossy().to_string()),
            p2p_exposure: Some("public".to_string()),
            p2p_listen_addr: Some("0.0.0.0:22000".to_string()),
            p2p_relay_endpoint: Some("1.2.3.4:22067".to_string()),
            p2p_relay_secret: Some(secret.clone()),
            ..Default::default()
        };

        run(&ctx, flags).unwrap();

        let backend_toml = std::fs::read_to_string(dir.path().join("seeder.toml")).unwrap();
        assert!(backend_toml.contains("exposure = \"public\""));
        assert!(backend_toml.contains("listen_addr = \"0.0.0.0:22000\""));
        assert!(backend_toml.contains("relay_endpoints = [\"1.2.3.4:22067\"]"));
        assert!(backend_toml.contains(&format!("relay_shared_secret = \"{secret}\"")));
    }

    #[test]
    fn noninteractive_p2p_rejects_invalid_posture() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = make_context(dir.path());

        let flags = InitFlags {
            backend_type: Some("p2p".to_string()),
            p2p_exposure: Some("publik".to_string()),
            ..Default::default()
        };

        let err = run(&ctx, flags).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("posture must be one of"), "error was: {msg}");
    }

    #[test]
    fn noninteractive_p2p_rejects_invalid_listen_addr() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = make_context(dir.path());

        let flags = InitFlags {
            backend_type: Some("p2p".to_string()),
            p2p_listen_addr: Some("not-a-socket-addr".to_string()),
            ..Default::default()
        };

        let err = run(&ctx, flags).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("invalid --p2p-listen-addr"),
            "error was: {msg}"
        );
    }

    #[test]
    fn noninteractive_p2p_rejects_short_relay_secret() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = make_context(dir.path());

        let flags = InitFlags {
            backend_type: Some("p2p".to_string()),
            p2p_relay_secret: Some("tooshort".to_string()),
            ..Default::default()
        };

        let err = run(&ctx, flags).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("relay shared secret must be exactly 64"),
            "error was: {msg}"
        );
    }
}
