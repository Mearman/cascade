//! Integration tests for the VFS tree routing.

use cascade_engine::backend::NullBackend;
use cascade_engine::vfs::VfsTree;
use std::sync::Arc;

#[test]
fn test_vfs_root_routing() {
    let root = Arc::new(NullBackend::new("root"));
    let tree = VfsTree::new(root);

    let (backend, rest) = tree.resolve(std::path::Path::new("Documents/report.txt"));
    assert_eq!(backend.id(), "root");
    assert_eq!(rest, std::path::PathBuf::from("Documents/report.txt"));
}

#[test]
fn test_vfs_child_routing() {
    let root = Arc::new(NullBackend::new("root"));
    let work = Arc::new(NullBackend::new("work"));
    let mut tree = VfsTree::new(root);
    tree.mount(std::path::PathBuf::from("Work"), work);

    let (backend, rest) = tree.resolve(std::path::Path::new("Work/report.txt"));
    assert_eq!(backend.id(), "work");
    assert_eq!(rest, std::path::PathBuf::from("report.txt"));
}

#[test]
fn test_vfs_nested_routing() {
    let root = Arc::new(NullBackend::new("root"));
    let work = Arc::new(NullBackend::new("work"));
    let assets = Arc::new(NullBackend::new("assets"));
    let mut tree = VfsTree::new(root);
    tree.mount(std::path::PathBuf::from("Work"), work);
    tree.mount(std::path::PathBuf::from("Work/Assets"), assets);

    let (backend, rest) = tree.resolve(std::path::Path::new("Work/Assets/logo.png"));
    assert_eq!(backend.id(), "assets");
    assert_eq!(rest, std::path::PathBuf::from("logo.png"));

    let (backend, rest) = tree.resolve(std::path::Path::new("Work/report.txt"));
    assert_eq!(backend.id(), "work");
    assert_eq!(rest, std::path::PathBuf::from("report.txt"));
}

#[test]
fn test_vfs_unmount() {
    let root = Arc::new(NullBackend::new("root"));
    let work = Arc::new(NullBackend::new("work"));
    let mut tree = VfsTree::new(root);
    tree.mount(std::path::PathBuf::from("Work"), work);
    assert_eq!(tree.children().len(), 1);

    tree.unmount(std::path::Path::new("Work"));
    assert!(tree.children().is_empty());
}
