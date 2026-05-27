//! TOML `.cascade.toml` parser.

use crate::types::CascadeConfig;

/// Parse a TOML-formatted `.cascade.toml` file.
pub fn parse(content: &str) -> anyhow::Result<CascadeConfig> {
    Ok(toml::from_str(content)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_toml_ignores() {
        let input = r#"
[[ignore]]
pattern = "*.log"

[[ignore]]
pattern = "build/"
dir_only = true

[[pin]]
path = "Documents/**"
"#;
        let config = parse(input).unwrap();
        assert_eq!(config.ignore.len(), 2);
        assert_eq!(config.ignore[0].pattern, "*.log");
        assert!(config.ignore[1].dir_only);
        assert_eq!(config.pin.len(), 1);
        assert_eq!(config.pin[0].path, "Documents/**");
    }

    #[test]
    fn parse_toml_cache_config() {
        let input = r#"
[cache]
max_size = "5GB"
max_age = "7d"
"#;
        let config = parse(input).unwrap();
        let cache = config.cache.unwrap();
        assert_eq!(cache.max_size, Some("5GB".to_string()));
        assert_eq!(cache.max_age, Some("7d".to_string()));
    }

    #[test]
    fn parse_empty_toml() {
        let config = parse("").unwrap();
        assert!(config.ignore.is_empty());
    }
}
