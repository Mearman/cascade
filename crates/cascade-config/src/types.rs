//! Cascade config types — shared across all four format parsers.

use std::fmt;
use std::time::Duration;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Parsed contents of a `.cascade` file.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CascadeConfig {
    #[serde(default)]
    pub ignore: Vec<IgnoreRule>,

    #[serde(default)]
    pub lifecycle: Vec<LifecyclePolicy>,

    #[serde(default)]
    pub pin: Vec<PinRule>,

    #[serde(default)]
    pub unpin: Vec<PinRule>,

    #[serde(default)]
    pub cache: Option<CacheConfig>,

    #[serde(default)]
    pub p2p: Option<P2PConfig>,

    #[serde(default)]
    pub device: Option<DeviceConfig>,
}

impl CascadeConfig {
    /// An empty config with no rules.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Merge another config into this one. Rules accumulate, scalars override.
    pub fn merge(&mut self, other: Self) {
        self.ignore.extend(other.ignore);
        self.lifecycle.extend(other.lifecycle);
        self.pin.extend(other.pin);
        self.unpin.extend(other.unpin);
        // Nearest-wins for scalar configs
        if other.cache.is_some() {
            self.cache = other.cache;
        }
        if other.p2p.is_some() {
            self.p2p = other.p2p;
        }
        // Device config is root-only — ignore from children
    }
}

/// An ignore rule (gitignore semantics).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IgnoreRule {
    pub pattern: String,
    #[serde(default)]
    pub negated: bool,
    #[serde(default)]
    pub dir_only: bool,
    #[serde(default)]
    pub conditions: Vec<String>,
}

/// A lifecycle policy for cache eviction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LifecyclePolicy {
    pub path: String,
    pub max_age: Option<String>,
    pub max_file_size: Option<String>,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub conditions: Vec<String>,
    #[serde(default)]
    pub if_expr: Option<String>,
}

/// A pin rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PinRule {
    pub path: String,
    #[serde(default)]
    pub conditions: Vec<String>,
}

/// Cache configuration.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CacheConfig {
    /// Maximum on-disk cache size. Parsed from human-readable byte strings
    /// (for example `5GB`, `512MiB`) at config load; malformed values are
    /// rejected during deserialisation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_size: Option<MaxSize>,
    /// Maximum age a cached file may reach before it is eligible for eviction.
    /// Parsed from human-readable duration strings (for example `7d`, `1h30m`)
    /// at config load; malformed values are rejected during deserialisation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age: Option<MaxAge>,
    /// The default cache-state posture for files in this subtree. Absent means
    /// the engine's own default applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_state: Option<CacheStatePosture>,
}

/// The declared default cache-state posture for a subtree.
///
/// This is the config-level posture, distinct from the engine's per-file
/// runtime `CacheState` (which also models transient states such as
/// downloading). Only the postures a `.cascade` file can declare appear here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheStatePosture {
    /// Keep matching files resident on disk; never evict them automatically.
    Pinned,
    /// Keep matching files metadata-only; fetch content on demand.
    Online,
    /// Let lifecycle policies and cache limits decide residency.
    Auto,
}

/// A maximum on-disk size, stored as a byte count.
///
/// Deserialises from a human-readable string such as `5GB` or `512MiB`.
/// Malformed strings are rejected at config load rather than at use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaxSize(bytesize::ByteSize);

impl MaxSize {
    /// The size in bytes.
    #[must_use]
    pub const fn as_bytes(self) -> u64 {
        self.0.as_u64()
    }
}

impl fmt::Display for MaxSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl<'de> Deserialize<'de> for MaxSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct MaxSizeVisitor;

        impl Visitor<'_> for MaxSizeVisitor {
            type Value = MaxSize;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a byte size such as \"5GB\" or \"512MiB\"")
            }

            fn visit_str<E>(self, value: &str) -> Result<MaxSize, E>
            where
                E: de::Error,
            {
                value
                    .parse::<bytesize::ByteSize>()
                    .map(MaxSize)
                    .map_err(de::Error::custom)
            }
        }

        deserializer.deserialize_str(MaxSizeVisitor)
    }
}

