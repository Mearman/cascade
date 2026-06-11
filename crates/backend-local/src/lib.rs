#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::string_slice
    )
)]
//! Local filesystem backend for Cascade.
//!
//! Adopts an existing local directory and presents it as a `Backend`.
//! Change detection uses a sidecar manifest (`.cascade-cache/manifest.jsonl`)
//! that records each file's mtime, size, and SHA-256 hash.

pub mod manifest;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use cascade_engine::backend::{Backend, BackendError};
use cascade_engine::types::{Change, Cursor, FileEntry, FileId, ItemId, Quota};
use manifest::{FileState, Manifest, walk_tree};
use sha2::Digest;
use tokio::sync::RwLock;

/// Operating mode for the local backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalMode {
    /// Mirror mode: all local changes are tracked and can be synced to cloud.
    Mirror,
    /// Upload-only mode: changes are detected but write operations from cloud
    /// are not applied to the local filesystem.
    UploadOnly,
}

/// Configuration for creating a local backend.
#[derive(Debug)]
pub struct LocalConfig {
    /// Unique backend identifier (e.g. "local-photos").
    pub id: String,
    /// Display name shown in mount points.
    pub display_name: String,
    /// Root directory to adopt.
    pub root_path: PathBuf,
    /// Operating mode.
    pub mode: LocalMode,
}

/// Create a local backend from a TOML config value.
///
/// Expected config keys:
/// - `root_path` — the directory to adopt (required)
/// - `mode` — "mirror" or "upload-only" (default: "mirror")
/// - `id` — backend identifier (default: "local")
/// - `display_name` — display name (default: "Local Files (\<basename\>)")
pub fn create_backend(config: &toml::Value) -> anyhow::Result<Box<dyn Backend>> {
    let root_path = config
        .get("root_path")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("local backend requires 'root_path' config"))?;

    if !root_path.exists() {
        anyhow::bail!("root_path does not exist: {}", root_path.display());
    }

    let mode = match config
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("mirror")
    {
        "mirror" => LocalMode::Mirror,
        "upload-only" => LocalMode::UploadOnly,
        other => anyhow::bail!("unknown local backend mode: {other}"),
    };

    let id = config
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("local")
        .to_string();

    let display_name = config
        .get("display_name")
        .and_then(|v| v.as_str())
        .map_or_else(
            || {
                format!(
                    "Local Files ({})",
                    root_path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown")
                )
            },
            String::from,
        );

    let manifest_path = root_path.join(".cascade-cache").join("manifest.jsonl");

    Ok(Box::new(LocalBackend {
        id,
        display_name,
        root: root_path,
        manifest_path,
        mode,
        manifest: Arc::new(RwLock::new(Manifest::default())),
    }))
}

/// Local filesystem backend.
#[derive(Debug)]
pub struct LocalBackend {
    id: String,
    display_name: String,
    root: PathBuf,
    manifest_path: PathBuf,
    mode: LocalMode,
    manifest: Arc<RwLock<Manifest>>,
}

impl LocalBackend {
    /// Ensure the manifest is loaded from disk. If this is the first call,
    /// or the manifest file exists but hasn't been loaded, reads it.
    async fn ensure_manifest_loaded(&self) -> anyhow::Result<()> {
        let mut manifest = self.manifest.write().await;
        // Reload from disk each time to pick up external changes.
        *manifest = Manifest::load(&self.manifest_path).await?;
        Ok(())
    }

    /// Persist the current manifest to disk.
    async fn save_manifest(&self) -> anyhow::Result<()> {
        let manifest = self.manifest.read().await;
        manifest.save(&self.manifest_path).await
    }

