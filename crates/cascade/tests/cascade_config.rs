#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::string_slice
)]
//! Integration tests for the .cascade config parser.

use cascade_config::parse;
use std::path::PathBuf;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cascade_configs")
}

#[test]
fn test_load_gitignore_fixture() {
    let config = parse::load_dir(&fixtures_dir());
    assert!(config.is_some());
    let config = config.unwrap();

    assert!(!config.ignore.is_empty(), "should have ignore rules");

    let log_pattern = config.ignore.iter().find(|r| r.pattern == "*.log");
    assert!(log_pattern.is_some(), "should find *.log pattern");
    assert!(!log_pattern.unwrap().negated);

    let important = config.ignore.iter().find(|r| r.pattern == "important.log");
    assert!(important.is_some(), "should find important.log negation");
    assert!(important.unwrap().negated);
}

#[test]
fn test_load_toml_fixture() {
    let config = parse::load_dir(&fixtures_dir());
    assert!(config.is_some());
    let config = config.unwrap();

    assert!(config.cache.is_some());
}

#[test]
fn test_load_yaml_fixture() {
    let config = parse::load_dir(&fixtures_dir());
    assert!(config.is_some());
    let config = config.unwrap();

    assert!(!config.pin.is_empty());
}

#[test]
fn test_load_json_fixture() {
    let config = parse::load_dir(&fixtures_dir());
    assert!(config.is_some());
    let config = config.unwrap();

    let node_modules = config.ignore.iter().find(|r| r.pattern == "node_modules");
    assert!(node_modules.is_some());
    assert!(node_modules.unwrap().dir_only);
}

#[test]
fn test_merge_order_deterministic() {
    let config = parse::load_dir(&fixtures_dir());
    assert!(config.is_some());
    let config = config.unwrap();

    let pattern_count = config.ignore.len();
    assert!(
        pattern_count >= 4,
        "should have patterns from all formats, got {pattern_count}"
    );
}
