//! Extended tests for the `eval` module.
//!
//! Covers identifier resolution for all context namespaces, mixed AND/OR/NOT
//! operator combinations, type coercion and out-of-range conversions, the
//! `matches` / `contains` / `!=` / `<=` / `>=` operators, and `Expr::Literal`
//! evaluation.

use crate::ast::{DurationUnit, Expr, Operand, Operator, SizeUnit, Value};
use crate::context::{
    DiskContext, EvalContext, FileFlags, NetworkContext, NetworkType, PeerContext, PowerContext,
    PowerSource, TimeContext,
};
use crate::eval::{evaluate, parse_expr};
use chrono::Datelike;

// ── Identifier resolution: FILE namespace ────────────────────────────────────

#[test]
fn file_mime_identifier_resolves() {
    let mut ctx = EvalContext::default();
    ctx.file.mime = "image/jpeg".to_string();
    let expr = parse_expr("FILE.mime == \"image/jpeg\"").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn file_ext_identifier_resolves() {
    let mut ctx = EvalContext::default();
    ctx.file.ext = "pdf".to_string();
    let expr = parse_expr("FILE.ext == \"pdf\"").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn file_name_identifier_resolves() {
    let mut ctx = EvalContext::default();
    ctx.file.name = "report.pdf".to_string();
    let expr = parse_expr("FILE.name == \"report.pdf\"").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn file_shared_identifier_resolves() {
    let mut ctx = EvalContext::default();
    ctx.file.flags = FileFlags::default().with_shared(true);
    let expr = parse_expr("FILE.shared == true").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn file_starred_identifier_resolves() {
    let mut ctx = EvalContext::default();
    ctx.file.flags = FileFlags::default().with_starred(true);
    let expr = parse_expr("FILE.starred == true").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn file_dirty_identifier_resolves() {
    let mut ctx = EvalContext::default();
    ctx.file.flags = FileFlags::default().with_dirty(true);
    let expr = parse_expr("FILE.dirty == true").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn file_year_identifier_resolves() {
    use chrono::TimeZone;
    let mut ctx = EvalContext::default();
    ctx.file.modified = chrono::Utc.with_ymd_and_hms(2024, 6, 1, 0, 0, 0).unwrap();
    let expr = parse_expr("FILE.year == 2024").unwrap();
    assert!(evaluate(&expr, &ctx));
}

// ── Identifier resolution: DEVICE namespace ──────────────────────────────────

#[test]
fn device_id_identifier_resolves() {
    let mut ctx = EvalContext::default();
    ctx.device.id = "abc123".to_string();
    let expr = parse_expr("DEVICE.id == \"abc123\"").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn device_name_identifier_resolves() {
    let mut ctx = EvalContext::default();
    ctx.device.name = "my-laptop".to_string();
    let expr = parse_expr("DEVICE.name == \"my-laptop\"").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn device_arch_identifier_resolves() {
    let mut ctx = EvalContext::default();
    ctx.device.arch = "aarch64".to_string();
    let expr = parse_expr("DEVICE.arch == \"aarch64\"").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn device_os_identifier_resolves() {
    let mut ctx = EvalContext::default();
    ctx.device.os = "linux".to_string();
    let expr = parse_expr("DEVICE.os == \"linux\"").unwrap();
    assert!(evaluate(&expr, &ctx));
}

// ── Identifier resolution: DISK namespace ────────────────────────────────────

#[test]
fn disk_used_identifier_is_total_minus_free() {
    // 10 GB total, 4 GB free → 6 GB used.
    let gb: u64 = 1024 * 1024 * 1024;
    let ctx = EvalContext {
        disk: DiskContext {
            total_bytes: 10 * gb,
            free_bytes: 4 * gb,
        },
        ..EvalContext::default()
    };
    // 6 GB in bytes as i64.
    let six_gb = i64::try_from(6 * gb).unwrap();
    let expr = parse_expr(&format!("DISK.used == {six_gb}")).unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn disk_free_resolves_to_free_bytes() {
    let ctx = EvalContext {
        disk: DiskContext {
            total_bytes: 100,
            free_bytes: 40,
        },
        ..EvalContext::default()
    };
    let expr = parse_expr("DISK.free == 40").unwrap();
    assert!(evaluate(&expr, &ctx));
}

// ── Identifier resolution: NETWORK namespace ─────────────────────────────────

#[test]
fn network_type_ethernet_resolves() {
    let mut ctx = EvalContext::default();
    ctx.network.if_type = NetworkType::Ethernet;
    let expr = parse_expr("NETWORK.type == \"ethernet\"").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn network_type_cellular_resolves() {
    let mut ctx = EvalContext::default();
    ctx.network.if_type = NetworkType::Cellular;
    let expr = parse_expr("NETWORK.type == \"cellular\"").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn network_type_unknown_resolves() {
    let ctx = EvalContext::default(); // default is Unknown
    let expr = parse_expr("NETWORK.type == \"unknown\"").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn network_metered_true_resolves() {
    let mut ctx = EvalContext::default();
    ctx.network.metered = true;
    let expr = parse_expr("NETWORK.metered == true").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn network_bandwidth_resolves_when_set() {
    let ctx = EvalContext {
        network: NetworkContext {
            if_type: NetworkType::Wifi,
            metered: false,
            bandwidth_bps: Some(100_000_000), // 100 Mbps
        },
        ..EvalContext::default()
    };
    let expr = parse_expr("NETWORK.bandwidth == 100000000").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn network_bandwidth_resolves_to_zero_when_none() {
    let ctx = EvalContext::default(); // bandwidth_bps is None
    let expr = parse_expr("NETWORK.bandwidth == 0").unwrap();
    assert!(evaluate(&expr, &ctx));
}

// ── Identifier resolution: POWER namespace ───────────────────────────────────

#[test]
fn power_source_ac_resolves() {
    let mut ctx = EvalContext::default();
    ctx.power.source = PowerSource::AC;
    let expr = parse_expr("POWER.source == \"ac\"").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn power_source_battery_resolves() {
    let mut ctx = EvalContext::default();
    ctx.power.source = PowerSource::Battery;
    let expr = parse_expr("POWER.source == \"battery\"").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn power_source_unknown_resolves() {
    let ctx = EvalContext::default(); // default is Unknown
    let expr = parse_expr("POWER.source == \"unknown\"").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn power_battery_none_resolves_to_zero() {
    let ctx = EvalContext {
        power: PowerContext {
            source: PowerSource::Unknown,
            battery_pct: None,
        },
        ..EvalContext::default()
    };
    let expr = parse_expr("POWER.battery == 0").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn power_battery_some_resolves_to_value() {
    let ctx = EvalContext {
        power: PowerContext {
            source: PowerSource::Battery,
            battery_pct: Some(75),
        },
        ..EvalContext::default()
    };
    let expr = parse_expr("POWER.battery == 75").unwrap();
    assert!(evaluate(&expr, &ctx));
}

// ── Identifier resolution: TIME namespace ────────────────────────────────────

#[test]
fn time_hour_resolves() {
    use chrono::TimeZone;
    let ctx = EvalContext {
        time: TimeContext {
            now: chrono::Utc.with_ymd_and_hms(2026, 1, 1, 14, 0, 0).unwrap(),
        },
        ..EvalContext::default()
    };
    let expr = parse_expr("TIME.hour == 14").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn time_day_resolves_to_weekday_number() {
    use chrono::{TimeZone, Weekday};
    // 2026-01-05 is a Monday (weekday number 1 in ISO 8601).
    let now = chrono::Utc.with_ymd_and_hms(2026, 1, 5, 0, 0, 0).unwrap();
    assert_eq!(
        now.weekday(),
        Weekday::Mon,
        "sanity check: 2026-01-05 must be a Monday"
    );
    let ctx = EvalContext {
        time: TimeContext { now },
        ..EvalContext::default()
    };
    let expr = parse_expr("TIME.day == 1").unwrap();
    assert!(evaluate(&expr, &ctx));
}

// ── Identifier resolution: PEER namespace ────────────────────────────────────

#[test]
fn peer_online_resolves_to_online_count() {
    let ctx = EvalContext {
        peer: PeerContext {
            online_count: 3,
            peers_with_file: 0,
        },
        ..EvalContext::default()
    };
    let expr = parse_expr("PEER.online == 3").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn peer_count_alias_resolves_to_online_count() {
    // `PEER.count` is documented as an alias for `PEER.online`.
    let ctx = EvalContext {
        peer: PeerContext {
            online_count: 5,
            peers_with_file: 0,
        },
        ..EvalContext::default()
    };
    let expr = parse_expr("PEER.count == 5").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn peer_has_file_resolves() {
    let ctx = EvalContext {
        peer: PeerContext {
            online_count: 4,
            peers_with_file: 2,
        },
        ..EvalContext::default()
    };
    let expr = parse_expr("PEER.has_file == 2").unwrap();
    assert!(evaluate(&expr, &ctx));
}

// ── Unknown identifier falls back to zero ────────────────────────────────────

#[test]
fn unknown_identifier_resolves_to_zero() {
    let ctx = EvalContext::default();
    let expr = parse_expr("UNKNOWN.field == 0").unwrap();
    assert!(evaluate(&expr, &ctx));
}

// ── NOT operator ─────────────────────────────────────────────────────────────

#[test]
fn not_operator_negates_true() {
    let mut ctx = EvalContext::default();
    ctx.file.flags = FileFlags::default().with_cached(true);
    // `!FILE.cached` should not parse as a single token; express as != true.
    let expr = parse_expr("FILE.cached != true").unwrap();
    assert!(!evaluate(&expr, &ctx));
}

#[test]
fn not_wrapping_comparison() {
    // Build `Not(Comparison)` directly — the grammar's `!primary` arm.
    let inner = Expr::Comparison {
        left: Operand::Identifier("FILE.size".to_string()),
        operator: Operator::Gt,
        right: Operand::Literal(Value::Integer(100)),
    };
    let expr = Expr::Not(Box::new(inner));
    let mut ctx = EvalContext::default();
    ctx.file.size = 200;
    // FILE.size > 100 is true, so NOT of that is false.
    assert!(!evaluate(&expr, &ctx));
}

#[test]
fn not_wrapping_false_comparison_becomes_true() {
    let inner = Expr::Comparison {
        left: Operand::Identifier("FILE.size".to_string()),
        operator: Operator::Gt,
        right: Operand::Literal(Value::Integer(100)),
    };
    let expr = Expr::Not(Box::new(inner));
    let mut ctx = EvalContext::default();
    ctx.file.size = 50; // less than 100, so NOT(false) == true
    assert!(evaluate(&expr, &ctx));
}

// ── Mixed AND / OR / NOT combinations ────────────────────────────────────────

#[test]
fn three_way_and_all_true() {
    let mut ctx = EvalContext::default();
    ctx.file.size = 200;
    ctx.network.metered = false;
    ctx.file.flags = FileFlags::default().with_cached(true);
    let expr =
        parse_expr("FILE.size > 100 && NETWORK.metered == false && FILE.cached == true").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn three_way_and_one_false() {
    let mut ctx = EvalContext::default();
    ctx.file.size = 200;
    ctx.network.metered = true; // this makes the middle clause false
    ctx.file.flags = FileFlags::default().with_cached(true);
    let expr =
        parse_expr("FILE.size > 100 && NETWORK.metered == false && FILE.cached == true").unwrap();
    assert!(!evaluate(&expr, &ctx));
}

#[test]
fn three_way_or_first_true() {
    let ctx = EvalContext::default(); // file.size == 0, metered == false
    let expr = parse_expr("FILE.size == 0 || FILE.cached == true || FILE.pinned == true").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn three_way_or_all_false() {
    let ctx = EvalContext::default(); // size 0, metered false — none match
    let expr = parse_expr("FILE.size > 100 || FILE.cached == true || FILE.pinned == true").unwrap();
    assert!(!evaluate(&expr, &ctx));
}

#[test]
fn and_of_or_subexpressions() {
    // (A || B) && (C || D) via direct AST construction.
    let a = Expr::Comparison {
        left: Operand::Identifier("FILE.size".to_string()),
        operator: Operator::Gt,
        right: Operand::Literal(Value::Integer(0)),
    };
    let b = Expr::Comparison {
        left: Operand::Identifier("FILE.cached".to_string()),
        operator: Operator::Eq,
        right: Operand::Literal(Value::Boolean(true)),
    };
    let c = Expr::Comparison {
        left: Operand::Identifier("NETWORK.metered".to_string()),
        operator: Operator::Eq,
        right: Operand::Literal(Value::Boolean(false)),
    };
    let d = Expr::Comparison {
        left: Operand::Identifier("FILE.pinned".to_string()),
        operator: Operator::Eq,
        right: Operand::Literal(Value::Boolean(true)),
    };
    let or_ab = Expr::Or(vec![a, b]);
    let or_cd = Expr::Or(vec![c, d]);
    let expr = Expr::And(vec![or_ab, or_cd]);

    let mut ctx = EvalContext::default();
    // (A=false || B=false) && (C=true || D=false) → false && true → false
    ctx.file.size = 0;
    assert!(!evaluate(&expr, &ctx));

    // (A=true || B=false) && (C=true || D=false) → true && true → true
    ctx.file.size = 10;
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn empty_and_is_vacuously_true() {
    // An `And([])` expression with no children should be vacuously true because
    // `Iterator::all` over an empty sequence returns `true`.
    let expr = Expr::And(vec![]);
    let ctx = EvalContext::default();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn empty_or_is_false() {
    // An `Or([])` expression with no children should be false because
    // `Iterator::any` over an empty sequence returns `false`.
    let expr = Expr::Or(vec![]);
    let ctx = EvalContext::default();
    assert!(!evaluate(&expr, &ctx));
}

// ── Expr::Literal evaluation ──────────────────────────────────────────────────

#[test]
fn literal_true_evaluates_to_true() {
    let expr = Expr::Literal(Value::Boolean(true));
    let ctx = EvalContext::default();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn literal_false_evaluates_to_false() {
    let expr = Expr::Literal(Value::Boolean(false));
    let ctx = EvalContext::default();
    assert!(!evaluate(&expr, &ctx));
}

#[test]
fn literal_non_boolean_evaluates_to_false() {
    // Only `Value::Boolean(true)` is truthy; integers, strings, etc. are all false.
    let ctx = EvalContext::default();
    assert!(!evaluate(&Expr::Literal(Value::Integer(1)), &ctx));
    assert!(!evaluate(
        &Expr::Literal(Value::String("yes".to_string())),
        &ctx
    ));
    assert!(!evaluate(
        &Expr::Literal(Value::Size(1, SizeUnit::Bytes)),
        &ctx
    ));
}

// ── `matches` operator ────────────────────────────────────────────────────────

#[test]
fn matches_exact_string_no_glob() {
    let mut ctx = EvalContext::default();
    ctx.file.ext = "pdf".to_string();
    let expr = parse_expr("FILE.ext matches \"pdf\"").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn matches_glob_star_at_end() {
    let mut ctx = EvalContext::default();
    ctx.file.name = "report-2026.pdf".to_string();
    let expr = parse_expr("FILE.name matches \"report*\"").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn matches_glob_star_at_start() {
    let mut ctx = EvalContext::default();
    ctx.file.name = "report-2026.pdf".to_string();
    let expr = parse_expr("FILE.name matches \"*.pdf\"").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn matches_glob_star_in_middle() {
    let mut ctx = EvalContext::default();
    ctx.file.name = "report-2026-final.pdf".to_string();
    let expr = parse_expr("FILE.name matches \"report*final.pdf\"").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn matches_glob_no_match_returns_false() {
    let mut ctx = EvalContext::default();
    ctx.file.name = "photo.jpg".to_string();
    let expr = parse_expr("FILE.name matches \"*.pdf\"").unwrap();
    assert!(!evaluate(&expr, &ctx));
}

#[test]
fn matches_on_non_strings_returns_false() {
    // `matches` is string-only; applying it to an integer operand should return false.
    let ctx = EvalContext::default();
    let expr = Expr::Comparison {
        left: Operand::Literal(Value::Integer(42)),
        operator: Operator::Matches,
        right: Operand::Literal(Value::String("*".to_string())),
    };
    assert!(!evaluate(&expr, &ctx));
}

// ── `contains` operator ───────────────────────────────────────────────────────

#[test]
fn contains_substring_present() {
    let mut ctx = EvalContext::default();
    ctx.file.name = "budget-2026-final.xlsx".to_string();
    let expr = parse_expr("FILE.name contains \"2026\"").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn contains_substring_absent() {
    let mut ctx = EvalContext::default();
    ctx.file.name = "budget-2026-final.xlsx".to_string();
    let expr = parse_expr("FILE.name contains \"2027\"").unwrap();
    assert!(!evaluate(&expr, &ctx));
}

// ── `!=` operator ─────────────────────────────────────────────────────────────

#[test]
fn ne_operator_true_when_different() {
    let mut ctx = EvalContext::default();
    ctx.network.if_type = NetworkType::Wifi;
    let expr = parse_expr("NETWORK.type != \"ethernet\"").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn ne_operator_false_when_equal() {
    let mut ctx = EvalContext::default();
    ctx.network.if_type = NetworkType::Ethernet;
    let expr = parse_expr("NETWORK.type != \"ethernet\"").unwrap();
    assert!(!evaluate(&expr, &ctx));
}

// ── `<=` and `>=` operators ───────────────────────────────────────────────────

#[test]
fn le_operator_true_when_less() {
    let mut ctx = EvalContext::default();
    ctx.file.size = 50;
    let expr = parse_expr("FILE.size <= 100").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn le_operator_true_when_equal() {
    let mut ctx = EvalContext::default();
    ctx.file.size = 100;
    let expr = parse_expr("FILE.size <= 100").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn le_operator_false_when_greater() {
    let mut ctx = EvalContext::default();
    ctx.file.size = 200;
    let expr = parse_expr("FILE.size <= 100").unwrap();
    assert!(!evaluate(&expr, &ctx));
}

#[test]
fn ge_operator_true_when_greater() {
    let mut ctx = EvalContext::default();
    ctx.file.size = 200;
    let expr = parse_expr("FILE.size >= 100").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn ge_operator_true_when_equal() {
    let mut ctx = EvalContext::default();
    ctx.file.size = 100;
    let expr = parse_expr("FILE.size >= 100").unwrap();
    assert!(evaluate(&expr, &ctx));
}

// ── Type coercion and cross-type size comparisons ─────────────────────────────

#[test]
fn integer_vs_size_comparison_cross_type() {
    // `FILE.size` resolves to an `Value::Integer` (raw bytes).
    // `10MB` resolves to `Value::Size(10, Megabytes)` = 10 * 1024 * 1024 bytes.
    let mut ctx = EvalContext::default();
    ctx.file.size = 10 * 1024 * 1024; // exactly 10 MB
    let expr = parse_expr("FILE.size >= 10MB").unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn size_unit_bytes_conversion() {
    let v = Value::Size(1024, SizeUnit::Kilobytes);
    assert_eq!(v.to_bytes(), Some(1024 * 1024));
}

#[test]
fn size_unit_terabytes_conversion() {
    let v = Value::Size(1, SizeUnit::Terabytes);
    // 1 TB = 1024^4 bytes.
    assert_eq!(v.to_bytes(), Some(1024 * 1024 * 1024 * 1024));
}

#[test]
fn duration_hours_conversion_to_seconds() {
    let v = Value::Duration(2, DurationUnit::Hours);
    // 2 hours = 7200 seconds.
    assert_eq!(v.to_seconds(), Some(2 * 3600));
}

#[test]
fn duration_weeks_conversion_to_seconds() {
    let v = Value::Duration(1, DurationUnit::Weeks);
    assert_eq!(v.to_seconds(), Some(604_800));
}

#[test]
fn duration_months_conversion_to_seconds() {
    // 1 month is treated as 30 days = 2,592,000 seconds.
    let v = Value::Duration(1, DurationUnit::Months);
    assert_eq!(v.to_seconds(), Some(2_592_000));
}

#[test]
fn duration_years_conversion_to_seconds() {
    // 1 year is treated as 365 days = 31,536,000 seconds.
    let v = Value::Duration(1, DurationUnit::Years);
    assert_eq!(v.to_seconds(), Some(31_536_000));
}

#[test]
fn non_duration_to_seconds_returns_none() {
    assert_eq!(Value::Integer(100).to_seconds(), None);
    assert_eq!(Value::Boolean(true).to_seconds(), None);
}

#[test]
fn non_size_to_bytes_returns_none() {
    assert_eq!(Value::Integer(100).to_bytes(), None);
    assert_eq!(Value::String("foo".to_string()).to_bytes(), None);
}

// ── Out-of-range u64→i64 saturation ──────────────────────────────────────────

#[test]
fn file_size_saturates_to_i64_max_for_huge_files() {
    // `u64::MAX` cannot fit in an `i64`; the evaluator must clamp to `i64::MAX`
    // rather than wrapping or panicking.
    let mut ctx = EvalContext::default();
    ctx.file.size = u64::MAX;
    let expr = parse_expr(&format!("FILE.size == {}", i64::MAX)).unwrap();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn disk_free_saturates_to_i64_max_for_huge_disk() {
    let ctx = EvalContext {
        disk: DiskContext {
            total_bytes: u64::MAX,
            free_bytes: u64::MAX,
        },
        ..EvalContext::default()
    };
    let expr = parse_expr(&format!("DISK.free == {}", i64::MAX)).unwrap();
    assert!(evaluate(&expr, &ctx));
}

// ── `value_eq` Percentage type ────────────────────────────────────────────────

#[test]
fn percentage_equality_via_direct_ast() {
    // `Percentage` is only produced by the parser (e.g. `75%`); test equality
    // via `value_eq` through the AST directly to avoid grammar ambiguity.
    let expr = Expr::Comparison {
        left: Operand::Literal(Value::Percentage(75)),
        operator: Operator::Eq,
        right: Operand::Literal(Value::Percentage(75)),
    };
    let ctx = EvalContext::default();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn percentage_inequality_via_direct_ast() {
    let expr = Expr::Comparison {
        left: Operand::Literal(Value::Percentage(50)),
        operator: Operator::Eq,
        right: Operand::Literal(Value::Percentage(75)),
    };
    let ctx = EvalContext::default();
    assert!(!evaluate(&expr, &ctx));
}

// ── `value_eq` cross-type Size equality ──────────────────────────────────────

#[test]
fn size_cross_type_eq_same_bytes() {
    // 1024 bytes == 1 KB (both convert to 1024 bytes).
    let expr = Expr::Comparison {
        left: Operand::Literal(Value::Size(1024, SizeUnit::Bytes)),
        operator: Operator::Eq,
        right: Operand::Literal(Value::Size(1, SizeUnit::Kilobytes)),
    };
    let ctx = EvalContext::default();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn size_cross_type_eq_different_bytes() {
    let expr = Expr::Comparison {
        left: Operand::Literal(Value::Size(2048, SizeUnit::Bytes)),
        operator: Operator::Eq,
        right: Operand::Literal(Value::Size(1, SizeUnit::Kilobytes)),
    };
    let ctx = EvalContext::default();
    assert!(!evaluate(&expr, &ctx));
}

// ── `value_eq` cross-type Duration equality ───────────────────────────────────

#[test]
fn duration_cross_type_eq_same_seconds() {
    // 60 seconds == 1 minute.
    let expr = Expr::Comparison {
        left: Operand::Literal(Value::Duration(60, DurationUnit::Seconds)),
        operator: Operator::Eq,
        right: Operand::Literal(Value::Duration(1, DurationUnit::Minutes)),
    };
    let ctx = EvalContext::default();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn duration_cross_type_eq_different_seconds() {
    let expr = Expr::Comparison {
        left: Operand::Literal(Value::Duration(30, DurationUnit::Seconds)),
        operator: Operator::Eq,
        right: Operand::Literal(Value::Duration(1, DurationUnit::Minutes)),
    };
    let ctx = EvalContext::default();
    assert!(!evaluate(&expr, &ctx));
}

// ── Duration ordering ─────────────────────────────────────────────────────────

#[test]
fn duration_ordering_lt() {
    let expr = Expr::Comparison {
        left: Operand::Literal(Value::Duration(1, DurationUnit::Hours)),
        operator: Operator::Lt,
        right: Operand::Literal(Value::Duration(1, DurationUnit::Days)),
    };
    let ctx = EvalContext::default();
    assert!(evaluate(&expr, &ctx));
}

#[test]
fn duration_milliseconds_are_truncated_to_zero_seconds() {
    // 500ms → 500/1000 = 0 seconds (integer division).
    let v = Value::Duration(500, DurationUnit::Milliseconds);
    assert_eq!(v.to_seconds(), Some(0));
}