    /// Convert a relative path to an absolute path under the root.
    fn absolute_path(&self, relative: &Path) -> PathBuf {
        // VFS paths are forward-slash rooted ("/foo/bar") on every OS. Strip the
        // leading separator unconditionally so the result nests under `root`.
        // Gating on `is_absolute()` was wrong on Windows: there "/foo" is not
        // absolute (it has no drive prefix), so the strip was skipped and
        // `join` resolved the path to the current drive's root — escaping the
        // export and making writes land where reads could not find them.
        // `strip_prefix("/")` matches the RootDir component on every platform; a
        // path without a leading separator is kept as-is.
        let nested = relative.strip_prefix("/").unwrap_or(relative);
        self.root.join(nested)
    }

    /// Convert a `FileState` into a `FileEntry`.
    fn state_to_entry(&self, state: &FileState, is_dir: bool) -> FileEntry {
        let path = Path::new(&state.path);
        let parent_relative = path.parent().unwrap_or_else(|| Path::new(""));
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        let id = ItemId::new(&self.id, &state.path);
        let parent_id = if parent_relative.as_os_str().is_empty() {
            ItemId::new(&self.id, "/")
        } else {
            ItemId::new(&self.id, parent_relative.to_string_lossy().as_ref())
        };

        let mut entry = if is_dir {
            FileEntry::dir(id, parent_id, name)
        } else {
            FileEntry::file(id, parent_id, name)
                .with_size(Some(state.size))
                .with_hash(Some(state.hash.clone()))
        };

        entry.mod_time = Some(
            chrono::DateTime::from_timestamp(state.mtime_secs, state.mtime_nanos)
                .unwrap_or(chrono::DateTime::UNIX_EPOCH),
        );

        entry
    }

    /// Build a `FileEntry` for a directory at the given relative path.
    fn dir_entry(&self, relative_path: &str) -> FileEntry {
        let path = Path::new(relative_path);
        let parent_relative = path.parent().unwrap_or_else(|| Path::new(""));
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        let id = ItemId::new(&self.id, relative_path);
        let parent_id = if parent_relative.as_os_str().is_empty() {
            ItemId::new(&self.id, "/")
        } else {
            ItemId::new(&self.id, parent_relative.to_string_lossy().as_ref())
        };

        FileEntry::dir(id, parent_id, name)
    }
}

#[async_trait]
impl Backend for LocalBackend {
    fn id(&self) -> &str {
        &self.id
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    /// The local backend roots its tree at the filesystem root marker `/`, so
    /// its top-level entries parent to `ItemId::new(self.id(), "/")` rather than
    /// the conventional `root` sentinel.
    fn root_native_id(&self) -> &'static str {
        "/"
    }

    async fn quota(&self) -> anyhow::Result<Option<Quota>> {
        // Report local disk usage for the root path.
        let available = fs2::available_space(&self.root)?;
        let total = fs2::total_space(&self.root)?;

        // Calculate used space by summing all files in the tree.
        let mut used: u64 = 0;
        for entry in walkdir::WalkDir::new(&self.root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| !manifest::should_skip_entry(e.path(), &self.root))
        {
            let entry = entry?;
            if entry.file_type().is_file() {
                used += entry.metadata()?.len();
            }
        }

        Ok(Some(Quota {
            total: Some(total),
            used: Some(used),
            available: Some(available),
        }))
    }

    async fn changes(&self, _cursor: Option<&Cursor>) -> anyhow::Result<(Vec<Change>, Cursor)> {
        self.ensure_manifest_loaded().await?;

        let current_states = walk_tree(&self.root).await?;

        let manifest = self.manifest.read().await;
        let diff = manifest.diff(&current_states);
        drop(manifest);

        let mut changes = Vec::new();

        for state in &diff.created {
            changes.push(Change::Created(self.state_to_entry(state, false)));
        }

        for (old, new) in &diff.modified {
            changes.push(Change::Updated {
                old: self.state_to_entry(old, false),
                new: self.state_to_entry(new, false),
            });
        }

        for state in &diff.deleted {
            changes.push(Change::Deleted(self.state_to_entry(state, false)));
        }

        // Update manifest with current state.
        let mut manifest = self.manifest.write().await;
        // Add created and modified.
        for state in &diff.created {
            manifest.update(std::slice::from_ref(state));
        }
        for (_, new) in &diff.modified {
            manifest.update(std::slice::from_ref(new));
        }
        // Remove deleted.
        let deleted_paths: Vec<&str> = diff.deleted.iter().map(|s| s.path.as_str()).collect();
        manifest.remove(&deleted_paths);
        drop(manifest);

        self.save_manifest().await?;

        // Use a timestamp-based cursor.
        let new_cursor = Cursor(format!("{}", chrono::Utc::now().timestamp()));
        Ok((changes, new_cursor))
    }

