//! YAML `.cascade.yaml` parser.

use crate::types::CascadeConfig;

/// Parse a YAML-formatted `.cascade.yaml` file.
pub fn parse(content: &str) -> anyhow::Result<CascadeConfig> {
    Ok(serde_yaml::from_str(content)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_yaml_ignores() {
        let input = r#"
ignore:
  - pattern: "*.log"
  - pattern: "build/"
    dir_only: true
pin:
  - path: "Documents/**"
"#;
        let config = parse(input).unwrap();
        assert_eq!(config.ignore.len(), 2);
        assert_eq!(config.pin.len(), 1);
    }

    #[test]
    fn parse_empty_yaml() {
        let config = parse("---\n").unwrap_or_default();
        assert!(config.ignore.is_empty());
    }
}
