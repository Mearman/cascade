//! Self-certifying candidate sets — `cascade-p2p` glue over the shared core.
//!
//! The envelope type, its signature construction, the threat model, and the
//! verify path all live in the wasm-safe [`cascade_announce_wire`] crate so the
//! announce server, the DHT, *and* the Cloudflare Worker that hosts the same
//! contract share one signing home and one verify path. This module is the thin
//! `cascade-p2p`-side glue: it re-exports the shared types and adds the two
//! conveniences that depend on `cascade-p2p`'s own [`Clock`] and [`Candidate`]
//! and so cannot live in the connectivity-free shared crate — the clock-stamped
//! expiry helpers and the verify-then-project step.

use std::time::Duration;

pub use cascade_announce_wire::seed::{keypair_for_device, signing_key_for_seed};
pub use cascade_announce_wire::signing::{SignedCandidates, VerifyError};
pub use cascade_announce_wire::verifying_key_for_device;

use crate::candidate::Candidate;
use crate::traversal::Clock;

/// Read the current wall clock as signed Unix milliseconds.
///
/// [`Clock`] reports unsigned milliseconds; the signed envelope carries `i64`
/// timestamps (matching the DHT BEP44 sequence number, also `i64`). A clock value
/// beyond `i64::MAX` is not representable as a date this side of the year 292
/// million, so saturating at `i64::MAX` is correct rather than a lossy cast.
#[must_use]
pub fn now_unix_ms(clock: &dyn Clock) -> i64 {
    i64::try_from(clock.now_unix_ms()).unwrap_or(i64::MAX)
}

/// Compute the expiry instant `ttl` after now, as signed Unix milliseconds.
///
/// Used by the announce and DHT publish paths to stamp a signed set's expiry. The
/// TTL is bounded (an hour-scale window), so the addition cannot overflow `i64`
/// for any realistic clock; a saturating add guards the theoretical edge without
/// a panic.
#[must_use]
pub fn expiry_from_now(clock: &dyn Clock, ttl: Duration) -> i64 {
    let ttl_ms = i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX);
    now_unix_ms(clock).saturating_add(ttl_ms)
}

/// Verify a [`SignedCandidates`] envelope and project it to in-memory
/// [`Candidate`]s in one step.
///
/// The common resolver path: verify the envelope (binding the resolved device
/// id, the signature, and the expiry), then map the surviving
/// [`cascade_announce_wire::WireCandidate`]s to [`Candidate`]s, dropping any
/// whose kind tag is unknown. Returns the rejection reason on failure so the
/// caller can log loudly. This lives here rather than in the shared crate because
/// [`Candidate`] is a `cascade-p2p` connectivity type.
pub fn verify_to_candidates(
    signed: &SignedCandidates,
    expected_device_id: &str,
    now_unix_ms: i64,
) -> Result<Vec<Candidate>, VerifyError> {
    let wire = signed.verify(expected_device_id, now_unix_ms)?;
    Ok(wire
        .iter()
        .filter_map(|c| Candidate::from_wire(*c))
        .collect())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    use crate::candidate::{Candidate, CandidateKind};
    use cascade_announce_wire::WireCandidate;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn wire(port: u16, kind: CandidateKind, pref: u16) -> WireCandidate {
        WireCandidate::from(Candidate::new(addr(port), kind, pref))
    }

    const TEST_NOW: i64 = 1_700_000_000_000;
    const TEST_EXPIRY: i64 = TEST_NOW + 3_600_000;

    #[test]
    fn verify_to_candidates_projects_known_kinds() {
        let candidates = vec![
            wire(22000, CandidateKind::Host, 65_535),
            wire(33000, CandidateKind::Relayed, 0),
        ];
        let signed = SignedCandidates::sign("DEVICE-A", candidates, TEST_EXPIRY);
        let projected = verify_to_candidates(&signed, "DEVICE-A", TEST_NOW).unwrap();
        assert_eq!(projected.len(), 2);
    }

    #[test]
    fn verify_to_candidates_propagates_rejection() {
        let signed = SignedCandidates::sign(
            "DEVICE-B",
            vec![wire(1, CandidateKind::Host, 0)],
            TEST_EXPIRY,
        );
        assert_eq!(
            verify_to_candidates(&signed, "DEVICE-A", TEST_NOW),
            Err(VerifyError::WrongDevice)
        );
    }
}
