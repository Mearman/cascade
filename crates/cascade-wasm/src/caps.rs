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
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::JsValue;

/// Detect the browser capabilities visible from the current global scope.
#[cfg(target_arch = "wasm32")]
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

/// Off-wasm there is no browser global to probe, so every capability is absent.
/// This host projection exists so the router compiles and is testable natively;
/// it keeps the same shape as the wasm path's output.
#[cfg(not(target_arch = "wasm32"))]
pub fn detect() -> Value {
    json!({
        "fileSystemAccess": false,
        "webRtc": false,
        "serviceWorker": false,
        "wasm": false,
        "indexedDb": false,
    })
}

/// Whether `target` has an own- or inherited property named `key`.
#[cfg(target_arch = "wasm32")]
fn has(target: &JsValue, key: &str) -> bool {
    js_sys::Reflect::has(target, &JsValue::from_str(key)).unwrap_or(false)
}

/// Whether the global's `navigator` exposes a property named `key`.
#[cfg(target_arch = "wasm32")]
fn navigator_has(global: &JsValue, key: &str) -> bool {
    match js_sys::Reflect::get(global, &JsValue::from_str("navigator")) {
        Ok(navigator) if !navigator.is_undefined() && !navigator.is_null() => has(&navigator, key),
        _ => false,
    }
}
