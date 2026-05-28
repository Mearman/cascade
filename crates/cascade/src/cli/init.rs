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
    pub endpoint: Option<String>,
    pub bucket: Option<String>,
    pub region: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
}

use super::CliContext;

/// Top-level configuration persisted to `~/.config/cascade/config.toml`.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct CascadeConfig {
    #[serde(default)]
    pub backends: toml::Table,
    #[serde(default)]
    pub mount: MountConfig,
}

/// Mount configuration.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct MountConfig {
    pub point: String,
}

/// A single backend configuration entry.
#[derive(Debug, Serialize, Deserialize)]
pub struct BackendConfig {
    #[serde(rename = "type")]
    pub backend_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

/// Supported backend types for the init wizard.
const BACKEND_TYPES: &[(&str, &str)] = &[("gdrive", "Google Drive"), ("s3", "S3-compatible")];

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
    backend_table.insert("account".to_string(), toml::Value::String(name.to_string()));

    match backend_type {
        "gdrive" => {
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
        other => anyhow::bail!("unsupported backend type '{other}'"),
    }

    let backend_toml = toml::to_string_pretty(&backend_table)?;
    let backend_config_path = ctx.config_dir.join(format!("{name}.toml"));
    std::fs::write(&backend_config_path, &backend_toml)?;
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

    // Register the backend in the state DB.
    let db = StateDb::open(&ctx.db_path)?;
    let display_name = BACKEND_TYPES
        .iter()
        .find(|&&(key, _)| key == backend_type)
        .map_or(backend_type, |&(_, name)| name);
    db.register_backend(
        name,
        backend_type,
        &format!("{display_name} ({name})"),
        None,
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
    if backend_type == "gdrive" {
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
    } else {
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
    let account: Option<String> = None;

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
    let backend_config = BackendConfig {
        backend_type: backend_type.to_string(),
        account,
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
        None,
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
        assert!(toml_str.contains("[mount]"));
        assert!(toml_str.contains("point = \"/Users/joe/Cloud\""));
    }

    #[test]
    fn cascade_config_round_trip() {
        let mut config = CascadeConfig::default();
        let backend = BackendConfig {
            backend_type: "gdrive".to_string(),
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
    }

    #[test]
    fn backend_config_without_account() {
        let backend = BackendConfig {
            backend_type: "local".to_string(),
            account: None,
        };
        let toml_str = toml::to_string_pretty(&backend).unwrap();
        assert!(toml_str.contains("type = \"local\""));
        assert!(!toml_str.contains("account"));
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

        // Main config references "s3" as the backend name.
        let config_toml = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(config_toml.contains("[backends.s3]"));
    }
}
