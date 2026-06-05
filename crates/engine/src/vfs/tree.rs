//! `VfsTree` — composes multiple backends with longest-prefix routing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::backend::Backend;
use crate::types::{Change, DirEntry, FileEntry, FileId, ItemId, SyncCursor};

/// VFS tree that routes operations to the correct backend by longest-prefix match.
pub struct VfsTree {
    /// The root backend — handles paths not covered by any child.
    root: Arc<dyn Backend>,

    /// Sorted list of (`path_prefix`, backend) bindings.
    /// Sorted longest-prefix-first so the first match wins.
    children: Vec<(PathBuf, Arc<dyn Backend>)>,
}

impl std::fmt::Debug for VfsTree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VfsTree")
            .field("root_id", &self.root.id())
            .field("child_count", &self.children.len())
            .finish_non_exhaustive()
    }
}

impl VfsTree {
    pub fn new(root: Arc<dyn Backend>) -> Self {
        Self {
            root,
            children: Vec::new(),
        }
    }

    /// Add a child backend bound to a path prefix.
    /// Maintains longest-prefix-first ordering.
    pub fn mount(&mut self, prefix: PathBuf, backend: Arc<dyn Backend>) {
        self.children.push((prefix, backend));
        // Sort longest path first so first match wins
        self.children
            .sort_by_key(|b| std::cmp::Reverse(b.0.as_os_str().len()));
    }

    /// Remove a child backend by prefix. Returns the backend if found.
    pub fn unmount(&mut self, prefix: &Path) -> Option<Arc<dyn Backend>> {
        let idx = self.children.iter().position(|(p, _)| p == prefix)?;
        Some(self.children.remove(idx).1)
    }

    /// Resolve a path to the correct backend and the remaining path within that backend.
    #[must_use]
    pub fn resolve(&self, path: &Path) -> (&Arc<dyn Backend>, PathBuf) {
        for (prefix, backend) in &self.children {
            if let Ok(rest) = path.strip_prefix(prefix) {
                return (backend, rest.to_path_buf());
            }
        }
        (&self.root, path.to_path_buf())
    }

    /// List directory entries, merging backend content with child mount points.
    pub async fn read_dir(&self, path: &Path) -> anyhow::Result<Vec<DirEntry>> {
        let mut entries = Vec::new();

        // Get entries from the backend that owns this path
        let (backend, _backend_path) = self.resolve(path);
        // For Phase 1 read-only, we query the backend for children
        let (changes, _) = backend.changes(None).await?;
        for change in changes {
            if let Change::Created(entry) = change {
                entries.push(DirEntry {
                    name: entry.name,
                    is_dir: entry.is_dir,
                });
            }
        }

        // Inject child mount point directories if this path is their parent
        for (child_prefix, _) in &self.children {
            if child_prefix.parent() == Some(path)
                && let Some(mount_dir_name) = child_prefix.file_name()
            {
                let mount_dir_name = mount_dir_name.to_string_lossy();
                if !entries.iter().any(|e| e.name == mount_dir_name) {
                    entries.push(DirEntry::dir(mount_dir_name.to_string()));
                }
            }
        }

        Ok(entries)
    }

    /// Move a file, handling cross-backend transfers.
    /// Phase 1 is read-only, so this always fails for cloud backends.
    pub async fn rename(&self, src: &Path, dst: &Path) -> anyhow::Result<()> {
        let (src_backend, src_path) = self.resolve(src);
        let (dst_backend, dst_path) = self.resolve(dst);

        if Arc::ptr_eq(src_backend, dst_backend) {
            src_backend.move_entry(&src_path, &dst_path).await?;
        } else {
            // Cross-backend — download, upload, delete original
            let entry = src_backend.metadata(&src_path).await?;
            let data = src_backend.download(&entry).await?;
            let parent_id = FileId(entry.parent_id.0.clone());
            dst_backend.upload(&dst_path, &data, &parent_id).await?;
            src_backend.delete(&entry).await?;
        }
        Ok(())
    }

    /// Get the root backend.
    #[must_use]
    pub fn root(&self) -> &Arc<dyn Backend> {
        &self.root
    }

    /// Get all child mounts.
    #[must_use]
    pub fn children(&self) -> &[(PathBuf, Arc<dyn Backend>)] {
        &self.children
    }

