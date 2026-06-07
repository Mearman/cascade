#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::string_slice
    )
)]
//! Browser WebRTC data-channel transport for Cascade.
//!
//! This crate is the Rust side of the WebRTC bridge. It compiles to
//! `wasm32-unknown-unknown` and calls into the browser-side JavaScript module at
//! `apps/web/src/wasm/webrtc.ts` through `wasm-bindgen`. The JS module owns the
//! `RTCPeerConnection`, the relay signalling WebSocket, and the data channel;
//! this crate wraps the resulting frame transport in a small Rust surface
//! (`send`, `on_frame`, `on_close`, `close`, `connected`).
//!
//! The config and error types (`WebRtcConfig`, `WebRtcError`) are portable:
//! they compile and are testable on native targets. The `#[cfg(any(target_arch =
//! "wasm32", test))]` gate makes them available for native unit tests.
//!
//! The JS-interop modules (`js`, `transport`) and the live transport types
//! (`WebRtcTransport`, `create_transport`, `supported`) are wasm32-only.
//!
//! Verify the WASM build with:
//!
//! ```text
//! cargo check -p cascade-p2p-webrtc --target wasm32-unknown-unknown
//! ```
//!
//! Run the wasm-bindgen-test suite (exercises the Rust marshalling paths
//! against a JS stub module):
//!
//! ```text
//! wasm-pack test --node crates/cascade-p2p-webrtc --features js-test-stub
//! ```
//!
//! ## Why not `cascade_p2p::transport::Transport`
//!
//! The existing `Transport` trait in `cascade-p2p` is built on `tokio` IO and a
//! native TLS stack, neither of which compiles to `wasm32-unknown-unknown`. The
//! browser's data channel is callback-driven and single-threaded, a different
//! shape from the native `recv_frame` / `send_frame` reader-writer split. This
//! crate therefore exposes its own callback-based transport surface rather than
//! implementing the native trait.

// The config and error types are portable: they compile on the host so the wire
// contract can be unit-tested natively.
#[cfg(any(target_arch = "wasm32", test))]
mod config;

// The JS-interop bindings and live transport are wasm32-only.
#[cfg(target_arch = "wasm32")]
mod js;
#[cfg(target_arch = "wasm32")]
mod transport;

// Portable re-exports: available to both wasm consumers and native test code.
#[cfg(any(target_arch = "wasm32", test))]
pub use config::{WebRtcConfig, WebRtcError};

// Wasm-only re-exports: the live transport is wasm32-only.
#[cfg(target_arch = "wasm32")]
pub use transport::{WebRtcTransport, create_transport, supported};

// wasm-bindgen-test suite: bridge exercises against the JS stub.
// The `test` gate ensures the module is only compiled as part of a `cargo test`
// invocation (where dev-dependencies like `wasm-bindgen-test` are available),
// never into the production lib. The `js-test-stub` feature gate ensures the
// stub-specific extern block and inspector API are present. The `wasm32` gate
// keeps it invisible to native `cargo test`.
#[cfg(all(test, target_arch = "wasm32", feature = "js-test-stub"))]
mod wasm_tests;
