//! Announce-directory request and response bodies.
//!
//! Two JSON endpoints, both rooted at the configured base URL:
//!
//! - `POST <base>/announce/<device_id>` with an [`AnnounceRequest`] body —
//!   replaces the stored candidate set for `device_id`.
//! - `GET  <base>/announce/<device_id>` — returns a [`LookupResponse`] with the
//!   most recently registered candidate set, or nothing when the id is unknown.
//!
//! The candidate set is carried inside a
//! [`crate::signing::SignedCandidates`] envelope: the announcing device signs
//! its candidates, the claimed device id, and an expiry. The carrier stores and
//! serves the signed blob verbatim and never inspects or vouches for it; the
//! looking-up client is the only party that verifies the signature.

use serde::{Deserialize, Serialize};

use crate::signing::SignedCandidates;

/// Maximum number of candidates accepted in a single announce request or
/// returned from a lookup.
///
/// Mirrors the `MAX_CANDIDATES_PER_FRAME` cap the BEP `Candidates` frame uses (a
/// device with more than a handful of host, server-reflexive and relayed
/// addresses is unrealistic), so the announce directory bounds its per-device
/// storage the same way the wire protocol bounds a frame.
pub const MAX_ANNOUNCE_CANDIDATES: usize = 64;

/// Body of a `POST <base>/announce/<device_id>` request.
///
/// Carries the signed candidate set the announcing device is currently reachable
/// on. A subsequent announce for the same id replaces the set in full —
/// candidates are not accumulated. The server stores the [`SignedCandidates`]
/// verbatim and never inspects it; the looking-up client is the only party that
/// verifies the signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnounceRequest {
    /// The device's self-signed candidate set.
    pub signed: SignedCandidates,
}

/// Body of a `GET <base>/announce/<device_id>` response.
///
/// An unknown device id yields `signed: None` rather than a `404`, so the client
/// models absence as "no candidates". A known id returns the signed blob exactly
/// as it was registered, for the client to verify.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookupResponse {
    /// The signed candidate set last registered for the looked-up device id, or
    /// `None` when the id is unknown.
    pub signed: Option<SignedCandidates>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    use crate::candidate::WireCandidate;

    fn wire(port: u16, kind: u8, priority: u32) -> WireCandidate {
        WireCandidate {
            address: SocketAddr::from(([127, 0, 0, 1], port)),
            kind,
            priority,
        }
    }

    #[test]
    fn announce_request_round_trips_through_json() {
        let request = AnnounceRequest {
            signed: SignedCandidates::sign(
                "DEVICE-A",
                vec![wire(22000, 0, 65_535), wire(33000, 1, 0)],
                1_700_000_000_000,
            ),
        };
        let json = serde_json::to_string(&request).unwrap();
        let decoded: AnnounceRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, request);
    }

    #[test]
    fn lookup_response_round_trips_through_json() {
        let response = LookupResponse {
            signed: Some(SignedCandidates::sign(
                "DEVICE-A",
                vec![wire(22000, 0, 1)],
                1_700_000_000_000,
            )),
        };
        let json = serde_json::to_string(&response).unwrap();
        let decoded: LookupResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, response);
    }

    #[test]
    fn lookup_response_models_unknown_id_as_none() {
        let response = LookupResponse { signed: None };
        let json = serde_json::to_string(&response).unwrap();
        let decoded: LookupResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, response);
        assert!(decoded.signed.is_none());
    }
}
