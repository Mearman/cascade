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
    use std::time::Instant;

    use crate::candidate::{Candidate, CandidateKind};
    use cascade_announce_wire::WireCandidate;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    /// A [`Clock`] whose wall clock returns a caller-chosen fixed value, so the
    /// clock-stamped expiry helpers can be exercised deterministically across
    /// the whole `u64` range — including the saturation edges the production
    /// [`crate::traversal::SystemClock`] can never reach.
    struct FixedClock {
        unix_ms: u64,
    }

    impl Clock for FixedClock {
        fn now(&self) -> Instant {
            // The monotonic clock is unused by the expiry helpers; return a
            // live instant so the contract is honoured without affecting the
            // wall-clock paths under test.
            Instant::now()
        }

        fn now_unix_ms(&self) -> u64 {
            self.unix_ms
        }
    }

    /// A representative in-range wall-clock value, well below the `i64` ceiling.
    const IN_RANGE_NOW_MS: u64 = 1_700_000_000_000;
    /// A one-hour TTL in milliseconds, matching the announce and DHT windows.
    const ONE_HOUR_MS: i64 = 3_600_000;

    #[test]
    fn now_unix_ms_reads_the_clocks_wall_time_as_signed() {
        // A wall-clock value comfortably inside the `i64` range maps across
        // unchanged: the helper only diverges from the raw clock at the
        // saturation edge.
        let clock = FixedClock {
            unix_ms: IN_RANGE_NOW_MS,
        };
        assert_eq!(now_unix_ms(&clock), i64::try_from(IN_RANGE_NOW_MS).unwrap());
    }

    #[test]
    fn now_unix_ms_saturates_at_i64_max_for_an_out_of_range_clock() {
        // A clock reporting a value above `i64::MAX` cannot be represented as a
        // signed timestamp; the documented behaviour is to saturate at
        // `i64::MAX` rather than wrap or panic. `u64::MAX` is the extreme of
        // that range.
        let clock = FixedClock { unix_ms: u64::MAX };
        assert_eq!(now_unix_ms(&clock), i64::MAX);
    }

    #[test]
    fn expiry_from_now_adds_the_ttl_to_the_current_wall_time() {
        let clock = FixedClock {
            unix_ms: IN_RANGE_NOW_MS,
        };
        let expiry = expiry_from_now(&clock, Duration::from_hours(1));
        assert_eq!(
            expiry,
            i64::try_from(IN_RANGE_NOW_MS).unwrap() + ONE_HOUR_MS,
        );
    }

    #[test]
    fn expiry_from_now_saturates_rather_than_overflowing() {
        // With the wall clock already at the signed ceiling, adding any TTL
        // must saturate at `i64::MAX` rather than wrap to a negative (past)
        // instant — an expiry that wrapped would reject every freshly-signed
        // set the moment it was produced.
        let clock = FixedClock { unix_ms: u64::MAX };
        let expiry = expiry_from_now(&clock, Duration::from_hours(1));
        assert_eq!(expiry, i64::MAX);
    }

    #[test]
    fn expiry_from_now_saturates_on_an_oversized_ttl() {
        // A TTL larger than `i64::MAX` milliseconds is itself clamped before
        // the add, so even from a zero wall clock the result saturates rather
        // than wrapping. [`Duration::MAX`] is the extreme such TTL.
        let clock = FixedClock { unix_ms: 0 };
        let expiry = expiry_from_now(&clock, Duration::MAX);
        assert_eq!(expiry, i64::MAX);
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
