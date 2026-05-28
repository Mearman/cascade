//! Parsers for the four `.cascade` formats.

pub mod gitignore;
pub mod json;
pub mod toml;
pub mod yaml;

use crate::types::CascadeConfig;

/// Trait for loading config from a directory.
/// Production implementations walk the filesystem; tests use in-memory maps.
pub trait ConfigLoader {
    fn load(&self, dir: &std::path::Path) -> Option<CascadeConfig>;
}

/// Parse all `.cascade*` files in a directory, merging in deterministic order:
/// gitignore-style → TOML → YAML → JSON.
pub fn load_dir(dir: &std::path::Path) -> Option<CascadeConfig> {
    let mut result: Option<CascadeConfig> = None;
    let mut merge = |config: CascadeConfig| {
        if let Some(existing) = result.as_mut() {
            existing.merge(config);
        } else {
            result = Some(config);
        }
    };

    // gitignore-style (no extension)
    let gitignore_path = dir.join(".cascade");
    if gitignore_path.exists()
        && let Ok(content) = std::fs::read_to_string(&gitignore_path)
    {
        merge(gitignore::parse(&content));
    }

    // TOML
    let toml_path = dir.join(".cascade.toml");
    if toml_path.exists()
        && let Ok(content) = std::fs::read_to_string(&toml_path)
        && let Ok(config) = toml::parse(&content)
    {
        merge(config);
    }

    // YAML
    let yaml_path = dir.join(".cascade.yaml");
    if yaml_path.exists()
        && let Ok(content) = std::fs::read_to_string(&yaml_path)
        && let Ok(config) = yaml::parse(&content)
    {
        merge(config);
    }

    // JSON
    let json_path = dir.join(".cascade.json");
    if json_path.exists()
        && let Ok(content) = std::fs::read_to_string(&json_path)
        && let Ok(config) = json::parse(&content)
    {
        merge(config);
    }

    result
}
