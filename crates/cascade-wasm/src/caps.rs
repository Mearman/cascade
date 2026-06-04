//! Browser capability detection from inside the WASM module.
//!
//! Probes the worker's global scope (and its `navigator`) for the feature
//! objects the PWA cares about — File System Access, WebRTC, service workers,
//! `WebAssembly`, and `IndexedDB`. Detection uses [`js_sys::Reflect`] rather
//! than `web_sys` so the crate keeps a single wasm-only dependency surface.
//!
//! The shape mirrors the `Capabilities` interface in
//! `apps/web/src/wasm/capabilities.ts` so the worker can hand the result back to
//! the main thread unchanged.

use serde_json::{Value, json};
use wasm_bindgen::JsValue;

/// Detect the browser capabilities visible from the current global scope.
pub fn detect() -> Value {
    let global: JsValue = js_sys::global().into();
    json!({
        "fileSystemAccess": has(&global, "showDirectoryPicker"),
        "webRtc": has(&global, "RTCPeerConnection"),
        "serviceWorker": navigator_has(&global, "serviceWorker"),
        "wasm": has(&global, "WebAssembly"),
        "indexedDb": has(&global, "indexedDB"),
    })
}

/// Whether `target` has an own- or inherited property named `key`.
fn has(target: &JsValue, key: &str) -> bool {
    js_sys::Reflect::has(target, &JsValue::from_str(key)).unwrap_or(false)
}

/// Whether the global's `navigator` exposes a property named `key`.
fn navigator_has(global: &JsValue, key: &str) -> bool {
    match js_sys::Reflect::get(global, &JsValue::from_str("navigator")) {
        Ok(navigator) if !navigator.is_undefined() && !navigator.is_null() => has(&navigator, key),
        _ => false,
    }
}
