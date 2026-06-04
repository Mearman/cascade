#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::string_slice
    )
)]
//! WASM entry point — exposes `cascade-config` and `cascade-expr` to JavaScript.
//!
//! Compiles to `wasm32-unknown-unknown`. On native targets this crate builds as
//! an empty lib with no exports — all public functions and the `context` module
//! are gated on `#[cfg(target_arch = "wasm32")]`.
//!
//! Verify the WASM build with:
//! ```text
//! cargo check -p cascade-wasm --target wasm32-unknown-unknown
//! ```

#[cfg(target_arch = "wasm32")]
mod context;

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

/// Parse a `.cascade` TOML string and return the config as a JavaScript object.
///
/// The returned value matches the `CascadeConfig` shape: `ignore`, `pin`,
/// `unpin`, `lifecycle`, `cache`, `p2p`, and `device` arrays / objects.
///
/// # Errors
///
/// Returns a JS error string if the TOML is malformed or cannot be converted to
/// a JS value.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn parse_config(toml_str: &str) -> Result<JsValue, JsValue> {
    let config = cascade_config::parse::toml::parse(toml_str)
        .map_err(|e| JsValue::from_str(&e.to_string()))?;
    let json = serde_json::to_string(&config).map_err(|e| JsValue::from_str(&e.to_string()))?;
    js_sys::JSON::parse(&json)
        .map_err(|_| JsValue::from_str("serialised config was not valid JSON"))
}

/// Evaluate a cascade expression string against a JSON context object.
///
/// `context_json` is a JSON object with optional fields that map to the
/// expression evaluation context. Absent fields default to zero / unknown.
/// Expected shape (all fields optional):
///
/// ```json
/// {
///   "file":    { "size": 4096, "mime": "text/plain", "ext": "txt",
///                "name": "notes.txt", "cached": false, "pinned": false,
///                "shared": false, "starred": false, "dirty": false },
///   "device":  { "id": "abc123", "name": "laptop",
///                "arch": "aarch64", "os": "macos" },
///   "disk":    { "total_bytes": 1000000000, "free_bytes": 500000000 },
///   "network": { "type": "wifi", "metered": false },
///   "power":   { "source": "ac", "battery_pct": 95 },
///   "peer":    { "online_count": 3, "peers_with_file": 1 }
/// }
/// ```
///
/// # Errors
///
/// Returns a JS error string if the expression is syntactically invalid or
/// `context_json` cannot be deserialised.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn eval_expression(expr: &str, context_json: &str) -> Result<bool, JsValue> {
    let ast =
        cascade_expr::eval::parse_expr(expr).map_err(|e| JsValue::from_str(&e.to_string()))?;
    let ctx = context::from_json(context_json).map_err(|e| JsValue::from_str(&e.to_string()))?;
    Ok(cascade_expr::eval::evaluate(&ast, &ctx))
}