    async fn metadata(&self, path: &Path) -> anyhow::Result<FileEntry> {
        let abs = self.absolute_path(path);
        if !abs.exists() {
            return Err(BackendError::NotFound(path.display().to_string()).into());
        }

        let relative = abs
            .strip_prefix(&self.root)
            .unwrap_or(&abs)
            .to_string_lossy()
            .to_string();

        let metadata = tokio::fs::metadata(&abs).await?;

        if metadata.is_dir() {
            return Ok(self.dir_entry(&relative));
        }

        let modified = metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let mtime_secs = i64::try_from(modified.as_secs()).unwrap_or(i64::MAX);
        let mtime_nanos = modified.subsec_nanos();
        let size = metadata.len();
        let hash = manifest::hash_file(&abs).await?;

        let state = FileState {
            path: relative,
            mtime_secs,
            mtime_nanos,
            size,
            hash,
        };

        Ok(self.state_to_entry(&state, false))
    }

    async fn download(&self, file: &FileEntry) -> anyhow::Result<Vec<u8>> {
        let relative = file.id.native_id();
        let abs = self.root.join(relative);

        if !abs.exists() {
            anyhow::bail!("file not found: {}", abs.display());
        }

        let data = tokio::fs::read(&abs).await?;

        tracing::debug!(file = %file.id, size = data.len(), "downloaded from local");
        Ok(data)
    }

    async fn read_range(
        &self,
        file: &FileEntry,
        offset: u64,
        length: u32,
    ) -> anyhow::Result<Vec<u8>> {
        let relative = file.id.native_id();
        let abs = self.root.join(relative);

        if !abs.exists() {
            anyhow::bail!("file not found: {}", abs.display());
        }

        // A zero-length request never touches the file.
        if length == 0 {
            return Ok(Vec::new());
        }

        let mut handle = tokio::fs::File::open(&abs).await?;
        // Seek past EOF is permitted; the subsequent read simply yields no
        // bytes, which matches the "empty at or past EOF" contract.
        tokio::io::AsyncSeekExt::seek(&mut handle, std::io::SeekFrom::Start(offset)).await?;

        let cap = usize::try_from(length).unwrap_or(usize::MAX);
        let mut buf = Vec::with_capacity(cap);
        let mut limited = tokio::io::AsyncReadExt::take(handle, u64::from(length));
        tokio::io::AsyncReadExt::read_to_end(&mut limited, &mut buf).await?;

        tracing::debug!(
            file = %file.id,
            offset,
            length,
            read = buf.len(),
            "range read from local"
        );
        Ok(buf)
    }

    async fn upload(
        &self,
        path: &Path,
        data: &[u8],
        _parent_id: &FileId,
    ) -> anyhow::Result<FileEntry> {
        if self.mode == LocalMode::UploadOnly {
            anyhow::bail!("local backend is in upload-only mode");
        }

        let abs = self.absolute_path(path);
        let relative = abs
            .strip_prefix(&self.root)
            .unwrap_or(&abs)
            .to_string_lossy()
            .into_owned();

        // Ensure parent directory exists.
        if let Some(parent) = abs.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        tokio::fs::write(&abs, data).await?;

        tracing::debug!(path = %relative, size = data.len(), "uploaded to local");

        // Build FileEntry for the newly written file.
        let metadata = tokio::fs::metadata(&abs).await?;
        let modified = metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();

        let hash = {
            let mut hasher = sha2::Sha256::new();
            sha2::Digest::update(&mut hasher, data);
            hex::encode(hasher.finalize())
        };

        let state = FileState {
            path: relative,
            mtime_secs: i64::try_from(modified.as_secs()).unwrap_or(i64::MAX),
            mtime_nanos: modified.subsec_nanos(),
            size: metadata.len(),
            hash,
        };

        // Update manifest.
        let mut manifest = self.manifest.write().await;
        manifest.update(std::slice::from_ref(&state));
        drop(manifest);
        self.save_manifest().await?;

        Ok(self.state_to_entry(&state, false))
    }

