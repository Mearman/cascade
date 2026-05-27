//! VfsTree — composes multiple backends with longest-prefix routing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::backend::Backend;
use crate::types::{Change, DirEntry, FileId};

/// VFS tree that routes operations to the correct backend by longest-prefix match.
pub struct VfsTree {
    /// The root backend — handles paths not covered by any child.
    root: Arc<dyn Backend>,

    /// Sorted list of (path_prefix, backend) bindings.
    /// Sorted longest-prefix-first so the first match wins.
    children: Vec<(PathBuf, Arc<dyn Backend>)>,
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
        self.children.sort_by(|a, b| b.0.as_os_str().len().cmp(&a.0.as_os_str().len()));
    }

    /// Remove a child backend by prefix. Returns the backend if found.
    pub fn unmount(&mut self, prefix: &Path) -> Option<Arc<dyn Backend>> {
        let idx = self.children.iter().position(|(p, _)| p == prefix)?;
        Some(self.children.remove(idx).1)
    }

    /// Resolve a path to the correct backend and the remaining path within that backend.
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
                if entry.is_dir || !entry.is_dir {
                    entries.push(DirEntry {
                        name: entry.name,
                        is_dir: entry.is_dir,
                    });
                }
            }
        }

        // Inject child mount point directories if this path is their parent
        for (child_prefix, _) in &self.children {
            if child_prefix.parent() == Some(path) {
                if let Some(mount_dir_name) = child_prefix.file_name() {
                    let mount_dir_name = mount_dir_name.to_string_lossy();
                    if !entries.iter().any(|e| e.name == mount_dir_name) {
                        entries.push(DirEntry::dir(mount_dir_name.to_string()));
                    }
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
            let mut data = Vec::new();
            src_backend.download(&entry, &mut data).await?;
            let parent_id = FileId(entry.parent_id.0.clone());
            dst_backend
                .upload(&dst_path, &mut &data[..], &parent_id)
                .await?;
            src_backend.delete(&entry).await?;
        }
        Ok(())
    }

    /// Get the root backend.
    pub fn root(&self) -> &Arc<dyn Backend> {
        &self.root
    }

    /// Get all child mounts.
    pub fn children(&self) -> &[(PathBuf, Arc<dyn Backend>)] {
        &self.children
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::NullBackend;

    fn make_tree() -> VfsTree {
        let root = Arc::new(NullBackend::new("root"));
        VfsTree::new(root)
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
        tree.mount(
            PathBuf::from("Work"),
            Arc::new(NullBackend::new("work")),
        );
        let (backend, rest) = tree.resolve(Path::new("Work/Projects/code.rs"));
        assert_eq!(backend.id(), "work");
        assert_eq!(rest, Path::new("Projects/code.rs"));
    }

    #[test]
    fn longest_prefix_wins() {
        let mut tree = make_tree();
        tree.mount(
            PathBuf::from("Work"),
            Arc::new(NullBackend::new("work")),
        );
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
        tree.mount(
            PathBuf::from("Work"),
            Arc::new(NullBackend::new("work")),
        );
        assert!(tree.unmount(Path::new("Work")).is_some());
        assert!(tree.children().is_empty());
    }
}
