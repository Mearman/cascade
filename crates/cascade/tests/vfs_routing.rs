//! Integration tests: VFS tree mount/unmount routing with nested backends.

use cascade_engine::backend::NullBackend;
use cascade_engine::vfs::VfsTree;
use std::path::Path;
use std::sync::Arc;

#[test]
fn root_routes_all_paths_to_single_backend() {
    let root = Arc::new(NullBackend::new("root"));
    let tree = VfsTree::new(root);

    let (backend, rest) = tree.resolve(Path::new("any/path/file.txt"));
    assert_eq!(backend.id(), "root");
    assert_eq!(rest, std::path::PathBuf::from("any/path/file.txt"));
}

#[test]
fn child_mount_overrides_parent() {
    let root = Arc::new(NullBackend::new("root"));
    let mut tree = VfsTree::new(root);
    tree.mount(
        std::path::PathBuf::from("Work"),
        Arc::new(NullBackend::new("work")),
    );

    // Paths under Work/ go to work backend.
    let (backend, rest) = tree.resolve(Path::new("Work/Projects/code.rs"));
    assert_eq!(backend.id(), "work");
    assert_eq!(rest, std::path::PathBuf::from("Projects/code.rs"));

    // Other paths still go to root.
    let (backend, rest) = tree.resolve(Path::new("Personal/photos/img.jpg"));
    assert_eq!(backend.id(), "root");
    assert_eq!(rest, std::path::PathBuf::from("Personal/photos/img.jpg"));
}

#[test]
fn nested_mount_longest_prefix_wins() {
    let root = Arc::new(NullBackend::new("root"));
    let mut tree = VfsTree::new(root);
    tree.mount(
        std::path::PathBuf::from("Work"),
        Arc::new(NullBackend::new("work")),
    );
    tree.mount(
        std::path::PathBuf::from("Work/Assets"),
        Arc::new(NullBackend::new("assets")),
    );

    // Work/Assets/file.png → assets (longer prefix wins).
    let (backend, rest) = tree.resolve(Path::new("Work/Assets/logo.png"));
    assert_eq!(backend.id(), "assets");
    assert_eq!(rest, std::path::PathBuf::from("logo.png"));

    // Work/Projects/ → work.
    let (backend, rest) = tree.resolve(Path::new("Work/Projects/app.rs"));
    assert_eq!(backend.id(), "work");
    assert_eq!(rest, std::path::PathBuf::from("Projects/app.rs"));
}

#[test]
fn unmount_removes_routing() {
    let root = Arc::new(NullBackend::new("root"));
    let mut tree = VfsTree::new(root);
    tree.mount(
        std::path::PathBuf::from("Work"),
        Arc::new(NullBackend::new("work")),
    );

    // Verify it's routed.
    let (backend, _) = tree.resolve(Path::new("Work/file.txt"));
    assert_eq!(backend.id(), "work");

    // Unmount.
    let removed = tree.unmount(Path::new("Work"));
    assert!(removed.is_some());
    assert_eq!(removed.unwrap().id(), "work");

    // Now falls through to root.
    let (backend, rest) = tree.resolve(Path::new("Work/file.txt"));
    assert_eq!(backend.id(), "root");
    assert_eq!(rest, std::path::PathBuf::from("Work/file.txt"));
}

#[test]
fn unmount_nonexistent_is_none() {
    let root = Arc::new(NullBackend::new("root"));
    let mut tree = VfsTree::new(root);
    assert!(tree.unmount(Path::new("nope")).is_none());
}

#[test]
fn multiple_sibling_mounts() {
    let root = Arc::new(NullBackend::new("root"));
    let mut tree = VfsTree::new(root);
    tree.mount(
        std::path::PathBuf::from("Work"),
        Arc::new(NullBackend::new("work")),
    );
    tree.mount(
        std::path::PathBuf::from("Personal"),
        Arc::new(NullBackend::new("personal")),
    );

    let (b1, _) = tree.resolve(Path::new("Work/doc.txt"));
    assert_eq!(b1.id(), "work");

    let (b2, _) = tree.resolve(Path::new("Personal/photo.jpg"));
    assert_eq!(b2.id(), "personal");

    let (b3, _) = tree.resolve(Path::new("Shared/readme.md"));
    assert_eq!(b3.id(), "root");
}
