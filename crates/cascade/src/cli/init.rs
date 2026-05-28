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
#[derive(Debug, Serialize, Deserialize)]
pub struct MountConfig {
    pub point: String,
}

impl Default for MountConfig {
    fn default() -> Self {
        Self {
            point: String::new(),
        }
    }
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
const BACKEND_TYPES: &[(&str, &str)] = &[
    ("gdrive", "Google Drive"),
    ("local", "Local filesystem"),
];

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
    let (backend_type, _backend_display_name) = BACKEND_TYPES[choice_idx - 1];
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

    // Step 3: Provider-specific setup.
    let mut account = None;
    if backend_type == "gdrive" {
        println!("Google Drive authentication required.");
        println!("Run `cascade backend auth {name}` after init to complete OAuth setup.");
        println!();
        account = Some(name.clone());
    }

    // Step 4: Mount point.
    let default_mount = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("Cloud");
    let default_mount_str = default_mount.to_string_lossy().to_string();
    println!("Mount point [{}]:", default_mount_str);
    let mount_input = read_input("Mount point")?;
    let mount_point = if mount_input.is_empty() {
        default_mount_str
    } else {
        let expanded = shellexpand::tilde(&mount_input).to_string();
        expanded
    };

    // Step 5: Write config.
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".cascade"))
        .join("cascade");
    std::fs::create_dir_all(&config_dir)?;

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
