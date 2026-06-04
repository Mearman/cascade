//! The WebRTC transport surface.
//!
//! [`WebRtcTransport`] wraps the browser-side frame transport and keeps the
//! Rust callback closures alive for the lifetime of the connection. The crate
//! runs in the browser's single-threaded WASM context, so the registered
//! handlers are held behind a [`RefCell`] rather than a thread-safe lock.

use std::cell::RefCell;

use js_sys::{Array, Object, Reflect, Uint8Array};
use serde::{Deserialize, Serialize};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};

use crate::js::{self, FrameTransport};

/// A failure crossing the WebRTC bridge.
#[derive(Debug, thiserror::Error)]
pub enum WebRtcError {
    /// A JavaScript-side call rejected or threw. The message carries the JS
    /// error text where one was available.
    #[error("WebRTC bridge call failed: {0}")]
    Js(String),
}

/// Configuration for establishing a peer connection.
///
/// The role is implied by `session_id`: `None` makes this peer the initiator
/// (it generates a session id and sends the SDP offer); `Some` makes it the
/// responder for an existing session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebRtcConfig {
    /// WebSocket URL of the relay used for signalling.
    pub relay_url: String,
    /// STUN server URLs for ICE candidate gathering.
    pub stun_servers: Vec<String>,
    /// The rendezvous session id, or `None` to act as the initiator.
    pub session_id: Option<String>,
}

/// A frame transport over a browser WebRTC data channel.
///
/// Holds the registered callback closures so they remain valid for as long as
/// the transport is alive; dropping the transport drops the closures.
pub struct WebRtcTransport {
    inner: FrameTransport,
    frame_handler: RefCell<Option<Closure<dyn FnMut(JsValue)>>>,
    close_handler: RefCell<Option<Closure<dyn FnMut()>>>,
}

impl std::fmt::Debug for WebRtcTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebRtcTransport").finish_non_exhaustive()
    }
}

/// Whether the browser exposes the WebRTC APIs this transport needs.
#[must_use]
pub fn supported() -> bool {
    js::is_web_rtc_supported()
}

/// Build a transport from the given configuration.
///
/// The returned transport is usable immediately; the underlying connection is
/// established asynchronously. Register handlers with
/// [`WebRtcTransport::on_frame`] / [`WebRtcTransport::on_close`] before sending.
pub fn create_transport(config: &WebRtcConfig) -> Result<WebRtcTransport, WebRtcError> {
    let js_config = to_js_config(config)?;
    Ok(WebRtcTransport {
        inner: js::create_peer_connection(&js_config),
        frame_handler: RefCell::new(None),
        close_handler: RefCell::new(None),
    })
}

impl WebRtcTransport {
    /// Send one frame over the data channel. Silently drops the frame if the
    /// channel is not yet open, matching the browser data-channel semantics.
    pub fn send(&self, data: &[u8]) {
        self.inner.send(data);
    }

    /// Whether the data channel is currently open.
    #[must_use]
    pub fn connected(&self) -> bool {
        self.inner.connected()
    }

    /// Register the handler invoked with each inbound frame's bytes. Replaces
    /// any previously registered frame handler.
    pub fn on_frame<F>(&self, handler: F)
    where
        F: Fn(Vec<u8>) + 'static,
    {
        let boxed: Box<dyn FnMut(JsValue)> = Box::new(move |data: JsValue| {
            handler(Uint8Array::new(&data).to_vec());
        });
        let closure = Closure::wrap(boxed);
        self.inner.on_frame(closure.as_ref().unchecked_ref());
        *self.frame_handler.borrow_mut() = Some(closure);
    }

    /// Register the handler invoked once when the transport closes. Replaces any
    /// previously registered close handler.
    pub fn on_close<F>(&self, handler: F)
    where
        F: Fn() + 'static,
    {
        let boxed: Box<dyn FnMut()> = Box::new(handler);
        let closure = Closure::wrap(boxed);
        self.inner.on_close(closure.as_ref().unchecked_ref());
        *self.close_handler.borrow_mut() = Some(closure);
    }

    /// Close the data channel and the underlying peer connection.
    pub fn close(&self) {
        self.inner.close();
    }
}

/// Build the `WebRtcConfig`-shaped JS object the bridge expects.
fn to_js_config(config: &WebRtcConfig) -> Result<JsValue, WebRtcError> {
    let object = Object::new();
    set(&object, "relayUrl", &JsValue::from_str(&config.relay_url))?;

    let servers = Array::new();
    for url in &config.stun_servers {
        servers.push(&JsValue::from_str(url));
    }
    set(&object, "stunServers", servers.as_ref())?;

    if let Some(session_id) = &config.session_id {
        set(&object, "sessionId", &JsValue::from_str(session_id))?;
    }

    Ok(object.into())
}

/// Set a property on a JS object, mapping a rejected write to a bridge error.
fn set(object: &Object, key: &str, value: &JsValue) -> Result<(), WebRtcError> {
    Reflect::set(object.as_ref(), &JsValue::from_str(key), value)
        .map(|_| ())
        .map_err(|e| WebRtcError::Js(describe(&e)))
}

/// Best-effort human-readable rendering of a JS error value.
fn describe(value: &JsValue) -> String {
    value.as_string().unwrap_or_else(|| format!("{value:?}"))
}
