//! Gitignore-style `.cascade` parser.
//!
//! Lines starting with `#` are comments. `!` negates an ignore.
//! Directives start with `:` for cache, lifecycle, pin, unpin, p2p.
//! Conditional blocks: `:[<expr>]` opens, `:[end]` closes.

use crate::types::{CascadeConfig, IgnoreRule};

/// Parse a gitignore-style `.cascade` file.
#[must_use]
pub fn parse(content: &str) -> CascadeConfig {
    let mut config = CascadeConfig::empty();
    let condition_stack: Vec<String> = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Conditional block open: :[<expr>]
        if let Some(expr) = trimmed.strip_prefix(":[").and_then(|s| s.strip_suffix(']')) {
            let _ = expr.trim(); // Would push to condition_stack in full impl
            continue;
        }

        // Conditional block close
        if trimmed == ":[end]" {
            continue;
        }

        // Directive
        if let Some(directive) = trimmed.strip_prefix(':') {
            parse_directive(directive.trim(), &condition_stack, &mut config);
            continue;
        }

        // Ignore pattern (gitignore syntax)
        let (negated, pattern) = trimmed
            .strip_prefix('!')
            .map_or((false, trimmed), |p| (true, p));

        let dir_only = pattern.ends_with('/');

        config.ignore.push(IgnoreRule {
            pattern: pattern.to_string(),
            negated,
            dir_only,
            conditions: condition_stack.clone(),
        });
    }

    config
}

/// Parse a directive line (after the `:` prefix).
const fn parse_directive(directive: &str, _conditions: &[String], _config: &mut CascadeConfig) {
    // Phase 1 only supports ignore rules.
    // Directives like `:cache`, `:lifecycle`, `:pin`, `:unpin`, `:p2p`
    // will be implemented in Phase 2+.
    let _ = directive;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_ignores() {
        let input = "\
# Build output
target/
*.o
*.d

# Editor files
*.swp
";
        let config = parse(input);
        assert_eq!(config.ignore.len(), 4);

        assert_eq!(config.ignore[0].pattern, "target/");
        assert!(config.ignore[0].dir_only);
        assert!(!config.ignore[0].negated);

        assert_eq!(config.ignore[1].pattern, "*.o");
        assert!(!config.ignore[1].dir_only);

        assert_eq!(config.ignore[2].pattern, "*.d");
        assert_eq!(config.ignore[3].pattern, "*.swp");
    }

    #[test]
    fn parse_negated_ignore() {
        let input = "\
*.log
!important.log
";
        let config = parse(input);
        assert_eq!(config.ignore.len(), 2);
        assert!(!config.ignore[0].negated);
        assert!(config.ignore[1].negated);
        assert_eq!(config.ignore[1].pattern, "important.log");
    }

    #[test]
    fn parse_empty_input() {
        let config = parse("");
        assert!(config.ignore.is_empty());
    }

    #[test]
    fn parse_comments_and_blanks() {
        let input = "\
# Comment 1

# Comment 2

";
        let config = parse(input);
        assert!(config.ignore.is_empty());
    }
}
