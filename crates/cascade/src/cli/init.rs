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
use serde::{Deserialize, Serialize};

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

/// Run the interactive init wizard.
pub fn run() -> Result<()> {
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
    let &(backend_type, _backend_display_name) = BACKEND_TYPES
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

    // Prepare config directory early — needed by both provider branches.
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".cascade"))
        .join("cascade");
    std::fs::create_dir_all(&config_dir)?;

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
        let backend_config_path = config_dir.join(format!("{name}.toml"));
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
        let backend_config_path = config_dir.join(format!("{name}.toml"));
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
    let config_path = config_dir.join("config.toml");

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
}
