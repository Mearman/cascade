//! wasm-bindgen-test suite for the WebRTC transport bridge.
//!
//! These tests exercise the Rust-side marshalling paths against a JS stub
//! module (tests/js/webrtc_stub.js). They run under `wasm-pack test --node`
//! with the `js-test-stub` feature enabled and are invisible to native
//! `cargo test`.
//!
//! Coverage boundary: these tests verify that the Rust layer correctly
//! marshals bytes across the boundary, registers closures, and reflects JS
//! state — NOT browser-specific WebRTC or signalling behaviour.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::string_slice
)]

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen_test::wasm_bindgen_test;

use crate::config::WebRtcConfig;
use crate::js;
use crate::transport::{WebRtcTransport, create_transport, supported};

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_node_experimental);

// ── Helpers ───────────────────────────────────────────────────────────────────

fn initiator_config() -> WebRtcConfig {
    WebRtcConfig {
        relay_url: "wss://relay.example/ws".into(),
        stun_servers: vec!["stun:stun.l.google.com:19302".into()],
        session_id: None,
    }
}

fn responder_config() -> WebRtcConfig {
    WebRtcConfig {
        relay_url: "wss://relay.example/ws".into(),
        stun_servers: vec!["stun:stun.l.google.com:19302".into()],
        session_id: Some("abc-123".into()),
    }
}

fn make_transport(config: &WebRtcConfig) -> WebRtcTransport {
    create_transport(config).unwrap()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// isWebRtcSupported returns true when the stub is configured that way.
#[wasm_bindgen_test]
fn supported_reads_bridge_true() {
    js::reset_state();
    js::set_supported(true);
    assert!(supported());
}

/// isWebRtcSupported returns false when the stub is configured that way.
#[wasm_bindgen_test]
fn supported_reads_bridge_false() {
    js::reset_state();
    js::set_supported(false);
    assert!(!supported());
}

/// createPeerConnection is called with camelCase config keys; the initiator
/// config must not include a sessionId key.
#[wasm_bindgen_test]
fn create_transport_passes_camel_case_initiator_config() {
    js::reset_state();
    js::set_supported(true);
    let _t = make_transport(&initiator_config());

    let json_str = js::get_last_config_json().expect("createPeerConnection was not called");
    let value: serde_json::Value = serde_json::from_str(&json_str).unwrap();

    assert!(
        value.get("relayUrl").is_some(),
        "relayUrl missing from config"
    );
    assert!(
        value.get("stunServers").is_some(),
        "stunServers missing from config"
    );
    assert!(
        value.get("sessionId").is_none(),
        "initiator config must not include sessionId"
    );
}

/// createPeerConnection for a responder config must include sessionId.
#[wasm_bindgen_test]
fn create_transport_passes_camel_case_responder_config() {
    js::reset_state();
    js::set_supported(true);
    let _t = make_transport(&responder_config());

    let json_str = js::get_last_config_json().expect("createPeerConnection was not called");
    let value: serde_json::Value = serde_json::from_str(&json_str).unwrap();

    assert_eq!(
        value.get("sessionId").and_then(|v| v.as_str()),
        Some("abc-123")
    );
}

/// connected() mirrors the stub's _connected flag.
#[wasm_bindgen_test]
fn connected_reflects_channel_state() {
    js::reset_state();
    js::set_supported(true);
    let t = make_transport(&initiator_config());

    // Initially false.
    let inner = js::get_last_transport();
    inner.set_connected(false);
    assert!(!t.connected());

    inner.set_connected(true);
    assert!(t.connected());
}

/// send() forwards bytes to the stub; the stub records them correctly.
#[wasm_bindgen_test]
fn send_forwards_bytes() {
    js::reset_state();
    js::set_supported(true);
    let t = make_transport(&initiator_config());

    t.send(&[9u8, 8, 7]);

    use js_sys::Uint8Array;
    use wasm_bindgen::JsCast;
    let inner = js::get_last_transport();
    let buf = inner
        .get_last_send_bytes()
        .expect("send was not recorded by stub");
    let recorded = Uint8Array::new(&buf.unchecked_into::<js_sys::ArrayBuffer>()).to_vec();
    assert_eq!(recorded, [9u8, 8, 7]);
}

/// on_frame closure receives correctly decoded bytes when the stub fires it.
#[wasm_bindgen_test]
fn on_frame_closure_receives_bytes() {
    js::reset_state();
    js::set_supported(true);
    let t = make_transport(&initiator_config());

    let received: Rc<RefCell<Option<Vec<u8>>>> = Rc::new(RefCell::new(None));
    let received_clone = Rc::clone(&received);
    t.on_frame(move |bytes| {
        *received_clone.borrow_mut() = Some(bytes);
    });

    // Have the stub synchronously fire the handler.
    let inner = js::get_last_transport();
    inner.trigger_frame(&[11u8, 22, 33]);

    let observed = received
        .borrow()
        .clone()
        .expect("frame handler was not called");
    assert_eq!(observed, [11u8, 22, 33]);
}

/// on_close closure fires when the stub invokes it; close() reaches the stub.
#[wasm_bindgen_test]
fn on_close_closure_fires_once() {
    js::reset_state();
    js::set_supported(true);
    let t = make_transport(&initiator_config());

    let fired: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
    let fired_clone = Rc::clone(&fired);
    t.on_close(move || {
        *fired_clone.borrow_mut() = true;
    });

    let inner = js::get_last_transport();
    assert!(!inner.was_closed(), "close should not have been called yet");

    t.close();
    assert!(inner.was_closed(), "close() did not reach the stub");
    assert!(*fired.borrow(), "on_close handler was not invoked");
}

/// Registering a second on_frame handler replaces the first; only the second
/// observes the frame.
#[wasm_bindgen_test]
fn on_frame_replaces_previous_handler() {
    js::reset_state();
    js::set_supported(true);
    let t = make_transport(&initiator_config());

    let first_saw: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
    let first_clone = Rc::clone(&first_saw);
    t.on_frame(move |_| {
        *first_clone.borrow_mut() = true;
    });

    let second_saw: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
    let second_clone = Rc::clone(&second_saw);
    t.on_frame(move |_| {
        *second_clone.borrow_mut() = true;
    });

    let inner = js::get_last_transport();
    inner.trigger_frame(&[1u8]);

    assert!(
        !*first_saw.borrow(),
        "first handler should have been replaced"
    );
    assert!(*second_saw.borrow(), "second handler was not invoked");
}
