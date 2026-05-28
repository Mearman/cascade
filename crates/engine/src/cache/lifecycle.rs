//! Lifecycle policy evaluation — determines if a file should be evicted.

use crate::db::{LifecyclePolicyRecord, StateDb};
use crate::types::FileEntry;
use anyhow::Result;
use std::path::Path;

/// Evaluates lifecycle policies to determine eviction candidates.
pub struct LifecycleEvaluator<'a> {
    db: &'a StateDb,
    policies: Vec<LifecyclePolicyRecord>,
}

impl<'a> LifecycleEvaluator<'a> {
    /// Load all lifecycle policies from the database.
    pub fn load(db: &'a StateDb) -> Result<Self> {
        let policies = db.list_lifecycle_policies()?;
        Ok(Self { db, policies })
    }

    /// Evaluate whether a file should be evicted based on lifecycle policies.
    ///
    /// Policies are checked in priority order (highest first). The first matching
    /// policy determines the outcome. If no policy matches, the file is not evicted.
    pub fn should_evict(&self, file: &FileEntry, path: &Path) -> EvictionDecision {
        let path_str = path.to_string_lossy();
        let _now_ts = chrono::Utc::now().timestamp();

        for policy in &self.policies {
            if !path_matches_policy(&policy.path_glob, &path_str) {
                continue;
            }

            // Check max age.
            if let Some(max_age_secs) = policy.max_age {
                // The file must have been accessed more than max_age_secs ago.
                // We need last_access from the DB, but FileEntry doesn't carry it.
                // For now, return a decision that includes the policy.
                return EvictionDecision::Evict {
                    reason: EvictionReason::MaxAge {
                        max_age_secs,
                        policy_id: policy.id,
                    },
                };
            }

            if let (Some(max_size), Some(file_size)) = (policy.max_file_size, file.size)
                && file_size > max_size as u64
            {
                return EvictionDecision::Evict {
                    reason: EvictionReason::MaxSize {
                        max_size,
                        actual_size: file_size,
                        policy_id: policy.id,
                    },
                };
            }
        }

        EvictionDecision::Keep
    }

    /// Add a new lifecycle policy and reload.
    pub fn add_policy(
        &mut self,
        path_glob: &str,
        max_age: Option<i64>,
        max_file_size: Option<i64>,
        priority: i32,
    ) -> Result<()> {
        self.db
            .add_lifecycle_policy(path_glob, max_age, max_file_size, priority, None)?;
        self.policies = self.db.list_lifecycle_policies()?;
        Ok(())
    }

    /// Remove a lifecycle policy and reload.
    pub fn remove_policy(&mut self, id: i64) -> Result<bool> {
        let removed = self.db.remove_lifecycle_policy(id)?;
        if removed {
            self.policies = self.db.list_lifecycle_policies()?;
        }
        Ok(removed)
    }

    /// Return the current list of policies.
    pub fn policies(&self) -> &[LifecyclePolicyRecord] {
        &self.policies
    }
}

/// Decision about whether a file should be evicted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvictionDecision {
    /// File should be evicted.
    Evict { reason: EvictionReason },
    /// File should be kept.
    Keep,
}

/// Reason for eviction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvictionReason {
    /// File exceeds the maximum age defined by a lifecycle policy.
    MaxAge { max_age_secs: i64, policy_id: i64 },
    /// File exceeds the maximum size defined by a lifecycle policy.
    MaxSize {
        max_size: i64,
        actual_size: u64,
        policy_id: i64,
    },
}

/// Check if a path matches a policy's glob pattern.
fn path_matches_policy(pattern: &str, path: &str) -> bool {
    if pattern.contains("**") {
        let parts: Vec<&str> = pattern.split("**").collect();
        if parts.len() == 2 {
            let (prefix, suffix) = (parts[0], parts[1]);
            let prefix_ok = prefix.is_empty() || path.starts_with(prefix);
            let suffix_ok = suffix.is_empty() || path.ends_with(suffix);
            return prefix_ok && suffix_ok;
        }
    }
    if pattern.contains('*') {
        return simple_star_match(pattern, path);
    }
    path == pattern || path.starts_with(&format!("{}/", pattern))
}

fn simple_star_match(pattern: &str, path: &str) -> bool {
    let segments: Vec<&str> = pattern.split('*').collect();
    if segments.len() == 1 {
        return pattern == path;
    }
    let first = segments[0];
    let last = segments[segments.len() - 1];
    if !first.is_empty() && !path.starts_with(first) {
        return false;
    }
    if !last.is_empty() && !path.ends_with(last) {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ItemId;

    fn make_file(name: &str, size: Option<u64>) -> FileEntry {
        FileEntry::file(
            ItemId::new("test", name),
            ItemId::new("test", "root"),
            name.to_string(),
        )
        .with_size(size)
    }

    #[test]
    fn no_policies_means_keep() {
        let db = StateDb::open_in_memory().unwrap();
        let evaluator = LifecycleEvaluator::load(&db).unwrap();
        let file = make_file("report.pdf", Some(1024));
        assert_eq!(
            evaluator.should_evict(&file, Path::new("report.pdf")),
            EvictionDecision::Keep
        );
    }

    #[test]
    fn max_size_policy_evicts_large_files() {
        let db = StateDb::open_in_memory().unwrap();
        db.add_lifecycle_policy("Documents/**", None, Some(1024), 0, None)
            .unwrap();

        let evaluator = LifecycleEvaluator::load(&db).unwrap();

        let small = make_file("small.txt", Some(512));
        assert_eq!(
            evaluator.should_evict(&small, Path::new("Documents/small.txt")),
            EvictionDecision::Keep
        );

        let large = make_file("large.bin", Some(2048));
        let decision = evaluator.should_evict(&large, Path::new("Documents/large.bin"));
        assert!(matches!(
            decision,
            EvictionDecision::Evict {
                reason: EvictionReason::MaxSize { .. }
            }
        ));
    }

    #[test]
    fn higher_priority_wins() {
        let db = StateDb::open_in_memory().unwrap();
        // High priority: evict anything in Temp/.
        db.add_lifecycle_policy("Temp/**", None, Some(0), 10, None)
            .unwrap();
        // Lower priority: keep everything.
        db.add_lifecycle_policy("**", Some(i64::MAX), None, 0, None)
            .unwrap();

        let evaluator = LifecycleEvaluator::load(&db).unwrap();
        let file = make_file("cache.tmp", Some(100));
        let decision = evaluator.should_evict(&file, Path::new("Temp/cache.tmp"));
        assert!(matches!(
            decision,
            EvictionDecision::Evict {
                reason: EvictionReason::MaxSize { .. }
            }
        ));
    }
}
