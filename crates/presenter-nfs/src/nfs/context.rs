//! NFS context — bridges NFS procedure handlers to the VFS tree.
//!
//! Provides the state needed by NFS procedures to answer queries about
//! files and directories in the virtual filesystem.

use cascade_engine::types::DirEntry;
use cascade_engine::vfs::VfsTree;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

/// Context shared between NFS procedure handlers and the VFS tree.
///
/// Maintains a file handle → VFS path mapping so NFS clients can
/// navigate the tree with opaque file handles.
pub struct NfsContext {
    /// The VFS tree to query for file metadata.
    vfs: Arc<RwLock<VfsTree>>,
    /// Map from file handle key (hash) to VFS path.
    fh_map: RwLock<HashMap<u64, String>>,
    /// Root file handle key — always maps to "/".
    root_fh_key: u64,
}

impl NfsContext {
    pub fn new(vfs: Arc<RwLock<VfsTree>>) -> Self {
        let root_fh_key = Self::path_to_key("/");
        let mut fh_map = HashMap::new();
        fh_map.insert(root_fh_key, "/".to_string());

        Self {
            vfs,
            fh_map: RwLock::new(fh_map),
            root_fh_key,
        }
    }

    /// Convert a VFS path to a file handle key.
    pub fn path_to_key(path: &str) -> u64 {
        let mut hash: u64 = 5381;
        for byte in path.bytes() {
            hash = hash.wrapping_mul(33).wrapping_add(byte as u64);
        }
        hash
    }

    /// Register a path and get its file handle key.
    pub fn register_path(&self, path: &str) -> u64 {
        let key = Self::path_to_key(path);
        let mut map = self.fh_map.write().unwrap();
        map.entry(key).or_insert_with(|| path.to_string());
        key
    }

    /// Look up the VFS path for a file handle key.
    pub fn lookup_path(&self, fh_key: u64) -> Option<String> {
        self.fh_map.read().unwrap().get(&fh_key).cloned()
    }

    /// Get the root file handle key.
    pub fn root_key(&self) -> u64 {
        self.root_fh_key
    }

    /// List directory contents at a VFS path.
    #[allow(clippy::await_holding_lock)]
    pub async fn list_dir(&self, path: &str) -> anyhow::Result<Vec<DirEntry>> {
        let vfs = self.vfs.read().unwrap();
        vfs.read_dir(std::path::Path::new(path)).await
    }

    /// Get file metadata at a VFS path.
    #[allow(clippy::await_holding_lock)]
    pub async fn metadata(&self, path: &str) -> anyhow::Result<cascade_engine::types::FileEntry> {
        let (backend, relative) = {
            let vfs = self.vfs.read().unwrap();
            let (backend, relative) = vfs.resolve(std::path::Path::new(path));
            (Arc::clone(backend), relative.to_path_buf())
        };
        backend.metadata(&relative).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_to_key_deterministic() {
        assert_eq!(NfsContext::path_to_key("/"), NfsContext::path_to_key("/"));
        assert_ne!(
            NfsContext::path_to_key("/"),
            NfsContext::path_to_key("/foo")
        );
    }

    #[test]
    fn register_and_lookup() {
        let vfs = Arc::new(RwLock::new(VfsTree::new(Arc::new(
            cascade_engine::backend::NullBackend::new("test"),
        ))));
        let ctx = NfsContext::new(vfs);

        let key = ctx.register_path("/Documents");
        assert_eq!(ctx.lookup_path(key), Some("/Documents".to_string()));
        assert_eq!(ctx.lookup_path(99999), None);
    }

    #[test]
    fn root_always_registered() {
        let vfs = Arc::new(RwLock::new(VfsTree::new(Arc::new(
            cascade_engine::backend::NullBackend::new("test"),
        ))));
        let ctx = NfsContext::new(vfs);
        assert_eq!(ctx.lookup_path(ctx.root_key()), Some("/".to_string()));
    }
}
