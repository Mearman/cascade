//! Cascade config types — shared across all four format parsers.

use serde::{Deserialize, Serialize};

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
    #[must_use] pub fn empty() -> Self {
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    pub max_size: Option<String>,
    pub max_age: Option<String>,
    pub default_state: Option<String>,
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
    #[must_use] pub fn is_ignored(&self, path: &str, is_dir: bool) -> bool {
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
            max_size: Some("1GB".to_string()),
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
            max_size: Some("5GB".to_string()),
            max_age: Some("7d".to_string()),
            default_state: None,
        });

        a.merge(b);
        assert_eq!(a.ignore.len(), 2);
        // Cache is nearest-wins (overridden)
        assert_eq!(a.cache.as_ref().unwrap().max_size, Some("5GB".to_string()));
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
