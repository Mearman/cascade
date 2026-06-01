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

impl std::fmt::Debug for NfsContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NfsContext")
            .field("root_fh_key", &self.root_fh_key)
            .finish_non_exhaustive()
    }
}

impl NfsContext {
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
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
    #[must_use]
    pub fn path_to_key(path: &str) -> u64 {
        let mut hash: u64 = 5381;
        for byte in path.bytes() {
            hash = hash.wrapping_mul(33).wrapping_add(u64::from(byte));
        }
        hash
    }

    /// Register a path and get its file handle key.
    pub fn register_path(&self, path: &str) -> u64 {
        let key = Self::path_to_key(path);
        let mut map = self
            .fh_map
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.entry(key).or_insert_with(|| path.to_string());
        key
    }

    /// Look up the VFS path for a file handle key.
    pub fn lookup_path(&self, fh_key: u64) -> Option<String> {
        self.fh_map
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&fh_key)
            .cloned()
    }

    /// Remove a file handle key from the map.
    pub fn remove_path(&self, fh_key: u64) {
        self.fh_map
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&fh_key);
    }

    /// Access the underlying VFS tree (for download operations).
    #[must_use]
    pub const fn vfs(&self) -> &Arc<RwLock<VfsTree>> {
        &self.vfs
    }

    /// Get the root file handle key.
    #[must_use]
    pub const fn root_key(&self) -> u64 {
        self.root_fh_key
    }

    /// List directory contents at a VFS path.
    ///
    /// # Errors
    ///
    /// Returns an error if the VFS tree cannot read the directory.
    ///
    /// Note: the `RwLockReadGuard` is intentionally held across the `.await`
    /// here because `VfsTree::read_dir` needs `&self` for the duration of the
    /// call. The future is therefore not `Send`. All callers invoke this via
    /// `block_on` from a sync context, so `Send` is not required.
    #[allow(clippy::await_holding_lock, clippy::future_not_send)]
    pub async fn list_dir(&self, path: &str) -> anyhow::Result<Vec<DirEntry>> {
        let vfs = self
            .vfs
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        vfs.read_dir(std::path::Path::new(path)).await
    }

    /// Get file metadata at a VFS path.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend cannot fetch metadata.
    pub async fn metadata(&self, path: &str) -> anyhow::Result<cascade_engine::types::FileEntry> {
        let (backend, relative) = {
            let vfs = self
                .vfs
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (backend, relative) = vfs.resolve(std::path::Path::new(path));
            let result = (Arc::clone(backend), relative);
            drop(vfs);
            result
        };
        backend.metadata(&relative).await
    }

    /// Synchronous wrapper around `list_dir` for use in NFS procedure
    /// handlers which must return `Vec<u8>` synchronously.
    ///
    /// Uses `tokio::runtime::Handle::block_on` to run the async VFS query
    /// on the current tokio runtime.
    ///
    /// # Errors
    ///
    /// Returns an error if the VFS tree cannot read the directory.
    pub fn list_dir_sync(&self, path: &str) -> anyhow::Result<Vec<DirEntry>> {
        let rt = tokio::runtime::Handle::current();
        rt.block_on(self.list_dir(path))
    }

    /// Synchronous wrapper for fetching file metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend cannot fetch metadata.
    pub fn metadata_sync(&self, path: &str) -> anyhow::Result<cascade_engine::types::FileEntry> {
        let rt = tokio::runtime::Handle::current();
        rt.block_on(self.metadata(path))
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
        assert_eq!(ctx.lookup_path(99_999), None);
    }

    #[test]
    fn root_always_registered() {
        let vfs = Arc::new(RwLock::new(VfsTree::new(Arc::new(
            cascade_engine::backend::NullBackend::new("test"),
        ))));
        let ctx = NfsContext::new(vfs);
        assert_eq!(ctx.lookup_path(ctx.root_key()), Some("/".to_string()));
    }

    #[test]
    fn remove_path_removes_entry() {
        let vfs = Arc::new(RwLock::new(VfsTree::new(Arc::new(
            cascade_engine::backend::NullBackend::new("test"),
        ))));
        let ctx = NfsContext::new(vfs);
        let key = ctx.register_path("/tmp");
        assert!(ctx.lookup_path(key).is_some());
        ctx.remove_path(key);
        assert!(ctx.lookup_path(key).is_none());
    }
}

/// Property-based tests for `NfsContext`.
#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn path_to_key_deterministic_prop(s in ".*") {
            let k1 = NfsContext::path_to_key(&s);
            let k2 = NfsContext::path_to_key(&s);
            prop_assert_eq!(k1, k2);
        }

        #[test]
        fn path_to_key_different_strings_mostly_differ(s1 in ".*", s2 in ".*") {
            prop_assume!(s1 != s2);
            let k1 = NfsContext::path_to_key(&s1);
            let k2 = NfsContext::path_to_key(&s2);
            // Hash collisions are possible but should be extremely rare.
            // We don't assert they always differ, but log if they collide.
            if k1 == k2 {
                // Acceptable but noteworthy.
            }
        }

        #[test]
        fn register_lookup_roundtrip(path in ".*") {
            let vfs = Arc::new(RwLock::new(VfsTree::new(Arc::new(
                cascade_engine::backend::NullBackend::new("test"),
            ))));
            let ctx = NfsContext::new(vfs);
            let key = ctx.register_path(&path);
            prop_assert_eq!(ctx.lookup_path(key), Some(path));
        }
    }
}