    /// Find a registered backend by its `id()`.
    ///
    /// Returns the root backend if its ID matches, otherwise the first child
    /// mount with a matching ID. Used by presenters that need to dispatch an
    /// operation by the `backend_id:native_id` portion of an `ItemId`.
    #[must_use]
    pub fn backend_by_id(&self, id: &str) -> Option<&Arc<dyn Backend>> {
        if self.root.id() == id {
            return Some(&self.root);
        }
        self.children
            .iter()
            .find(|(_, backend)| backend.id() == id)
            .map(|(_, backend)| backend)
    }

    /// List the immediate children of a directory identified by its `ItemId`.
    ///
    /// Routes to the owning backend via `backend_id`, then calls
    /// `Backend::list_children` with the native ID portion. Returns
    /// `BackendError::NotFound` wrapped in `anyhow::Error` when no backend
    /// is registered for the item's `backend_id`.
    pub async fn list_children_by_id(&self, id: &ItemId) -> anyhow::Result<Vec<FileEntry>> {
        let backend = self.backend_by_id(id.backend_id()).ok_or_else(|| {
            anyhow::anyhow!("no backend registered for item id {}", id.backend_id())
        })?;
        backend.list_children(id.native_id()).await
    }

    /// Return the cursor representing the current state of all items
    /// under `parent_id`.
    ///
    /// Used by presenters (e.g. the File Provider extension) to decide
    /// whether the system's last-known anchor is still current or a
    /// full re-enumeration is needed.
    ///
    /// The cursor is derived from a SHA-256 over the sorted `(id, name,
    /// is_dir, size, mod_time)` tuples of the parent's immediate
    /// children, as returned by the owning backend. Any create,
    /// rename, size change, or delete changes the hash; the empty
    /// directory hashes to a stable non-empty value (the hash of an
    /// empty input).
    ///
    /// This is a v1 derivation — the engine may move to an incremental
    /// scheme in future, but consumers must keep treating the cursor as
    /// opaque bytes.
    pub async fn current_sync_cursor(&self, parent_id: &ItemId) -> anyhow::Result<SyncCursor> {
        let backend = self
            .backend_by_id(parent_id.backend_id())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no backend registered for parent id {}",
                    parent_id.backend_id()
                )
            })?
            .clone();
        derive_sync_cursor(backend.as_ref(), parent_id.native_id()).await
    }
}

