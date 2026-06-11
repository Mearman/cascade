//! Inode management — a path-aware, bidirectional map between VFS paths and
//! FUSE inode numbers.
//!
//! The root inode is always 1 and maps to the neutral VFS root path `"/"`.
//! Subsequent inodes are allocated sequentially. The primary identity of an
//! inode is its VFS path: `readdir`, `lookup`, `getattr`, and `read` all derive
//! a path from an inode and route it through `VfsTree::resolve` /
//! `VfsTree::read_dir`, which already merge backend children with child-mount
//! injection and apply the shadow rule.
//!
//! A parallel `ItemId` index is retained for the presenter's sync bookkeeping
//! (`upsert_item` / `delete_item` receive `VfsItem`s and `ItemId`s, not paths),
//! and shares the single inode allocator so both views agree on inode numbers.

use std::collections::HashMap;

use cascade_engine::types::ItemId;

/// The root inode number, always mapped to the neutral VFS root path.
pub const ROOT_INODE: u64 = 1;

/// The neutral VFS root path. The root inode resolves to this path; every other
/// inode resolves to a VFS-absolute path beneath it.
pub const ROOT_PATH: &str = "/";

/// Path-aware bidirectional map between VFS paths and FUSE inode numbers.
///
/// Each inode has a canonical VFS path. A secondary `ItemId` index lets the
/// presenter's sync handlers allocate and remove inodes by `ItemId` without
/// having to reconstruct the path, while still sharing one inode space with the
/// path-keyed view the read handlers use.
#[derive(Debug)]
pub struct InodeMap {
    /// VFS path → inode number.
    path_to_inode: HashMap<String, u64>,
    /// Inode number → VFS path.
    inode_to_path: HashMap<u64, String>,
    /// `ItemId` → inode number (sync bookkeeping).
    id_to_inode: HashMap<ItemId, u64>,
    /// Inode number → `ItemId` (sync bookkeeping).
    inode_to_id: HashMap<u64, ItemId>,
    /// Next available inode number.
    next_inode: u64,
}

impl InodeMap {
    /// Create a new inode map with the root pre-allocated at inode 1.
    ///
    /// The root inode maps to the neutral root path `"/"`. The supplied
    /// `root_id` is also registered in the `ItemId` index so the presenter's
    /// sync handlers can resolve the root by id, keeping the two views in step.
    #[must_use]
    pub fn new(root_id: ItemId) -> Self {
        let mut map = Self {
            path_to_inode: HashMap::new(),
            inode_to_path: HashMap::new(),
            id_to_inode: HashMap::new(),
            inode_to_id: HashMap::new(),
            next_inode: ROOT_INODE + 1,
        };
        map.path_to_inode.insert(ROOT_PATH.to_owned(), ROOT_INODE);
        map.inode_to_path.insert(ROOT_INODE, ROOT_PATH.to_owned());
        map.id_to_inode.insert(root_id.clone(), ROOT_INODE);
        map.inode_to_id.insert(ROOT_INODE, root_id);
        map
    }

    /// Allocate an inode for the given VFS path. If one already exists, returns
    /// it. Idempotent: the same path always resolves to the same inode.
    pub fn allocate_path(&mut self, path: &str) -> u64 {
        if let Some(&inode) = self.path_to_inode.get(path) {
            return inode;
        }
        let inode = self.next_inode;
        self.next_inode += 1;
        self.path_to_inode.insert(path.to_owned(), inode);
        self.inode_to_path.insert(inode, path.to_owned());
        inode
    }

    /// Look up the VFS path for an inode number.
    #[must_use]
    pub fn path_for(&self, inode: u64) -> Option<&str> {
        self.inode_to_path.get(&inode).map(String::as_str)
    }

    /// Look up the inode number for a VFS path.
    #[must_use]
    pub fn inode_for_path(&self, path: &str) -> Option<u64> {
        self.path_to_inode.get(path).copied()
    }

    /// Remove a VFS path and its associated inode from the map.
    pub fn remove_path(&mut self, path: &str) {
        if let Some(inode) = self.path_to_inode.remove(path) {
            self.inode_to_path.remove(&inode);
            if let Some(id) = self.inode_to_id.remove(&inode) {
                self.id_to_inode.remove(&id);
            }
        }
    }

    /// Allocate an inode for the given `ItemId`, binding it to the supplied VFS
    /// path. Used by the presenter's sync path, which receives a `VfsItem`
    /// carrying both halves. If the path already has an inode, the `ItemId`
    /// index is pointed at the same inode so both views stay consistent.
    pub fn allocate_id_with_path(&mut self, id: ItemId, path: &str) -> u64 {
        let inode = self.allocate_path(path);
        self.id_to_inode.insert(id.clone(), inode);
        self.inode_to_id.insert(inode, id);
        inode
    }

    /// Look up the inode number for an `ItemId`.
    #[must_use]
    pub fn get_inode(&self, id: &ItemId) -> Option<u64> {
        self.id_to_inode.get(id).copied()
    }

    /// Look up the `ItemId` for an inode number.
    #[must_use]
    pub fn get_id(&self, inode: u64) -> Option<&ItemId> {
        self.inode_to_id.get(&inode)
    }

