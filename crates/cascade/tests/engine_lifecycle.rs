#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::string_slice
)]
//! Integration tests for the unified Engine lifecycle.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cascade_engine::backend::{MountedBackend, NullBackend};
use cascade_engine::engine::{Engine, EngineConfig, NativeEngine};

fn make_engine_with_backends(
    backends: Vec<Arc<dyn cascade_engine::backend::Backend>>,
) -> NativeEngine {
    let dir = tempfile::tempdir().unwrap();
    Engine::new(EngineConfig {
        db_path: dir.path().join("state.db"),
        mount_point: PathBuf::from("/tmp/test-mount"),
        backends: backends
            .into_iter()
            .map(MountedBackend::at_default)
            .collect(),
        cache_dir: None,
        enable_p2p: false,
        p2p_data_dir: None,
        p2p_posture: None,
        p2p_relay_endpoints: Vec::new(),
        p2p_relay_shared_secret: None,
        backend_factory: None,
    })
    .unwrap()
}

#[tokio::test]
async fn full_engine_lifecycle() {
    let engine = make_engine_with_backends(vec![Arc::new(NullBackend::new("test"))]);

    // Engine should report as running before shutdown.
    let status = engine.status().await;
    assert!(status.running);
    assert_eq!(status.backends.len(), 1);

    // Start background tasks.
    let handle = engine.start().unwrap();

    // Pin a path.
    engine.pin("Documents/**", true).await.unwrap();
    let pins = engine.list_pins().await.unwrap();
    assert_eq!(pins.len(), 1);

    // Shut down.
    engine.shutdown();
    drop(handle);

    let status = engine.status().await;
    assert!(!status.running);
}

#[tokio::test]
async fn engine_with_two_backends() {
    let engine = make_engine_with_backends(vec![
        Arc::new(NullBackend::new("root")),
        Arc::new(NullBackend::new("work")),
    ]);

    let status = engine.status().await;
    assert_eq!(status.backends.len(), 2);

    // Both backends mount as children of the neutral root.
    let tree = engine.vfs().read().unwrap();
    assert_eq!(tree.children().len(), 2);
    drop(tree);
}

#[tokio::test]
async fn engine_start_stop_idempotent() {
    let engine = make_engine_with_backends(vec![Arc::new(NullBackend::new("test"))]);

    // Start and shutdown twice — should not panic or deadlock.
    let handle = engine.start().unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    engine.shutdown();
    drop(handle);
}

#[tokio::test]
async fn engine_pin_unpin_affects_status() {
    let engine = make_engine_with_backends(vec![Arc::new(NullBackend::new("test"))]);

    engine.pin("Photos/**", true).await.unwrap();
    engine.pin("Documents/report.pdf", false).await.unwrap();

    let pins = engine.list_pins().await.unwrap();
    assert_eq!(pins.len(), 2);

    // Unpin one.
    let removed = engine.unpin("Photos/**").await.unwrap();
    assert!(removed);

    let pins = engine.list_pins().await.unwrap();
    assert_eq!(pins.len(), 1);
    assert_eq!(pins[0].path_glob, "Documents/report.pdf");
}

#[tokio::test]
async fn engine_mount_unmount_during_runtime() {
    let engine = make_engine_with_backends(vec![Arc::new(NullBackend::new("root"))]);

    // The configured backend already mounts as a child of the neutral root.
    let baseline = engine.vfs().read().unwrap().children().len();
    assert_eq!(baseline, 1);

    // Mount a second backend.
    engine.mount_backend(
        PathBuf::from("Projects"),
        Arc::new(NullBackend::new("projects")),
    );

    let tree = engine.vfs().read().unwrap();
    assert_eq!(tree.children().len(), baseline + 1);
    drop(tree);

    // Unmount it.
    engine.unmount_backend(Path::new("Projects"));

    let tree = engine.vfs().read().unwrap();
    assert_eq!(tree.children().len(), baseline);
}
