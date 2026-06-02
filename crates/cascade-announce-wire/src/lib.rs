//! Wasm-safe announce/DHT wire types and rendezvous primitives.
//!
//! The cascade rendezvous directory (the announce server, the DHT, and the
//! Cloudflare Worker that hosts the same HTTP contract on soft state) shares a
//! small set of types and crypto primitives that must compile identically on the
//! native targets *and* on `wasm32-unknown-unknown`. This crate is that shared
//! core, deliberately free of the connectivity stack (`cascade-p2p`), tokio,
//! rustls, and reqwest so it builds clean for a Worker:
//!
//! - [`candidate::WireCandidate`] — the JSON-serialisable projection of one
//!   reachable address. `cascade-p2p` owns the `From<Candidate>` and
//!   `to_candidate` conversions; this crate owns the wire shape.
//! - [`seed`] — the device-id → BEP44 ed25519 seed derivation and the single
//!   seed→keypair construction, shared by the announce and DHT signing paths.
//! - [`signing::SignedCandidates`] — the self-certifying envelope and its
//!   `verify`, with the full threat model documented on the module.
//! - [`wire`] — the `AnnounceRequest` / `LookupResponse` HTTP bodies and the
//!   per-device candidate cap.
//! - [`auth`] — the `HMAC-SHA256` client-auth primitive and the announce write
//!   authentication built on it, shared with the relay-server handshake.
//! - [`handler`] — the stateless announce-directory request handling (routing,
//!   write authentication, size bounds, blob round-trip) over a [`handler::BlobStore`]
//!   contract, so the announce Worker's logic is unit-tested on the native target
//!   and only the KV/Worker glue is wasm-only.

pub mod auth;
pub mod candidate;
pub mod handler;
pub mod seed;
pub mod signing;
pub mod wire;

pub use candidate::WireCandidate;
pub use seed::{DhtKey, keypair_for_device, signing_key_for_seed, verifying_key_for_device};
pub use signing::{SignedCandidates, VerifyError};
pub use wire::{AnnounceRequest, LookupResponse, MAX_ANNOUNCE_CANDIDATES};
