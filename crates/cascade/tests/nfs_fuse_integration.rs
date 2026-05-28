//! Integration tests for NFS and FUSE presenters with the engine's VFS tree.
//!
//! Exercises the full VfsPresenter trait cycle (upsert → fetch → delete)
//! with NullBackend and verifies the presenters integrate correctly with
//! the engine's VfsTree.

use std::sync::{Arc, RwLock};

use cascade_engine::backend::NullBackend;
use cascade_engine::presenter::VfsPresenter;
use cascade_engine::types::{CacheState, ItemId, VfsItem};
use cascade_engine::vfs::VfsTree;
use cascade_presenter_fuse::FusePresenter;
use cascade_presenter_nfs::NfsPresenter;

fn make_vfs() -> Arc<RwLock<VfsTree>> {
    Arc::new(RwLock::new(VfsTree::new(Arc::new(NullBackend::new(
        "test-backend",
    )))))
}

fn make_item(name: &str, is_dir: bool) -> VfsItem {
    VfsItem {
        id: ItemId::new("test-backend", name),
        parent_id: ItemId::new("test-backend", "root"),
        name: name.to_string(),
        is_dir,
        size: if is_dir { None } else { Some(256) },
        mod_time: None,
        cache_state: CacheState::Online,
        mime_type: None,
    }
}

// --- NFS presenter integration ---

#[tokio::test]
async fn nfs_upsert_delete_cycle() {
    let vfs = make_vfs();
    let presenter = NfsPresenter::with_vfs("/mnt/test", vfs);

    let item = make_item("docs", true);
    let id = item.id.clone();

    // Upsert should register the item.
    presenter.upsert_item(item).await.unwrap();

    // Delete should remove it.
    presenter.delete_item(&id).await.unwrap();
}

#[tokio::test]
async fn nfs_update_state_is_ok() {
    let vfs = make_vfs();
    let presenter = NfsPresenter::with_vfs("/mnt/test", vfs);
    let id = ItemId::new("test-backend", "file1");

    presenter
        .update_state(&id, CacheState::Cached)
        .await
        .unwrap();
}

#[tokio::test]
async fn nfs_evict_nonexistent_is_ok() {
    let vfs = make_vfs();
    let presenter = NfsPresenter::with_vfs("/mnt/test", vfs);
    let id = ItemId::new("test-backend", "phantom");

    presenter.evict_item(&id).await.unwrap();
}

#[tokio::test]
async fn nfs_multiple_upserts() {
    let vfs = make_vfs();
    let presenter = NfsPresenter::with_vfs("/mnt/test", vfs);

    for name in &["file1", "file2", "dir1", "file3"] {
        let item = make_item(name, *name == "dir1");
        presenter.upsert_item(item).await.unwrap();
    }

    // Delete them all.
    for name in &["file1", "file2", "dir1", "file3"] {
        let id = ItemId::new("test-backend", name);
        presenter.delete_item(&id).await.unwrap();
    }
}

// --- FUSE presenter integration ---

#[tokio::test]
async fn fuse_upsert_delete_cycle() {
    let vfs = make_vfs();
    let root = ItemId::new("test-backend", "root");
    let presenter = FusePresenter::with_vfs(root, vfs);

    let item = make_item("notes.txt", false);
    let id = item.id.clone();

    // Upsert should allocate an inode.
    presenter.upsert_item(item).await.unwrap();
    let map = presenter.inode_map().lock().unwrap();
    assert!(map.get_inode(&id).is_some());
    drop(map);

    // Delete should remove it.
    presenter.delete_item(&id).await.unwrap();
    let map = presenter.inode_map().lock().unwrap();
    assert!(map.get_inode(&id).is_none());
}

#[tokio::test]
async fn fuse_update_state_is_ok() {
    let vfs = make_vfs();
    let root = ItemId::new("test-backend", "root");
    let presenter = FusePresenter::with_vfs(root, vfs);

    presenter
        .update_state(&ItemId::new("test-backend", "file1"), CacheState::Pinned)
        .await
        .unwrap();
}

#[tokio::test]
async fn fuse_evict_nonexistent_is_ok() {
    let vfs = make_vfs();
    let root = ItemId::new("test-backend", "root");
    let presenter = FusePresenter::with_vfs(root, vfs);

    presenter
        .evict_item(&ItemId::new("test-backend", "phantom"))
        .await
        .unwrap();
}

#[tokio::test]
async fn fuse_multiple_upserts_allocate_unique_inodes() {
    let vfs = make_vfs();
    let root = ItemId::new("test-backend", "root");
    let presenter = FusePresenter::with_vfs(root, vfs);

    let names = vec!["a", "b", "c", "d", "e"];
    let mut inodes = Vec::new();
    for name in &names {
        let item = make_item(name, false);
        let id = item.id.clone();
        presenter.upsert_item(item).await.unwrap();
        let map = presenter.inode_map().lock().unwrap();
        inodes.push(map.get_inode(&id).unwrap());
    }

    // All inodes should be unique.
    let mut sorted = inodes.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), inodes.len());

    // Root is at inode 1, so first child is at inode 2.
    assert_eq!(inodes[0], 2);
}

// --- Cross-presenter: both presenters share the same VFS tree ---

#[tokio::test]
async fn both_presenters_share_vfs() {
    let vfs = make_vfs();
    let root = ItemId::new("test-backend", "root");

    let nfs = NfsPresenter::with_vfs("/mnt/nfs", Arc::clone(&vfs));
    let fuse = FusePresenter::with_vfs(root, vfs);

    // Both presenters should work with the same VFS.
    let item = make_item("shared.txt", false);
    let id = item.id.clone();

    nfs.upsert_item(item.clone()).await.unwrap();
    fuse.upsert_item(item).await.unwrap();

    nfs.delete_item(&id).await.unwrap();
    fuse.delete_item(&id).await.unwrap();
}