/// Compute the cursor for a backend's view of a parent directory.
///
/// Free function (rather than a `VfsTree` method) so callers that already
/// hold the owning `Arc<dyn Backend>` can derive the cursor without
/// re-acquiring a lock on the tree. Used by both `VfsTree::current_sync_cursor`
/// and presenter-side handlers that need to release a synchronous lock
/// before awaiting.
pub async fn derive_sync_cursor(
    backend: &dyn Backend,
    parent_native_id: &str,
) -> anyhow::Result<SyncCursor> {
    let mut entries = backend.list_children(parent_native_id).await?;
    entries.sort_by(|a, b| a.id.0.cmp(&b.id.0));

    let mut hasher = Sha256::new();
    for entry in &entries {
        hasher.update(entry.id.0.as_bytes());
        hasher.update([0u8]);
        hasher.update(entry.name.as_bytes());
        hasher.update([0u8]);
        hasher.update([u8::from(entry.is_dir)]);
        hasher.update(entry.size.unwrap_or(0).to_be_bytes());
        hasher.update(entry.mod_time.map_or(0i64, |t| t.timestamp()).to_be_bytes());
    }
    Ok(SyncCursor::new(hasher.finalize().to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::NullBackend;
    use crate::types::{Cursor, FileEntry, Quota};
    use async_trait::async_trait;
    use chrono::{TimeZone, Utc};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Duration;

    /// Minimal in-memory backend that supports `list_children` so the
    /// VFS-level cursor derivation can be exercised without a real
    /// cloud backend.
    #[derive(Debug)]
    struct StubBackend {
        id: String,
        files: Mutex<HashMap<String, FileEntry>>,
    }

    impl StubBackend {
        fn new(id: &str) -> Self {
            Self {
                id: id.to_string(),
                files: Mutex::new(HashMap::new()),
            }
        }

        fn upsert(&self, entry: FileEntry) {
            self.files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(entry.id.0.clone(), entry);
        }

        fn remove(&self, id: &ItemId) {
            self.files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id.0);
        }
    }

    #[async_trait]
    impl Backend for StubBackend {
        fn id(&self) -> &str {
            &self.id
        }

        fn display_name(&self) -> &str {
            &self.id
        }

        async fn quota(&self) -> anyhow::Result<Option<Quota>> {
            Ok(None)
        }

        async fn changes(&self, _cursor: Option<&Cursor>) -> anyhow::Result<(Vec<Change>, Cursor)> {
            Ok((vec![], Cursor("stub".to_string())))
        }

        async fn metadata(&self, _path: &Path) -> anyhow::Result<FileEntry> {
            anyhow::bail!("metadata not implemented")
        }

        async fn download(&self, _file: &FileEntry) -> anyhow::Result<Vec<u8>> {
            anyhow::bail!("download not implemented")
        }

        async fn upload(
            &self,
            _path: &Path,
            _data: &[u8],
            _parent_id: &FileId,
        ) -> anyhow::Result<FileEntry> {
            anyhow::bail!("upload not implemented")
        }

        async fn update(&self, _file_id: &FileId, _data: &[u8]) -> anyhow::Result<FileEntry> {
            anyhow::bail!("update not implemented")
        }

        async fn create_dir(&self, _path: &Path) -> anyhow::Result<FileEntry> {
            anyhow::bail!("create_dir not implemented")
        }

        async fn delete(&self, _file: &FileEntry) -> anyhow::Result<()> {
            anyhow::bail!("delete not implemented")
        }

        async fn move_entry(&self, _src: &Path, _dst: &Path) -> anyhow::Result<FileEntry> {
            anyhow::bail!("move_entry not implemented")
        }

        async fn list_children(&self, parent_native_id: &str) -> anyhow::Result<Vec<FileEntry>> {
            let files = self
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let prefix = format!("{}:", self.id);
            let parent_full = format!("{prefix}{parent_native_id}");
            Ok(files
                .values()
                .filter(|entry| entry.parent_id.0 == parent_full)
                .cloned()
                .collect())
        }

        async fn poll_interval(&self) -> Option<Duration> {
            None
        }
    }

    fn make_tree() -> VfsTree {
        let root = Arc::new(NullBackend::new("root"));
        VfsTree::new(root)
    }

    fn make_entry(backend: &str, native: &str, parent_native: &str, name: &str) -> FileEntry {
        FileEntry {
            id: ItemId::new(backend, native),
            parent_id: ItemId::new(backend, parent_native),
            name: name.to_string(),
            is_dir: false,
            size: Some(name.len() as u64),
            mod_time: Some(Utc.timestamp_opt(1_000_000, 0).unwrap()),
            mime_type: None,
            hash: None,
        }
    }

    #[test]
    fn resolve_root_path() {
        let tree = make_tree();
        let (backend, rest) = tree.resolve(Path::new("Documents/report.txt"));
        assert_eq!(backend.id(), "root");
        assert_eq!(rest, Path::new("Documents/report.txt"));
    }

    #[test]
    fn resolve_child_path() {
        let mut tree = make_tree();
        tree.mount(PathBuf::from("Work"), Arc::new(NullBackend::new("work")));
        let (backend, rest) = tree.resolve(Path::new("Work/Projects/code.rs"));
        assert_eq!(backend.id(), "work");
        assert_eq!(rest, Path::new("Projects/code.rs"));
    }

    #[test]
    fn longest_prefix_wins() {
        let mut tree = make_tree();
        tree.mount(PathBuf::from("Work"), Arc::new(NullBackend::new("work")));
        tree.mount(
            PathBuf::from("Work/Assets"),
            Arc::new(NullBackend::new("assets")),
        );

        // Work/Assets/logo.png -> assets backend
        let (backend, rest) = tree.resolve(Path::new("Work/Assets/logo.png"));
        assert_eq!(backend.id(), "assets");
        assert_eq!(rest, Path::new("logo.png"));

        // Work/report.txt -> work backend
        let (backend, rest) = tree.resolve(Path::new("Work/report.txt"));
        assert_eq!(backend.id(), "work");
        assert_eq!(rest, Path::new("report.txt"));
    }

    #[test]
    fn unmount_removes_child() {
        let mut tree = make_tree();
        tree.mount(PathBuf::from("Work"), Arc::new(NullBackend::new("work")));
        assert!(tree.unmount(Path::new("Work")).is_some());
        assert!(tree.children().is_empty());
    }

    #[test]
    fn backend_by_id_finds_root_and_children() {
        let mut tree = make_tree();
        tree.mount(PathBuf::from("Work"), Arc::new(NullBackend::new("work")));
        tree.mount(
            PathBuf::from("Assets"),
            Arc::new(NullBackend::new("assets")),
        );

        assert_eq!(tree.backend_by_id("root").map(|b| b.id()), Some("root"));
        assert_eq!(tree.backend_by_id("work").map(|b| b.id()), Some("work"));
        assert_eq!(tree.backend_by_id("assets").map(|b| b.id()), Some("assets"));
        assert!(tree.backend_by_id("missing").is_none());
    }

    #[tokio::test]
    async fn current_sync_cursor_is_stable_when_no_changes() {
        let backend = Arc::new(StubBackend::new("stub"));
        backend.upsert(make_entry("stub", "f1", "root", "a.txt"));
        backend.upsert(make_entry("stub", "f2", "root", "b.txt"));
        let tree = VfsTree::new(backend);
        let parent = ItemId::new("stub", "root");

        let first = tree.current_sync_cursor(&parent).await.unwrap();
        let second = tree.current_sync_cursor(&parent).await.unwrap();
        assert_eq!(first, second);
        assert!(!first.is_empty());
    }

    #[tokio::test]
    async fn current_sync_cursor_changes_after_upsert() {
        let backend = Arc::new(StubBackend::new("stub"));
        backend.upsert(make_entry("stub", "f1", "root", "a.txt"));
        let tree = VfsTree::new(backend.clone());
        let parent = ItemId::new("stub", "root");

        let before = tree.current_sync_cursor(&parent).await.unwrap();
        backend.upsert(make_entry("stub", "f2", "root", "b.txt"));
        let after = tree.current_sync_cursor(&parent).await.unwrap();
        assert_ne!(before, after);
    }

    #[tokio::test]
    async fn current_sync_cursor_changes_after_modification() {
        let backend = Arc::new(StubBackend::new("stub"));
        backend.upsert(make_entry("stub", "f1", "root", "a.txt"));
        let tree = VfsTree::new(backend.clone());
        let parent = ItemId::new("stub", "root");

        let before = tree.current_sync_cursor(&parent).await.unwrap();
        let mut updated = make_entry("stub", "f1", "root", "a.txt");
        updated.size = Some(9999);
        backend.upsert(updated);
        let after = tree.current_sync_cursor(&parent).await.unwrap();
        assert_ne!(before, after);
    }

    #[tokio::test]
    async fn current_sync_cursor_changes_after_delete() {
        let backend = Arc::new(StubBackend::new("stub"));
        backend.upsert(make_entry("stub", "f1", "root", "a.txt"));
        backend.upsert(make_entry("stub", "f2", "root", "b.txt"));
        let tree = VfsTree::new(backend.clone());
        let parent = ItemId::new("stub", "root");

        let before = tree.current_sync_cursor(&parent).await.unwrap();
        backend.remove(&ItemId::new("stub", "f2"));
        let after = tree.current_sync_cursor(&parent).await.unwrap();
        assert_ne!(before, after);
    }

    #[tokio::test]
    async fn current_sync_cursor_for_empty_directory_is_non_empty() {
        let backend = Arc::new(StubBackend::new("stub"));
        let tree = VfsTree::new(backend);
        let parent = ItemId::new("stub", "empty");

        let cursor = tree.current_sync_cursor(&parent).await.unwrap();
        // SHA-256 of an empty input is a 32-byte non-empty digest.
        assert!(!cursor.is_empty());
        assert_eq!(cursor.as_bytes().len(), 32);
    }

    #[tokio::test]
    async fn current_sync_cursor_fails_for_unknown_backend() {
        let backend = Arc::new(StubBackend::new("stub"));
        let tree = VfsTree::new(backend);
        let parent = ItemId::new("ghost", "root");

        let err = tree.current_sync_cursor(&parent).await.unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }
}
