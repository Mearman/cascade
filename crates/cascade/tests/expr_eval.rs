//! Integration tests: expression evaluation with platform context providers
//! and conditional rule evaluation in config resolver.

use cascade_engine::config::ConfigResolver;
use cascade_expr::context::*;
use cascade_expr::eval;
use cascade_expr::providers;
use std::path::{Path, PathBuf};

// ── Expression parsing and evaluation ──

#[test]
fn parse_and_evaluate_size_comparison() {
    let expr = eval::parse_expr("FILE.size > 10MB").unwrap();
    let mut ctx = EvalContext::default();
    ctx.file.size = 20 * 1024 * 1024; // 20MB
    assert!(eval::evaluate(&expr, &ctx));

    ctx.file.size = 5 * 1024 * 1024; // 5MB
    assert!(!eval::evaluate(&expr, &ctx));
}

#[test]
fn parse_and_evaluate_network_type() {
    let expr = eval::parse_expr("NETWORK.type == \"wifi\"").unwrap();
    let mut ctx = EvalContext::default();
    ctx.network.if_type = NetworkType::Wifi;
    assert!(eval::evaluate(&expr, &ctx));

    ctx.network.if_type = NetworkType::Ethernet;
    assert!(!eval::evaluate(&expr, &ctx));
}

#[test]
fn parse_and_evaluate_compound_and() {
    let expr = eval::parse_expr("FILE.size > 1MB && POWER.source == \"ac\"").unwrap();
    let mut ctx = EvalContext::default();
    ctx.file.size = 5 * 1024 * 1024;
    ctx.power.source = PowerSource::AC;
    assert!(eval::evaluate(&expr, &ctx));

    ctx.power.source = PowerSource::Battery;
    assert!(!eval::evaluate(&expr, &ctx));
}

#[test]
fn parse_and_evaluate_compound_or() {
    let expr = eval::parse_expr("FILE.cached == true || FILE.pinned == true").unwrap();
    let mut ctx = EvalContext::default();
    assert!(!eval::evaluate(&expr, &ctx));

    ctx.file.flags = ctx.file.flags.with_cached(true);
    assert!(eval::evaluate(&expr, &ctx));
}

#[test]
fn parse_and_evaluate_disk_free() {
    let expr = eval::parse_expr("DISK.free > 1GB").unwrap();
    let mut ctx = EvalContext::default();
    ctx.disk.free_bytes = 5 * 1024 * 1024 * 1024; // 5GB
    assert!(eval::evaluate(&expr, &ctx));

    ctx.disk.free_bytes = 500 * 1024 * 1024; // 500MB
    assert!(!eval::evaluate(&expr, &ctx));
}

// ── Platform providers ──

#[test]
fn providers_collect_real_system_info() {
    let ctx = providers::collect_default();
    // Device info should be populated.
    assert!(!ctx.device.arch.is_empty());
    assert!(!ctx.device.os.is_empty());
    assert!(!ctx.device.name.is_empty());
    // Time should be current.
    assert!(ctx.time.now.timestamp() > 0);
}

#[test]
#[cfg(unix)]
fn disk_provider_returns_nonzero() {
    let disk = providers::DiskProvider::collect_root();
    assert!(
        disk.total_bytes > 0,
        "root filesystem should report total > 0"
    );
    assert!(
        disk.free_bytes > 0,
        "root filesystem should have free space"
    );
}

#[test]
fn file_context_from_entry_builds_correctly() {
    let fc = FileContext::from_entry("photo.jpg", 2048, Some("image/jpeg"), true, false);
    assert_eq!(fc.name, "photo.jpg");
    assert_eq!(fc.size, 2048);
    assert_eq!(fc.ext, "jpg");
    assert_eq!(fc.mime, "image/jpeg");
    assert!(fc.flags.cached());
    assert!(!fc.flags.pinned());
}

#[test]
fn evaluate_with_real_context() {
    let ctx = providers::collect_default();
    // Disk should have free space on a dev machine.
    let expr = eval::parse_expr("DISK.free > 0").unwrap();
    assert!(eval::evaluate(&expr, &ctx));

    // Time hour should be 0-23.
    let expr = eval::parse_expr("TIME.hour >= 0").unwrap();
    assert!(eval::evaluate(&expr, &ctx));
    let expr = eval::parse_expr("TIME.hour < 24").unwrap();
    assert!(eval::evaluate(&expr, &ctx));
}

// ── Conditional config resolver ──

#[test]
fn config_resolver_basic_ignore_without_context() {
    let resolver = ConfigResolver::new(PathBuf::from("/tmp/expr-test"));
    // No .cascade file exists, so nothing should be ignored.
    let path = Path::new("/tmp/expr-test/Documents/notes.txt");
    assert!(!resolver.is_ignored(path, false));
}

#[test]
fn config_resolver_with_context_no_config() {
    let resolver = ConfigResolver::new(PathBuf::from("/tmp/expr-test"));
    let ctx = EvalContext::default();
    let path = Path::new("/tmp/expr-test/Documents/notes.txt");
    assert!(!resolver.is_ignored_with_context(path, false, Some(&ctx)));
}
