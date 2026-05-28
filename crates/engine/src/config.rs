//! Config integration — resolves `.cascade` config for file filtering.
//!
//! The engine uses `cascade-config` to resolve directory-walk configs,
//! then applies ignore rules when processing backend changes.

use cascade_config::ResolvedConfig;
use cascade_expr::context::EvalContext;
use cascade_expr::eval;
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

impl std::fmt::Debug for ConfigResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigResolver")
            .field("mount_root", &self.mount_root)
            .finish_non_exhaustive()
    }
}

impl ConfigResolver {
    #[must_use] pub const fn new(mount_root: PathBuf) -> Self {
        Self {
            mount_root,
            cache: RwLock::new(Vec::new()),
        }
    }

    /// Check if a file at the given path should be ignored based on the
    /// resolved `.cascade` config for its parent directory.
    ///
    /// If an `EvalContext` is provided, conditional rules (those with `conditions`
    /// in the `.cascade` file) are evaluated against it. Rules with conditions
    /// that evaluate to false are skipped.
    pub fn is_ignored(&self, file_path: &Path, is_dir: bool) -> bool {
        self.is_ignored_with_context(file_path, is_dir, None)
    }

    /// Check if a file should be ignored, evaluating conditional rules against
    /// the provided context.
    pub fn is_ignored_with_context(
        &self,
        file_path: &Path,
        is_dir: bool,
        ctx: Option<&EvalContext>,
    ) -> bool {
        let parent = file_path.parent().unwrap_or_else(|| Path::new("/"));
        let config = self.resolve_for_dir(parent);

        // If no context, use the standard is_ignored (which ignores conditions).
        let Some(ctx) = ctx else {
            let name = file_path
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            return config.is_ignored(&name, is_dir);
        };

        // With context: evaluate each rule's conditions.
        let name = file_path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());

        let mut ignored = false;
        for rule in &config.ignores {
            // Evaluate conditions.
            let conditions_met =
                rule.conditions.is_empty() || rule.conditions.iter().all(|cond| {
                    match eval::parse_expr(cond) {
                        Ok(expr) => eval::evaluate(&expr, ctx),
                        Err(e) => {
                            tracing::warn!(expr = %cond, error = %e, "failed to parse condition");
                            false
                        }
                    }
                });

            if conditions_met && (!is_dir || !rule.dir_only) && glob_match(&rule.pattern, &name) {
                ignored = !rule.negated;
            }
        }
        ignored
    }

    /// Resolve the config for a directory, using the cache if available.
    fn resolve_for_dir(&self, dir: &Path) -> ResolvedConfig {
        // Check cache first.
        {
            let cache = self.cache.read().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((_, config)) = cache.iter().find(|(p, _)| p == dir) {
                return config.clone();
            }
        }

        // Resolve from the cascade-config crate.
        let config = cascade_config::merge::resolve(&self.mount_root, dir);

        // Cache the result.
        {
            let mut cache = self.cache.write().unwrap_or_else(std::sync::PoisonError::into_inner);
            cache.push((dir.to_path_buf(), config.clone()));
        }

        config
    }

    /// Invalidate cached configs for a directory and its children.
    /// Call this when `.cascade` files are created or modified.
    pub fn invalidate(&self, dir: &Path) {
        let mut cache = self.cache.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.retain(|(p, _)| !p.starts_with(dir));
    }
}

/// Simple glob matching. Supports `*` (any non-slash) and `**` (any including slashes).
fn glob_match(pattern: &str, path: &str) -> bool {
    if pattern.contains("**") {
        let parts: Vec<&str> = pattern.split("**").collect();
        if parts.len() == 2 {
            let prefix = parts.first().copied().unwrap_or("");
            let suffix = parts.get(1).copied().unwrap_or("");
            let prefix_ok = prefix.is_empty() || path.starts_with(prefix);
            let suffix_ok = suffix.is_empty() || path.ends_with(suffix);
            return prefix_ok && suffix_ok;
        }
    }
    if pattern.contains('*') {
        return star_match(pattern, path);
    }
    pattern == path || path.ends_with(&format!("/{pattern}"))
}

fn star_match(pattern: &str, path: &str) -> bool {
    let segments: Vec<&str> = pattern.split('*').collect();
    if segments.len() == 1 {
        return pattern == path;
    }
    let first = segments.first().copied().unwrap_or("");
    let last = segments.last().copied().unwrap_or("");
    let mut idx = 0;
    if !first.is_empty() {
        let rest = path.get(idx..).unwrap_or("");
        if !rest.starts_with(first) {
            return false;
        }
        idx += first.len();
    }
    if !last.is_empty() && !path.ends_with(last) {
        return false;
    }
    let end = if last.is_empty() {
        path.len()
    } else {
        path.len().saturating_sub(last.len())
    };
    let remaining = path.get(idx..end).unwrap_or("");
    let mut search_from = 0;
    let middle = segments.get(1..segments.len().saturating_sub(1)).unwrap_or(&[]);
    for seg in middle {
        if seg.is_empty() {
            continue;
        }
        let rest = remaining.get(search_from..).unwrap_or("");
        if let Some(pos) = rest.find(seg) {
            search_from += pos + seg.len();
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
    fn config_resolver_caches_results() {
        let resolver = ConfigResolver::new(PathBuf::from("/tmp/test-mount"));
        // Both calls should succeed (even if there's no actual .cascade file).
        let path = Path::new("/tmp/test-mount/Documents/notes.txt");
        let _ = resolver.is_ignored(path, false);
        let _ = resolver.is_ignored(path, false);

        let cache = resolver.cache.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn config_resolver_invalidate() {
        let resolver = ConfigResolver::new(PathBuf::from("/tmp/test-mount"));
        let path = Path::new("/tmp/test-mount/Documents/notes.txt");
        let _ = resolver.is_ignored(path, false);

        resolver.invalidate(Path::new("/tmp/test-mount/Documents"));

        let cache = resolver.cache.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(cache.is_empty());
    }
}
