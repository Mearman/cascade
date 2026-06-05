#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::string_slice
)]
//! Integration tests for the local filesystem backend.
//!
//! Exercises the full Backend trait contract against a real temp directory,
//! including lifecycle operations, change detection, manifest tracking,
//! hash-based modification detection, and VFS tree integration.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cascade_backend_local::create_backend;
use cascade_engine::backend::Backend;
use cascade_engine::types::{Change, FileId};
use cascade_engine::vfs::tree::VfsTree;

/// Helper: build a TOML config for a local backend rooted at `dir`.
fn make_config(dir: &Path, id: &str, mode: &str) -> toml::Value {
    let mut table = toml::map::Map::new();
    table.insert(
        "root_path".to_string(),
        toml::Value::String(dir.to_str().unwrap().to_string()),
    );
    table.insert("id".to_string(), toml::Value::String(id.to_string()));
    table.insert("mode".to_string(), toml::Value::String(mode.to_string()));
    toml::Value::Table(table)
}

/// Helper: create a backend from a temp dir in mirror mode.
fn make_backend(dir: &Path, id: &str) -> Box<dyn Backend> {
    create_backend(&make_config(dir, id, "mirror")).unwrap()
}

// ---------------------------------------------------------------------------
// Task 1: Full lifecycle integration tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_lifecycle_create_detect_download_delete() {
    let dir = tempfile::tempdir().unwrap();
    let backend = make_backend(dir.path(), "lifecycle");

    // Empty tree — first changes() returns nothing.
    let (changes, cursor) = backend.changes(None).await.unwrap();
    assert!(changes.is_empty(), "empty dir should yield no changes");

    // Create a file on disk.
    tokio::fs::write(dir.path().join("hello.txt"), b"hello world")
        .await
        .unwrap();

    // changes() detects the new file.
    let (changes, cursor) = backend.changes(Some(&cursor)).await.unwrap();
    assert_eq!(changes.len(), 1);
    match &changes[0] {
        Change::Created(entry) => {
            assert_eq!(entry.name, "hello.txt");
            assert!(!entry.is_dir);
        }
        other => panic!("expected Created, got {other:?}"),
    }

    // metadata() returns the file.
    let entry = backend.metadata(Path::new("hello.txt")).await.unwrap();
    assert_eq!(entry.name, "hello.txt");
    assert_eq!(entry.size, Some(11));
    assert!(entry.hash.is_some());

    // download() reads content.
    let buf = backend.download(&entry).await.unwrap();
    assert_eq!(buf, b"hello world");

    // delete() removes it.
    backend.delete(&entry).await.unwrap();
    assert!(!dir.path().join("hello.txt").exists());

    // changes() does NOT report the deletion because delete() already
    // updated the manifest. External deletions (outside the backend) are
    // detected, but internal operations are tracked immediately.
    let (changes, _) = backend.changes(Some(&cursor)).await.unwrap();
    assert!(changes.is_empty(), "delete() already updates the manifest");

    // For external deletion detection, see the dedicated test below.
}

#[tokio::test]
async fn changes_detects_external_deletion() {
    let dir = tempfile::tempdir().unwrap();
    let backend = make_backend(dir.path(), "ext-delete");

    // Create and upload a file through the backend.
    let data = b"to be deleted";
    let parent_id = FileId("/".to_string());
    let _entry = backend
        .upload(Path::new("doomed.txt"), data, &parent_id)
        .await
        .unwrap();

    // Seed the manifest.
    let (_, cursor) = backend.changes(None).await.unwrap();

    // Delete the file directly on disk (external deletion).
    tokio::fs::remove_file(dir.path().join("doomed.txt"))
        .await
        .unwrap();

    let (changes, _) = backend.changes(Some(&cursor)).await.unwrap();
    assert_eq!(changes.len(), 1);
    match &changes[0] {
        Change::Deleted(entry) => assert_eq!(entry.name, "doomed.txt"),
        other => panic!("expected Deleted, got {other:?}"),
    }
}

