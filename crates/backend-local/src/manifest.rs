//! Manifest — tracks file state for change detection.
//!
//! The manifest is a JSONL sidecar file stored at `<root>/.cascade-cache/manifest.jsonl`.
//! Each line records one file's state: `{path, mtime_secs, mtime_nanos, size, hash}`.
//! On each `changes()` call, the backend walks the directory tree and diffs
//! against the manifest to detect new, modified, and deleted files.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Recorded state for a single file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileState {
    /// Path relative to the backend root.
    pub path: String,
    /// Modification time — seconds since epoch.
    pub mtime_secs: i64,
    /// Modification time — nanosecond fraction.
    pub mtime_nanos: u32,
    /// File size in bytes.
    pub size: u64,
    /// Hex-encoded SHA-256 hash of the file contents.
    pub hash: String,
}

/// The manifest maps relative paths to their last-known state.
#[derive(Debug, Clone, Default)]
pub struct Manifest {
    entries: HashMap<String, FileState>,
}

impl Manifest {
    /// Load a manifest from a JSONL file. Returns an empty manifest if the
    /// file does not exist.
    pub async fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let content = tokio::fs::read_to_string(path).await?;
        let mut entries = HashMap::new();

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<FileState>(line) {
                Ok(state) => {
                    entries.insert(state.path.clone(), state);
                }
                Err(e) => {
                    tracing::warn!(line, "skipping malformed manifest entry: {e}");
                }
            }
        }

        Ok(Self { entries })
    }

    /// Save the manifest to a JSONL file. Creates parent directories as needed.
    pub async fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Sort entries by path for deterministic output.
        let mut sorted: Vec<&FileState> = self.entries.values().collect();
        sorted.sort_by(|a, b| a.path.cmp(&b.path));

        let mut content = String::new();
        for entry in sorted {
            content.push_str(&serde_json::to_string(entry)?);
            content.push('\n');
        }

        tokio::fs::write(path, content).await?;
        Ok(())
    }

    /// Diff the current filesystem state against this manifest.
    ///
    /// Returns three sets:
    /// - `created`: files present on disk but not in the manifest
    /// - `modified`: files present on disk with a changed mtime or size (re-hash confirmed)
    /// - `deleted`: files in the manifest but not present on disk
    #[must_use]
    pub fn diff(&self, current: &[FileState]) -> DiffResult {
        let current_paths: HashSet<&str> = current.iter().map(|s| s.path.as_str()).collect();
        let mut created = Vec::new();
        let mut modified = Vec::new();
        let mut deleted = Vec::new();

        // Find created and modified.
        for state in current {
            match self.entries.get(&state.path) {
                None => {
                    created.push(state.clone());
                }
                Some(old) => {
                    // Quick check: if mtime and size match, the file is unchanged.
                    if old.mtime_secs != state.mtime_secs
                        || old.mtime_nanos != state.mtime_nanos
                        || old.size != state.size
                    {
                        // Mtime or size changed — verify with hash comparison.
                        if old.hash != state.hash {
                            modified.push((old.clone(), state.clone()));
                        }
                    }
                }
            }
        }

        // Find deleted — in manifest but not on disk.
        for (path, state) in &self.entries {
            if !current_paths.contains(path.as_str()) {
                deleted.push(state.clone());
            }
        }

        DiffResult {
            created,
            modified,
            deleted,
        }
    }

    /// Update the manifest with a set of current file states, replacing any
    /// existing entries for those paths.
    pub fn update(&mut self, states: &[FileState]) {
        for state in states {
            self.entries.insert(state.path.clone(), state.clone());
        }
    }

    /// Remove entries for the given paths from the manifest.
    pub fn remove(&mut self, paths: &[&str]) {
        for path in paths {
            self.entries.remove(*path);
        }
    }

    /// Look up a file state by relative path.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&FileState> {
        self.entries.get(path)
    }

    /// Number of entries in the manifest.
    #[cfg(test)]
    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }
}

/// Result of diffing the filesystem against the manifest.
#[derive(Debug)]
pub struct DiffResult {
    pub created: Vec<FileState>,
    pub modified: Vec<(FileState, FileState)>,
    pub deleted: Vec<FileState>,
}

