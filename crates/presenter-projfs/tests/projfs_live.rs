#![cfg(all(feature = "projfs-live", target_os = "windows"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::doc_markdown
)]
//! Live `ProjFS` integration test — drives a real `PrjStartVirtualizing`
//! mount and asserts the kernel callback round-trip.
//!
//! Gated behind the `projfs-live` feature flag *and* `target_os =
//! "windows"`, so regular `cargo test` runs skip this file entirely. Only
//! the dedicated `projfs-live` CI job (and Windows developers who opt in
//! with `--features projfs-live`) execute it. The job re-checks the
//! Client-ProjFS optional feature and the PrjFlt minifilter before running,
//! so a missing prerequisite fails loudly rather than producing an opaque
//! HRESULT here.
//!
//! # What it proves
//!
//! As an external integration test, this file can only reach the crate's
//! public API: [`ProjFsPresenter`], [`ContentProvider`], and the engine's
//! [`ItemId`]/[`VfsItem`]. That constraint is the point — it forces a
//! genuine OS-driven exercise of the mount rather than poking at internal
//! helpers.
//!
//! - **Enumeration**: `std::fs::read_dir` over the mount must list exactly
//!   the seeded child names, driving `QueryFileName`,
//!   `Start`/`Get`/`End` directory enumeration, and `GetPlaceholderInfo`.
//! - **Content read**: `std::fs::read` of a seeded file must return the
//!   exact bytes *and* increment the provider's read counter, driving
//!   `GetPlaceholderInfo` then `GetFileData` →
//!   `ContentProvider::read_range` → `PrjWriteFileData`. If `GetFileData`
//!   never ran (browse-only) the read would error; if a stale placeholder
//!   were served without round-tripping into Rust the counter assertion
//!   would fail.
//! - **Notification + delete**: removing a file must make it disappear from
//!   a subsequent enumeration, which only happens if the notification path
//!   and the OS-level delete completed against the live mount.
//!
//! Cancel is intentionally not asserted live — a cold read is not reliably
//! cancellable within the checkpoint window, so cancellation has dedicated
//! cross-platform unit-test coverage in the crate's `lib.rs`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use cascade_engine::presenter::VfsPresenter;
use cascade_engine::types::{CacheState, ItemId, VfsItem};
use cascade_presenter_projfs::{ContentProvider, ProjFsPresenter};

/// Test-local content provider mirroring the crate's `MockContentProvider`,
/// plus a shared atomic counter incremented on every `read_range` call so
/// the test can prove the kernel actually routed a read back into Rust.
#[derive(Debug)]
struct CountingProvider {
    files: Mutex<HashMap<ItemId, Vec<u8>>>,
    reads: Arc<AtomicUsize>,
}

impl CountingProvider {
    fn new(reads: Arc<AtomicUsize>) -> Self {
        Self {
            files: Mutex::new(HashMap::new()),
            reads,
        }
    }

    fn insert(&self, id: ItemId, bytes: Vec<u8>) {
        self.files.lock().unwrap().insert(id, bytes);
    }
}

impl ContentProvider for CountingProvider {
    fn read_range(&self, id: &ItemId, offset: u64, length: u32) -> std::io::Result<Vec<u8>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let files = self.files.lock().unwrap();
        let Some(bytes) = files.get(id) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no such id",
            ));
        };
        let start = usize::try_from(offset)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
        if start >= bytes.len() {
            return Ok(Vec::new());
        }
        let want = usize::try_from(length)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
        let end = start.saturating_add(want).min(bytes.len());
        Ok(bytes[start..end].to_vec())
    }
}

/// RAII guard that stops the virtualisation and drops the `TempDir` in the
/// correct order. `stop()` drains in-flight callbacks via
/// `PrjStopVirtualizing` *before* the `TempDir` is removed — removing the
/// directory while callbacks could still dereference the raw context would
/// be a use-after-free. Mirrors the Drop discipline in the p2p
/// `nat_integration` harness.
struct LiveMount {
    presenter: Arc<ProjFsPresenter>,
    handle: tokio::runtime::Handle,
    // Held so it outlives the mount; dropped last.
    _dir: tempfile::TempDir,
}

impl Drop for LiveMount {
    fn drop(&mut self) {
        let presenter = Arc::clone(&self.presenter);
        // Block on stop() so callbacks drain before _dir is removed.
        let _ = self.handle.block_on(async move { presenter.stop().await });
    }
}

