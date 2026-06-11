//! `VfsTree` — composes multiple backends with longest-prefix routing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::backend::Backend;
use crate::types::{DirEntry, FileEntry, FileId, ItemId, SyncCursor};

/// Backend id of the neutral VFS root.
///
/// The root is a synthetic [`NullBackend`](crate::backend::NullBackend) that
/// owns no content; it exists only as the container the configured backends
/// mount beneath. It is never registered in the state database and never
/// appears in `list_backends`. The engine constructs the tree's root with this
/// id, and the sync runner stamps it as the `parent_id` of the synthetic
/// mount-point directories it hydrates into the presenter, so both halves agree
/// on a single neutral-root identity.
pub const NEUTRAL_ROOT_ID: &str = "__cascade_root__";

/// The [`ItemId`] of the neutral root's synthetic container.
///
/// Top-level mount-point directories (a backend mounted directly under the
/// neutral root) carry this as their `parent_id`, marking them as the neutral
/// root's children. The native id half is the conventional `root` sentinel the
/// default `Backend::is_root_native_id` recognises.
#[must_use]
pub fn neutral_root_item_id() -> ItemId {
    ItemId::new(NEUTRAL_ROOT_ID, "root")
}

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

    /// Resolve a path to the correct backend and the remaining path within that
    /// backend.
    ///
    /// Routing is longest-prefix-first: `children` is kept sorted by descending
    /// prefix length (see [`Self::mount`]), so the first child whose prefix is a
    /// leading component of `path` wins and nested mounts route to the innermost
    /// backend. A backend mounted at `/` carries the empty prefix; because the
    /// empty prefix has length zero it always sorts last, it is only matched
    /// once every explicit prefix has been ruled out. That at-root rule is made
    /// explicit here (mirroring the `WebDAV` presenter's `backend_for_path`)
    /// rather than relying on `strip_prefix("")` happening to succeed at the end
    /// of the loop, so the invariant survives any future change to the ordering.
    #[must_use]
    pub fn resolve(&self, path: &Path) -> (&Arc<dyn Backend>, PathBuf) {
        let mut at_root: Option<&Arc<dyn Backend>> = None;
        for (prefix, backend) in &self.children {
            if prefix.as_os_str().is_empty() {
                // At-root child: the final fallback, tried after explicit prefixes.
                at_root = Some(backend);
                continue;
            }
            if let Ok(rest) = path.strip_prefix(prefix) {
                return (backend, rest.to_path_buf());
            }
        }
        if let Some(backend) = at_root {
            return (backend, path.to_path_buf());
        }
        (&self.root, path.to_path_buf())
    }

    /// Resolve `path` to the owning backend, its backend-relative sub-path, and
    /// the synthetic child-mount directory names to inject under it.
    ///
    /// This is the synchronous half of [`Self::read_dir`], split out so a
    /// presenter holding the tree behind a synchronous lock can compute the
    /// routing and injection set, release the lock, and then await the
    /// native-id resolution and `Backend::list_children` without holding the
    /// guard across the await point. The returned backend `Arc` is cloned so it
    /// outlives the lock.
    ///
    /// The returned sub-path is the backend-relative *path*, not the native id
    /// `Backend::list_children` expects. A presenter reproducing [`Self::read_dir`]
    /// must resolve it to a native id with the free [`resolve_listing_native_id`]
    /// function before calling `Backend::list_children`, exactly as `read_dir`
    /// does.
    ///
    /// The injected names follow the shared `WebDAV`/`NFS` rule: any child mount
    /// whose prefix has `path` as its direct parent contributes its final path
    /// segment as a directory name. Non-UTF-8 paths are rejected explicitly
    /// rather than silently listing the wrong directory.
    pub fn resolve_listing(
        &self,
        path: &Path,
    ) -> anyhow::Result<(Arc<dyn Backend>, String, Vec<String>)> {
        let (backend, backend_path) = self.resolve(path);
        let backend_path_str = backend_path
            .to_str()
            .ok_or_else(|| {
                anyhow::anyhow!("path contains non-UTF-8 bytes: {}", backend_path.display())
            })?
            .to_owned();

        let mut mount_names = Vec::new();
        for (child_prefix, _) in &self.children {
            if child_prefix.parent() == Some(path)
                && let Some(mount_dir_name) = child_prefix.file_name()
            {
                mount_names.push(mount_dir_name.to_string_lossy().into_owned());
            }
        }

        Ok((Arc::clone(backend), backend_path_str, mount_names))
    }

    /// List directory entries, merging backend content with child mount points.
    ///
    /// Resolves `path` to the owning backend and backend-relative sub-path,
    /// resolves that sub-path to the directory's *native id* (the identifier
    /// `Backend::list_children` actually takes — see [`resolve_listing_native_id`]),
    /// then calls `Backend::list_children` with that native id so only the
    /// immediate children of the requested directory are fetched, not the entire
    /// backend tree. The injected child-mount directories follow the shared
    /// `WebDAV`/`NFS` shadow rule: any child mount whose prefix has `path` as its
    /// direct parent is injected as a synthetic directory entry, shadowing a
    /// same-named real entry if one exists.
    ///
    /// The routing and injection set are computed by [`Self::resolve_listing`],
    /// the native id by [`resolve_listing_native_id`], and the result merged by
    /// the free [`merge_listing`] function, so a presenter that must release a
    /// synchronous lock before awaiting can reproduce this exact behaviour
    /// without duplicating the merge logic.
    pub async fn read_dir(&self, path: &Path) -> anyhow::Result<Vec<DirEntry>> {
        let (backend, backend_path, mount_names) = self.resolve_listing(path)?;
        let native_id = resolve_listing_native_id(backend.as_ref(), &backend_path).await?;
        let children = backend.list_children(&native_id).await?;
        Ok(merge_listing(children, &mount_names))
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

    /// Find the backend that owns a VFS path by longest-prefix match.
    ///
    /// The `path` argument is a VFS-absolute path without a leading slash
    /// (e.g. `personal/Documents/report.txt`). This mirrors the storage
    /// representation in `files.path` and `VfsItem.path`.
    ///
    /// - A backend mounted at the empty prefix (i.e. at "/") matches every
    ///   path, so it is tried last after all explicit mounts.
    /// - An explicit child mount matches when `path` starts with its prefix
    ///   followed by either a `/` or the end of the string (so `Work` does
    ///   not match `Workbench`).
    /// - When no child mount matches, the root backend is returned.
    ///
    /// Returns the matching backend and the remaining backend-relative path.
    #[must_use]
    pub fn backend_for_path<'a>(&'a self, path: &'a str) -> (&'a Arc<dyn Backend>, &'a str) {
        for (prefix, backend) in &self.children {
            let prefix_str = prefix.to_string_lossy();
            if prefix_str.is_empty() {
                // Empty-prefix mount (at-root backend) — only returned as a
                // fallback after all explicit prefix mounts are exhausted.
                continue;
            }
            if path == prefix_str.as_ref() {
                // Exact match: path IS the mount directory.
                return (backend, "");
            }
            let with_slash = format!("{prefix_str}/");
            if let Some(rest) = path.strip_prefix(with_slash.as_str()) {
                return (backend, rest);
            }
        }
        // Fall back to the root backend. Check for an at-root (empty-prefix)
        // child mount first; if present it covers everything the explicit
        // mounts didn't claim.
        if let Some((_, at_root)) = self.children.iter().find(|(p, _)| p.as_os_str().is_empty()) {
            return (at_root, path);
        }
        (&self.root, path)
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

/// Resolve a backend-relative directory *path* to the *native id* that
/// `Backend::list_children` expects.
///
/// `Backend::list_children` is contracted on a native parent id (the `root`
/// sentinel, or a backend-specific node id such as a Google Drive folder id),
/// not on a path. [`VfsTree::resolve_listing`] yields the backend-relative path,
/// so this function bridges the two before the call.
///
/// - The mount root carries the empty `backend_path`; it resolves to the
///   backend's declared [`Backend::root_native_id`] (the conventional `root`
///   sentinel for the cloud backends, `/` for the local-filesystem backend),
///   matching the `{backend_id}:root` `ItemId` the engine and presenters use to
///   enumerate a mount's top level.
/// - A non-empty `backend_path` is resolved through [`Backend::metadata`], whose
///   returned [`FileEntry`] carries the directory's real native id in its
///   [`ItemId`]; that native id is then handed to `Backend::list_children`.
///
/// Free function (rather than a `VfsTree` method) so a presenter that releases a
/// synchronous lock before awaiting — cloning the owning `Arc<dyn Backend>`
/// first — can reproduce [`VfsTree::read_dir`] exactly without re-implementing
/// the path-to-native-id step.
pub async fn resolve_listing_native_id(
    backend: &dyn Backend,
    backend_path: &str,
) -> anyhow::Result<String> {
    if backend_path.is_empty() {
        return Ok(backend.root_native_id().to_owned());
    }
    let entry = backend.metadata(Path::new(backend_path)).await?;
    Ok(entry.id.native_id().to_owned())
}

/// Merge a backend's immediate children with injected child-mount directory
/// names, applying the shadow rule.
///
/// `backend_children` are the real entries returned by `Backend::list_children`;
/// `mount_names` are the synthetic child-mount directory names from
/// [`VfsTree::resolve_listing`]. Each mount name is appended as a directory
/// entry only if no real entry already carries that name, so an injected mount
/// directory shadows (rather than duplicates) a same-named backend entry.
///
/// Free function (rather than a `VfsTree` method) so a presenter that releases a
/// synchronous lock before awaiting `list_children` can produce a listing
/// identical to [`VfsTree::read_dir`] without re-implementing the merge.
#[must_use]
pub fn merge_listing(backend_children: Vec<FileEntry>, mount_names: &[String]) -> Vec<DirEntry> {
    let mut entries: Vec<DirEntry> = backend_children
        .into_iter()
        .map(|entry| DirEntry {
            name: entry.name,
            is_dir: entry.is_dir,
        })
        .collect();

    for mount_dir_name in mount_names {
        if !entries.iter().any(|e| &e.name == mount_dir_name) {
            entries.push(DirEntry::dir(mount_dir_name.clone()));
        }
    }

    entries
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
    use crate::types::{Change, Cursor, FileEntry, Quota};
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
            path: name.to_string(),
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

    /// In-memory backend that supports `metadata`, `download`, `upload`,
    /// `delete`, and `move_entry` — the operations exercised by
    /// `VfsTree::read_dir` and `VfsTree::rename`. Tracks content by native id
    /// and records delete and move operations so cross-backend behaviour can
    /// be verified.
    #[derive(Debug)]
    struct MemBackend {
        id: String,
        files: Mutex<HashMap<String, FileEntry>>,
        content: Mutex<HashMap<String, Vec<u8>>>,
        deleted: Mutex<Vec<String>>,
        moved: Mutex<Vec<(String, String)>>,
    }

    impl MemBackend {
        fn new(id: &str) -> Self {
            Self {
                id: id.to_owned(),
                files: Mutex::new(HashMap::new()),
                content: Mutex::new(HashMap::new()),
                deleted: Vec::new().into(),
                moved: Vec::new().into(),
            }
        }

        fn put(&self, native_id: &str, parent: &str, name: &str, data: Vec<u8>) {
            let entry = FileEntry {
                id: ItemId::new(&self.id, native_id),
                parent_id: ItemId::new(&self.id, parent),
                path: name.to_owned(),
                name: name.to_owned(),
                is_dir: false,
                size: Some(data.len() as u64),
                mod_time: None,
                mime_type: None,
                hash: None,
            };
            self.files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(native_id.to_owned(), entry);
            self.content
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(native_id.to_owned(), data);
        }

        fn deleted(&self) -> Vec<String> {
            self.deleted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        fn moved(&self) -> Vec<(String, String)> {
            self.moved
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    #[async_trait]
    impl Backend for MemBackend {
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
            let files = self
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let changes = files.values().map(|e| Change::Created(e.clone())).collect();
            Ok((changes, Cursor("mem".to_owned())))
        }

        async fn metadata(&self, path: &Path) -> anyhow::Result<FileEntry> {
            let target = path.to_string_lossy().to_string();
            let files = self
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            files
                .values()
                .find(|e| e.name == target || e.id.native_id() == target)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no entry at {target}"))
        }

        async fn download(&self, file: &FileEntry) -> anyhow::Result<Vec<u8>> {
            let native = file.id.native_id().to_owned();
            let content = self
                .content
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            content
                .get(&native)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no content for {native}"))
        }

        async fn upload(
            &self,
            path: &Path,
            data: &[u8],
            parent_id: &FileId,
        ) -> anyhow::Result<FileEntry> {
            let native = path.to_string_lossy().to_string();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .map_or_else(String::new, ToOwned::to_owned);
            let entry = FileEntry {
                id: ItemId::new(&self.id, &native),
                parent_id: ItemId::new(&self.id, &parent_id.0),
                path: name.clone(),
                name,
                is_dir: false,
                size: Some(data.len() as u64),
                mod_time: None,
                mime_type: None,
                hash: None,
            };
            self.files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(native.clone(), entry.clone());
            self.content
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(native, data.to_vec());
            Ok(entry)
        }

        async fn update(&self, _file_id: &FileId, _data: &[u8]) -> anyhow::Result<FileEntry> {
            anyhow::bail!("update not supported in MemBackend")
        }

        async fn create_dir(&self, _path: &Path) -> anyhow::Result<FileEntry> {
            anyhow::bail!("create_dir not supported in MemBackend")
        }

        async fn delete(&self, file: &FileEntry) -> anyhow::Result<()> {
            let native = file.id.native_id().to_owned();
            self.files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&native);
            self.content
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&native);
            self.deleted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(native);
            Ok(())
        }

        async fn move_entry(&self, src: &Path, dst: &Path) -> anyhow::Result<FileEntry> {
            let src_key = src.to_string_lossy().to_string();
            let dst_key = dst.to_string_lossy().to_string();
            self.moved
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((src_key.clone(), dst_key.clone()));
            let mut files = self
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = files
                .values()
                .find(|e| e.name == src_key || e.id.native_id() == src_key)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no entry at {src_key}"))?;
            let remove_key = entry.id.native_id().to_owned();
            files.remove(&remove_key);
            let new_entry = FileEntry {
                id: ItemId::new(&self.id, &dst_key),
                name: dst
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map_or_else(|| entry.name.clone(), ToOwned::to_owned),
                ..entry
            };
            files.insert(dst_key, new_entry.clone());
            Ok(new_entry)
        }

        async fn list_children(&self, _parent: &str) -> anyhow::Result<Vec<FileEntry>> {
            let files = self
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Ok(files.values().cloned().collect())
        }

        async fn poll_interval(&self) -> Option<Duration> {
            None
        }
    }

    #[test]
    fn new_creates_tree_with_root_backend() {
        let tree = make_tree();
        assert_eq!(tree.root().id(), "root");
        assert!(tree.children().is_empty());
    }

    #[test]
    fn mount_adds_child_at_prefix() {
        let mut tree = make_tree();
        tree.mount(PathBuf::from("Work"), Arc::new(NullBackend::new("work")));
        tree.mount(
            PathBuf::from("Assets"),
            Arc::new(NullBackend::new("assets")),
        );

        let prefixes: Vec<&std::ffi::OsStr> =
            tree.children().iter().map(|(p, _)| p.as_os_str()).collect();
        assert_eq!(prefixes.len(), 2);
        assert!(prefixes.contains(&std::ffi::OsStr::new("Work")));
        assert!(prefixes.contains(&std::ffi::OsStr::new("Assets")));
    }

    #[test]
    fn mount_orders_longest_prefix_first() {
        let mut tree = make_tree();
        // Mount the shorter prefix first; the longer one should still
        // end up ahead of it in the iteration order.
        tree.mount(PathBuf::from("Work"), Arc::new(NullBackend::new("work")));
        tree.mount(
            PathBuf::from("Work/Projects"),
            Arc::new(NullBackend::new("projects")),
        );

        let prefixes: Vec<String> = tree
            .children()
            .iter()
            .map(|(p, _)| p.to_string_lossy().to_string())
            .collect();
        assert_eq!(prefixes.first().map(String::as_str), Some("Work/Projects"));
    }

    #[test]
    fn unmount_returns_none_for_missing_prefix() {
        let mut tree = make_tree();
        let removed = tree.unmount(Path::new("Does/Not/Exist"));
        assert!(removed.is_none());
        assert!(tree.children().is_empty());
    }

    #[test]
    fn unmount_returns_the_removed_backend() {
        let mut tree = make_tree();
        tree.mount(PathBuf::from("Work"), Arc::new(NullBackend::new("work")));
        let removed = tree.unmount(Path::new("Work"));
        assert!(removed.is_some());
        let removed = removed.expect("present");
        assert_eq!(removed.id(), "work");
    }

    #[test]
    fn resolve_picks_child_for_partial_overlap() {
        // A shorter prefix should not win when a longer one matches.
        let mut tree = make_tree();
        tree.mount(PathBuf::from("Work"), Arc::new(NullBackend::new("work")));
        tree.mount(
            PathBuf::from("Workbench"),
            Arc::new(NullBackend::new("bench")),
        );

        let (backend, rest) = tree.resolve(Path::new("Workbench/tool"));
        assert_eq!(backend.id(), "bench");
        assert_eq!(rest, Path::new("tool"));
    }

    #[test]
    fn resolve_falls_back_to_root_when_no_prefix_matches() {
        let mut tree = make_tree();
        tree.mount(PathBuf::from("Work"), Arc::new(NullBackend::new("work")));
        let (backend, rest) = tree.resolve(Path::new("Personal/notes.txt"));
        assert_eq!(backend.id(), "root");
        assert_eq!(rest, Path::new("Personal/notes.txt"));
    }

    #[test]
    fn resolve_at_root_child_is_final_fallback_not_a_shadow() {
        // A backend mounted at the empty prefix (the "/" case) must own only the
        // paths no explicit prefix claims, never shadow an explicit mount — even
        // if the at-root child is encountered first in the children list. This
        // exercises the explicit at-root branch rather than relying on the
        // length-descending sort placing the empty prefix last.
        let mut tree = make_tree();
        // Mount the at-root backend first so it sits ahead of the explicit one
        // in iteration order before the sort; the sort keeps it last by length,
        // but the explicit at-root branch guarantees correctness regardless.
        tree.mount(PathBuf::new(), Arc::new(NullBackend::new("atroot")));
        tree.mount(PathBuf::from("Work"), Arc::new(NullBackend::new("work")));

        // An explicit-prefix path routes to the explicit backend with the prefix
        // stripped, not to the at-root backend.
        let (backend, rest) = tree.resolve(Path::new("Work/report.txt"));
        assert_eq!(backend.id(), "work");
        assert_eq!(rest, Path::new("report.txt"));

        // An unclaimed path falls through to the at-root backend with its path
        // carried verbatim.
        let (backend, rest) = tree.resolve(Path::new("Personal/notes.txt"));
        assert_eq!(backend.id(), "atroot");
        assert_eq!(rest, Path::new("Personal/notes.txt"));
    }

    #[tokio::test]
    async fn read_dir_includes_backend_entries() {
        let backend = Arc::new(MemBackend::new("root"));
        backend.put("a", "root", "alpha.txt", b"a".to_vec());
        backend.put("b", "root", "beta.txt", b"bb".to_vec());

        let tree = VfsTree::new(backend);
        // The mount root: an empty backend-relative path resolves to the
        // backend's root native id without a `metadata` round-trip.
        let entries = tree.read_dir(Path::new("")).await.expect("read_dir");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"alpha.txt"));
        assert!(names.contains(&"beta.txt"));
    }

    #[tokio::test]
    async fn read_dir_injects_child_mount_point_into_parent() {
        let mut tree = VfsTree::new(Arc::new(MemBackend::new("root")));
        tree.mount(PathBuf::from("Work"), Arc::new(MemBackend::new("work")));
        let entries = tree.read_dir(Path::new("")).await.expect("read_dir");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"Work"),
            "mount point must be injected: {names:?}"
        );
        let work = entries
            .iter()
            .find(|e| e.name == "Work")
            .expect("Work entry");
        assert!(work.is_dir, "injected mount point must be a directory");
    }

    #[tokio::test]
    async fn read_dir_does_not_duplicate_mount_point_already_in_backend() {
        // A backend that already reports "Work" via changes() should not
        // cause the injected entry to be added a second time.
        let backend = Arc::new(MemBackend::new("root"));
        backend.put("w", "root", "Work", b"x".to_vec());
        let mut tree = VfsTree::new(backend);
        tree.mount(PathBuf::from("Work"), Arc::new(MemBackend::new("work")));
        let entries = tree.read_dir(Path::new("")).await.expect("read_dir");
        let work_count = entries.iter().filter(|e| e.name == "Work").count();
        assert_eq!(work_count, 1);
    }

    #[tokio::test]
    async fn rename_within_same_backend_calls_move_entry() {
        let backend = Arc::new(MemBackend::new("root"));
        backend.put("a", "root", "old.txt", b"data".to_vec());
        let tree = VfsTree::new(backend.clone());

        tree.rename(Path::new("old.txt"), Path::new("new.txt"))
            .await
            .expect("rename");

        let moved = backend.moved();
        assert_eq!(moved, vec![("old.txt".to_owned(), "new.txt".to_owned())]);
        assert!(backend.deleted().is_empty());
    }

    #[tokio::test]
    async fn rename_across_backends_downloads_uploads_and_deletes() {
        let src = Arc::new(MemBackend::new("src"));
        let dst = Arc::new(MemBackend::new("dst"));
        src.put("a", "root", "file.txt", b"hello".to_vec());

        let mut tree = VfsTree::new(Arc::new(MemBackend::new("root")));
        tree.mount(PathBuf::from("Work"), src.clone());
        tree.mount(PathBuf::from("Archive"), dst.clone());

        tree.rename(Path::new("Work/file.txt"), Path::new("Archive/file.txt"))
            .await
            .expect("rename");

        // Source should have been deleted.
        assert_eq!(src.deleted(), vec!["a".to_owned()]);
        // Destination should now hold the file with the original content.
        let stored = dst
            .metadata(Path::new("file.txt"))
            .await
            .expect("destination entry exists");
        let downloaded = dst.download(&stored).await.expect("download");
        assert_eq!(downloaded, b"hello".to_vec());
    }

    /// Cross-backend rename: the source receives the mount-relative (not the
    /// full VFS) path on `metadata`, the destination receives the mount-relative
    /// path on `upload`, and the source's entry is deleted.
    ///
    /// This verifies that `VfsTree::resolve` correctly strips each backend's
    /// mount prefix so the backends always operate on their native paths,
    /// regardless of where they are mounted in the VFS tree.
    #[tokio::test]
    async fn rename_across_mounts_uses_backend_relative_paths() {
        let src = Arc::new(MemBackend::new("src"));
        let dst = Arc::new(MemBackend::new("dst"));

        // Source has a file at `file.txt` (native), mounted under `Personal`.
        src.put("n1", "root", "file.txt", b"cross-backend".to_vec());

        let mut tree = VfsTree::new(Arc::new(MemBackend::new("root")));
        tree.mount(PathBuf::from("Personal"), src.clone());
        tree.mount(PathBuf::from("Shared"), dst.clone());

        // Move `Personal/file.txt` (full VFS path) → `Shared/moved.txt`.
        tree.rename(
            Path::new("Personal/file.txt"),
            Path::new("Shared/moved.txt"),
        )
        .await
        .expect("cross-mount rename");

        // Source entry was deleted.
        assert_eq!(src.deleted(), vec!["n1".to_owned()]);

        // Destination received the mount-relative path (`moved.txt`, not
        // `Shared/moved.txt`) and has the original content.
        let stored = dst
            .metadata(Path::new("moved.txt"))
            .await
            .expect("destination entry must exist at mount-relative path");
        let content = dst.download(&stored).await.expect("download");
        assert_eq!(content, b"cross-backend".to_vec());

        // No entries remain in the source backend.
        let src_children = src.list_children("root").await.expect("list");
        assert!(
            src_children.is_empty(),
            "source entry must be gone: {src_children:?}"
        );
    }

    #[tokio::test]
    async fn list_children_by_id_delegates_to_named_backend() {
        let src = Arc::new(MemBackend::new("src"));
        src.put("c1", "parent", "child.txt", b"x".to_vec());

        let mut tree = VfsTree::new(Arc::new(MemBackend::new("root")));
        tree.mount(PathBuf::from("Work"), src.clone());

        let id = ItemId::new("src", "parent");
        let children = tree
            .list_children_by_id(&id)
            .await
            .expect("list_children_by_id");
        assert!(children.iter().any(|c| c.name == "child.txt"));
    }

    #[tokio::test]
    async fn list_children_by_id_errors_for_unknown_backend() {
        let tree = make_tree();
        let id = ItemId::new("ghost", "parent");
        let err = tree.list_children_by_id(&id).await.unwrap_err();
        assert!(err.to_string().contains("ghost"));
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

    // --- backend_for_path tests -----------------------------------------

    #[test]
    fn backend_for_path_falls_back_to_root() {
        let tree = make_tree();
        let (backend, rest) = tree.backend_for_path("Documents/notes.txt");
        assert_eq!(backend.id(), "root");
        assert_eq!(rest, "Documents/notes.txt");
    }

    #[test]
    fn backend_for_path_matches_child_mount() {
        let mut tree = make_tree();
        tree.mount(PathBuf::from("Work"), Arc::new(NullBackend::new("work")));
        let (backend, rest) = tree.backend_for_path("Work/Projects/code.rs");
        assert_eq!(backend.id(), "work");
        assert_eq!(rest, "Projects/code.rs");
    }

    #[test]
    fn backend_for_path_exact_mount_name() {
        let mut tree = make_tree();
        tree.mount(PathBuf::from("Work"), Arc::new(NullBackend::new("work")));
        // Exact match — the path IS the mount point directory.
        let (backend, rest) = tree.backend_for_path("Work");
        assert_eq!(backend.id(), "work");
        assert_eq!(rest, "");
    }

    #[test]
    fn backend_for_path_no_partial_prefix_confusion() {
        let mut tree = make_tree();
        tree.mount(PathBuf::from("Work"), Arc::new(NullBackend::new("work")));
        tree.mount(
            PathBuf::from("Workbench"),
            Arc::new(NullBackend::new("bench")),
        );
        // "Work" must not match "Workbench/tool".
        let (backend, rest) = tree.backend_for_path("Workbench/tool");
        assert_eq!(backend.id(), "bench");
        assert_eq!(rest, "tool");
    }

    #[test]
    fn backend_for_path_nested_mount_longest_prefix() {
        let mut tree = make_tree();
        tree.mount(PathBuf::from("Work"), Arc::new(NullBackend::new("work")));
        tree.mount(
            PathBuf::from("Work/Assets"),
            Arc::new(NullBackend::new("assets")),
        );
        // Nested: Work/Assets/logo.png routes to assets.
        let (backend, rest) = tree.backend_for_path("Work/Assets/logo.png");
        assert_eq!(backend.id(), "assets");
        assert_eq!(rest, "logo.png");

        // Non-nested: Work/report.txt routes to work.
        let (backend, rest) = tree.backend_for_path("Work/report.txt");
        assert_eq!(backend.id(), "work");
        assert_eq!(rest, "report.txt");
    }

    #[test]
    fn backend_for_path_at_root_mount_covers_all() {
        let mut tree = make_tree();
        // Empty-prefix = mounted at "/".
        tree.mount(PathBuf::from(""), Arc::new(NullBackend::new("gdrive")));
        let (backend, rest) = tree.backend_for_path("Documents/notes.txt");
        assert_eq!(backend.id(), "gdrive");
        assert_eq!(rest, "Documents/notes.txt");
    }

    #[test]
    fn backend_for_path_explicit_mount_wins_over_at_root() {
        let mut tree = make_tree();
        // Explicit mount should win over at-root backend.
        tree.mount(PathBuf::from(""), Arc::new(NullBackend::new("gdrive")));
        tree.mount(PathBuf::from("Work"), Arc::new(NullBackend::new("work")));
        let (backend, rest) = tree.backend_for_path("Work/file.txt");
        assert_eq!(backend.id(), "work");
        assert_eq!(rest, "file.txt");

        // Unmatched path falls back to at-root gdrive.
        let (backend, rest) = tree.backend_for_path("Personal/notes.txt");
        assert_eq!(backend.id(), "gdrive");
        assert_eq!(rest, "Personal/notes.txt");
    }

    // --- read_dir list_children scoping tests --------------------------------

    /// `read_dir` must call `list_children` with the backend-relative path
    /// rather than a full-tree snapshot via `changes(None)`.  The `MemBackend`
    /// stub ignores the `parent_native_id` parameter and returns all of its
    /// files, so these tests verify the routing and injection logic rather than
    /// any per-directory filtering (which is the owning backend's
    /// responsibility).
    ///
    /// At-root (empty-prefix) backend: `read_dir("")` resolves to
    /// `(at_root_backend, "")`, which is passed to `list_children("")`.
    /// The resulting entries plus the injected child-mount dir must both appear
    /// in the output; a same-named real entry is shadowed by the injected one
    /// (de-duplicated to exactly one occurrence).
    #[tokio::test]
    async fn read_dir_at_root_backend_lists_children_and_injects_mounts() {
        // At-root backend (empty prefix) carrying a single real file.
        let gdrive = Arc::new(MemBackend::new("gdrive"));
        gdrive.put("d1", "root", "Documents", b"".to_vec());

        // A second backend mounted under Work.
        let work = Arc::new(MemBackend::new("work"));
        work.put("w1", "root", "report.txt", b"x".to_vec());

        let mut tree = VfsTree::new(Arc::new(NullBackend::new("null-root")));
        tree.mount(PathBuf::new(), gdrive.clone()); // at-root
        tree.mount(PathBuf::from("Work"), work.clone());

        // read_dir of the VFS root (""): the at-root backend owns this path.
        let entries = tree.read_dir(Path::new("")).await.expect("read_dir");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();

        // Real entry from the at-root backend.
        assert!(
            names.contains(&"Documents"),
            "real entry from at-root backend must appear: {names:?}"
        );
        // Injected mount-point dir for the explicit Work mount.
        assert!(
            names.contains(&"Work"),
            "child mount point must be injected: {names:?}"
        );
    }

    /// A same-named real entry from the owning backend must not be duplicated
    /// when the child-mount injection loop encounters the same name.
    #[tokio::test]
    async fn read_dir_at_root_backend_shadows_real_entry_with_mount_point() {
        // At-root backend has a real entry named "Work" — same as a child mount.
        let gdrive = Arc::new(MemBackend::new("gdrive"));
        gdrive.put("real-work", "root", "Work", b"".to_vec());

        let work = Arc::new(MemBackend::new("work"));
        let mut tree = VfsTree::new(Arc::new(NullBackend::new("null-root")));
        tree.mount(PathBuf::new(), gdrive.clone()); // at-root
        tree.mount(PathBuf::from("Work"), work);

        let entries = tree.read_dir(Path::new("")).await.expect("read_dir");
        let work_count = entries.iter().filter(|e| e.name == "Work").count();
        assert_eq!(
            work_count, 1,
            "\"Work\" must appear exactly once even when both the backend and the mount inject it"
        );
    }

    /// Explicit-prefix backend: `read_dir("Work")` resolves to
    /// `(work_backend, "")`, which is passed to `list_children("")`.
    /// The child-mount injection loop must still inject a grandchild mount
    /// (e.g. `Work/Assets`) as a dir entry under `Work`.
    #[tokio::test]
    async fn read_dir_explicit_prefix_backend_injects_nested_child_mount() {
        let work = Arc::new(MemBackend::new("work"));
        work.put("r1", "root", "readme.txt", b"hi".to_vec());

        let assets = Arc::new(MemBackend::new("assets"));
        let mut tree = VfsTree::new(Arc::new(NullBackend::new("null-root")));
        tree.mount(PathBuf::from("Work"), work.clone());
        tree.mount(PathBuf::from("Work/Assets"), assets);

        // Reading the Work directory: owning backend is "work", backend_path is "".
        let entries = tree.read_dir(Path::new("Work")).await.expect("read_dir");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();

        // Real entry from the work backend.
        assert!(
            names.contains(&"readme.txt"),
            "real entry from owning backend must appear: {names:?}"
        );
        // The "Work/Assets" mount must inject "Assets" under Work.
        assert!(
            names.contains(&"Assets"),
            "nested child-mount dir must be injected under parent: {names:?}"
        );
        let assets_entry = entries.iter().find(|e| e.name == "Assets").expect("Assets");
        assert!(
            assets_entry.is_dir,
            "injected nested mount must be a directory"
        );
    }

    // --- read_dir native-id resolution regression tests ----------------------

    /// Backend that honours the real `Backend::list_children` contract: it
    /// returns children only when handed the *native id* of a directory it
    /// knows, and an empty listing for anything else.
    ///
    /// This is the inverse of `MemBackend`, whose `list_children` ignores its
    /// argument entirely and so masks whether the caller passes a path or a
    /// native id. `NativeIdBackend` exposes the difference: if `read_dir`
    /// regresses to passing the backend-relative *path*, the requested native id
    /// never matches and the listing comes back empty, failing these tests.
    ///
    /// `children_by_native_id` maps a parent native id to the entries filed
    /// under it; `path_to_native_id` maps a backend-relative directory path to
    /// the native id `metadata` resolves it to. `root_native_id` defaults to the
    /// conventional `root` sentinel that `resolve_listing_native_id` uses for the
    /// mount root.
    #[derive(Debug)]
    struct NativeIdBackend {
        id: String,
        children_by_native_id: HashMap<String, Vec<FileEntry>>,
        path_to_native_id: HashMap<String, String>,
    }

    impl NativeIdBackend {
        fn new(id: &str) -> Self {
            Self {
                id: id.to_owned(),
                children_by_native_id: HashMap::new(),
                path_to_native_id: HashMap::new(),
            }
        }

        /// File `child_name` under the directory addressed by `parent_native_id`.
        fn add_child(&mut self, parent_native_id: &str, child_native: &str, child_name: &str) {
            let entry = FileEntry {
                id: ItemId::new(&self.id, child_native),
                parent_id: ItemId::new(&self.id, parent_native_id),
                path: child_name.to_owned(),
                name: child_name.to_owned(),
                is_dir: false,
                size: Some(child_name.len() as u64),
                mod_time: None,
                mime_type: None,
                hash: None,
            };
            self.children_by_native_id
                .entry(parent_native_id.to_owned())
                .or_default()
                .push(entry);
        }

        /// Register that the directory at backend-relative `path` has native id
        /// `native_id`, so `metadata(path)` resolves to it.
        fn map_path(&mut self, path: &str, native_id: &str) {
            self.path_to_native_id
                .insert(path.to_owned(), native_id.to_owned());
        }
    }

    #[async_trait]
    impl Backend for NativeIdBackend {
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
            Ok((vec![], Cursor("native".to_owned())))
        }

        async fn metadata(&self, path: &Path) -> anyhow::Result<FileEntry> {
            let key = path.to_string_lossy().to_string();
            let native_id = self
                .path_to_native_id
                .get(&key)
                .ok_or_else(|| anyhow::anyhow!("no entry at path {key}"))?;
            Ok(FileEntry {
                id: ItemId::new(&self.id, native_id),
                parent_id: ItemId::new(&self.id, "root"),
                path: key.clone(),
                name: path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map_or_else(String::new, ToOwned::to_owned),
                is_dir: true,
                size: None,
                mod_time: None,
                mime_type: None,
                hash: None,
            })
        }

        async fn download(&self, _file: &FileEntry) -> anyhow::Result<Vec<u8>> {
            anyhow::bail!("download not supported in NativeIdBackend")
        }

        async fn upload(
            &self,
            _path: &Path,
            _data: &[u8],
            _parent_id: &FileId,
        ) -> anyhow::Result<FileEntry> {
            anyhow::bail!("upload not supported in NativeIdBackend")
        }

        async fn update(&self, _file_id: &FileId, _data: &[u8]) -> anyhow::Result<FileEntry> {
            anyhow::bail!("update not supported in NativeIdBackend")
        }

        async fn create_dir(&self, _path: &Path) -> anyhow::Result<FileEntry> {
            anyhow::bail!("create_dir not supported in NativeIdBackend")
        }

        async fn delete(&self, _file: &FileEntry) -> anyhow::Result<()> {
            anyhow::bail!("delete not supported in NativeIdBackend")
        }

        async fn move_entry(&self, _src: &Path, _dst: &Path) -> anyhow::Result<FileEntry> {
            anyhow::bail!("move_entry not supported in NativeIdBackend")
        }

        async fn list_children(&self, parent_native_id: &str) -> anyhow::Result<Vec<FileEntry>> {
            // Honour the native-id contract: a path-shaped argument matches no
            // known native id and yields an empty listing.
            Ok(self
                .children_by_native_id
                .get(parent_native_id)
                .cloned()
                .unwrap_or_default())
        }

        async fn poll_interval(&self) -> Option<Duration> {
            None
        }
    }

    /// At the mount root the backend-relative path is empty, so `read_dir` must
    /// resolve it to the backend's `root_native_id` (`root`) and list the
    /// children filed there. The previous path-as-id code passed the empty
    /// string, which matches no native id and returns nothing — this test would
    /// fail against that regression.
    #[tokio::test]
    async fn read_dir_at_root_resolves_root_native_id_not_empty_path() {
        let mut gdrive = NativeIdBackend::new("gdrive");
        gdrive.add_child("root", "n-alpha", "alpha.txt");
        gdrive.add_child("root", "n-beta", "beta.txt");

        let tree = VfsTree::new(Arc::new(gdrive));
        let entries = tree.read_dir(Path::new("")).await.expect("read_dir");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();

        assert!(
            names.contains(&"alpha.txt") && names.contains(&"beta.txt"),
            "root listing must resolve the empty path to the root native id: {names:?}"
        );
    }

    /// For a sub-path `read_dir` must resolve the directory's native id via
    /// `metadata` and list children under that native id — not under the
    /// path-shaped argument. The previous code passed the sub-path verbatim,
    /// which matches no native id and returns nothing.
    #[tokio::test]
    async fn read_dir_subpath_resolves_metadata_native_id_not_path() {
        let mut gdrive = NativeIdBackend::new("gdrive");
        // The directory at backend-relative path "Reports" has native id
        // "folder-xyz"; its children are filed under that native id.
        gdrive.map_path("Reports", "folder-xyz");
        gdrive.add_child("folder-xyz", "n-q1", "q1.pdf");
        gdrive.add_child("folder-xyz", "n-q2", "q2.pdf");
        // A red herring keyed by the path itself: if read_dir wrongly passed the
        // path, it would still find nothing here, proving the native-id route.
        gdrive.add_child("Reports", "wrong", "should-not-appear.pdf");

        // Mount the backend at-root so "Reports" routes to it with backend_path
        // "Reports".
        let mut tree = VfsTree::new(Arc::new(NullBackend::new("null-root")));
        tree.mount(PathBuf::new(), Arc::new(gdrive));

        let entries = tree.read_dir(Path::new("Reports")).await.expect("read_dir");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();

        assert!(
            names.contains(&"q1.pdf") && names.contains(&"q2.pdf"),
            "sub-path listing must resolve the metadata native id: {names:?}"
        );
        assert!(
            !names.contains(&"should-not-appear.pdf"),
            "children must come from the native id, not the path: {names:?}"
        );
    }

    /// The native-id resolution must not disturb child-mount injection: the
    /// at-root case still injects an explicit child mount, and a sub-path case
    /// still injects a nested mount, exactly as before.
    #[tokio::test]
    async fn read_dir_native_id_resolution_preserves_mount_injection() {
        // At-root: empty path resolves to the root native id, and the explicit
        // "Work" mount is still injected.
        let mut gdrive = NativeIdBackend::new("gdrive");
        gdrive.add_child("root", "n-doc", "Documents");

        let mut tree = VfsTree::new(Arc::new(NullBackend::new("null-root")));
        tree.mount(PathBuf::new(), Arc::new(gdrive));
        tree.mount(
            PathBuf::from("Work"),
            Arc::new(NativeIdBackend::new("work")),
        );

        let entries = tree.read_dir(Path::new("")).await.expect("read_dir");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"Documents"),
            "real root entry must appear: {names:?}"
        );
        assert!(
            names.contains(&"Work"),
            "explicit child mount must still be injected at root: {names:?}"
        );

        // Mount root of an explicit mount: reading "Work" resolves to the work
        // backend with an empty backend-relative path, which maps to its root
        // native id; the nested "Work/Assets" mount is still injected under it.
        let mut work = NativeIdBackend::new("work2");
        work.add_child("root", "n-readme", "readme.txt");

        let mut tree = VfsTree::new(Arc::new(NullBackend::new("null-root")));
        tree.mount(PathBuf::from("Work"), Arc::new(work));
        tree.mount(
            PathBuf::from("Work/Assets"),
            Arc::new(NativeIdBackend::new("assets")),
        );

        let entries = tree.read_dir(Path::new("Work")).await.expect("read_dir");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"readme.txt"),
            "real entry under Work must appear: {names:?}"
        );
        assert!(
            names.contains(&"Assets"),
            "nested child mount must still be injected: {names:?}"
        );
    }

    /// `resolve_listing_native_id` is exercised directly: the empty path maps to
    /// the backend's `root_native_id`, and a registered path maps to the native
    /// id `metadata` returns.
    #[tokio::test]
    async fn resolve_listing_native_id_maps_empty_and_subpath() {
        let mut gdrive = NativeIdBackend::new("gdrive");
        gdrive.map_path("Reports", "folder-xyz");
        let gdrive: Arc<dyn Backend> = Arc::new(gdrive);

        let at_root = resolve_listing_native_id(gdrive.as_ref(), "")
            .await
            .expect("root native id");
        assert_eq!(at_root, "root");

        let nested = resolve_listing_native_id(gdrive.as_ref(), "Reports")
            .await
            .expect("metadata native id");
        assert_eq!(nested, "folder-xyz");
    }
}