impl Serialize for MaxSize {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

/// A maximum cache-file age, stored as a duration.
///
/// Deserialises from a human-readable string such as `7d` or `1h30m`.
/// Malformed strings are rejected at config load rather than at use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaxAge(Duration);

impl MaxAge {
    /// The age in whole seconds.
    #[must_use]
    pub const fn as_secs(self) -> u64 {
        self.0.as_secs()
    }
}

impl fmt::Display for MaxAge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", humantime::format_duration(self.0))
    }
}

impl<'de> Deserialize<'de> for MaxAge {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct MaxAgeVisitor;

        impl Visitor<'_> for MaxAgeVisitor {
            type Value = MaxAge;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a duration such as \"7d\" or \"1h30m\"")
            }

            fn visit_str<E>(self, value: &str) -> Result<MaxAge, E>
            where
                E: de::Error,
            {
                humantime::parse_duration(value)
                    .map(MaxAge)
                    .map_err(de::Error::custom)
            }
        }

        deserializer.deserialize_str(MaxAgeVisitor)
    }
}

impl Serialize for MaxAge {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&humantime::format_duration(self.0).to_string())
    }
}

/// P2P configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct P2PConfig {
    pub enabled: Option<bool>,
    pub share: Option<Vec<String>>,
}

/// Device configuration (root-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceConfig {
    pub name: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Management-plane capability grants declared for this node. Lets a fleet
    /// provision authority declaratively rather than only imperatively. Like
    /// the rest of `DeviceConfig`, this is root-only — grants in child
    /// `.cascade` files are ignored.
    #[serde(default)]
    pub grants: Vec<GrantConfig>,
}

/// A declarative management-plane grant in the root device config.
///
/// This is the config-level shape only: the `granted_by` device is supplied by
/// the engine at load time (it is the local node owner), and the `capability`
/// string is validated against the engine's capability vocabulary there. The
/// config crate stays free of the engine's domain enums so the dependency
/// direction (engine depends on config, never the reverse) is preserved.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantConfig {
    /// The device this grant authorises, by device ID.
    pub grantee: String,
    /// The capability conferred, in its colon-delimited wire form (for example
    /// `status:read`). Validated by the engine on load.
    pub capability: String,
    /// The scope the capability applies over.
    pub scope: ScopeConfig,
    /// When the grant expires, as an RFC 3339 timestamp. Absent means it never
    /// expires. Parsed by the engine on load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
}

/// The scope of a [`GrantConfig`] — node-wide or a folder subtree.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScopeConfig {
    /// The whole node.
    Node,
    /// A folder subtree, identified by its path prefix.
    Folder {
        /// The folder path prefix.
        path: String,
    },
}

/// Resolved config after walking from mount root to a target directory.
#[derive(Debug, Default, Clone)]
pub struct ResolvedConfig {
    pub ignores: Vec<IgnoreRule>,
    pub lifecycle: Vec<LifecyclePolicy>,
    pub pins: Vec<PinRule>,
    pub unpins: Vec<PinRule>,
    pub cache: Option<CacheConfig>,
    pub p2p: Option<P2PConfig>,
}

impl ResolvedConfig {
    /// Check if a given path should be ignored.
    #[must_use]
    pub fn is_ignored(&self, path: &str, is_dir: bool) -> bool {
        let mut ignored = false;
        for rule in &self.ignores {
            if (!is_dir || !rule.dir_only) && glob_match(&rule.pattern, path) {
                ignored = !rule.negated;
            }
        }
        ignored
    }
}

