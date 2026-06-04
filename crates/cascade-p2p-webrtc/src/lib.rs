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
//! On native targets the crate builds as an empty lib — every module and public
//! item is gated on `#[cfg(target_arch = "wasm32")]`, so an ordinary
//! `cargo check --workspace` succeeds without pulling in any wasm-only
//! dependency. Verify the WASM build with:
//!
//! ```text
//! cargo check -p cascade-p2p-webrtc --target wasm32-unknown-unknown
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

#[cfg(target_arch = "wasm32")]
mod js;
#[cfg(target_arch = "wasm32")]
mod transport;

#[cfg(target_arch = "wasm32")]
pub use transport::{WebRtcConfig, WebRtcError, WebRtcTransport, create_transport, supported};
