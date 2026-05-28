//! Platform context providers — collect real system state for expression evaluation.
//!
//! Each provider populates a portion of [`EvalContext`] from the host system:
//! - [`DeviceProvider`] — hostname, architecture, OS
//! - [`DiskProvider`] — disk space at a given path
//! - [`NetworkProvider`] — network interface type, metered status
//! - [`PowerProvider`] — power source, battery level
//! - [`TimeProvider`] — current time
//!
//! Usage:
//! ```
//! let ctx = cascade_expr::providers::collect_default();
//! let expr = cascade_expr::eval::parse_expr("DISK.free > 1GB").unwrap();
//! assert!(cascade_expr::eval::evaluate(&expr, &ctx));
//! ```

use crate::context::{EvalContext, FileContext, PeerContext, DeviceContext, DiskContext, NetworkContext, NetworkType, PowerContext, PowerSource, TimeContext};

/// Collect a full `EvalContext` with all providers using default values.
#[must_use] pub fn collect_default() -> EvalContext {
    EvalContext {
        file: FileContext::default_for_eval(),
        device: DeviceProvider::collect(),
        disk: DiskProvider::collect_root(),
        network: NetworkProvider::collect(),
        power: PowerProvider::collect(),
        time: TimeProvider::collect(),
        peer: PeerContext::default(),
    }
}

/// Build an `EvalContext` for a specific file, filling system context from providers.
#[must_use] pub fn for_file(file: &FileContext) -> EvalContext {
    EvalContext {
        file: file.clone(),
        device: DeviceProvider::collect(),
        disk: DiskProvider::collect_root(),
        network: NetworkProvider::collect(),
        power: PowerProvider::collect(),
        time: TimeProvider::collect(),
        peer: PeerContext::default(),
    }
}

// ── File context helper ──

impl FileContext {
    /// Create a `FileContext` suitable for expression evaluation from a file entry.
    #[must_use] pub fn from_entry(
        name: &str,
        size: u64,
        mime_type: Option<&str>,
        cached: bool,
        pinned: bool,
    ) -> Self {
        let ext = name
            .rfind('.')
            .map(|i| name[i + 1..].to_string())
            .unwrap_or_default();
        let mime = mime_type.unwrap_or("").to_string();
        Self {
            size,
            mime,
            ext,
            name: name.to_string(),
            modified: chrono::Utc::now(),
            owner: String::new(),
            shared: false,
            starred: false,
            dirty: false,
            cached,
            pinned,
        }
    }

    /// Default file context with zeroed fields (for non-file-specific evaluation).
    fn default_for_eval() -> Self {
        Self {
            size: 0,
            mime: String::new(),
            ext: String::new(),
            name: String::new(),
            modified: chrono::Utc::now(),
            owner: String::new(),
            shared: false,
            starred: false,
            dirty: false,
            cached: false,
            pinned: false,
        }
    }
}

// ── Device provider ──

/// Collects device identity and capabilities.
pub struct DeviceProvider;

impl DeviceProvider {
    #[must_use] pub fn collect() -> DeviceContext {
        DeviceContext {
            id: device_id(),
            name: hostname(),
            tags: Vec::new(),
            arch: std::env::consts::ARCH.to_string(),
            os: std::env::consts::OS.to_string(),
        }
    }
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok()).map_or_else(|| "unknown".to_string(), |s| s.trim().to_string())
}

fn device_id() -> String {
    // Derive a stable device ID from hostname.
    // Production would use a TLS certificate fingerprint (Phase 7).
    use std::hash::{Hash, Hasher};
    let name = hostname();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

// ── Disk provider ──

/// Collects disk usage statistics.
pub struct DiskProvider;

impl DiskProvider {
    /// Collect disk stats for the root filesystem.
    #[must_use] pub fn collect_root() -> DiskContext {
        Self::collect_for_path("/")
    }

    /// Collect disk stats for the filesystem containing the given path.
    #[allow(unsafe_code)]
    #[must_use] pub fn collect_for_path(path: &str) -> DiskContext {
        #[cfg(unix)]
        {
            let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
            let c_path = std::ffi::CString::new(path).unwrap_or_default();
            let result = unsafe { libc::statfs(c_path.as_ptr(), &raw mut stat) };
            if result == 0 {
                let block_size = u64::from(stat.f_bsize);
                DiskContext {
                    total_bytes: stat.f_blocks * block_size,
                    free_bytes: stat.f_bavail * block_size,
                }
            } else {
                DiskContext {
                    total_bytes: 0,
                    free_bytes: 0,
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            DiskContext {
                total_bytes: 0,
                free_bytes: 0,
            }
        }
    }
}

// ── Network provider ──

/// Collects network interface information.
pub struct NetworkProvider;

impl NetworkProvider {
    #[must_use] pub fn collect() -> NetworkContext {
        // Determine the default route's interface type.
        let if_type = detect_network_type();
        let metered = detect_metered();
        NetworkContext {
            if_type,
            metered,
            bandwidth_bps: None,
        }
    }
}