/// Compute the SHA-256 hash of a file's contents.
pub async fn hash_file(path: &Path) -> anyhow::Result<String> {
    let data = tokio::fs::read(path).await?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    Ok(hex::encode(hasher.finalize()))
}

/// Walk a directory tree and collect `FileState` for every file found.
/// Skips the `.cascade-cache` directory and any `.cascade` config files.
/// Directories are not recorded — only files.
pub async fn walk_tree(root: &Path) -> anyhow::Result<Vec<FileState>> {
    let mut states = Vec::new();

    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !should_skip_entry(e.path(), root))
    {
        let entry = entry?;
        let path = entry.path();

        if !path.is_file() {
            continue;
        }

        let relative = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

        let metadata = tokio::fs::metadata(path).await?;
        let modified = metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let mtime_secs = i64::try_from(modified.as_secs()).unwrap_or(i64::MAX);
        let mtime_nanos = modified.subsec_nanos();
        let size = metadata.len();

        let hash = hash_file(path).await?;

        states.push(FileState {
            path: relative,
            mtime_secs,
            mtime_nanos,
            size,
            hash,
        });
    }

    Ok(states)
}

/// Whether a path should be skipped during directory walking.
#[must_use]
pub fn should_skip_entry(path: &Path, root: &Path) -> bool {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let components: Vec<&str> = relative
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();

    // Skip the manifest cache directory.
    if components.first() == Some(&".cascade-cache") {
        return true;
    }

    // Skip .cascade config files (they are not content).
    if components
        .last()
        .is_some_and(|name| name.starts_with(".cascade"))
    {
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_diff_detects_created() {
        let manifest = Manifest::default();
        let current = vec![FileState {
            path: "hello.txt".to_string(),
            mtime_secs: 100,
            mtime_nanos: 0,
            size: 5,
            hash: "abc".to_string(),
        }];
        let diff = manifest.diff(&current);
        assert_eq!(diff.created.len(), 1);
        assert_eq!(diff.created[0].path, "hello.txt");
        assert!(diff.modified.is_empty());
        assert!(diff.deleted.is_empty());
    }

    #[test]
    fn manifest_diff_detects_modified() {
        let old_state = FileState {
            path: "hello.txt".to_string(),
            mtime_secs: 100,
            mtime_nanos: 0,
            size: 5,
            hash: "abc".to_string(),
        };
        let mut manifest = Manifest::default();
        manifest.update(&[old_state]);

        let new_state = FileState {
            path: "hello.txt".to_string(),
            mtime_secs: 200,
            mtime_nanos: 0,
            size: 6,
            hash: "def".to_string(),
        };
        let diff = manifest.diff(&[new_state]);
        assert!(diff.created.is_empty());
        assert_eq!(diff.modified.len(), 1);
        assert_eq!(diff.modified[0].1.hash, "def");
        assert!(diff.deleted.is_empty());
    }

    #[test]
    fn manifest_diff_detects_deleted() {
        let old_state = FileState {
            path: "hello.txt".to_string(),
            mtime_secs: 100,
            mtime_nanos: 0,
            size: 5,
            hash: "abc".to_string(),
        };
        let mut manifest = Manifest::default();
        manifest.update(&[old_state]);

        let diff = manifest.diff(&[]);
        assert!(diff.created.is_empty());
        assert!(diff.modified.is_empty());
        assert_eq!(diff.deleted.len(), 1);
        assert_eq!(diff.deleted[0].path, "hello.txt");
    }

    #[test]
    fn manifest_diff_unchanged_file_not_reported() {
        let state = FileState {
            path: "hello.txt".to_string(),
            mtime_secs: 100,
            mtime_nanos: 0,
            size: 5,
            hash: "abc".to_string(),
        };
        let mut manifest = Manifest::default();
        manifest.update(std::slice::from_ref(&state));

        let diff = manifest.diff(&[state]);
        assert!(diff.created.is_empty());
        assert!(diff.modified.is_empty());
        assert!(diff.deleted.is_empty());
    }

    #[test]
    fn manifest_diff_mtime_change_but_same_hash_not_modified() {
        let state = FileState {
            path: "hello.txt".to_string(),
            mtime_secs: 100,
            mtime_nanos: 0,
            size: 5,
            hash: "abc".to_string(),
        };
        let mut manifest = Manifest::default();
        manifest.update(&[state]);

        // Mtime changed but hash is the same — not modified.
        let current = FileState {
            path: "hello.txt".to_string(),
            mtime_secs: 200,
            mtime_nanos: 0,
            size: 5,
            hash: "abc".to_string(),
        };
        let diff = manifest.diff(&[current]);
        assert!(diff.created.is_empty());
        assert!(diff.modified.is_empty());
        assert!(diff.deleted.is_empty());
    }

    #[test]
    fn manifest_update_replaces_existing() {
        let state = FileState {
            path: "hello.txt".to_string(),
            mtime_secs: 100,
            mtime_nanos: 0,
            size: 5,
            hash: "abc".to_string(),
        };
        let mut manifest = Manifest::default();
        manifest.update(&[state]);

        let updated = FileState {
            path: "hello.txt".to_string(),
            mtime_secs: 200,
            mtime_nanos: 0,
            size: 6,
            hash: "def".to_string(),
        };
        manifest.update(&[updated]);

        assert_eq!(manifest.entry_count(), 1);
        assert_eq!(manifest.get("hello.txt").unwrap().hash, "def");
    }

    #[test]
    fn manifest_remove_deletes_entries() {
        let state = FileState {
            path: "hello.txt".to_string(),
            mtime_secs: 100,
            mtime_nanos: 0,
            size: 5,
            hash: "abc".to_string(),
        };
        let mut manifest = Manifest::default();
        manifest.update(&[state]);
        assert_eq!(manifest.entry_count(), 1);

        manifest.remove(&["hello.txt"]);
        assert_eq!(manifest.entry_count(), 0);
    }

    #[tokio::test]
    async fn manifest_save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.jsonl");

        let state = FileState {
            path: "hello.txt".to_string(),
            mtime_secs: 100,
            mtime_nanos: 500,
            size: 42,
            hash: "deadbeef".to_string(),
        };

        let mut manifest = Manifest::default();
        manifest.update(&[state]);
        manifest.save(&path).await.unwrap();

        let loaded = Manifest::load(&path).await.unwrap();
        assert_eq!(loaded.entry_count(), 1);
        let loaded_state = loaded.get("hello.txt").unwrap();
        assert_eq!(loaded_state.mtime_secs, 100);
        assert_eq!(loaded_state.mtime_nanos, 500);
        assert_eq!(loaded_state.size, 42);
        assert_eq!(loaded_state.hash, "deadbeef");
    }

    #[tokio::test]
    async fn hash_file_produces_sha256() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        tokio::fs::write(&file_path, b"hello world").await.unwrap();

        let hash = hash_file(&file_path).await.unwrap();
        // SHA-256 of "hello world"
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn skip_cascade_cache_directory() {
        let root = Path::new("/tmp/test");
        assert!(should_skip_entry(
            &root.join(".cascade-cache/manifest.jsonl"),
            root
        ));
        assert!(should_skip_entry(&root.join(".cascade-cache"), root));
    }

    #[test]
    fn skip_cascade_config_files() {
        let root = Path::new("/tmp/test");
        assert!(should_skip_entry(&root.join(".cascade"), root));
        assert!(should_skip_entry(&root.join(".cascade.toml"), root));
    }

    #[test]
    fn do_not_skip_normal_files() {
        let root = Path::new("/tmp/test");
        assert!(!should_skip_entry(&root.join("Documents/report.pdf"), root));
        assert!(!should_skip_entry(&root.join("hello.txt"), root));
    }

    #[tokio::test]
    async fn walk_tree_discovers_files() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("hello.txt"), b"hello")
            .await
            .unwrap();
        tokio::fs::create_dir(dir.path().join("subdir"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("subdir/nested.txt"), b"nested")
            .await
            .unwrap();

        // Should be skipped.
        tokio::fs::create_dir(dir.path().join(".cascade-cache"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join(".cascade-cache/manifest.jsonl"), b"{}")
            .await
            .unwrap();

        let states = walk_tree(dir.path()).await.unwrap();
        let paths: Vec<&str> = states.iter().map(|s| s.path.as_str()).collect();

        assert!(paths.contains(&"hello.txt"));
        assert!(paths.iter().any(|p| p.contains("nested.txt")));
        assert!(!paths.iter().any(|p| p.contains("cascade-cache")));
    }
}
