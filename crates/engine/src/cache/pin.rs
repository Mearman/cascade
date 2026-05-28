//! Pin matching — determines if a path is covered by any pin rule.

use crate::db::{PinRuleRecord, StateDb};
use anyhow::Result;
use std::path::Path;

/// Matches file paths against pin rules stored in the state DB.
pub struct PinMatcher<'a> {
    db: &'a StateDb,
    rules: Vec<PinRuleRecord>,
}

impl std::fmt::Debug for PinMatcher<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinMatcher")
            .field("rule_count", &self.rules.len())
            .finish_non_exhaustive()
    }
}

impl<'a> PinMatcher<'a> {
    /// Load all pin rules from the database.
    pub fn load(db: &'a StateDb) -> Result<Self> {
        let rules = db.list_pin_rules()?;
        Ok(Self { db, rules })
    }

    /// Check if a path matches any pin rule.
    #[must_use]
    pub fn is_pinned(&self, path: &Path) -> bool {
        let path_str = path.to_string_lossy();
        self.rules.iter().any(|rule| {
            if rule.recursive {
                // Recursive: exact, prefix, or glob match.
                path_str == rule.path_glob
                    || path_str.starts_with(&format!("{}/", rule.path_glob))
                    || glob_match_exact(&rule.path_glob, &path_str)
            } else {
                // Non-recursive: exact match or glob match only (no prefix).
                path_str == rule.path_glob || glob_match_exact(&rule.path_glob, &path_str)
            }
        })
    }

    /// Add a new pin rule and reload.
    pub fn add_rule(&mut self, path_glob: &str, recursive: bool) -> Result<()> {
        self.db.add_pin_rule(path_glob, recursive, None)?;
        self.rules = self.db.list_pin_rules()?;
        Ok(())
    }

    /// Remove a pin rule and reload.
    pub fn remove_rule(&mut self, path_glob: &str) -> Result<bool> {
        let removed = self.db.remove_pin_rule(path_glob)?;
        if removed {
            self.rules = self.db.list_pin_rules()?;
        }
        Ok(removed)
    }

    /// Return the current list of rules.
    #[must_use]
    pub fn rules(&self) -> &[PinRuleRecord] {
        &self.rules
    }
}

/// Simple glob matching for path patterns.
/// Supports `*` (any non-slash) and `**` (any including slashes).
fn glob_match_exact(pattern: &str, path: &str) -> bool {
    if pattern.contains("**") {
        let parts: Vec<&str> = pattern.split("**").collect();
        if parts.len() == 2 {
            let prefix = parts.first().copied().unwrap_or("");
            let suffix = parts.get(1).copied().unwrap_or("");
            let prefix_ok = prefix.is_empty() || path.starts_with(prefix);
            if !prefix_ok {
                return false;
            }
            // ** matches zero or more path segments.
            // Strip leading / from suffix — the separator is implicit.
            let suffix = suffix.strip_prefix('/').unwrap_or(suffix);
            if suffix.is_empty() {
                return true;
            }
            // The part after the prefix.
            let after_prefix = path.get(prefix.len()..).unwrap_or("");
            let after_prefix = after_prefix.strip_prefix('/').unwrap_or(after_prefix);
            if after_prefix.is_empty() {
                return false;
            }
            // Find the last segment and match it against the suffix pattern.
            // The ** matches everything between prefix and the last segment(s).
            if suffix.contains('/') {
                // Multi-segment suffix: find the rightmost occurrence.
                let trimmed_suffix = suffix.trim_start_matches('*');
                if let Some(pos) = after_prefix.rfind(trimmed_suffix) {
                    let from_pos = after_prefix.get(pos..).unwrap_or("");
                    if from_pos.ends_with(trimmed_suffix) {
                        return true;
                    }
                    let tail_len = suffix.len().min(after_prefix.len());
                    let tail_start = after_prefix.len() - tail_len;
                    let tail = after_prefix.get(tail_start..).unwrap_or("");
                    return star_match_path(suffix, tail);
                }
                return false;
            }
            // Single-segment suffix: match the last path segment.
            let last_segment = after_prefix.rsplit('/').next().unwrap_or(after_prefix);
            if suffix.contains('*') {
                return star_match_path(suffix, last_segment);
            }
            return last_segment == suffix;
        }
    }
    if pattern.contains('*') {
        return star_match_path(pattern, path);
    }
    pattern == path
}

/// Match a pattern with `*` wildcards (no `**`) against a path.
fn star_match_path(pattern: &str, path: &str) -> bool {
    let segments: Vec<&str> = pattern.split('*').collect();
    if segments.len() == 1 {
        return pattern == path;
    }
    let first = segments.first().copied().unwrap_or("");
    let last = segments.last().copied().unwrap_or("");
    if !first.is_empty() && !path.starts_with(first) {
        return false;
    }
    if !last.is_empty() && !path.ends_with(last) {
        return false;
    }
    let start = if first.is_empty() { 0 } else { first.len() };
    let end = if last.is_empty() {
        path.len()
    } else {
        path.len().saturating_sub(last.len())
    };
    if start > end {
        return false;
    }
    let remaining = path.get(start..end).unwrap_or("");
    let mut search_from = 0;
    let middle = segments
        .get(1..segments.len().saturating_sub(1))
        .unwrap_or(&[]);
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
    fn exact_match_is_pinned() {
        let db = StateDb::open_in_memory().unwrap();
        db.add_pin_rule("Documents/report.pdf", true, None).unwrap();
        let matcher = PinMatcher::load(&db).unwrap();
        assert!(matcher.is_pinned(Path::new("Documents/report.pdf")));
        assert!(!matcher.is_pinned(Path::new("Documents/other.pdf")));
    }

    #[test]
    fn recursive_match_covers_children() {
        let db = StateDb::open_in_memory().unwrap();
        db.add_pin_rule("Documents", true, None).unwrap();
        let matcher = PinMatcher::load(&db).unwrap();
        assert!(matcher.is_pinned(Path::new("Documents")));
        assert!(matcher.is_pinned(Path::new("Documents/report.pdf")));
        assert!(matcher.is_pinned(Path::new("Documents/Projects/code.rs")));
        assert!(!matcher.is_pinned(Path::new("Photos/img.jpg")));
    }

    #[test]
    fn non_recursive_does_not_cover_children() {
        let db = StateDb::open_in_memory().unwrap();
        db.add_pin_rule("Documents", false, None).unwrap();
        let matcher = PinMatcher::load(&db).unwrap();
        assert!(matcher.is_pinned(Path::new("Documents")));
        assert!(!matcher.is_pinned(Path::new("Documents/report.pdf")));
    }

    #[test]
    fn glob_pattern_match() {
        let db = StateDb::open_in_memory().unwrap();
        db.add_pin_rule("Documents/**/*.pdf", true, None).unwrap();
        let matcher = PinMatcher::load(&db).unwrap();
        assert!(matcher.is_pinned(Path::new("Documents/report.pdf")));
        assert!(matcher.is_pinned(Path::new("Documents/Projects/plan.pdf")));
        assert!(!matcher.is_pinned(Path::new("Documents/report.txt")));
    }
}
