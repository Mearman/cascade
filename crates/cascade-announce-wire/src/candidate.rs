//! Serialisable candidate projection.
//!
//! [`WireCandidate`] is the JSON-serialisable shape of one reachable transport
//! address that the announce directory and the DHT carry. It is deliberately
//! self-contained — it carries the kind as its stable wire tag (a `u8`) rather
//! than referencing the in-memory `CandidateKind` enum — so this crate stays
//! free of the connectivity stack (`cascade-p2p`) and compiles cleanly to
//! `wasm32` for the Worker. `cascade-p2p` owns the `From<Candidate>` and
//! `to_candidate` conversions; this crate owns only the wire shape and the
//! bytes the signature covers.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

/// Serialisable form of a connectivity candidate.
///
/// The `kind` is carried as its stable wire tag (`0` host, `1` server-reflexive,
/// `2` relayed) rather than an enum so the JSON shape is stable across releases,
/// exactly as the BEP encoding does. `priority` is carried so the looker-up sees
/// the same RFC 8445 ordering the announcer computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireCandidate {
    /// Reachable address (`IPv4` or `IPv6`) plus port.
    pub address: SocketAddr,
    /// Candidate kind as the stable wire tag — `0` host, `1` server-reflexive,
    /// `2` relayed.
    pub kind: u8,
    /// Precomputed RFC 8445 priority, carried so the recipient need not
    /// re-derive it.
    pub priority: u32,
}
