//! Config integration — resolves `.cascade` config for file filtering.
//!
//! The engine uses `cascade-config` to resolve directory-walk configs,
//! then applies ignore rules when processing backend changes.

use cascade_config::ResolvedConfig;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// Manages resolved configs for the VFS tree.
///
/// Configs are resolved lazily and cached. The mount root is used as
/// the starting point for the directory walk.
pub struct ConfigResolver {
    mount_root: PathBuf,
    cache: RwLock<Vec<(PathBuf, ResolvedConfig)>>,
}

impl ConfigResolver {
    pub fn new(mount_root: PathBuf) -> Self {
        Self {
            mount_root,
            cache: RwLock::new(Vec::new()),
        }
    }

    /// Check if a file at the given path should be ignored based on the
    /// resolved `.cascade` config for its parent directory.
    pub fn is_ignored(&self, file_path: &Path, is_dir: bool) -> bool {
        let parent = file_path.parent().unwrap_or(Path::new("/"));
        let config = self.resolve_for_dir(parent);
        let name = file_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        config.is_ignored(&name, is_dir)
    }

    /// Resolve the config for a directory, using the cache if available.
    fn resolve_for_dir(&self, dir: &Path) -> ResolvedConfig {
        // Check cache first.
        {
            let cache = self.cache.read().unwrap();
            if let Some((_, config)) = cache.iter().find(|(p, _)| p == dir) {
                return config.clone();
            }
        }

        // Resolve from the cascade-config crate.
        let config = cascade_config::merge::resolve(&self.mount_root, dir);

        // Cache the result.
        {
            let mut cache = self.cache.write().unwrap();
            cache.push((dir.to_path_buf(), config.clone()));
        }

        config
    }

    /// Invalidate cached configs for a directory and its children.
    /// Call this when `.cascade` files are created or modified.
    pub fn invalidate(&self, dir: &Path) {
        let mut cache = self.cache.write().unwrap();
        cache.retain(|(p, _)| !p.starts_with(dir));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_resolver_caches_results() {
        let resolver = ConfigResolver::new(PathBuf::from("/tmp/test-mount"));
        // Both calls should succeed (even if there's no actual .cascade file).
        let path = Path::new("/tmp/test-mount/Documents/notes.txt");
        let _ = resolver.is_ignored(path, false);
        let _ = resolver.is_ignored(path, false);

        let cache = resolver.cache.read().unwrap();
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn config_resolver_invalidate() {
        let resolver = ConfigResolver::new(PathBuf::from("/tmp/test-mount"));
        let path = Path::new("/tmp/test-mount/Documents/notes.txt");
        let _ = resolver.is_ignored(path, false);

        resolver.invalidate(Path::new("/tmp/test-mount/Documents"));

        let cache = resolver.cache.read().unwrap();
        assert!(cache.is_empty());
    }
}
