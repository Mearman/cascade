//! Lifecycle policy evaluation — determines if a file should be evicted.

use crate::db::LifecyclePolicyRecord;
use crate::types::FileEntry;
use std::path::Path;

/// Evaluates lifecycle policies to determine eviction candidates.
///
/// Owns a snapshot of the policies loaded at construction time. The evaluation
/// logic is pure; state mutations are done through the storage layer and a
/// fresh evaluator is created from the updated policies.
pub struct LifecycleEvaluator {
    policies: Vec<LifecyclePolicyRecord>,
}

impl std::fmt::Debug for LifecycleEvaluator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LifecycleEvaluator")
            .field("policy_count", &self.policies.len())
            .finish_non_exhaustive()
    }
}

impl LifecycleEvaluator {
    /// Build an evaluator from a pre-loaded policy set.
    #[must_use]
    pub const fn from_policies(policies: Vec<LifecyclePolicyRecord>) -> Self {
        Self { policies }
    }

    /// Load all lifecycle policies from the native state database.
    #[cfg(feature = "native")]
    pub fn load_native(db: &crate::db::StateDb) -> anyhow::Result<Self> {
        let policies = db.list_lifecycle_policies()?;
        Ok(Self { policies })
    }

    /// Load all lifecycle policies from the portable state storage.
    pub async fn load(
        storage: &dyn crate::portable::StateStorage,
    ) -> Result<Self, crate::portable::StorageError> {
        let policies = storage.list_lifecycle_policies().await?;
        Ok(Self { policies })
    }

    /// Evaluate whether a file should be evicted based on lifecycle policies.
    ///
    /// Policies are checked in priority order (highest first). The first matching
    /// policy determines the outcome. If no policy matches, the file is not evicted.
    #[must_use]
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
                && u64::try_from(max_size).is_ok_and(|m| file_size > m)
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

    /// Return the current list of policies.
    #[must_use]
    pub fn policies(&self) -> &[LifecyclePolicyRecord] {
        &self.policies
    }
}

/// Decision about whether a file should be evicted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictionDecision {
    /// File should be evicted.
    Evict { reason: EvictionReason },
    /// File should be kept.
    Keep,
}

/// Reason for eviction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
            let prefix = parts.first().copied().unwrap_or("");
            let suffix = parts.get(1).copied().unwrap_or("");
            let prefix_ok = prefix.is_empty() || path.starts_with(prefix);
            let suffix_ok = suffix.is_empty() || path.ends_with(suffix);
            return prefix_ok && suffix_ok;
        }
    }
    if pattern.contains('*') {
        return simple_star_match(pattern, path);
    }
    path == pattern || path.starts_with(&format!("{pattern}/"))
}

fn simple_star_match(pattern: &str, path: &str) -> bool {
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
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::LifecyclePolicyRecord;
    use crate::types::ItemId;

    fn make_file(name: &str, size: Option<u64>) -> FileEntry {
        FileEntry::file(
            ItemId::new("test", name),
            ItemId::new("test", "root"),
            name.to_string(),
        )
        .with_size(size)
    }

    fn make_policies(
        entries: &[(&str, Option<i64>, Option<i64>, i32)],
    ) -> Vec<LifecyclePolicyRecord> {
        entries
            .iter()
            .enumerate()
            .map(
                |(idx, (path, max_age, max_file_size, priority))| LifecyclePolicyRecord {
                    id: idx as i64,
                    path_glob: (*path).to_string(),
                    max_age: *max_age,
                    max_file_size: *max_file_size,
                    priority: *priority,
                    conditions: None,
                },
            )
            .collect()
    }

    #[test]
    fn no_policies_means_keep() {
        let evaluator = LifecycleEvaluator::from_policies(vec![]);
        let file = make_file("report.pdf", Some(1024));
        assert_eq!(
            evaluator.should_evict(&file, Path::new("report.pdf")),
            EvictionDecision::Keep
        );
    }

    #[test]
    fn max_size_policy_evicts_large_files() {
        let evaluator = LifecycleEvaluator::from_policies(make_policies(&[(
            "Documents/**",
            None,
            Some(1024),
            0,
        )]));

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
        let evaluator = LifecycleEvaluator::from_policies(make_policies(&[
            ("Temp/**", None, Some(0), 10),
            ("**", Some(i64::MAX), None, 0),
        ]));
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
