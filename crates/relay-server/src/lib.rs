//! Cascade relay server.
//!
//! An opaque byte-pipe relay that pairs two `WebSocket` clients sharing a
//! session ID and shuttles binary frames between them. The relay never
//! inspects the bytes — clients establish their own end-to-end TLS over the
//! tunnel, so the relay operator sees only ciphertext.
//!
//! Sessions are gated by an HMAC handshake: each client proves possession of
//! a shared secret by sending `HMAC-SHA256(secret, device_id || session_id)`
//! as the first binary frame. Two peers must authenticate independently
//! before either is admitted to the byte-pipe.
//!
//! Architecture
//!
//! ```text
//! Client A ─┐                                    ┌─ Client B
//!           │  WebSocket (TLS-wrapped payload)   │
//!           ├──> /join/<session_id>  ──────────  ┤
//!           │                                    │
//!           └─── (HMAC handshake then byte-pipe) ┘
//! ```
//!
//! The session ID is the rendezvous key — the server is a passive
//! matchmaker that does not speak the inner protocol.

pub mod auth;
pub mod config;
pub mod metrics;
pub mod pipe;
pub mod server;
pub mod session;

pub use config::RelayConfig;
pub use server::run_relay;
