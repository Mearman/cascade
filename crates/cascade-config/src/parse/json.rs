//! JSON `.cascade.json` parser.

use crate::types::CascadeConfig;

/// Parse a JSON-formatted `.cascade.json` file.
pub fn parse(content: &str) -> anyhow::Result<CascadeConfig> {
    Ok(serde_json::from_str(content)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_json_ignores() {
        let input = r#"{
            "ignore": [
                {"pattern": "*.log"},
                {"pattern": "build/", "dir_only": true}
            ],
            "pin": [
                {"path": "Documents/**"}
            ]
        }"#;
        let config = parse(input).unwrap();
        assert_eq!(config.ignore.len(), 2);
        assert_eq!(config.pin.len(), 1);
    }

    #[test]
    fn parse_empty_json_object() {
        let config = parse("{}").unwrap();
        assert!(config.ignore.is_empty());
    }
}
