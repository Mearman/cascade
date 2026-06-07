//! Config merge — directory walk producing a [`ResolvedConfig`].

use std::path::{Path, PathBuf};

use crate::parse::load_dir;
use crate::types::{CascadeConfig, ResolvedConfig};

/// Walk from mount root to target directory, layering `.cascade` configs
/// with child-overrides-parent precedence.
#[must_use]
pub fn resolve(mount_root: &Path, target_dir: &Path) -> ResolvedConfig {
    let mut builder = ResolvedConfigBuilder::new();

    for dir in ancestors_between(mount_root, target_dir) {
        if let Some(config) = load_dir(&dir) {
            builder.apply(config);
        }
    }

    builder.build()
}

/// Resolve config from an explicit list of configs (for testing).
#[must_use]
pub fn resolve_from_configs(configs: Vec<CascadeConfig>) -> ResolvedConfig {
    let mut builder = ResolvedConfigBuilder::new();
    for config in configs {
        builder.apply(config);
    }
    builder.build()
}

/// Collect directories from `mount_root` down to `target_dir` (inclusive).
fn ancestors_between(root: &Path, target: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    // Walk up from target to root, collecting, then reverse
    let mut current = Some(target);
    while let Some(dir) = current {
        if dir == root || dir.starts_with(root) {
            dirs.push(dir.to_path_buf());
        }
        if dir == root {
            break;
        }
        current = dir.parent();
    }

    dirs.reverse();
    dirs
}

/// Builder that applies configs in order, respecting merge semantics.
struct ResolvedConfigBuilder {
    config: ResolvedConfig,
}

impl ResolvedConfigBuilder {
    fn new() -> Self {
        Self {
            config: ResolvedConfig::default(),
        }
    }

    /// Apply a config layer. Rules accumulate; scalar settings use nearest-wins.
    fn apply(&mut self, layer: CascadeConfig) {
        // Ignore rules accumulate
        self.config.ignores.extend(layer.ignore);

        // Lifecycle policies accumulate (child-first evaluation order)
        self.config.lifecycle.extend(layer.lifecycle);

        // Pin/unpin rules accumulate
        self.config.pins.extend(layer.pin);
        self.config.unpins.extend(layer.unpin);

        // Max file length rules accumulate
        self.config.max_file_length.extend(layer.max_file_length);

        // Cache settings: nearest-wins (child overrides parent)
        if layer.cache.is_some() {
            self.config.cache = layer.cache;
        }

        // P2P: nearest-wins for folder-level config
        if layer.p2p.is_some() {
            self.config.p2p = layer.p2p;
        }

        // Device config is root-only — child configs are ignored
    }

    fn build(self) -> ResolvedConfig {
        self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CacheConfig, IgnoreRule, P2PConfig, PinRule};

    #[test]
    fn ancestors_between_collects_root_to_target() {
        let root = Path::new("/mount");
        let target = Path::new("/mount/Work/Projects");
        let dirs = ancestors_between(root, target);
        assert_eq!(dirs.len(), 3);
        assert_eq!(dirs[0], PathBuf::from("/mount"));
        assert_eq!(dirs[1], PathBuf::from("/mount/Work"));
        assert_eq!(dirs[2], PathBuf::from("/mount/Work/Projects"));
    }

    #[test]
    fn ancestors_between_root_equals_target() {
        let root = Path::new("/mount");
        let dirs = ancestors_between(root, root);
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0], PathBuf::from("/mount"));
    }

    #[test]
    fn resolve_accumulates_ignores() {
        let configs = vec![
            {
                let mut c = CascadeConfig::empty();
                c.ignore.push(IgnoreRule {
                    pattern: "*.log".to_string(),
                    negated: false,
                    dir_only: false,
                    conditions: vec![],
                });
                c
            },
            {
                let mut c = CascadeConfig::empty();
                c.ignore.push(IgnoreRule {
                    pattern: "build/".to_string(),
                    negated: false,
                    dir_only: true,
                    conditions: vec![],
                });
                c
            },
        ];
        let resolved = resolve_from_configs(configs);
        assert_eq!(resolved.ignores.len(), 2);
    }

    #[test]
    fn resolve_nearest_wins_for_cache() {
        let cache_1gb: CacheConfig = toml::from_str("max_size = \"1GB\"").unwrap();
        let cache_5gb: CacheConfig =
            toml::from_str("max_size = \"5GB\"\nmax_age = \"7d\"").unwrap();
        let configs = vec![
            {
                let mut c = CascadeConfig::empty();
                c.cache = Some(cache_1gb);
                c
            },
            {
                let mut c = CascadeConfig::empty();
                c.cache = Some(cache_5gb);
                c
            },
        ];
        let resolved = resolve_from_configs(configs);
        // Second config wins
        assert_eq!(resolved.cache.unwrap().max_size, cache_5gb.max_size);
    }

    #[test]
    fn resolve_accumulates_pins_and_unpins() {
        let pin = |path: &str| PinRule {
            path: path.to_string(),
            conditions: vec![],
        };
        let configs = vec![
            {
                let mut c = CascadeConfig::empty();
                c.pin.push(pin("Documents/Accounts"));
                c
            },
            {
                let mut c = CascadeConfig::empty();
                c.pin.push(pin("Work/Active"));
                c.unpin.push(pin("Work/Active/archive"));
                c
            },
        ];
        let resolved = resolve_from_configs(configs);
        assert_eq!(resolved.pins.len(), 2, "pins from both layers accumulate");
        assert_eq!(resolved.unpins.len(), 1);
    }

    #[test]
    fn resolve_nearest_wins_for_p2p() {
        let parent: P2PConfig = toml::from_str("enabled = false").unwrap();
        let child: P2PConfig = toml::from_str("enabled = true").unwrap();
        let configs = vec![
            {
                let mut c = CascadeConfig::empty();
                c.p2p = Some(parent);
                c
            },
            {
                let mut c = CascadeConfig::empty();
                c.p2p = Some(child);
                c
            },
        ];
        let resolved = resolve_from_configs(configs);
        assert_eq!(
            resolved.p2p.and_then(|p| p.enabled),
            Some(true),
            "the child (nearest) p2p config wins"
        );
    }

    #[test]
    fn ancestors_between_target_outside_root_is_empty() {
        // A target that is not under the mount root yields no directories, so
        // resolve() applies no layers rather than walking arbitrary parents.
        let dirs = ancestors_between(Path::new("/mount"), Path::new("/elsewhere/x"));
        assert!(dirs.is_empty());
    }

    #[test]
    fn is_ignored_applies_last_match_and_negation() {
        let rule = |pattern: &str, negated: bool| IgnoreRule {
            pattern: pattern.to_string(),
            negated,
            dir_only: false,
            conditions: vec![],
        };
        let mut c = CascadeConfig::empty();
        // Ignore everything, then re-include *.rs — last match wins.
        c.ignore.push(rule("*", false));
        c.ignore.push(rule("*.rs", true));
        let resolved = resolve_from_configs(vec![c]);
        assert!(resolved.is_ignored("notes.txt", false), "covered by '*'");
        assert!(
            !resolved.is_ignored("main.rs", false),
            "re-included by negated '*.rs'"
        );
    }
}
