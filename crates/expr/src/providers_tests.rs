//! Extended tests for the `providers` module.
//!
//! These tests cover provider accessors that are pure or mockable, including
//! `FileContext` construction helpers, `DeviceContext` field shapes, the NUL-path
//! edge case in `DiskProvider`, and the `for_file` convenience constructor.

use crate::context::{FileContext, FileFlags, NetworkType, PeerContext, PowerSource};
use crate::providers::{DeviceProvider, NetworkProvider, PowerProvider, for_file};
// `DiskProvider`'s real implementation (and the tests exercising it) are
// unix-only — statfs has no Windows equivalent here — so the import is gated to
// match, avoiding an unused-import error on Windows.
#[cfg(unix)]
use crate::providers::DiskProvider;

// ── FileContext helpers ──────────────────────────────────────────────────────

#[test]
fn file_context_from_entry_dotted_name_extracts_last_ext() {
    // A name like "archive.tar.gz" should extract only the final extension.
    let fc = FileContext::from_entry("archive.tar.gz", 0, None, false, false);
    assert_eq!(fc.ext, "gz");
    assert_eq!(fc.name, "archive.tar.gz");
}

#[test]
fn file_context_from_entry_all_flags_set() {
    let fc = FileContext::from_entry("doc.txt", 512, Some("text/plain"), true, true);
    assert!(fc.flags.cached());
    assert!(fc.flags.pinned());
    // Flags not set via `from_entry` should default off.
    assert!(!fc.flags.shared());
    assert!(!fc.flags.starred());
    assert!(!fc.flags.dirty());
}

#[test]
fn file_context_from_entry_no_flags_set() {
    let fc = FileContext::from_entry("img.png", 4096, Some("image/png"), false, false);
    assert!(!fc.flags.cached());
    assert!(!fc.flags.pinned());
}

#[test]
fn file_context_age_is_non_negative() {
    let fc = FileContext::from_entry("note.md", 100, None, false, false);
    // `age()` measures time since modification; the file was just created so the
    // duration must be >= 0.
    assert!(fc.age().num_seconds() >= 0);
}

#[test]
fn file_context_year_matches_current_year() {
    use chrono::Datelike;
    let fc = FileContext::from_entry("readme.md", 0, None, false, false);
    let current_year = chrono::Utc::now().year();
    // `from_entry` stamps `modified` with `Utc::now()`, so the year must match.
    assert_eq!(i64::from(fc.year()), i64::from(current_year));
}

// ── DeviceProvider ───────────────────────────────────────────────────────────

#[test]
fn device_id_is_sixteen_hex_chars() {
    let device = DeviceProvider::collect();
    // The ID is formatted with `{:016x}`, so exactly 16 lowercase hex digits.
    assert_eq!(device.id.len(), 16);
    assert!(
        device.id.chars().all(|c| c.is_ascii_hexdigit()),
        "device ID contains non-hex character: {}",
        device.id,
    );
}

#[test]
fn device_arch_and_os_match_compile_consts() {
    let device = DeviceProvider::collect();
    assert_eq!(device.arch, std::env::consts::ARCH);
    assert_eq!(device.os, std::env::consts::OS);
}

#[test]
fn device_tags_empty_by_default() {
    let device = DeviceProvider::collect();
    assert!(device.tags.is_empty());
}

// ── DiskProvider ─────────────────────────────────────────────────────────────

#[test]
#[cfg(unix)]
fn disk_provider_nul_path_returns_zeroed_context() {
    // A path containing a NUL byte cannot be passed to `statfs`; the provider
    // must return a zeroed `DiskContext` rather than panicking or reading garbage.
    let disk = DiskProvider::collect_for_path("bad\0path");
    assert_eq!(disk.total_bytes, 0);
    assert_eq!(disk.free_bytes, 0);
}

#[test]
#[cfg(unix)]
fn disk_context_used_bytes_is_difference() {
    // `used_bytes()` should equal `total_bytes - free_bytes`.
    let disk = DiskProvider::collect_root();
    assert_eq!(disk.used_bytes(), disk.total_bytes - disk.free_bytes);
}

// ── NetworkProvider ───────────────────────────────────────────────────────────

#[test]
fn network_type_display_values() {
    // Confirm the string representations used in expression evaluation.
    assert_eq!(NetworkType::Wifi.to_string(), "wifi");
    assert_eq!(NetworkType::Ethernet.to_string(), "ethernet");
    assert_eq!(NetworkType::Cellular.to_string(), "cellular");
    assert_eq!(NetworkType::Unknown.to_string(), "unknown");
}

#[test]
fn network_provider_collects_without_panic() {
    // On any platform the provider must return a valid context without panicking.
    let net = NetworkProvider::collect();
    let _ = net.if_type.to_string();
    // `bandwidth_bps` is `None` by default (not yet probed).
    assert!(net.bandwidth_bps.is_none());
}

// ── PowerProvider ─────────────────────────────────────────────────────────────

#[test]
fn power_source_display_values() {
    assert_eq!(PowerSource::AC.to_string(), "ac");
    assert_eq!(PowerSource::Battery.to_string(), "battery");
    assert_eq!(PowerSource::Unknown.to_string(), "unknown");
}

#[test]
fn power_battery_pct_is_valid_range_or_none() {
    let power = PowerProvider::collect();
    if let Some(pct) = power.battery_pct {
        // A u8 is always 0–255; the pmset parser only emits 0–100.
        assert!(pct <= 100, "battery_pct out of range: {pct}");
    }
    // `None` is perfectly valid on desktops / CI without a battery.
}

// ── `for_file` constructor ────────────────────────────────────────────────────

#[test]
fn for_file_propagates_file_context() {
    let file = FileContext::from_entry(
        "test.bin",
        8192,
        Some("application/octet-stream"),
        true,
        false,
    );
    let ctx = for_file(&file);
    assert_eq!(ctx.file.name, "test.bin");
    assert_eq!(ctx.file.size, 8192);
    assert!(ctx.file.flags.cached());
    assert!(!ctx.file.flags.pinned());
    // System context is populated (not zeroed).
    assert!(!ctx.device.arch.is_empty());
}

// ── PeerContext defaults ──────────────────────────────────────────────────────

#[test]
fn peer_context_default_is_zero() {
    let peer = PeerContext::default();
    assert_eq!(peer.online_count, 0);
    assert_eq!(peer.peers_with_file, 0);
}

// ── FileFlags builder roundtrips ──────────────────────────────────────────────

#[test]
fn file_flags_all_builder_methods_roundtrip() {
    let flags = FileFlags::default()
        .with_shared(true)
        .with_starred(true)
        .with_dirty(true)
        .with_cached(true)
        .with_pinned(true);
    assert!(flags.shared());
    assert!(flags.starred());
    assert!(flags.dirty());
    assert!(flags.cached());
    assert!(flags.pinned());
}

#[test]
fn file_flags_toggle_off_clears_bit() {
    let flags = FileFlags::default()
        .with_cached(true)
        .with_pinned(true)
        .with_cached(false);
    assert!(!flags.cached());
    assert!(flags.pinned());
}