/// Simple glob matching. Supports `*` (any non-slash) and `**` (any including slashes).
/// Full production matching uses the `ignore` crate from ripgrep; this covers
/// the basic cases needed for Phase 1.
fn glob_match(pattern: &str, path: &str) -> bool {
    if pattern.contains("**") {
        let parts: Vec<&str> = pattern.split("**").collect();
        if let [prefix, suffix] = parts.as_slice() {
            let prefix_ok = prefix.is_empty() || path.starts_with(*prefix);
            let suffix_ok = suffix.is_empty() || path.ends_with(*suffix);
            return prefix_ok && suffix_ok;
        }
    }
    // Single `*` matches any sequence of non-separator characters
    if pattern.contains('*') {
        return star_match(pattern, path);
    }
    pattern == path || path.ends_with(&format!("/{pattern}"))
}

/// Match a pattern with `*` wildcards (no `**`) against a path.
fn star_match(pattern: &str, path: &str) -> bool {
    let segments: Vec<&str> = pattern.split('*').collect();
    if segments.len() == 1 {
        return pattern == path;
    }
    // Must match start and end
    let first = segments.first().copied().unwrap_or("");
    let last = segments.last().copied().unwrap_or("");

    // Strip the fixed prefix from the front of path
    let after_prefix = if first.is_empty() {
        path
    } else if let Some(rest) = path.strip_prefix(first) {
        rest
    } else {
        return false;
    };

    // Strip the fixed suffix from the end of the remaining path
    let middle = if last.is_empty() {
        after_prefix
    } else if let Some(rest) = after_prefix.strip_suffix(last) {
        rest
    } else {
        return false;
    };

    // Check intermediate segments appear in order within `middle`
    let inner_segments = segments.get(1..segments.len() - 1).unwrap_or(&[]);
    let mut remaining = middle;
    for seg in inner_segments {
        if seg.is_empty() {
            continue;
        }
        // Split on the first occurrence of `seg`; advance past it.
        if let Some((_before, after)) = remaining.split_once(seg) {
            remaining = after;
        } else {
            return false;
        }
    }
    true
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a byte-size string the way config load does. Test-only helper.
    fn parse_max_size(s: &str) -> MaxSize {
        let toml = format!("max_size = \"{s}\"");
        let cache: CacheConfig = toml::from_str(&toml).unwrap();
        cache.max_size.unwrap()
    }

    /// Parse a duration string the way config load does. Test-only helper.
    fn parse_max_age(s: &str) -> MaxAge {
        let toml = format!("max_age = \"{s}\"");
        let cache: CacheConfig = toml::from_str(&toml).unwrap();
        cache.max_age.unwrap()
    }

    #[test]
    fn max_size_parses_decimal_and_binary_units() {
        assert_eq!(parse_max_size("5GB").as_bytes(), 5_000_000_000);
        assert_eq!(parse_max_size("512MiB").as_bytes(), 512 * 1024 * 1024);
    }

    #[test]
    fn max_size_rejects_malformed_at_load() {
        let result: Result<CacheConfig, _> = toml::from_str("max_size = \"not-a-size\"");
        assert!(result.is_err());
    }

    #[test]
    fn max_age_parses_compound_durations() {
        assert_eq!(parse_max_age("7d").as_secs(), 7 * 24 * 60 * 60);
        assert_eq!(parse_max_age("1h30m").as_secs(), 90 * 60);
    }

    #[test]
    fn max_age_rejects_malformed_at_load() {
        let result: Result<CacheConfig, _> = toml::from_str("max_age = \"whenever\"");
        assert!(result.is_err());
    }

    #[test]
    fn default_state_parses_known_postures() {
        let cache: CacheConfig = toml::from_str("default_state = \"pinned\"").unwrap();
        assert_eq!(cache.default_state, Some(CacheStatePosture::Pinned));
        let cache: CacheConfig = toml::from_str("default_state = \"online\"").unwrap();
        assert_eq!(cache.default_state, Some(CacheStatePosture::Online));
        let cache: CacheConfig = toml::from_str("default_state = \"auto\"").unwrap();
        assert_eq!(cache.default_state, Some(CacheStatePosture::Auto));
    }

    #[test]
    fn default_state_rejects_unknown_posture() {
        let result: Result<CacheConfig, _> = toml::from_str("default_state = \"sometimes\"");
        assert!(result.is_err());
    }

    #[test]
    fn cascade_config_merge_accumulates_rules() {
        let mut a = CascadeConfig::empty();
        a.ignore.push(IgnoreRule {
            pattern: "*.log".to_string(),
            negated: false,
            dir_only: false,
            conditions: vec![],
        });
        a.cache = Some(CacheConfig {
            max_size: Some(parse_max_size("1GB")),
            max_age: None,
            default_state: None,
        });

        let mut b = CascadeConfig::empty();
        b.ignore.push(IgnoreRule {
            pattern: "!important.log".to_string(),
            negated: true,
            dir_only: false,
            conditions: vec![],
        });
        b.cache = Some(CacheConfig {
            max_size: Some(parse_max_size("5GB")),
            max_age: Some(parse_max_age("7d")),
            default_state: None,
        });

        a.merge(b);
        assert_eq!(a.ignore.len(), 2);
        // Cache is nearest-wins (overridden)
        assert_eq!(
            a.cache.as_ref().unwrap().max_size,
            Some(parse_max_size("5GB"))
        );
    }

    #[test]
    fn device_config_with_grants_deserialises() {
        let toml = r#"
            [device]
            name = "node-a"

            [[device.grants]]
            grantee = "MANAGERDEVICEID"
            capability = "pin:write"
            scope = { kind = "folder", path = "/work" }

            [[device.grants]]
            grantee = "MANAGERDEVICEID"
            capability = "status:read"
            scope = { kind = "node" }
            expires = "2026-12-31T00:00:00Z"
        "#;
        let config: CascadeConfig = toml::from_str(toml).unwrap();
        let device = config.device.unwrap();
        assert_eq!(device.name, "node-a");
        assert_eq!(device.grants.len(), 2);

        let first = &device.grants[0];
        assert_eq!(first.grantee, "MANAGERDEVICEID");
        assert_eq!(first.capability, "pin:write");
        assert!(matches!(&first.scope, ScopeConfig::Folder { path } if path == "/work"));
        assert!(first.expires.is_none());

        let second = &device.grants[1];
        assert!(matches!(second.scope, ScopeConfig::Node));
        assert_eq!(second.expires.as_deref(), Some("2026-12-31T00:00:00Z"));
    }

    #[test]
    fn device_grants_do_not_propagate_through_merge() {
        // Device config (and its grants) is root-only: merging a child config
        // never carries device grants into the parent.
        let mut root = CascadeConfig::empty();
        root.device = Some(DeviceConfig {
            name: "node-a".to_string(),
            tags: vec![],
            grants: vec![GrantConfig {
                grantee: "MANAGER".to_string(),
                capability: "status:read".to_string(),
                scope: ScopeConfig::Node,
                expires: None,
            }],
        });

        let mut child = CascadeConfig::empty();
        child.device = Some(DeviceConfig {
            name: "child".to_string(),
            tags: vec![],
            grants: vec![GrantConfig {
                grantee: "INTRUDER".to_string(),
                capability: "grant:admin".to_string(),
                scope: ScopeConfig::Node,
                expires: None,
            }],
        });

        root.merge(child);
        // The root's device config is untouched by the child's.
        let device = root.device.unwrap();
        assert_eq!(device.name, "node-a");
        assert_eq!(device.grants.len(), 1);
        assert_eq!(device.grants[0].grantee, "MANAGER");
    }

    #[test]
    fn resolved_config_is_ignored() {
        let mut config = ResolvedConfig::default();
        config.ignores.push(IgnoreRule {
            pattern: "*.log".to_string(),
            negated: false,
            dir_only: false,
            conditions: vec![],
        });
        config.ignores.push(IgnoreRule {
            pattern: "keep.log".to_string(),
            negated: true,
            dir_only: false,
            conditions: vec![],
        });

        assert!(config.is_ignored("debug.log", false));
        assert!(!config.is_ignored("keep.log", false));
        assert!(!config.is_ignored("main.rs", false));
    }
}
