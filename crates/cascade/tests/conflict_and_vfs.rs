//! Integration tests: conflict detection, multi-backend VFS, write operations.

use cascade_engine::backend::NullBackend;
use cascade_engine::sync::conflict::{ConflictCheck, check_conflict, conflict_name};
use cascade_engine::types::{FileEntry, ItemId};
use cascade_engine::vfs::VfsTree;
use std::path::Path;
use std::sync::Arc;

fn make_entry(name: &str, id: &str, hash: Option<&str>) -> FileEntry {
    FileEntry::file(
        ItemId::new("test", id),
        ItemId::new("test", "root"),
        name.to_string(),
    )
    .with_size(Some(100))
    .with_hash(hash.map(String::from))
}

// ── Conflict detection ──

#[test]
fn no_conflict_when_local_not_dirty() {
    let local = make_entry("file.txt", "1", Some("abc"));
    let remote = make_entry("file.txt", "1", Some("def"));
    assert_eq!(
        check_conflict(&local, &remote, false),
        ConflictCheck::NoConflict
    );
}

#[test]
fn no_conflict_when_identical_content() {
    let local = make_entry("file.txt", "1", Some("abc"));
    let remote = make_entry("file.txt", "1", Some("abc"));
    assert_eq!(
        check_conflict(&local, &remote, true),
        ConflictCheck::NoConflict
    );
}

#[test]
fn conflict_when_both_changed_and_dirty() {
    let local = make_entry("file.txt", "1", Some("abc"));
    let remote = make_entry("file.txt", "1", Some("def"));
    let result = check_conflict(&local, &remote, true);
    assert!(matches!(result, ConflictCheck::Conflict { .. }));
}

#[test]
fn conflict_name_preserves_extension() {
    let name = conflict_name("report.pdf", "desktop");
    assert!(name.contains("desktop"));
    assert!(name.ends_with(".conflict.pdf"));
}

#[test]
fn conflict_name_handles_no_extension() {
    let name = conflict_name("Makefile", "laptop");
    assert!(name.contains("laptop"));
    assert!(name.ends_with(".conflict"));
}

// ── Multi-backend VFS ──

#[test]
fn multiple_backends_route_correctly() {
    let personal = Arc::new(NullBackend::new("personal"));
    let work = Arc::new(NullBackend::new("work"));
    let mut tree = VfsTree::new(personal);
    tree.mount(std::path::PathBuf::from("Work"), work);

    // Root goes to personal.
    let (backend, rest) = tree.resolve(Path::new("Documents/report.pdf"));
    assert_eq!(backend.id(), "personal");
    assert_eq!(rest, std::path::PathBuf::from("Documents/report.pdf"));

    // Work/ goes to work backend.
    let (backend, rest) = tree.resolve(Path::new("Work/Projects/app.rs"));
    assert_eq!(backend.id(), "work");
    assert_eq!(rest, std::path::PathBuf::from("Projects/app.rs"));
}

#[test]
fn unmount_restores_parent_routing() {
    let personal = Arc::new(NullBackend::new("personal"));
    let work = Arc::new(NullBackend::new("work"));
    let mut tree = VfsTree::new(personal.clone());
    tree.mount(std::path::PathBuf::from("Work"), work);

    // Verify work routing.
    let (backend, _) = tree.resolve(Path::new("Work/file.txt"));
    assert_eq!(backend.id(), "work");

    // Unmount.
    let removed = tree.unmount(Path::new("Work"));
    assert!(removed.is_some());

    // Falls through to personal.
    let (backend, rest) = tree.resolve(Path::new("Work/file.txt"));
    assert_eq!(backend.id(), "personal");
    assert_eq!(rest, std::path::PathBuf::from("Work/file.txt"));
}

#[test]
fn deeply_nested_mounts() {
    let root = Arc::new(NullBackend::new("root"));
    let work = Arc::new(NullBackend::new("work"));
    let assets = Arc::new(NullBackend::new("assets"));
    let mut tree = VfsTree::new(root);
    tree.mount(std::path::PathBuf::from("Work"), work);
    tree.mount(std::path::PathBuf::from("Work/Assets"), assets);

    // Root.
    let (b, _) = tree.resolve(Path::new("Personal/photo.jpg"));
    assert_eq!(b.id(), "root");

    // Work level.
    let (b, rest) = tree.resolve(Path::new("Work/Projects/app.rs"));
    assert_eq!(b.id(), "work");
    assert_eq!(rest, std::path::PathBuf::from("Projects/app.rs"));

    // Assets level (deepest wins).
    let (b, rest) = tree.resolve(Path::new("Work/Assets/logo.png"));
    assert_eq!(b.id(), "assets");
    assert_eq!(rest, std::path::PathBuf::from("logo.png"));
}