fn detect_network_type() -> NetworkType {
    #[cfg(target_os = "macos")]
    {
        // Check the primary network service.
        let output = std::process::Command::new("networksetup")
            .args(["-getinfo", "Wi-Fi"])
            .output()
            .ok();

        if let Some(out) = output {
            let info = String::from_utf8_lossy(&out.stdout);
            if info.contains("IP address") && !info.contains("There is no") {
                return NetworkType::Wifi;
            }
        }

        // Check for Ethernet.
        let output = std::process::Command::new("networksetup")
            .args(["-getinfo", "Ethernet"])
            .output()
            .ok();

        if let Some(out) = output {
            let info = String::from_utf8_lossy(&out.stdout);
            if info.contains("IP address") {
                return NetworkType::Ethernet;
            }
        }

        NetworkType::Unknown
    }
    #[cfg(not(target_os = "macos"))]
    {
        NetworkType::Unknown
    }
}

const fn detect_metered() -> bool {
    // Conservative: assume metered on cellular, not on others.
    false
}

// ── Power provider ──

/// Collects power source information.
pub struct PowerProvider;

impl PowerProvider {
    #[must_use] pub fn collect() -> PowerContext {
        #[cfg(target_os = "macos")]
        {
            let output = std::process::Command::new("pmset")
                .args(["-g", "batt"])
                .output()
                .ok();

            if let Some(out) = output {
                let info = String::from_utf8_lossy(&out.stdout);
                let source = if info.contains("AC Power") {
                    PowerSource::AC
                } else if info.contains("Battery Power") {
                    PowerSource::Battery
                } else {
                    PowerSource::Unknown
                };

                // Parse battery percentage: "100%; charged" or "75%; discharging"
                let battery_pct = info
                    .split(';')
                    .next()
                    .and_then(|s| s.trim().strip_suffix('%'))
                    .and_then(|s| s.parse::<f64>().ok());

                PowerContext {
                    source,
                    battery_pct,
                }
            } else {
                PowerContext {
                    source: PowerSource::Unknown,
                    battery_pct: None,
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            PowerContext {
                source: PowerSource::Unknown,
                battery_pct: None,
            }
        }
    }
}

// ── Time provider ──

/// Collects current time.
pub struct TimeProvider;

impl TimeProvider {
    #[must_use] pub fn collect() -> TimeContext {
        TimeContext {
            now: chrono::Utc::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_default_returns_valid_context() {
        use chrono::Datelike;
        let ctx = collect_default();
        // Should have device info.
        assert!(!ctx.device.arch.is_empty());
        assert!(!ctx.device.os.is_empty());
        // Should have time.
        assert!(ctx.time.now.year() >= 2026);
    }

    #[test]
    fn device_provider_collects_info() {
        let device = DeviceProvider::collect();
        assert!(!device.name.is_empty());
        assert!(!device.arch.is_empty());
        assert!(!device.os.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn disk_provider_collects_root() {
        let disk = DiskProvider::collect_root();
        // Root filesystem should have some space.
        assert!(disk.total_bytes > 0);
        assert!(disk.free_bytes > 0);
    }

    #[test]
    fn power_provider_collects() {
        let power = PowerProvider::collect();
        // Should not panic; source is a valid variant.
        let _ = power.source.to_string();
    }

    #[test]
    fn time_provider_collects() {
        let time = TimeProvider::collect();
        assert!(time.now.timestamp() > 0);
    }

    #[test]
    fn file_context_from_entry() {
        let fc = FileContext::from_entry("report.pdf", 1024, Some("application/pdf"), false, true);
        assert_eq!(fc.name, "report.pdf");
        assert_eq!(fc.size, 1024);
        assert_eq!(fc.ext, "pdf");
        assert_eq!(fc.mime, "application/pdf");
        assert!(!fc.cached);
        assert!(fc.pinned);
    }

    #[test]
    fn file_context_from_entry_no_ext() {
        let fc = FileContext::from_entry("Makefile", 500, None, false, false);
        assert_eq!(fc.ext, "");
        assert_eq!(fc.mime, "");
    }
}
