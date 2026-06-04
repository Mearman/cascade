//! `wasm-bindgen` bindings to the browser-side File System Access module.
//!
//! Each binding maps one-to-one onto an exported function of
//! `apps/web/src/wasm/fsaccess.ts`. The directory handle and the change-detection
//! snapshot are opaque browser objects (`FileSystemDirectoryHandle` and a
//! `Map`), so they cross the boundary as [`JsValue`] rather than a custom
//! `wasm-bindgen` type: the JS interface types are erased at runtime and have no
//! class to import. The async functions return rejected promises as
//! `Err(JsValue)` via `catch`.

use wasm_bindgen::prelude::*;

// The module path is resolved by wasm-bindgen relative to this crate's root, so
// it walks up to the workspace root to reach the browser-side module.
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
