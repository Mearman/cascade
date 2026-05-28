//! Integration tests for cascade init config file handling.

use serde::{Deserialize, Serialize};

/// Configuration structures duplicated from init.rs for test isolation.
/// These mirror the CascadeConfig, MountConfig, and BackendConfig types.

#[derive(Debug, Serialize, Deserialize, Default)]
struct CascadeConfig {
    #[serde(default)]
    backends: toml::Table,
    #[serde(default)]
    mount: MountConfig,
}

#[derive(Debug, Serialize, Deserialize)]
struct MountConfig {
    point: String,
}

impl Default for MountConfig {
    fn default() -> Self {
        Self {
            point: String::new(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct BackendConfig {
    #[serde(rename = "type")]
    backend_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<String>,
}

#[test]
fn config_file_generation_gdrive() {
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

    // Verify structure.
    assert!(toml_str.contains("[backends.personal]"));
    assert!(toml_str.contains("type = \"gdrive\""));
    assert!(toml_str.contains("account = \"personal\""));
    assert!(toml_str.contains("[mount]"));
    assert!(toml_str.contains("point = \"/home/user/Cloud\""));
}

#[test]
fn config_file_parsing() {
    let toml_str = r#"
[backends.personal]
type = "gdrive"
account = "personal"

[mount]
point = "/home/user/Cloud"
"#;

    let config: CascadeConfig = toml::from_str(toml_str).unwrap();

    assert!(config.backends.contains_key("personal"));
    assert_eq!(config.mount.point, "/home/user/Cloud");

    // Verify backend config details.
    let backend_value = config.backends.get("personal").unwrap();
    let table = backend_value.as_table().unwrap();
    assert_eq!(table.get("type").unwrap().as_str(), Some("gdrive"));
    assert_eq!(table.get("account").unwrap().as_str(), Some("personal"));
}

#[test]
fn config_file_round_trip() {
    let mut config = CascadeConfig::default();

    let gdrive = BackendConfig {
        backend_type: "gdrive".to_string(),
        account: Some("work".to_string()),
    };
    config
        .backends
        .insert("work".into(), toml::Value::try_from(&gdrive).unwrap());

    let local = BackendConfig {
        backend_type: "local".to_string(),
        account: None,
    };
    config
        .backends
        .insert("local".into(), toml::Value::try_from(&local).unwrap());

    config.mount = MountConfig {
        point: "/mnt/cascade".to_string(),
    };

    let toml_str = toml::to_string_pretty(&config).unwrap();
    let parsed: CascadeConfig = toml::from_str(&toml_str).unwrap();

    assert_eq!(parsed.mount.point, "/mnt/cascade");
    assert_eq!(parsed.backends.len(), 2);
    assert!(parsed.backends.contains_key("work"));
    assert!(parsed.backends.contains_key("local"));
}

#[test]
fn backend_config_serialisation_local() {
    let backend = BackendConfig {
        backend_type: "local".to_string(),
        account: None,
    };

    let toml_str = toml::to_string_pretty(&backend).unwrap();
    assert!(toml_str.contains("type = \"local\""));
    // account should be omitted when None.
    assert!(!toml_str.contains("account"));
}

#[test]
fn config_with_multiple_backends() {
    let toml_str = r#"
[backends.personal]
type = "gdrive"
account = "personal"

[backends.work]
type = "gdrive"
account = "work"

[backends.archive]
type = "s3"

[mount]
point = "/Users/joe/Cloud"
"#;

    let config: CascadeConfig = toml::from_str(toml_str).unwrap();

    assert_eq!(config.backends.len(), 3);
    assert!(config.backends.contains_key("personal"));
    assert!(config.backends.contains_key("work"));
    assert!(config.backends.contains_key("archive"));
    assert_eq!(config.mount.point, "/Users/joe/Cloud");
}
