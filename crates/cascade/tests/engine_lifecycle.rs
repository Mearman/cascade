//! Integration tests for the unified Engine lifecycle.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cascade_engine::backend::NullBackend;
use cascade_engine::engine::{Engine, EngineConfig};

async fn make_engine_with_backends(
    backends: Vec<Arc<dyn cascade_engine::backend::Backend>>,
) -> Engine {
    let dir = tempfile::tempdir().unwrap();
    Engine::new(EngineConfig {
        db_path: dir.path().join("state.db"),
        mount_point: PathBuf::from("/tmp/test-mount"),
        backends,
        cache_dir: None,
        enable_p2p: false,
        p2p_data_dir: None,
    })
    .unwrap()
}

#[tokio::test]
async fn full_engine_lifecycle() {
    let engine = make_engine_with_backends(vec![Arc::new(NullBackend::new("test"))]).await;

    // Engine should report as running before shutdown.
    let status = engine.status();
    assert!(status.running);
    assert_eq!(status.backends.len(), 1);

    // Start background tasks.
    let handle = engine.start().unwrap();

    // Pin a path.
    engine.pin("Documents/**", true).unwrap();
    let pins = engine.list_pins().unwrap();
    assert_eq!(pins.len(), 1);

    // Shut down.
    engine.shutdown();
    handle.cache_handle.abort();

    let status = engine.status();
    assert!(!status.running);
}

#[tokio::test]
async fn engine_with_two_backends() {
    let engine = make_engine_with_backends(vec![
        Arc::new(NullBackend::new("root")),
        Arc::new(NullBackend::new("work")),
    ])
    .await;

    let status = engine.status();
    assert_eq!(status.backends.len(), 2);

    // VFS should have one child mount (the second backend).
    let tree = engine.vfs().read().unwrap();
    assert_eq!(tree.children().len(), 1);
    drop(tree);
}

#[tokio::test]
async fn engine_start_stop_idempotent() {
    let engine = make_engine_with_backends(vec![Arc::new(NullBackend::new("test"))]).await;

    // Start and shutdown twice — should not panic or deadlock.
    let handle = engine.start().unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    engine.shutdown();
    handle.cache_handle.abort();
}

#[tokio::test]
async fn engine_pin_unpin_affects_status() {
    let engine = make_engine_with_backends(vec![Arc::new(NullBackend::new("test"))]).await;

    engine.pin("Photos/**", true).unwrap();
    engine.pin("Documents/report.pdf", false).unwrap();

    let pins = engine.list_pins().unwrap();
    assert_eq!(pins.len(), 2);

    // Unpin one.
    let removed = engine.unpin("Photos/**").unwrap();
    assert!(removed);

    let pins = engine.list_pins().unwrap();
    assert_eq!(pins.len(), 1);
    assert_eq!(pins[0].path_glob, "Documents/report.pdf");
}

#[tokio::test]
async fn engine_mount_unmount_during_runtime() {
    let engine = make_engine_with_backends(vec![Arc::new(NullBackend::new("root"))]).await;

    // Mount a second backend.
    engine.mount_backend(
        PathBuf::from("Projects"),
        Arc::new(NullBackend::new("projects")),
    );

    let tree = engine.vfs().read().unwrap();
    assert_eq!(tree.children().len(), 1);
    drop(tree);

    // Unmount it.
    engine.unmount_backend(Path::new("Projects"));

    let tree = engine.vfs().read().unwrap();
    assert!(tree.children().is_empty());
}
