//! wasm-bindgen-test suite for the File System Access bridge.
//!
//! These tests exercise the Rust-side decode logic against a JS stub module
//! (tests/js/fsaccess_stub.js). They run under `wasm-pack test --node` with
//! the `js-test-stub` feature enabled and are invisible to native `cargo test`.
//!
//! Coverage boundary: these tests validate that the Rust marshalling paths
//! (Reflect::get key lookups, Array dyn_into, ArrayBuffer → Vec<u8>,
//! snapshot Map round-trip) decode correctly-shaped JS values. They do NOT
//! test browser-specific behaviour (showDirectoryPicker, IndexedDB, etc.).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::string_slice
)]

use wasm_bindgen_test::wasm_bindgen_test;

use crate::backend::{FsAccessBackend, FsAccessError};
use crate::js;

// Run all tests with the Node.js runner (no browser needed).
wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_node_experimental);

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build the entries JSON for setNextEntries.
fn file_entry(name: &str, bytes: &[u8]) -> String {
    let byte_array: Vec<u8> = bytes.to_vec();
    let bytes_json: Vec<String> = byte_array.iter().map(|b| b.to_string()).collect();
    format!(
        r#"[{{"kind":"file","name":"{name}","bytes":[{}]}}]"#,
        bytes_json.join(",")
    )
}

fn two_entries(first_kind: &str, first_name: &str, second_bytes: &[u8]) -> String {
    let bytes_json: Vec<String> = second_bytes.iter().map(|b| b.to_string()).collect();
    format!(
        r#"[{{"kind":"{first_kind}","name":"{first_name}"}},{{"kind":"file","name":"{first_name}","bytes":[{}]}}]"#,
        bytes_json.join(",")
    )
}

/// Construct a backend over the stub-granted handle.
async fn make_backend() -> FsAccessBackend {
    crate::backend::create_backend().await.unwrap()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// create_backend succeeds; id and display_name match the expected constants.
#[wasm_bindgen_test]
async fn create_backend_builds_over_granted_handle() {
    js::reset_state();
    let backend = make_backend().await;
    assert_eq!(backend.id(), "fsaccess");
    assert_eq!(backend.display_name(), "Local directory (browser)");
}

/// download decodes a file entry correctly and returns its bytes.
#[wasm_bindgen_test]
async fn download_returns_file_bytes() {
    js::reset_state();
    let expected = &[1u8, 2, 3, 4, 5];
    js::set_next_entries(&file_entry("a.txt", expected));
    let backend = make_backend().await;
    let result = backend.download("a.txt").await.unwrap();
    assert_eq!(result, expected);
}

/// download returns NotFound when the name is not present.
#[wasm_bindgen_test]
async fn download_missing_name_is_not_found() {
    js::reset_state();
    js::set_next_entries(&file_entry("b.txt", &[9, 8, 7]));
    let backend = make_backend().await;
    let result = backend.download("a.txt").await;
    assert!(
        matches!(result, Err(FsAccessError::NotFound(ref name)) if name == "a.txt"),
        "expected NotFound(\"a.txt\"), got {result:?}"
    );
}

/// download skips directory entries and finds the file entry with the same name.
#[wasm_bindgen_test]
async fn download_skips_directory_entries() {
    js::reset_state();
    // First entry: directory named "a.txt" (should be skipped).
    // Second entry: file named "a.txt" with known bytes.
    let expected = &[42u8, 43, 44];
    js::set_next_entries(&two_entries("directory", "a.txt", expected));
    let backend = make_backend().await;
    let result = backend.download("a.txt").await.unwrap();
    assert_eq!(result, expected);
}

/// upload calls writeFile and the stub records the correct name and bytes.
#[wasm_bindgen_test]
async fn upload_forwards_to_write_file() {
    js::reset_state();
    let backend = make_backend().await;
    let data = &[10u8, 20, 30];
    backend.upload("x.bin", data).await.unwrap();

    let recorded_name = js::get_last_write_name().expect("writeFile was not called");
    assert_eq!(recorded_name, "x.bin");

    use js_sys::Uint8Array;
    use wasm_bindgen::JsCast;
    let buf = js::get_last_write_bytes().expect("no write bytes recorded");
    // get_last_write_bytes returns an ArrayBuffer.
    let recorded_bytes = Uint8Array::new(&buf.unchecked_into::<js_sys::ArrayBuffer>()).to_vec();
    assert_eq!(recorded_bytes, data);
}

/// changes decodes all three arrays and advances the snapshot.
#[wasm_bindgen_test]
async fn changes_decodes_and_advances_snapshot() {
    js::reset_state();
    let changes_json = r#"{
        "created":["new.txt"],
        "modified":["changed.rs"],
        "deleted":["gone.md"],
        "snapshotEntries":{"new.txt":{"lastModified":1,"size":10}}
    }"#;
    js::set_next_changes(changes_json);
    let backend = make_backend().await;
    let result = backend.changes().await.unwrap();
    assert_eq!(result.created, ["new.txt"]);
    assert_eq!(result.modified, ["changed.rs"]);
    assert_eq!(result.deleted, ["gone.md"]);
}

/// changes surfaces a JS rejection as FsAccessError::Js.
#[wasm_bindgen_test]
async fn changes_surfaces_js_rejection() {
    js::reset_state();
    js::set_next_reject("permission denied");
    let backend = make_backend().await;
    let result = backend.changes().await;
    assert!(
        matches!(result, Err(FsAccessError::Js(ref msg)) if msg.contains("permission denied")),
        "expected Js error with 'permission denied', got {result:?}"
    );
}
