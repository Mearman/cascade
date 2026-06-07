// Stub module for wasm-bindgen-test --node.
//
// Mirrors the contract of apps/web/src/wasm/webrtc.ts exactly: the Rust
// extern block (under feature js-test-stub) imports this module instead of the
// production .ts file. Node can load plain .js without a transpiler.
//
// Exported functions match the wasm-bindgen extern block signatures:
//   - isWebRtcSupported  → returns a controllable bool
//   - createPeerConnection → returns a StubFrameTransport instance
//
// StubFrameTransport exposes the FrameTransport interface plus an inspector
// API so Rust tests can read the recorded state via extra stub imports.

// ── Module-level state ────────────────────────────────────────────────────────

// The value returned by isWebRtcSupported.
let SUPPORTED = true;

// Config object received by the last createPeerConnection call (as JSON).
let LAST_CONFIG_JSON = null;

// ── Inspector exports ─────────────────────────────────────────────────────────

export function setSupported(value) {
  SUPPORTED = value;
}

export function getLastConfigJson() {
  return LAST_CONFIG_JSON;
}

export function resetState() {
  SUPPORTED = true;
  LAST_CONFIG_JSON = null;
}

// ── FrameTransport stub ───────────────────────────────────────────────────────

class StubFrameTransport {
  constructor(config) {
    // Capture the config as JSON for inspection from Rust tests.
    LAST_CONFIG_JSON = JSON.stringify(config);

    this._connected = false;
    this._lastSend = null;
    this._frameHandler = null;
    this._closeHandler = null;
  }

  // FrameTransport interface

  send(data) {
    // data arrives as a Uint8Array.
    this._lastSend = new Uint8Array(data);
  }

  onFrame(handler) {
    this._frameHandler = handler;
  }

  onClose(handler) {
    this._closeHandler = handler;
  }

  close() {
    this._closed = true;
    if (this._closeHandler) this._closeHandler();
  }

  get connected() {
    return this._connected;
  }

  // Inspector methods — called from Rust via stub imports

  setConnected(value) {
    this._connected = value;
  }

  getLastSendBytes() {
    return this._lastSend ? this._lastSend.buffer : null;
  }

  // Invoke the currently registered frame handler with the given bytes.
  // Called from Rust to simulate an inbound frame.
  triggerFrame(data) {
    if (this._frameHandler) {
      // The Rust Closure<dyn FnMut(JsValue)> receives a JsValue; pass a
      // Uint8Array so that Uint8Array::new(&data).to_vec() in transport.rs
      // decodes it correctly.
      this._frameHandler(new Uint8Array(data));
    }
  }

  // Invoke the close handler. Called from Rust to simulate transport close.
  triggerClose() {
    if (this._closeHandler) this._closeHandler();
  }

  wasClosed() {
    return this._closed === true;
  }
}

// Holds the most recently created StubFrameTransport so tests can reach it.
let LAST_TRANSPORT = null;

export function getLastTransport() {
  return LAST_TRANSPORT;
}

// ── Bridge exports ────────────────────────────────────────────────────────────

export function isWebRtcSupported() {
  return SUPPORTED;
}

export function createPeerConnection(config) {
  LAST_TRANSPORT = new StubFrameTransport(config);
  return LAST_TRANSPORT;
}
