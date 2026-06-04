//! `wasm-bindgen` bindings to the browser-side WebRTC module.
//!
//! Each binding maps onto an export of `apps/web/src/wasm/webrtc.ts`. The
//! configuration crosses the boundary as a plain JS object ([`JsValue`]) built
//! by [`crate::transport`]; the returned [`FrameTransport`] is the opaque
//! data-channel wrapper the JS module constructs.

use wasm_bindgen::prelude::*;

// The module path is resolved by wasm-bindgen relative to this crate's root, so
// it walks up to the workspace root to reach the browser-side module.
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