/// Seed a root directory plus two child files, install the provider, and
/// start virtualising. Returns the mount path, the guard, and the shared
/// read counter.
async fn setup() -> (std::path::PathBuf, LiveMount, Arc<AtomicUsize>) {
    let dir = tempfile::tempdir().unwrap();
    let mount_path = dir.path().to_path_buf();

    let reads = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(CountingProvider::new(Arc::clone(&reads)));

    let root_id = ItemId::new("test", "root");

    let alpha_id = ItemId::new("test", "alpha");
    let beta_id = ItemId::new("test", "beta");
    let alpha_payload = b"alpha file contents".to_vec();
    let beta_payload = b"beta file contents, slightly longer".to_vec();
    provider.insert(alpha_id.clone(), alpha_payload.clone());
    provider.insert(beta_id.clone(), beta_payload.clone());

    // Build the presenter with the provider BEFORE start — the content
    // provider is cloned into the callback context at start, so installing
    // it afterwards would have no effect.
    let presenter = Arc::new(
        ProjFsPresenter::new(&mount_path)
            .with_root(root_id.clone())
            .with_content_provider(provider),
    );

    // Seed the projection: a root directory item plus two child files
    // parented to it.
    let root_item = VfsItem {
        id: root_id.clone(),
        parent_id: ItemId::new("test", "void"),
        name: "root".to_string(),
        is_dir: true,
        size: None,
        mod_time: None,
        cache_state: CacheState::Online,
        mime_type: None,
    };
    let alpha_item = VfsItem {
        id: alpha_id,
        parent_id: root_id.clone(),
        name: "alpha.txt".to_string(),
        is_dir: false,
        size: Some(u64::try_from(alpha_payload.len()).unwrap()),
        mod_time: None,
        cache_state: CacheState::Online,
        mime_type: Some("text/plain".to_string()),
    };
    let beta_item = VfsItem {
        id: beta_id,
        parent_id: root_id,
        name: "beta.txt".to_string(),
        is_dir: false,
        size: Some(u64::try_from(beta_payload.len()).unwrap()),
        mod_time: None,
        cache_state: CacheState::Online,
        mime_type: Some("text/plain".to_string()),
    };
    presenter.upsert_item(root_item).await.unwrap();
    presenter.upsert_item(alpha_item).await.unwrap();
    presenter.upsert_item(beta_item).await.unwrap();

    presenter
        .start(&mount_path)
        .await
        .expect("PrjStartVirtualizing should succeed on a ProjFS-enabled runner");

    let guard = LiveMount {
        presenter,
        handle: tokio::runtime::Handle::current(),
        _dir: dir,
    };
    (mount_path, guard, reads)
}

/// Enumeration over the live mount lists exactly the seeded child names.
/// Uses `spawn_blocking` so the synchronous `read_dir` (and the kernel
/// callbacks it triggers, which call `blocking_read`) run off the test's
/// async reactor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enumeration_lists_seeded_children() {
    let (mount, _guard, _reads) = setup().await;

    let names = tokio::task::spawn_blocking(move || {
        let mut names: Vec<String> = std::fs::read_dir(&mount)
            .expect("read_dir on the live mount should succeed")
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    })
    .await
    .unwrap();

    assert_eq!(
        names,
        vec!["alpha.txt".to_string(), "beta.txt".to_string()],
        "enumeration must list exactly the seeded children"
    );
}

/// Reading a seeded file returns its exact bytes and routes through the
/// provider (read counter advances). This is the strongest honesty signal:
/// a browse-only mount would error on read, and a placeholder served
/// without a Rust round-trip would leave the counter at zero.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn content_read_round_trips_through_provider() {
    let (mount, _guard, reads) = setup().await;
    let expected = b"alpha file contents".to_vec();

    let got = tokio::task::spawn_blocking(move || std::fs::read(mount.join("alpha.txt")))
        .await
        .unwrap()
        .expect("reading a seeded file should succeed via GetFileData");

    assert!(!got.is_empty(), "read must return a non-empty payload");
    assert_eq!(got, expected, "read bytes must match the seeded payload");
    assert!(
        reads.load(Ordering::SeqCst) > 0,
        "GetFileData must have called the provider's read_range at least once"
    );
}

/// Deleting a file makes it disappear from a subsequent enumeration,
/// exercising the notification path and the OS-level delete against the
/// live mount.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_removes_file_from_enumeration() {
    let (mount, _guard, _reads) = setup().await;

    let mount_for_delete = mount.clone();
    tokio::task::spawn_blocking(move || std::fs::remove_file(mount_for_delete.join("beta.txt")))
        .await
        .unwrap()
        .expect("removing a seeded file should succeed");

    let names = tokio::task::spawn_blocking(move || {
        let mut names: Vec<String> = std::fs::read_dir(&mount)
            .expect("read_dir after delete should succeed")
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    })
    .await
    .unwrap();

    assert!(
        !names.contains(&"beta.txt".to_string()),
        "deleted file must not appear in enumeration"
    );
}
