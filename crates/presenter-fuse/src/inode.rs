//! Inode management — bidirectional map between VFS `ItemId` and FUSE inode numbers.
//!
//! Root inode is always 1. Subsequent inodes are allocated sequentially.

use std::collections::HashMap;

use cascade_engine::types::ItemId;

/// The root inode number, always mapped to the root VFS directory.
pub const ROOT_INODE: u64 = 1;

/// Bidirectional map between VFS `ItemId` and FUSE inode numbers.
pub struct InodeMap {
    /// `ItemId` → inode number.
    id_to_inode: HashMap<ItemId, u64>,
    /// Inode number → `ItemId`.
    inode_to_id: HashMap<u64, ItemId>,
    /// Next available inode number.
    next_inode: u64,
}

impl InodeMap {
    /// Create a new inode map with the root entry pre-allocated at inode 1.
    #[must_use] pub fn new(root_id: ItemId) -> Self {
        let mut map = Self {
            id_to_inode: HashMap::new(),
            inode_to_id: HashMap::new(),
            next_inode: ROOT_INODE + 1,
        };
        map.id_to_inode.insert(root_id.clone(), ROOT_INODE);
        map.inode_to_id.insert(ROOT_INODE, root_id);
        map
    }

    /// Allocate an inode for the given `ItemId`. If one already exists, returns it.
    pub fn allocate(&mut self, id: ItemId) -> u64 {
        if let Some(&inode) = self.id_to_inode.get(&id) {
            return inode;
        }
        let inode = self.next_inode;
        self.next_inode += 1;
        self.id_to_inode.insert(id.clone(), inode);
        self.inode_to_id.insert(inode, id);
        inode
    }

    /// Look up the inode number for an `ItemId`.
    #[must_use] pub fn get_inode(&self, id: &ItemId) -> Option<u64> {
        self.id_to_inode.get(id).copied()
    }

    /// Look up the `ItemId` for an inode number.
    #[must_use] pub fn get_id(&self, inode: u64) -> Option<&ItemId> {
        self.inode_to_id.get(&inode)
    }

    /// Remove an `ItemId` and its associated inode from the map.
    pub fn remove(&mut self, id: &ItemId) {
        if let Some(inode) = self.id_to_inode.remove(id) {
            self.inode_to_id.remove(&inode);
        }
    }

    /// Number of mapped entries.
    #[must_use] pub fn len(&self) -> usize {
        self.id_to_inode.len()
    }

    /// Whether the map is empty (it never is — root is always present).
    #[must_use] pub fn is_empty(&self) -> bool {
        self.id_to_inode.is_empty()
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
        assert_eq!(map.get_inode(&root_id()), Some(ROOT_INODE));
        assert_eq!(map.get_id(ROOT_INODE), Some(&root_id()));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn allocate_sequential_inodes() {
        let mut map = InodeMap::new(root_id());
        let child_a = ItemId::new("gdrive", "file_a");
        let child_b = ItemId::new("gdrive", "file_b");

        let inode_a = map.allocate(child_a.clone());
        let inode_b = map.allocate(child_b.clone());

        assert_eq!(inode_a, 2);
        assert_eq!(inode_b, 3);
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn allocate_idempotent() {
        let mut map = InodeMap::new(root_id());
        let child = ItemId::new("gdrive", "file_a");

        let first = map.allocate(child.clone());
        let second = map.allocate(child.clone());

        assert_eq!(first, second);
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn remove_item() {
        let mut map = InodeMap::new(root_id());
        let child = ItemId::new("gdrive", "file_a");
        let inode = map.allocate(child.clone());

        map.remove(&child);

        assert_eq!(map.get_inode(&child), None);
        assert_eq!(map.get_id(inode), None);
        assert_eq!(map.len(), 1); // root remains
    }

    #[test]
    fn remove_nonexistent_is_noop() {
        let mut map = InodeMap::new(root_id());
        let phantom = ItemId::new("gdrive", "phantom");
        map.remove(&phantom);
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn bidirectional_lookup() {
        let mut map = InodeMap::new(root_id());
        let child = ItemId::new("gdrive", "docs");
        let inode = map.allocate(child.clone());

        assert_eq!(map.get_inode(&child), Some(inode));
        assert_eq!(map.get_id(inode), Some(&child));
    }
}

/// Property-based tests for InodeMap.
#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn allocate_n_inodes_all_unique(ids in prop::collection::vec("[a-z]{1,10}", 1..50)) {
            let root = ItemId::new("test", "root");
            let mut map = InodeMap::new(root.clone());

            // Deduplicate to ensure we test distinct IDs.
            let mut seen = std::collections::HashSet::new();
            let mut inodes = Vec::new();
            for id_str in &ids {
                if !seen.insert(id_str.clone()) {
                    continue;
                }
                let id = ItemId::new("test", id_str);
                let inode = map.allocate(id);
                inodes.push(inode);
            }

            // All inodes should be unique.
            let mut sorted = inodes.clone();
            sorted.sort();
            sorted.dedup();
            prop_assert_eq!(sorted.len(), inodes.len());
        }

        #[test]
        fn allocate_lookup_roundtrip(ids in prop::collection::vec("[a-z]{1,10}", 1..50)) {
            let root = ItemId::new("test", "root");
            let mut map = InodeMap::new(root);

            for id_str in &ids {
                let id = ItemId::new("test", id_str);
                let inode = map.allocate(id.clone());
                prop_assert_eq!(map.get_inode(&id), Some(inode));
                prop_assert_eq!(map.get_id(inode), Some(&id));
            }
        }

        #[test]
        fn allocate_idempotent_prop(id_str in "[a-z]{1,10}") {
            let root = ItemId::new("test", "root");
            let mut map = InodeMap::new(root);
            let id = ItemId::new("test", &id_str);

            let first = map.allocate(id.clone());
            let second = map.allocate(id.clone());
            prop_assert_eq!(first, second);
        }
    }
}