    /// Remove an `ItemId` (and its bound inode and path) from the map.
    pub fn remove(&mut self, id: &ItemId) {
        if let Some(inode) = self.id_to_inode.remove(id) {
            self.inode_to_id.remove(&inode);
            if let Some(path) = self.inode_to_path.remove(&inode) {
                self.path_to_inode.remove(&path);
            }
        }
    }

    /// Number of distinct inodes mapped by path.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inode_to_path.len()
    }

    /// Whether the map is empty (it never is — root is always present).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inode_to_path.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root_id() -> ItemId {
        ItemId::new("gdrive", "root")
    }

    #[test]
    fn new_has_root_at_inode_1() {
        let map = InodeMap::new(root_id());
        assert_eq!(map.inode_for_path(ROOT_PATH), Some(ROOT_INODE));
        assert_eq!(map.path_for(ROOT_INODE), Some(ROOT_PATH));
        assert_eq!(map.get_inode(&root_id()), Some(ROOT_INODE));
        assert_eq!(map.get_id(ROOT_INODE), Some(&root_id()));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn allocate_path_sequential_inodes() {
        let mut map = InodeMap::new(root_id());
        let inode_a = map.allocate_path("work");
        let inode_b = map.allocate_path("personal");
        assert_eq!(inode_a, 2);
        assert_eq!(inode_b, 3);
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn allocate_path_idempotent() {
        let mut map = InodeMap::new(root_id());
        let first = map.allocate_path("work/projects");
        let second = map.allocate_path("work/projects");
        assert_eq!(first, second);
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn path_inode_roundtrip() {
        let mut map = InodeMap::new(root_id());
        let inode = map.allocate_path("work/report.txt");
        assert_eq!(map.inode_for_path("work/report.txt"), Some(inode));
        assert_eq!(map.path_for(inode), Some("work/report.txt"));
    }

    #[test]
    fn remove_path_clears_both_directions() {
        let mut map = InodeMap::new(root_id());
        let inode = map.allocate_path("work");
        map.remove_path("work");
        assert_eq!(map.inode_for_path("work"), None);
        assert_eq!(map.path_for(inode), None);
        assert_eq!(map.len(), 1); // root remains
    }

    #[test]
    fn allocate_id_with_path_binds_both_views() {
        let mut map = InodeMap::new(root_id());
        let id = ItemId::new("gdrive", "file1");
        let inode = map.allocate_id_with_path(id.clone(), "work/file1.txt");
        assert_eq!(map.get_inode(&id), Some(inode));
        assert_eq!(map.inode_for_path("work/file1.txt"), Some(inode));
        assert_eq!(map.path_for(inode), Some("work/file1.txt"));
        assert_eq!(map.get_id(inode), Some(&id));
    }

    #[test]
    fn allocate_id_with_path_reuses_existing_path_inode() {
        let mut map = InodeMap::new(root_id());
        let path_inode = map.allocate_path("work/file1.txt");
        let id = ItemId::new("gdrive", "file1");
        let id_inode = map.allocate_id_with_path(id.clone(), "work/file1.txt");
        assert_eq!(path_inode, id_inode);
        assert_eq!(map.get_inode(&id), Some(path_inode));
    }

    #[test]
    fn remove_by_id_clears_path() {
        let mut map = InodeMap::new(root_id());
        let id = ItemId::new("gdrive", "file1");
        let inode = map.allocate_id_with_path(id.clone(), "work/file1.txt");
        map.remove(&id);
        assert_eq!(map.get_inode(&id), None);
        assert_eq!(map.path_for(inode), None);
        assert_eq!(map.inode_for_path("work/file1.txt"), None);
        assert_eq!(map.len(), 1); // root remains
    }

    #[test]
    fn remove_nonexistent_is_noop() {
        let mut map = InodeMap::new(root_id());
        let phantom = ItemId::new("gdrive", "phantom");
        map.remove(&phantom);
        map.remove_path("nope");
        assert_eq!(map.len(), 1);
    }
}

/// Property-based tests for `InodeMap`.
#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn allocate_n_paths_all_unique(paths in prop::collection::vec("[a-z]{1,10}", 1..50)) {
            let root = ItemId::new("test", "root");
            let mut map = InodeMap::new(root);

            // Deduplicate to ensure we test distinct paths.
            let mut seen = std::collections::HashSet::new();
            let mut inodes = Vec::new();
            for path in &paths {
                if !seen.insert(path.clone()) {
                    continue;
                }
                let inode = map.allocate_path(path);
                inodes.push(inode);
            }

            // All inodes should be unique.
            let mut sorted = inodes.clone();
            sorted.sort_unstable();
            sorted.dedup();
            prop_assert_eq!(sorted.len(), inodes.len());
        }

        #[test]
        fn allocate_lookup_roundtrip(paths in prop::collection::vec("[a-z]{1,10}", 1..50)) {
            let root = ItemId::new("test", "root");
            let mut map = InodeMap::new(root);

            for path in &paths {
                let inode = map.allocate_path(path);
                prop_assert_eq!(map.inode_for_path(path), Some(inode));
                prop_assert_eq!(map.path_for(inode), Some(path.as_str()));
            }
        }

        #[test]
        fn allocate_path_idempotent_prop(path in "[a-z]{1,10}") {
            let root = ItemId::new("test", "root");
            let mut map = InodeMap::new(root);

            let first = map.allocate_path(&path);
            let second = map.allocate_path(&path);
            prop_assert_eq!(first, second);
        }
    }
}
