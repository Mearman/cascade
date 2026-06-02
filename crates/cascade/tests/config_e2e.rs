#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::string_slice
)]
//! Integration tests: .cascade config parse → resolved → ignore filtering.
//!
//! Tests the full config pipeline: write temp files in multiple formats,
//! resolve via directory walk, and verify ignore rules are accumulated
//! and applied correctly.

use cascade_config::merge;
use cascade_config::parse;
use cascade_config::{MaxAge, MaxSize};
use std::fs;
use tempfile::TempDir;

#[test]
fn gitignore_format_accumulates_rules() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join(".cascade"),
        "*.log\n*.tmp\n!important.log\n",
    )
    .unwrap();

    let config = parse::load_dir(dir.path()).expect("should parse gitignore");
    assert_eq!(config.ignore.len(), 3);
    assert_eq!(config.ignore[0].pattern, "*.log");
    assert!(!config.ignore[0].negated);
    assert_eq!(config.ignore[1].pattern, "*.tmp");
    assert!(!config.ignore[1].negated);
    assert_eq!(config.ignore[2].pattern, "important.log");
    assert!(config.ignore[2].negated);
}

#[test]
fn toml_format_parses_all_sections() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join(".cascade.toml"),
        r#"
[[ignore]]
pattern = "*.log"

[[ignore]]
pattern = "build/"

[cache]
max_size = "5GB"
max_age = "7d"
"#,
    )
    .unwrap();

    let config = parse::load_dir(dir.path()).expect("should parse TOML");
    assert_eq!(config.ignore.len(), 2);
    assert!(config.cache.is_some());
    let cache = config.cache.as_ref().unwrap();
    assert_eq!(cache.max_size.map(MaxSize::as_bytes), Some(5_000_000_000));
    assert_eq!(cache.max_age.map(MaxAge::as_secs), Some(7 * 24 * 60 * 60));
}

#[test]
fn yaml_format_parses_ignores() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join(".cascade.yaml"),
        "ignore:\n  - pattern: '*.o'\n  - pattern: '*.d'\n",
    )
    .unwrap();

    let config = parse::load_dir(dir.path()).expect("should parse YAML");
    assert_eq!(config.ignore.len(), 2);
    assert_eq!(config.ignore[0].pattern, "*.o");
    assert_eq!(config.ignore[1].pattern, "*.d");
}

#[test]
fn json_format_parses_ignores() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join(".cascade.json"),
        r#"{"ignore": [{"pattern": "node_modules/"}, {"pattern": ".env"}]}"#,
    )
    .unwrap();

    let config = parse::load_dir(dir.path()).expect("should parse JSON");
    assert_eq!(config.ignore.len(), 2);
    assert_eq!(config.ignore[0].pattern, "node_modules/");
    assert_eq!(config.ignore[1].pattern, ".env");
}

#[test]
fn directory_walk_merges_parent_into_child() {
    let root = TempDir::new().unwrap();
    let child = root.path().join("project");
    fs::create_dir_all(&child).unwrap();

    // Parent has broad rules (gitignore format).
    fs::write(root.path().join(".cascade"), "*.log\n*.tmp\n").unwrap();
    // Child overrides with tighter rules (TOML format).
    fs::write(
        child.join(".cascade.toml"),
        "[[ignore]]\npattern = 'secret.key'\n",
    )
    .unwrap();

    let resolved = merge::resolve(root.path(), &child);
    // Parent's 2 + child's 1 = 3 rules.
    assert_eq!(resolved.ignores.len(), 3);
}

#[test]
fn resolved_config_is_ignored_matches_patterns() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join(".cascade"),
        "*.log\nbuild\n!important.log\n",
    )
    .unwrap();

    let resolved = merge::resolve(dir.path(), dir.path());

    // Plain pattern matches.
    assert!(resolved.is_ignored("error.log", false));
    assert!(resolved.is_ignored("build", true));

    // Negation overrides.
    assert!(!resolved.is_ignored("important.log", false));

    // Non-matching names are not ignored.
    assert!(!resolved.is_ignored("main.rs", false));
}

#[test]
fn multiple_formats_in_one_directory_merge() {
    let dir = TempDir::new().unwrap();
    // Gitignore format.
    fs::write(dir.path().join(".cascade"), "*.log\n").unwrap();
    // TOML format.
    fs::write(
        dir.path().join(".cascade.toml"),
        "[[ignore]]\npattern = '*.tmp'\n",
    )
    .unwrap();

    let resolved = merge::resolve(dir.path(), dir.path());
    // Both formats contribute to the ignore list.
    let patterns: Vec<&str> = resolved
        .ignores
        .iter()
        .map(|r| r.pattern.as_str())
        .collect();
    assert!(
        patterns.contains(&"*.log"),
        "should have *.log from gitignore: {patterns:?}"
    );
    assert!(
        patterns.contains(&"*.tmp"),
        "should have *.tmp from TOML: {patterns:?}"
    );
}

#[test]
fn no_cascade_files_returns_empty_config() {
    let dir = TempDir::new().unwrap();
    let resolved = merge::resolve(dir.path(), dir.path());
    assert!(resolved.ignores.is_empty());
    assert!(resolved.pins.is_empty());
}
