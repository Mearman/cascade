//! `wasm-bindgen` bindings to the browser-side File System Access module.
//!
//! Each binding maps one-to-one onto an exported function of
//! `apps/web/src/wasm/fsaccess.ts`. The directory handle and the change-detection
//! snapshot are opaque browser objects (`FileSystemDirectoryHandle` and a
//! `Map`), so they cross the boundary as [`JsValue`] rather than a custom
//! `wasm-bindgen` type: the JS interface types are erased at runtime and have no
//! class to import. The async functions return rejected promises as
//! `Err(JsValue)` via `catch`.
//!
//! Under the `js-test-stub` feature the extern block imports a plain `.js` stub
//! module (located at `tests/js/fsaccess_stub.js`) so that `wasm-pack test
//! --node` can load the glue without a TypeScript transpiler. The stub mirrors
//! the production module's exports exactly; only the backing implementation
//! differs.

use wasm_bindgen::prelude::*;

// Production build: import the real TypeScript module.
// The module path is resolved by wasm-bindgen relative to this crate's root, so
// it walks up to the workspace root to reach the browser-side module.
#[cfg(not(feature = "js-test-stub"))]
#[wasm_bindgen(module = "/../../apps/web/src/wasm/fsaccess.ts")]
extern "C" {
    /// Prompt the user to grant read-write access to a directory, returning the
    /// granted `FileSystemDirectoryHandle`.
    #[wasm_bindgen(js_name = "requestDirectory", catch)]
    pub(crate) async fn request_directory() -> Result<JsValue, JsValue>;

    /// List the immediate children of a directory handle as a JS array of
    /// `FileSystemDirectoryHandle` / `FileSystemFileHandle` objects.
    #[wasm_bindgen(js_name = "enumerateDirectory", catch)]
    pub(crate) async fn enumerate_directory(handle: &JsValue) -> Result<JsValue, JsValue>;

    /// Read the full contents of a file handle as an `ArrayBuffer`.
    #[wasm_bindgen(js_name = "readFile", catch)]
    pub(crate) async fn read_file(handle: &JsValue) -> Result<JsValue, JsValue>;

    /// Write `data` to a named file within a directory, creating it if needed.
    #[wasm_bindgen(js_name = "writeFile", catch)]
    pub(crate) async fn write_file(dir: &JsValue, name: &str, data: &[u8]) -> Result<(), JsValue>;

    /// Compare a directory against a previous snapshot, returning a
    /// `DirectoryChanges` object (created / modified / deleted name arrays and a
    /// fresh snapshot `Map`).
    #[wasm_bindgen(js_name = "detectChanges", catch)]
    pub(crate) async fn detect_changes(
        handle: &JsValue,
        snapshot: &JsValue,
    ) -> Result<JsValue, JsValue>;
}

// Test-stub build: import the plain .js stub module so `wasm-pack test --node`
// can load the glue without a TypeScript transpiler or browser globals.
// The stub is never included in a production wasm build.
#[cfg(feature = "js-test-stub")]
#[wasm_bindgen(module = "/tests/js/fsaccess_stub.js")]
extern "C" {
    /// Stub: returns a fake directory handle object.
    #[wasm_bindgen(js_name = "requestDirectory", catch)]
    pub(crate) async fn request_directory() -> Result<JsValue, JsValue>;

    /// Stub: returns an Array of `{kind, name}` handle objects from state set
    /// by [`set_next_entries`].
    #[wasm_bindgen(js_name = "enumerateDirectory", catch)]
    pub(crate) async fn enumerate_directory(handle: &JsValue) -> Result<JsValue, JsValue>;

    /// Stub: returns an `ArrayBuffer` for the given handle's configured bytes.
    #[wasm_bindgen(js_name = "readFile", catch)]
    pub(crate) async fn read_file(handle: &JsValue) -> Result<JsValue, JsValue>;

    /// Stub: records the write in module state; see [`get_last_write_name`].
    #[wasm_bindgen(js_name = "writeFile", catch)]
    pub(crate) async fn write_file(dir: &JsValue, name: &str, data: &[u8]) -> Result<(), JsValue>;

    /// Stub: returns a `DirectoryChanges`-shaped object from state set by
    /// [`set_next_changes`] / [`set_next_reject`].
    #[wasm_bindgen(js_name = "detectChanges", catch)]
    pub(crate) async fn detect_changes(
        handle: &JsValue,
        snapshot: &JsValue,
    ) -> Result<JsValue, JsValue>;

    // ── Stub inspector API ────────────────────────────────────────────────────

    /// Set the entries that the next [`enumerate_directory`] call returns.
    /// `entries_json` is a JSON array of `{kind, name, bytes?}` objects.
    #[wasm_bindgen(js_name = "setNextEntries")]
    pub(crate) fn set_next_entries(entries_json: &str);

    /// Set the `DirectoryChanges`-shaped object returned by [`detect_changes`].
    #[wasm_bindgen(js_name = "setNextChanges")]
    pub(crate) fn set_next_changes(changes_json: &str);

    /// Make the next [`detect_changes`] call reject with the given message.
    #[wasm_bindgen(js_name = "setNextReject")]
    pub(crate) fn set_next_reject(message: &str);

    /// Return the name from the last [`write_file`] call, or `null`.
    #[wasm_bindgen(js_name = "getLastWriteName")]
    pub(crate) fn get_last_write_name() -> Option<String>;

    /// Return the bytes from the last [`write_file`] call as an `ArrayBuffer`,
    /// or `null`.
    #[wasm_bindgen(js_name = "getLastWriteBytes")]
    pub(crate) fn get_last_write_bytes() -> Option<JsValue>;

    /// Reset all stub state between tests.
    #[wasm_bindgen(js_name = "resetState")]
    pub(crate) fn reset_state();
}