#[tokio::test]
async fn directory_operations_create_list() {
    let dir = tempfile::tempdir().unwrap();
    let backend = make_backend(dir.path(), "dirs");

    // Create a directory.
    let dir_entry = backend.create_dir(Path::new("photos")).await.unwrap();
    assert_eq!(dir_entry.name, "photos");
    assert!(dir_entry.is_dir);

    // metadata() returns is_dir = true.
    let meta = backend.metadata(Path::new("photos")).await.unwrap();
    assert!(meta.is_dir);
    assert_eq!(meta.name, "photos");

    // Seed the manifest with the current state (empty aside from the directory).
    // The manifest only tracks files, not directories, so this will be empty.
    let (_, cursor) = backend.changes(None).await.unwrap();

    // Now write a file into the directory (external to the backend).
    tokio::fs::write(dir.path().join("photos/sunset.jpg"), b"jpeg-data")
        .await
        .unwrap();

    let (changes, _) = backend.changes(Some(&cursor)).await.unwrap();
    let names: Vec<&str> = changes
        .iter()
        .filter_map(|c| match c {
            Change::Created(e) => Some(e.name.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        names.iter().any(|n| n.contains("sunset.jpg")),
        "should detect the child file, got {names:?}"
    );
}

#[tokio::test]
async fn move_rename_file() {
    let dir = tempfile::tempdir().unwrap();
    let backend = make_backend(dir.path(), "move");

    // Upload a file.
    let data = b"move me";
    let parent_id = FileId("/".to_string());
    let entry = backend
        .upload(Path::new("alpha.txt"), data, &parent_id)
        .await
        .unwrap();
    assert_eq!(entry.name, "alpha.txt");

    // Move it.
    let moved = backend
        .move_entry(Path::new("alpha.txt"), Path::new("beta.txt"))
        .await
        .unwrap();
    assert_eq!(moved.name, "beta.txt");

    // metadata(B) succeeds.
    let meta_b = backend.metadata(Path::new("beta.txt")).await.unwrap();
    assert_eq!(meta_b.name, "beta.txt");

    // metadata(A) fails.
    let result = backend.metadata(Path::new("alpha.txt")).await;
    assert!(result.is_err(), "original path should be gone");
}

#[tokio::test]
async fn manifest_tracking_idempotent_changes() {
    let dir = tempfile::tempdir().unwrap();
    let backend = make_backend(dir.path(), "manifest");

    // Write a file.
    tokio::fs::write(dir.path().join("stable.txt"), b"content")
        .await
        .unwrap();

    // First changes() — returns Created.
    let (changes, cursor) = backend.changes(None).await.unwrap();
    assert_eq!(changes.len(), 1);
    match &changes[0] {
        Change::Created(e) => assert_eq!(e.name, "stable.txt"),
        other => panic!("expected Created, got {other:?}"),
    }

    // Second changes() — nothing (manifest matches disk).
    let (changes, cursor) = backend.changes(Some(&cursor)).await.unwrap();
    assert!(
        changes.is_empty(),
        "no changes expected when manifest matches disk, got {changes:?}"
    );

    // Third call — still nothing.
    let (changes, _) = backend.changes(Some(&cursor)).await.unwrap();
    assert!(changes.is_empty());
}

#[tokio::test]
async fn hash_based_change_detection() {
    let dir = tempfile::tempdir().unwrap();
    let backend = make_backend(dir.path(), "hash-detect");

    // Upload a file via the backend so it's in the manifest.
    let data = b"original content";
    let parent_id = FileId("/".to_string());
    let _entry = backend
        .upload(Path::new("doc.txt"), data, &parent_id)
        .await
        .unwrap();

    // Seed the manifest.
    let (_, cursor) = backend.changes(None).await.unwrap();

    // Modify file on disk directly (bypass backend to simulate external change).
    // New content has a different size (32 bytes vs 17 bytes) and different hash,
    // which the manifest diff detects via size mismatch triggering a hash comparison.
    tokio::fs::write(
        dir.path().join("doc.txt"),
        b"modified content here! extra bytes",
    )
    .await
    .unwrap();

    let (changes, _) = backend.changes(Some(&cursor)).await.unwrap();
    assert_eq!(changes.len(), 1, "should detect the modification");
    match &changes[0] {
        Change::Updated { old, new } => {
            assert_eq!(old.name, "doc.txt");
            assert_eq!(new.name, "doc.txt");
            assert_ne!(
                old.hash, new.hash,
                "hashes should differ after modification"
            );
        }
        other => panic!("expected Updated, got {other:?}"),
    }
}

#[tokio::test]
async fn vfs_tree_integration() {
    let dir = tempfile::tempdir().unwrap();
    let backend: Arc<dyn Backend> = Arc::from(make_backend(dir.path(), "vfs-local"));

    // Populate the directory with some files.
    tokio::fs::write(dir.path().join("readme.md"), b"# Hello")
        .await
        .unwrap();
    tokio::fs::create_dir(dir.path().join("src")).await.unwrap();
    tokio::fs::write(dir.path().join("src/main.rs"), b"fn main() {}")
        .await
        .unwrap();

    // Seed the manifest so changes() returns items.
    let backend_ref = Arc::clone(&backend);
    let (_, _) = backend_ref.changes(None).await.unwrap();

    // Mount in VfsTree.
    let mut tree = VfsTree::new(backend);
    let child_backend = Arc::new(cascade_engine::backend::NullBackend::new("null-child"));
    tree.mount(PathBuf::from("nested"), child_backend);

    // Resolve the root path — should go to the local backend.
    let (resolved, rest) = tree.resolve(Path::new("readme.md"));
    assert_eq!(resolved.id(), "vfs-local");
    assert_eq!(rest, Path::new("readme.md"));

    // Resolve a child mount path — should go to null-child.
    let (resolved, rest) = tree.resolve(Path::new("nested/deep/file.txt"));
    assert_eq!(resolved.id(), "null-child");
    assert_eq!(rest, Path::new("deep/file.txt"));

    // Resolve the src directory — should go to the local backend.
    let (resolved, rest) = tree.resolve(Path::new("src/main.rs"));
    assert_eq!(resolved.id(), "vfs-local");
    assert_eq!(rest, Path::new("src/main.rs"));
}

#[tokio::test]
async fn upload_only_mode_blocks_writes_allows_reads() {
    let dir = tempfile::tempdir().unwrap();

    // Pre-populate a file so we can read it.
    tokio::fs::write(dir.path().join("existing.txt"), b"data")
        .await
        .unwrap();

    let config = make_config(dir.path(), "upload-only", "upload-only");
    let backend = create_backend(&config).unwrap();

    // Reads succeed.
    let meta = backend.metadata(Path::new("existing.txt")).await.unwrap();
    assert_eq!(meta.name, "existing.txt");

    let buf = backend.download(&meta).await.unwrap();
    assert_eq!(buf, b"data");

    // Upload fails.
    let data = b"new";
    let parent_id = FileId("/".to_string());
    let result = backend.upload(Path::new("new.txt"), data, &parent_id).await;
    assert!(result.is_err());

    // Create dir fails.
    let result = backend.create_dir(Path::new("subdir")).await;
    assert!(result.is_err());

    // Delete fails.
    let result = backend.delete(&meta).await;
    assert!(result.is_err());

    // Move fails.
    let result = backend
        .move_entry(Path::new("existing.txt"), Path::new("moved.txt"))
        .await;
    assert!(result.is_err());

    // Changes and quota still work.
    let (changes, _) = backend.changes(None).await.unwrap();
    assert!(!changes.is_empty(), "should detect existing file");

    let quota = backend.quota().await.unwrap();
    assert!(quota.is_some());
}

// ---------------------------------------------------------------------------
// Property tests (proptest)
// ---------------------------------------------------------------------------

mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn upload_download_roundtrip(content in any::<Vec<u8>>()) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let dir = tempfile::tempdir().unwrap();
                let backend = make_backend(dir.path(), "prop-roundtrip");

                let parent_id = FileId("/".to_string());
                            let entry = backend
                    .upload(Path::new("prop-file.bin"), &content, &parent_id)
                    .await
                    .unwrap();

                let buf = backend.download(&entry).await.unwrap();

                prop_assert_eq!(buf, content);
                Ok(())
            })?;
        }

        #[test]
        fn metadata_name_matches_upload(filename in "[a-zA-Z0-9_\\-]{1,20}\\.txt") {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let dir = tempfile::tempdir().unwrap();
                let backend = make_backend(dir.path(), "prop-name");

                let data = b"content";
                let parent_id = FileId("/".to_string());
                            let _entry = backend
                    .upload(Path::new(&filename), data, &parent_id)
                    .await
                    .unwrap();

                let meta = backend.metadata(Path::new(&filename)).await.unwrap();
                prop_assert_eq!(meta.name, filename);
                Ok(())
            })?;
        }

        #[test]
        fn walk_discovers_all_files(
            files in prop::collection::hash_map(
                "[a-z]{1,5}",
                any::<u8>().prop_map(|v| (v as usize).clamp(1, 100)),
                1..10
            )
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let dir = tempfile::tempdir().unwrap();
                let backend = make_backend(dir.path(), "prop-walk");

                for (name, size) in &files {
                    let content: Vec<u8> = (0..u8::try_from(*size).unwrap()).cycle().take(*size).collect();
                    tokio::fs::write(dir.path().join(format!("{name}.txt")), &content)
                        .await
                        .unwrap();
                }

                let (changes, _) = backend.changes(None).await.unwrap();
                let discovered: Vec<&str> = changes
                    .iter()
                    .filter_map(|c| match c {
                        Change::Created(e) => Some(e.name.as_str()),
                        _ => None,
                    })
                    .collect();

                let expected = files.len();
                prop_assert_eq!(discovered.len(), expected,
                    "should discover all {} files, got {:?}", expected, discovered);

                Ok(())
            })?;
        }
    }
}
