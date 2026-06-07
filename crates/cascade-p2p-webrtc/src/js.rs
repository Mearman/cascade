//! `wasm-bindgen` bindings to the browser-side WebRTC module.
//!
//! Each binding maps onto an export of `apps/web/src/wasm/webrtc.ts`. The
//! configuration crosses the boundary as a plain JS object ([`JsValue`]) built
//! by [`crate::transport`]; the returned [`FrameTransport`] is the opaque
//! data-channel wrapper the JS module constructs.
//!
//! Under the `js-test-stub` feature the extern block imports a plain `.js` stub
//! module (located at `tests/js/webrtc_stub.js`) so that `wasm-pack test --node`
//! can load the glue without a TypeScript transpiler or browser globals.
//! The stub mirrors the production exports exactly; only the backing
//! implementation differs.

use wasm_bindgen::prelude::*;

// Production build: import the real TypeScript module.
// The module path is resolved by wasm-bindgen relative to this crate's root, so
// it walks up to the workspace root to reach the browser-side module.
#[cfg(not(feature = "js-test-stub"))]
#[wasm_bindgen(module = "/../../apps/web/src/wasm/webrtc.ts")]
extern "C" {
    /// The browser-side frame transport over a WebRTC data channel.
    pub(crate) type FrameTransport;

    /// Report whether `RTCPeerConnection` and `RTCDataChannel` are available.
    #[wasm_bindgen(js_name = "isWebRtcSupported")]
    pub(crate) fn is_web_rtc_supported() -> bool;

    /// Create a peer connection from a `WebRtcConfig`-shaped JS object.
    #[wasm_bindgen(js_name = "createPeerConnection")]
    pub(crate) fn create_peer_connection(config: &JsValue) -> FrameTransport;

    /// Send one frame over the data channel. A no-op until the channel opens.
    #[wasm_bindgen(method)]
    pub(crate) fn send(this: &FrameTransport, data: &[u8]);

    /// Register the handler invoked with each inbound frame.
    #[wasm_bindgen(method, js_name = "onFrame")]
    pub(crate) fn on_frame(this: &FrameTransport, handler: &js_sys::Function);

    /// Register the handler invoked once when the transport closes.
    #[wasm_bindgen(method, js_name = "onClose")]
    pub(crate) fn on_close(this: &FrameTransport, handler: &js_sys::Function);

    /// Close the data channel and the underlying peer connection.
    #[wasm_bindgen(method)]
    pub(crate) fn close(this: &FrameTransport);

    /// Whether the data channel is currently open.
    #[wasm_bindgen(method, getter)]
    pub(crate) fn connected(this: &FrameTransport) -> bool;
}

// Test-stub build: import the plain .js stub module so `wasm-pack test --node`
// can load the glue without a TypeScript transpiler or browser globals.
// The stub is never included in a production wasm build.
#[cfg(feature = "js-test-stub")]
#[wasm_bindgen(module = "/tests/js/webrtc_stub.js")]
extern "C" {
    /// Stub frame transport returned by [`create_peer_connection`].
    pub(crate) type FrameTransport;

    /// Stub: returns the controllable value set by [`set_supported`].
    #[wasm_bindgen(js_name = "isWebRtcSupported")]
    pub(crate) fn is_web_rtc_supported() -> bool;

    /// Stub: creates a `StubFrameTransport` and records the config JSON.
    #[wasm_bindgen(js_name = "createPeerConnection")]
    pub(crate) fn create_peer_connection(config: &JsValue) -> FrameTransport;

    /// Stub: records the sent bytes; see [`StubFrameTransport::get_last_send_bytes`].
    #[wasm_bindgen(method)]
    pub(crate) fn send(this: &FrameTransport, data: &[u8]);

    /// Stub: stores the handler for later invocation via [`StubFrameTransport::trigger_frame`].
    #[wasm_bindgen(method, js_name = "onFrame")]
    pub(crate) fn on_frame(this: &FrameTransport, handler: &js_sys::Function);

    /// Stub: stores the handler for later invocation via [`StubFrameTransport::trigger_close`].
    #[wasm_bindgen(method, js_name = "onClose")]
    pub(crate) fn on_close(this: &FrameTransport, handler: &js_sys::Function);

    /// Stub: sets the `_closed` flag and fires the close handler.
    #[wasm_bindgen(method)]
    pub(crate) fn close(this: &FrameTransport);

    /// Stub: returns the controllable `_connected` flag.
    #[wasm_bindgen(method, getter)]
    pub(crate) fn connected(this: &FrameTransport) -> bool;

    // ── Stub inspector/driver API ─────────────────────────────────────────────

    /// Set the value returned by [`is_web_rtc_supported`].
    #[wasm_bindgen(js_name = "setSupported")]
    pub(crate) fn set_supported(value: bool);

    /// Return the config JSON passed to the last [`create_peer_connection`].
    #[wasm_bindgen(js_name = "getLastConfigJson")]
    pub(crate) fn get_last_config_json() -> Option<String>;

    /// Return the most recently created transport so tests can drive it.
    #[wasm_bindgen(js_name = "getLastTransport")]
    pub(crate) fn get_last_transport() -> FrameTransport;

    /// Set the `connected` flag on the stub transport.
    #[wasm_bindgen(method, js_name = "setConnected")]
    pub(crate) fn set_connected(this: &FrameTransport, value: bool);

    /// Return the bytes from the last `send` call as an `ArrayBuffer`.
    #[wasm_bindgen(method, js_name = "getLastSendBytes")]
    pub(crate) fn get_last_send_bytes(this: &FrameTransport) -> Option<JsValue>;

    /// Invoke the registered frame handler with the given bytes.
    #[wasm_bindgen(method, js_name = "triggerFrame")]
    pub(crate) fn trigger_frame(this: &FrameTransport, data: &[u8]);

    /// Invoke the registered close handler.
    #[wasm_bindgen(method, js_name = "triggerClose")]
    pub(crate) fn trigger_close(this: &FrameTransport);

    /// Whether `close()` has been called on this transport.
    #[wasm_bindgen(method, js_name = "wasClosed")]
    pub(crate) fn was_closed(this: &FrameTransport) -> bool;

    /// Reset module-level state between tests.
    #[wasm_bindgen(js_name = "resetState")]
    pub(crate) fn reset_state();
}
