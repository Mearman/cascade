//! Integration tests: state DB file lifecycle — register backend, insert,
//! query, update cache state, delete, sync cursor round-trip.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use cascade_engine::db::StateDb;
use cascade_engine::types::{CacheState, FileEntry, ItemId};
use std::str::FromStr;

fn make_entry(name: &str, id: &str, is_dir: bool) -> FileEntry {
    FileEntry {
        id: ItemId::new("test", id),
        parent_id: ItemId::new("test", "root"),
        name: name.to_string(),
        is_dir,
        size: if is_dir { None } else { Some(1024) },
        mod_time: None,
        mime_type: if is_dir {
            None
        } else {
            Some("text/plain".to_string())
        },
        hash: None,
    }
}

#[test]
fn full_file_lifecycle() {
    let db = StateDb::open_in_memory().unwrap();
    db.register_backend("test", "test", "Test Backend", None, None)
        .unwrap();

    // Insert a file.
    let file = make_entry("report.txt", "file1", false);
    db.upsert_file(&file).unwrap();

    // Query it back.
    let retrieved = db.get_file(&file.id).unwrap().expect("file should exist");
    assert_eq!(retrieved.name, "report.txt");
    assert!(!retrieved.is_dir);
    assert_eq!(retrieved.size, Some(1024));

    // Update cache state: online → cached.
    db.update_cache_state(&file.id, CacheState::Cached).unwrap();
    let state = db
        .get_cache_state(&file.id)
        .unwrap()
        .expect("should have state");
    assert_eq!(state, CacheState::Cached);

    // Update again: cached → pinned.
    db.update_cache_state(&file.id, CacheState::Pinned).unwrap();
    let state = db.get_cache_state(&file.id).unwrap().unwrap();
    assert_eq!(state, CacheState::Pinned);

    // Delete.
    db.delete_file(&file.id).unwrap();
    assert!(db.get_file(&file.id).unwrap().is_none());
}

#[test]
fn directory_entries() {
    let db = StateDb::open_in_memory().unwrap();
    db.register_backend("test", "test", "Test", None, None)
        .unwrap();

    let dir = make_entry("Documents", "dir1", true);
    db.upsert_file(&dir).unwrap();

    let retrieved = db.get_file(&dir.id).unwrap().unwrap();
    assert!(retrieved.is_dir);
    assert!(retrieved.size.is_none());
}

#[test]
fn sync_cursor_round_trip() {
    let db = StateDb::open_in_memory().unwrap();
    db.register_backend("test", "test", "Test", None, None)
        .unwrap();

    // No cursor initially.
    assert!(db.get_cursor("test").unwrap().is_none());

    // Set a cursor.
    let cursor = cascade_engine::types::Cursor("page-token-123".to_string());
    db.set_cursor("test", &cursor).unwrap();

    // Read it back.
    let retrieved = db.get_cursor("test").unwrap().unwrap();
    assert_eq!(retrieved.0, "page-token-123");

    // Update the cursor.
    let new_cursor = cascade_engine::types::Cursor("page-token-456".to_string());
    db.set_cursor("test", &new_cursor).unwrap();

    let retrieved = db.get_cursor("test").unwrap().unwrap();
    assert_eq!(retrieved.0, "page-token-456");
}

#[test]
fn multiple_backends_with_independent_cursors() {
    let db = StateDb::open_in_memory().unwrap();
    db.register_backend("gdrive", "gdrive", "Google Drive", None, None)
        .unwrap();
    db.register_backend("dropbox", "dropbox", "Dropbox", None, None)
        .unwrap();

    let cursor1 = cascade_engine::types::Cursor("gdrive-token".to_string());
    let cursor2 = cascade_engine::types::Cursor("dropbox-token".to_string());
    db.set_cursor("gdrive", &cursor1).unwrap();
    db.set_cursor("dropbox", &cursor2).unwrap();

    assert_eq!(db.get_cursor("gdrive").unwrap().unwrap().0, "gdrive-token");
    assert_eq!(
        db.get_cursor("dropbox").unwrap().unwrap().0,
        "dropbox-token"
    );
}

#[test]
fn upsert_replaces_existing() {
    let db = StateDb::open_in_memory().unwrap();
    db.register_backend("test", "test", "Test", None, None)
        .unwrap();

    let v1 = make_entry("file.txt", "f1", false);
    db.upsert_file(&v1).unwrap();

    let v2 = FileEntry {
        size: Some(2048),
        ..v1.clone()
    };
    db.upsert_file(&v2).unwrap();

    let retrieved = db.get_file(&v1.id).unwrap().unwrap();
    assert_eq!(retrieved.size, Some(2048));
}

#[test]
fn cache_state_from_str() {
    assert_eq!(CacheState::from_str("online").unwrap(), CacheState::Online);
    assert_eq!(CacheState::from_str("cached").unwrap(), CacheState::Cached);
    assert_eq!(CacheState::from_str("pinned").unwrap(), CacheState::Pinned);
    assert!(CacheState::from_str("invalid").is_err());
}

#[test]
fn cache_state_display() {
    assert_eq!(CacheState::Online.to_string(), "online");
    assert_eq!(CacheState::Cached.to_string(), "cached");
    assert_eq!(CacheState::Pinned.to_string(), "pinned");
}