    async fn update(&self, file_id: &FileId, data: &[u8]) -> anyhow::Result<FileEntry> {
        let relative = file_id.native_id();
        let full_path = self.root.join(relative);

        if let Some(parent) = full_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&full_path, data).await?;

        let size = u64::try_from(data.len()).unwrap_or(u64::MAX);
        let metadata = tokio::fs::metadata(&full_path).await?;
        let modified = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| {
                chrono::DateTime::from_timestamp(i64::try_from(d.as_secs()).unwrap_or(0), 0)
            });

        let name = full_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let parent_relative = Path::new(&relative)
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("")
            .to_string();
        let parent_id = ItemId::new(&self.id, &parent_relative);

        Ok(FileEntry {
            id: ItemId::new(&self.id, relative),
            parent_id,
            // Path defaults to name at this phase; later phases thread the
            // mount-prefixed VFS path through the local backend entry.
            path: name.clone(),
            name,
            is_dir: false,
            size: Some(size),
            mod_time: modified,
            mime_type: None,
            hash: None,
        })
    }

    async fn create_dir(&self, path: &Path) -> anyhow::Result<FileEntry> {
        if self.mode == LocalMode::UploadOnly {
            anyhow::bail!("local backend is in upload-only mode");
        }

        let abs = self.absolute_path(path);
        let relative = abs
            .strip_prefix(&self.root)
            .unwrap_or(&abs)
            .to_string_lossy()
            .into_owned();

        tokio::fs::create_dir_all(&abs).await?;

        tracing::debug!(path = %relative, "created directory");

        Ok(self.dir_entry(&relative))
    }

    async fn delete(&self, file: &FileEntry) -> anyhow::Result<()> {
        if self.mode == LocalMode::UploadOnly {
            anyhow::bail!("local backend is in upload-only mode");
        }

        let relative = file.id.native_id();
        let abs = self.root.join(relative);

        if abs.is_dir() {
            // Non-recursive: `remove_dir` errors on a non-empty directory rather
            // than recursively destroying its contents. A recursive delete here
            // is silent data loss for callers (e.g. an NFS RMDIR), which must see
            // a not-empty refusal — RFC 1813 §3.3.13 NFS3ERR_NOTEMPTY — not a
            // wiped subtree. The not-empty case is tagged `Conflict` so presenters
            // can map it to the protocol's not-empty status.
            tokio::fs::remove_dir(&abs).await.map_err(|err| {
                if err.kind() == std::io::ErrorKind::DirectoryNotEmpty {
                    anyhow::Error::from(BackendError::Conflict(format!(
                        "directory not empty: {}",
                        abs.display()
                    )))
                } else {
                    anyhow::Error::from(err)
                }
            })?;
        } else {
            tokio::fs::remove_file(&abs).await?;
        }

        // Remove from manifest.
        let mut manifest = self.manifest.write().await;
        manifest.remove(&[relative]);
        drop(manifest);
        self.save_manifest().await?;

        tracing::debug!(file = %file.id, "deleted");
        Ok(())
    }

    async fn move_entry(&self, src: &Path, dst: &Path) -> anyhow::Result<FileEntry> {
        if self.mode == LocalMode::UploadOnly {
            anyhow::bail!("local backend is in upload-only mode");
        }

        let src_abs = self.absolute_path(src);
        let dst_abs = self.absolute_path(dst);

        if !src_abs.exists() {
            anyhow::bail!("source path not found: {}", src.display());
        }

        // Ensure destination parent exists.
        if let Some(parent) = dst_abs.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        tokio::fs::rename(&src_abs, &dst_abs).await?;

        // Update manifest: remove old, add new.
        let src_relative = src.to_string_lossy().to_string();
        let dst_relative = dst.to_string_lossy().to_string();

        let mut manifest = self.manifest.write().await;
        manifest.remove(&[&src_relative]);

        // Compute state for the new path.
        let metadata = tokio::fs::metadata(&dst_abs).await?;
        let modified = metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let hash = manifest::hash_file(&dst_abs).await?;

        let new_state = FileState {
            path: dst_relative.clone(),
            mtime_secs: i64::try_from(modified.as_secs()).unwrap_or(i64::MAX),
            mtime_nanos: modified.subsec_nanos(),
            size: metadata.len(),
            hash,
        };
        manifest.update(std::slice::from_ref(&new_state));
        drop(manifest);
        self.save_manifest().await?;

        tracing::debug!(src = %src_relative, dst = %dst_relative, "moved");

        Ok(self.state_to_entry(&new_state, metadata.is_dir()))
    }

    async fn poll_interval(&self) -> Option<Duration> {
        Some(Duration::from_secs(5))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a TOML config table programmatically.
    fn make_config(root_path: &str, extra: &[(&str, &str)]) -> toml::Value {
        let mut table = toml::map::Map::new();
        table.insert(
            "root_path".to_string(),
            toml::Value::String(root_path.to_string()),
        );
        for (key, value) in extra {
            table.insert(key.to_string(), toml::Value::String(value.to_string()));
        }
        toml::Value::Table(table)
    }

    #[test]
    fn create_backend_from_config() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(
            dir.path().to_str().unwrap(),
            &[("mode", "mirror"), ("id", "local-test")],
        );
        let backend = create_backend(&config).unwrap();
        assert_eq!(backend.id(), "local-test");
    }

    #[test]
    fn create_backend_requires_root_path() {
        let config = toml::Value::Table(toml::map::Map::new());
        let err = create_backend(&config).err().unwrap();
        assert!(err.to_string().contains("root_path"));
    }

    #[test]
    fn create_backend_rejects_nonexistent_root() {
        let config = make_config("/nonexistent/path/that/does/not/exist", &[]);
        let err = create_backend(&config).err().unwrap();
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn create_backend_rejects_bad_mode() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path().to_str().unwrap(), &[("mode", "invalid")]);
        let err = create_backend(&config).err().unwrap();
        assert!(err.to_string().contains("unknown"));
    }

    #[tokio::test]
    async fn poll_interval_is_5_seconds() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path().to_str().unwrap(), &[]);
        let backend = create_backend(&config).unwrap();
        assert_eq!(backend.poll_interval().await, Some(Duration::from_secs(5)));
    }

    #[tokio::test]
    async fn metadata_returns_file_entry() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("test.txt"), b"hello world")
            .await
            .unwrap();

        let config = make_config(dir.path().to_str().unwrap(), &[("id", "test-local")]);
        let backend = create_backend(&config).unwrap();
        let entry = backend.metadata(Path::new("test.txt")).await.unwrap();

        assert_eq!(entry.name, "test.txt");
        assert!(!entry.is_dir);
        assert_eq!(entry.size, Some(11));
        assert!(entry.hash.is_some());
    }

    #[tokio::test]
    async fn metadata_for_directory() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::create_dir(dir.path().join("subdir"))
            .await
            .unwrap();

        let config = make_config(dir.path().to_str().unwrap(), &[("id", "test-local")]);
        let backend = create_backend(&config).unwrap();
        let entry = backend.metadata(Path::new("subdir")).await.unwrap();

        assert_eq!(entry.name, "subdir");
        assert!(entry.is_dir);
    }

    #[tokio::test]
    async fn download_reads_file() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("test.txt"), b"hello world")
            .await
            .unwrap();

        let config = make_config(dir.path().to_str().unwrap(), &[("id", "test-local")]);
        let backend = create_backend(&config).unwrap();
        let entry = backend.metadata(Path::new("test.txt")).await.unwrap();

        let buf = backend.download(&entry).await.unwrap();

        assert_eq!(buf, b"hello world");
    }

    #[tokio::test]
    async fn read_range_mid_range() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("test.txt"), b"hello world")
            .await
            .unwrap();

        let config = make_config(dir.path().to_str().unwrap(), &[("id", "test-local")]);
        let backend = create_backend(&config).unwrap();
        let entry = backend.metadata(Path::new("test.txt")).await.unwrap();

        assert_eq!(backend.read_range(&entry, 6, 5).await.unwrap(), b"world");
    }

    #[tokio::test]
    async fn read_range_whole_file_via_covering_length() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("test.txt"), b"hello world")
            .await
            .unwrap();

        let config = make_config(dir.path().to_str().unwrap(), &[("id", "test-local")]);
        let backend = create_backend(&config).unwrap();
        let entry = backend.metadata(Path::new("test.txt")).await.unwrap();

        assert_eq!(
            backend.read_range(&entry, 0, 11).await.unwrap(),
            b"hello world"
        );
    }

    #[tokio::test]
    async fn read_range_length_past_eof_clamps() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("test.txt"), b"hello world")
            .await
            .unwrap();

        let config = make_config(dir.path().to_str().unwrap(), &[("id", "test-local")]);
        let backend = create_backend(&config).unwrap();
        let entry = backend.metadata(Path::new("test.txt")).await.unwrap();

        // Length runs past EOF; the result clamps to the available bytes.
        assert_eq!(backend.read_range(&entry, 6, 999).await.unwrap(), b"world");
    }

    #[tokio::test]
    async fn read_range_offset_at_or_past_eof_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("test.txt"), b"hello world")
            .await
            .unwrap();

        let config = make_config(dir.path().to_str().unwrap(), &[("id", "test-local")]);
        let backend = create_backend(&config).unwrap();
        let entry = backend.metadata(Path::new("test.txt")).await.unwrap();

        // Offset exactly at EOF -> empty.
        assert!(backend.read_range(&entry, 11, 10).await.unwrap().is_empty());
        // Offset well past EOF -> empty, no panic.
        assert!(
            backend
                .read_range(&entry, 1000, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn read_range_zero_length_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("test.txt"), b"hello world")
            .await
            .unwrap();

        let config = make_config(dir.path().to_str().unwrap(), &[("id", "test-local")]);
        let backend = create_backend(&config).unwrap();
        let entry = backend.metadata(Path::new("test.txt")).await.unwrap();

        assert!(backend.read_range(&entry, 0, 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn read_range_missing_file_bails() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("test.txt"), b"hello world")
            .await
            .unwrap();

        let config = make_config(dir.path().to_str().unwrap(), &[("id", "test-local")]);
        let backend = create_backend(&config).unwrap();
        let mut entry = backend.metadata(Path::new("test.txt")).await.unwrap();
        // Point the entry at a path that does not exist.
        entry.id = ItemId::new("test-local", "missing.txt");

        let err = backend.read_range(&entry, 0, 4).await.err().unwrap();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn upload_writes_file() {
        let dir = tempfile::tempdir().unwrap();

        let config = make_config(dir.path().to_str().unwrap(), &[("id", "test-local")]);
        let backend = create_backend(&config).unwrap();

        let data = b"uploaded content";
        let parent_id = FileId("/".to_string());

        let entry = backend
            .upload(Path::new("new-file.txt"), data, &parent_id)
            .await
            .unwrap();

        assert_eq!(entry.name, "new-file.txt");

        let written = tokio::fs::read(dir.path().join("new-file.txt"))
            .await
            .unwrap();
        assert_eq!(written, b"uploaded content");
    }

    #[tokio::test]
    async fn create_dir_creates_directory() {
        let dir = tempfile::tempdir().unwrap();

        let config = make_config(dir.path().to_str().unwrap(), &[("id", "test-local")]);
        let backend = create_backend(&config).unwrap();

        let entry = backend.create_dir(Path::new("new-dir")).await.unwrap();
        assert_eq!(entry.name, "new-dir");
        assert!(entry.is_dir);
        assert!(dir.path().join("new-dir").is_dir());
    }

    #[tokio::test]
    async fn delete_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("to-delete.txt"), b"bye")
            .await
            .unwrap();

        let config = make_config(dir.path().to_str().unwrap(), &[("id", "test-local")]);
        let backend = create_backend(&config).unwrap();
        let entry = backend.metadata(Path::new("to-delete.txt")).await.unwrap();

        backend.delete(&entry).await.unwrap();
        assert!(!dir.path().join("to-delete.txt").exists());
    }

    #[tokio::test]
    async fn move_entry_renames_file() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("original.txt"), b"content")
            .await
            .unwrap();

        let config = make_config(dir.path().to_str().unwrap(), &[("id", "test-local")]);
        let backend = create_backend(&config).unwrap();

        let entry = backend
            .move_entry(Path::new("original.txt"), Path::new("renamed.txt"))
            .await
            .unwrap();

        assert_eq!(entry.name, "renamed.txt");
        assert!(!dir.path().join("original.txt").exists());
        assert!(dir.path().join("renamed.txt").exists());
    }

    #[tokio::test]
    async fn changes_detects_new_file() {
        let dir = tempfile::tempdir().unwrap();

        let config = make_config(dir.path().to_str().unwrap(), &[("id", "test-local")]);
        let backend = create_backend(&config).unwrap();

        // First call — empty.
        let (changes, cursor) = backend.changes(None).await.unwrap();
        assert!(changes.is_empty());

        // Write a file.
        tokio::fs::write(dir.path().join("hello.txt"), b"hello")
            .await
            .unwrap();

        let (changes, cursor) = backend.changes(Some(&cursor)).await.unwrap();
        assert_eq!(changes.len(), 1);
        match &changes[0] {
            Change::Created(entry) => assert_eq!(entry.name, "hello.txt"),
            other => panic!("expected Created, got {other:?}"),
        }

        // No more changes after manifest update.
        let (changes, _) = backend.changes(Some(&cursor)).await.unwrap();
        assert!(changes.is_empty());
    }

    #[tokio::test]
    async fn changes_detects_deletion() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("to-delete.txt"), b"bye")
            .await
            .unwrap();

        let config = make_config(dir.path().to_str().unwrap(), &[("id", "test-local")]);
        let backend = create_backend(&config).unwrap();

        // Seed the manifest.
        let (_, cursor) = backend.changes(None).await.unwrap();

        // Delete the file.
        tokio::fs::remove_file(dir.path().join("to-delete.txt"))
            .await
            .unwrap();

        let (changes, _) = backend.changes(Some(&cursor)).await.unwrap();
        assert_eq!(changes.len(), 1);
        match &changes[0] {
            Change::Deleted(entry) => assert_eq!(entry.name, "to-delete.txt"),
            other => panic!("expected Deleted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upload_only_mode_blocks_writes() {
        let dir = tempfile::tempdir().unwrap();

        let config = make_config(
            dir.path().to_str().unwrap(),
            &[("id", "test-local"), ("mode", "upload-only")],
        );
        let backend = create_backend(&config).unwrap();

        let data = b"content";
        let parent_id = FileId("/".to_string());

        let result = backend
            .upload(Path::new("file.txt"), data, &parent_id)
            .await;
        assert!(result.is_err());
    }
}
