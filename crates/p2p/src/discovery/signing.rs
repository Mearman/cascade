//! Self-certifying candidate sets.
//!
//! An announce server (and the Mainline DHT) is a blind carrier: it stores
//! and serves a device's candidate set for any peer that asks. A naive carrier
//! could substitute a different device's addresses, relabel an envelope, or
//! replay a stale set. The candidate set is made *self-certifying* — the
//! announcing device signs the set together with the device id it claims and an
//! expiry, and the resolver verifies the signature before trusting a single
//! address. This binds the addresses to the claimed id and bounds replay.
//!
//! ## Threat model: what this does and does not defend against
//!
//! The signing key is *derived from the device id* — the same public,
//! deterministically-derived BEP44 keypair the DHT path uses (see below). The
//! device id is itself a hash of the device's public TLS certificate
//! ([`crate::identity`]), exchanged on every handshake and carried in plaintext
//! in announce request paths and DHT targets. So the verifying key is derivable
//! by anyone, *and so is the signing key*: any party that knows the device id —
//! which includes the carrier and any active man-in-the-middle — can re-derive
//! the identical keypair and produce a validly-signed envelope for that id.
//!
//! What this construction therefore achieves:
//! - **Substitution defence.** A set signed under device A's derived key does
//!   not verify when resolved as device B, because B's derived key differs. A
//!   carrier cannot serve A's stored set in answer to a query for B, nor relabel
//!   A's envelope as B without the signature failing.
//! - **Tamper evidence on a single id.** A carrier cannot flip a byte of a
//!   stored envelope (an address, the expiry) without invalidating the
//!   signature, so a *passive* carrier cannot silently alter a set without
//!   re-deriving the id's key and re-signing.
//! - **Replay bound.** A captured envelope stops verifying once its expiry
//!   passes.
//!
//! What it does **not** achieve, and must not be claimed to: it is *not*
//! forgery- or MITM-resistant against a party that knows the device id. Because
//! the signing key is public-id-derived rather than tied to the device's TLS
//! private key, an active attacker who knows the id (the carrier, a MITM) can
//! forge a fully-valid envelope pointing at attacker-controlled addresses, and
//! [`SignedCandidates::verify`] will accept it. The signature proves only "the
//! author knew the (public) device id", not "the author is the device". This
//! limitation is inherited deliberately from the reused BEP44 derivation, which
//! requires the verifying key to be derivable from the id alone so a peer that
//! has never met the announcer can address and check its stored value without a
//! shared secret. Binding the set to the device's TLS private key instead would
//! break that property; the discovery layer accepts the weaker guarantee and
//! relies on the authenticated TLS handshake (device id = hash of the presented
//! certificate) to reject a connection to a forged address at connect time.
//!
//! ## The signing key is the device's BEP44 key
//!
//! The DHT path ([`super::dht`]) already derives a deterministic ed25519
//! keypair from the device id — that derivation is what lets any peer that
//! knows the id address (and now verify) the announcer's stored value without
//! a shared secret. This module reuses *exactly* that derivation rather than
//! introducing a second key: [`keypair_for_device`] is the single home for
//! turning a device id into its ed25519 keypair, and the DHT live node builds
//! its BEP44 `SigningKey` from the same helper. One key, one derivation, two
//! transports.
//!
//! The seed is the [`super::dht::DhtKey`] (`SHA-256` of a domain prefix and the
//! device id), exactly the 32-byte ed25519 seed width, so the keypair here is
//! byte-for-byte the one the DHT signs its mutable item with.
//!
//! ## What the signature binds
//!
//! [`SignedCandidates`] signs three things together: the candidate set, the
//! device id the announcer is claiming, and an expiry timestamp. The envelope
//! carries no public key — the verifier *derives* the verifying key from the
//! device id it is resolving and checks the signature against that. This is the
//! substitution defence: a set signed under device A's derived key only verifies
//! when resolved as device A, because resolving as device B derives B's key,
//! which never validates A's signature. There is no carried key to substitute.
//! Binding the device id into the signed bytes stops a relabelled envelope (its
//! `device_id` field changed by a carrier) from validating, since the changed
//! bytes no longer match the signature. Binding the expiry bounds replay: a
//! captured envelope stops verifying once its expiry passes. The signature
//! covers a canonical byte encoding built field-by-field (not JSON), so it does
//! not depend on map key ordering or whitespace in any serialiser. As the threat
//! model above states, none of this prevents a party that already knows the
//! (public) device id from minting a fresh valid envelope for that id — the
//! defence is against substitution, relabelling, tampering, and replay, not
//! against forgery by an id-knowing attacker.

use std::time::Duration;

use ed25519_dalek::{SIGNATURE_LENGTH, Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::announce::WireCandidate;
use super::dht::DhtKey;
use crate::candidate::Candidate;
use crate::traversal::Clock;

/// Domain-separation tag prefixed to every signed candidate-set payload.
///
/// The device key is derived for, and used by, more than one purpose; prefixing
/// the signed bytes with a fixed, purpose-specific tag ensures a signature
/// produced here can never be mistaken for — or replayed as — a signature over
/// some other structure that happened to share a byte prefix. Versioned so a
/// future change to the signed layout is a clean break rather than a silent
/// reinterpretation of old bytes.
const SIGNED_CANDIDATES_DOMAIN: &[u8] = b"cascade-announce-signed-candidates-v1";

/// Build the ed25519 signing keypair from a device-id-derived [`DhtKey`] seed.
///
/// The [`DhtKey`] *is* the 32-byte ed25519 seed, so this reconstructs the
/// identical keypair on every node that knows the device id. This is the single
/// seed→keypair site: both the announce signing path (via
/// [`keypair_for_device`]) and the DHT live node's BEP44 `MutableItem` signing
/// build their key here, so the two transports cannot drift onto different
/// keys.
#[must_use]
pub fn signing_key_for_seed(seed: &DhtKey) -> SigningKey {
    SigningKey::from_bytes(seed.as_bytes())
}

/// Derive the ed25519 keypair a device signs its announced candidate set with.
///
/// The seed is the device-id-derived [`DhtKey`] — the same 32-byte value the
/// DHT BEP44 path uses — so the keypair is identical whether the candidate set
/// travels over an announce server or the Mainline DHT, and any peer that knows
/// the device id derives the same verifying key to check the signature.
#[must_use]
pub fn keypair_for_device(device_id: &str) -> SigningKey {
    signing_key_for_seed(&DhtKey::from_device_id(device_id))
}

/// Derive the public verifying key for a device id.
///
/// The resolver derives this key from the id it is resolving and checks the
/// envelope's signature against it. The envelope carries no key of its own, so
/// there is nothing to substitute: a set signed by another device's key simply
/// fails to verify under the resolved id — the substitution defence.
#[must_use]
pub fn verifying_key_for_device(device_id: &str) -> VerifyingKey {
    keypair_for_device(device_id).verifying_key()
}

/// Read the current wall clock as signed Unix milliseconds.
///
/// [`Clock`] reports unsigned milliseconds; the signed envelope carries `i64`
/// timestamps (matching the DHT BEP44 sequence number, also `i64`). A clock
/// value beyond `i64::MAX` is not representable as a date this side of the year
/// 292 million, so saturating at `i64::MAX` is correct rather than a lossy cast.
#[must_use]
pub fn now_unix_ms(clock: &dyn Clock) -> i64 {
    i64::try_from(clock.now_unix_ms()).unwrap_or(i64::MAX)
}

/// Compute the expiry instant `ttl` after now, as signed Unix milliseconds.
///
/// Used by the announce and DHT publish paths to stamp a signed set's expiry.
/// The TTL is bounded (an hour-scale window), so the addition cannot overflow
/// `i64` for any realistic clock; a saturating add guards the theoretical edge
/// without a panic.
#[must_use]
pub fn expiry_from_now(clock: &dyn Clock, ttl: Duration) -> i64 {
    let ttl_ms = i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX);
    now_unix_ms(clock).saturating_add(ttl_ms)
}

/// Why verifying a [`SignedCandidates`] envelope failed.
///
/// Every variant is a hard rejection: the resolver discards the envelope and
/// treats the device as offering no candidates. None is recoverable, and none
/// is a panic — a hostile carrier must never be able to crash the resolver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum VerifyError {
    /// The envelope claims a different device id than the one being resolved. A
    /// carrier serving one device's set in answer to a query for another is
    /// rejected before any cryptography runs.
    #[error("envelope claims a different device id than the one being resolved")]
    WrongDevice,

    /// The signature did not verify against the signed payload using the key
    /// derived from the resolved device id — the envelope was forged, tampered
    /// with, or signed by a different device.
    #[error("signature verification failed")]
    BadSignature,

    /// The expiry has passed: the envelope is stale and must not be trusted,
    /// bounding how long a captured set can be replayed.
    #[error("signed candidate set expired at {expires_at_unix_ms} ms (now {now_unix_ms} ms)")]
    Expired {
        /// The envelope's expiry, in Unix milliseconds.
        expires_at_unix_ms: i64,
        /// The verifier's current wall clock, in Unix milliseconds.
        now_unix_ms: i64,
    },
}

/// A device's candidate set, signed by the device's own key.
///
/// This is the wire shape stored and served by the announce server and the
/// DHT. Both carriers treat it as opaque: they never inspect the candidates,
/// the key, or the signature — they only hand the blob back verbatim. The
/// resolver is the only party that interprets it, and it does so only after
/// [`Self::verify`] succeeds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedCandidates {
    /// The device id the signer is claiming. Verification re-derives the
    /// verifying key from this id and checks the signature against it, and the
    /// id is itself part of the signed bytes, so the field is authenticated
    /// rather than merely advisory — a carrier cannot relabel it without
    /// invalidating the signature.
    pub device_id: String,

    /// Candidates the device is reachable on, in announcer-computed priority
    /// order.
    pub candidates: Vec<WireCandidate>,

    /// Expiry, in Unix milliseconds. After this instant the envelope no longer
    /// verifies, bounding replay of a captured set.
    pub expires_at_unix_ms: i64,

    /// ed25519 signature (raw 64 bytes) over the canonical encoding of the
    /// device id, candidates, and expiry. Verified with the key derived from
    /// the resolved device id — no public key is carried, so there is nothing
    /// for a carrier to substitute. Carried as base64 on the wire because serde
    /// does not derive (de)serialisation for byte arrays this wide.
    #[serde(with = "base64_signature")]
    pub signature: [u8; SIGNATURE_LENGTH],
}

/// Base64 (de)serialisation for the fixed-width signature byte array.
///
/// serde derives array (de)serialisation only up to 32 bytes by value, and JSON
/// arrays-of-numbers are wasteful and fragile; encoding as base64 gives a
/// compact, stable string field. The decode rejects a wrong-length blob loudly,
/// which the signature check then catches.
mod base64_signature {
    use data_encoding::BASE64;
    use ed25519_dalek::SIGNATURE_LENGTH;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        bytes: &[u8; SIGNATURE_LENGTH],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&BASE64.encode(bytes))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<[u8; SIGNATURE_LENGTH], D::Error> {
        let encoded = String::deserialize(deserializer)?;
        let decoded = BASE64
            .decode(encoded.as_bytes())
            .map_err(serde::de::Error::custom)?;
        <[u8; SIGNATURE_LENGTH]>::try_from(decoded.as_slice()).map_err(|_| {
            serde::de::Error::invalid_length(decoded.len(), &"64-byte ed25519 signature")
        })
    }
}

impl SignedCandidates {
    /// Build a canonical byte encoding of the signed fields.
    ///
    /// The signature covers these bytes, not the JSON form, so verification is
    /// independent of serialiser whitespace or map ordering. The layout is
    /// length-prefixed and fixed-width throughout so it is unambiguous: no two
    /// distinct `(device_id, candidates, expiry)` tuples can produce the same
    /// bytes. Lengths are written as `u64` big-endian; the candidate count and
    /// device-id length come from in-memory `usize` values that are bounded
    /// well below `u64::MAX`, and the `u64` conversion is therefore total.
    fn signing_bytes(
        device_id: &str,
        candidates: &[WireCandidate],
        expires_at_unix_ms: i64,
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(SIGNED_CANDIDATES_DOMAIN);

        let device_id_bytes = device_id.as_bytes();
        bytes.extend_from_slice(&device_id_len_prefix(device_id_bytes.len()));
        bytes.extend_from_slice(device_id_bytes);

        bytes.extend_from_slice(&expires_at_unix_ms.to_be_bytes());

        bytes.extend_from_slice(&candidate_count_prefix(candidates.len()));
        for candidate in candidates {
            // Each candidate is encoded field-by-field at fixed width so the
            // set's contribution to the signed bytes is canonical and unforgeable
            // without re-signing.
            match candidate.address {
                std::net::SocketAddr::V4(v4) => {
                    bytes.push(4);
                    bytes.extend_from_slice(&v4.ip().octets());
                    bytes.extend_from_slice(&v4.port().to_be_bytes());
                }
                std::net::SocketAddr::V6(v6) => {
                    bytes.push(6);
                    bytes.extend_from_slice(&v6.ip().octets());
                    bytes.extend_from_slice(&v6.port().to_be_bytes());
                }
            }
            bytes.push(candidate.kind);
            bytes.extend_from_slice(&candidate.priority.to_be_bytes());
        }
        bytes
    }

    /// Sign `candidates` for `device_id`, expiring at `expires_at_unix_ms`.
    ///
    /// Derives the device's keypair from its id (the same key the DHT path
    /// signs with), then signs the canonical encoding. The resulting envelope
    /// is self-certifying: any peer that knows `device_id` can verify it
    /// without trusting whoever carried it.
    #[must_use]
    pub fn sign(device_id: &str, candidates: Vec<WireCandidate>, expires_at_unix_ms: i64) -> Self {
        let key = keypair_for_device(device_id);
        let message = Self::signing_bytes(device_id, &candidates, expires_at_unix_ms);
        let signature = key.sign(&message);
        Self {
            device_id: device_id.to_owned(),
            candidates,
            expires_at_unix_ms,
            signature: signature.to_bytes(),
        }
    }

    /// Verify the envelope against the device id being resolved and the current
    /// wall clock, returning the carried candidates on success.
    ///
    /// All defences are enforced, and any failure is a hard rejection (never a
    /// panic, never a silent acceptance):
    /// - the envelope's claimed `device_id` must equal `expected_device_id`,
    ///   rejecting a substituted set before any cryptography runs;
    /// - the signature must verify (`verify_strict`) against the canonical bytes
    ///   *using the key derived from `expected_device_id`*, so a tampered
    ///   candidate or expiry, or a set signed by a different device, is caught
    ///   — there is no carried key to trust;
    /// - the expiry must be in the future relative to `now_unix_ms`, bounding
    ///   replay.
    ///
    /// On success the carried [`WireCandidate`]s are returned for the caller to
    /// project into [`Candidate`]s; an unknown kind tag is dropped at that
    /// projection step, exactly as the unsigned path did.
    pub fn verify(
        &self,
        expected_device_id: &str,
        now_unix_ms: i64,
    ) -> Result<&[WireCandidate], VerifyError> {
        // The claimed id in the envelope and the id being resolved must agree.
        // Cheap and rejects a relabelled or substituted envelope before any
        // curve arithmetic.
        if self.device_id != expected_device_id {
            return Err(VerifyError::WrongDevice);
        }

        // Derive the verifying key from the resolved id — the envelope carries
        // no key, so there is nothing to substitute. A set signed by a different
        // device fails here because its signature was made with a different key.
        let verifying_key = verifying_key_for_device(expected_device_id);

        let message =
            Self::signing_bytes(&self.device_id, &self.candidates, self.expires_at_unix_ms);
        let signature = Signature::from_bytes(&self.signature);
        verifying_key
            .verify_strict(&message, &signature)
            .map_err(|_| VerifyError::BadSignature)?;

        if self.expires_at_unix_ms <= now_unix_ms {
            return Err(VerifyError::Expired {
                expires_at_unix_ms: self.expires_at_unix_ms,
                now_unix_ms,
            });
        }

        Ok(&self.candidates)
    }

    /// Verify and project to in-memory [`Candidate`]s in one step.
    ///
    /// The common resolver path: verify the envelope, then map the surviving
    /// [`WireCandidate`]s to [`Candidate`]s, dropping any whose kind tag is
    /// unknown. Returns the rejection reason on failure so the caller can log
    /// loudly.
    pub fn verify_to_candidates(
        &self,
        expected_device_id: &str,
        now_unix_ms: i64,
    ) -> Result<Vec<Candidate>, VerifyError> {
        let wire = self.verify(expected_device_id, now_unix_ms)?;
        Ok(wire.iter().filter_map(|c| c.to_candidate()).collect())
    }
}

/// Encode a device-id byte length as the `u64` big-endian prefix used in the
/// signed bytes. A device id is a base32 SHA-256 (52 bytes); the `usize`→`u64`
/// widening is total on every supported platform.
fn device_id_len_prefix(len: usize) -> [u8; 8] {
    u64::try_from(len).unwrap_or(u64::MAX).to_be_bytes()
}

/// Encode a candidate count as the `u64` big-endian prefix used in the signed
/// bytes. The honest announce and DHT producers cap the count at
/// [`super::announce::MAX_ANNOUNCE_CANDIDATES`] before signing, and the carriers
/// reject (announce server) or cannot store (DHT 1000-byte ceiling) anything
/// larger; the encoding itself imposes no cap, but a `usize` candidate count is
/// bounded well below `u64::MAX`, so the `usize`→`u64` widening is total on every
/// supported platform regardless.
fn candidate_count_prefix(count: usize) -> [u8; 8] {
    u64::try_from(count).unwrap_or(u64::MAX).to_be_bytes()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    use crate::candidate::{Candidate, CandidateKind};

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn wire(port: u16, kind: CandidateKind, pref: u16) -> WireCandidate {
        WireCandidate::from(Candidate::new(addr(port), kind, pref))
    }

    /// One hour past the fixed test epoch — comfortably unexpired for the
    /// round-trip tests, which verify against an earlier `now`.
    const TEST_NOW: i64 = 1_700_000_000_000;
    const TEST_EXPIRY: i64 = TEST_NOW + 3_600_000;

    #[test]
    fn sign_then_verify_round_trips() {
        let candidates = vec![
            wire(22000, CandidateKind::Host, 65_535),
            wire(33000, CandidateKind::ServerReflexive, 0),
        ];
        let signed = SignedCandidates::sign("DEVICE-A", candidates.clone(), TEST_EXPIRY);
        let verified = signed.verify("DEVICE-A", TEST_NOW).unwrap();
        assert_eq!(verified, candidates.as_slice());
    }

    #[test]
    fn tampering_with_a_candidate_byte_is_rejected() {
        let mut signed = SignedCandidates::sign(
            "DEVICE-A",
            vec![wire(22000, CandidateKind::Host, 1)],
            TEST_EXPIRY,
        );
        // Mutate a candidate after signing: the signature no longer covers it.
        signed.candidates[0].priority ^= 0x01;
        assert_eq!(
            signed.verify("DEVICE-A", TEST_NOW),
            Err(VerifyError::BadSignature)
        );
    }

    #[test]
    fn tampering_with_a_single_signature_byte_is_rejected() {
        let mut signed = SignedCandidates::sign(
            "DEVICE-A",
            vec![wire(22000, CandidateKind::Host, 1)],
            TEST_EXPIRY,
        );
        signed.signature[0] ^= 0x01;
        assert_eq!(
            signed.verify("DEVICE-A", TEST_NOW),
            Err(VerifyError::BadSignature)
        );
    }

    #[test]
    fn tampering_with_the_expiry_is_rejected() {
        let mut signed = SignedCandidates::sign(
            "DEVICE-A",
            vec![wire(22000, CandidateKind::Host, 1)],
            TEST_EXPIRY,
        );
        // Extend the expiry without re-signing: the signature covers the
        // original expiry, so the change is caught before the freshness check.
        signed.expires_at_unix_ms = TEST_EXPIRY + 1_000_000;
        assert_eq!(
            signed.verify("DEVICE-A", TEST_NOW),
            Err(VerifyError::BadSignature)
        );
    }

    #[test]
    fn blob_signed_by_a_different_device_is_rejected_when_resolving_another_id() {
        // DEVICE-B signs its own set; an attacker stores it under DEVICE-A's
        // key on the carrier. Resolving DEVICE-A must reject it.
        let signed = SignedCandidates::sign(
            "DEVICE-B",
            vec![wire(22000, CandidateKind::Host, 1)],
            TEST_EXPIRY,
        );
        assert_eq!(
            signed.verify("DEVICE-A", TEST_NOW),
            Err(VerifyError::WrongDevice)
        );
    }

    #[test]
    fn relabelled_envelope_for_another_device_is_rejected() {
        // An attacker re-labels DEVICE-B's envelope as DEVICE-A, leaving B's
        // signature. The claimed id now matches the query, so the cheap
        // device-id check passes — but the signed bytes covered the original
        // "DEVICE-B" id, so verifying with A's derived key over the relabelled
        // bytes fails the signature check.
        let mut signed = SignedCandidates::sign(
            "DEVICE-B",
            vec![wire(1, CandidateKind::Host, 0)],
            TEST_EXPIRY,
        );
        signed.device_id = "DEVICE-A".to_owned();
        assert_eq!(
            signed.verify("DEVICE-A", TEST_NOW),
            Err(VerifyError::BadSignature)
        );
    }

    #[test]
    fn expired_envelope_is_rejected() {
        let signed =
            SignedCandidates::sign("DEVICE-A", vec![wire(1, CandidateKind::Host, 0)], TEST_NOW);
        // now == expiry: expiry is inclusive-past, so this is stale.
        let err = signed.verify("DEVICE-A", TEST_NOW).unwrap_err();
        assert!(matches!(err, VerifyError::Expired { .. }));
        // And strictly past expiry is also rejected.
        let err = signed.verify("DEVICE-A", TEST_NOW + 1).unwrap_err();
        assert!(matches!(err, VerifyError::Expired { .. }));
    }

    #[test]
    fn verify_to_candidates_projects_known_kinds() {
        let candidates = vec![
            wire(22000, CandidateKind::Host, 65_535),
            wire(33000, CandidateKind::Relayed, 0),
        ];
        let signed = SignedCandidates::sign("DEVICE-A", candidates, TEST_EXPIRY);
        let projected = signed.verify_to_candidates("DEVICE-A", TEST_NOW).unwrap();
        assert_eq!(projected.len(), 2);
    }

    #[test]
    fn json_round_trip_preserves_the_envelope() {
        let signed = SignedCandidates::sign(
            "DEVICE-A",
            vec![wire(22000, CandidateKind::Host, 65_535)],
            TEST_EXPIRY,
        );
        let json = serde_json::to_string(&signed).unwrap();
        let decoded: SignedCandidates = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, signed);
        // The decoded envelope still verifies — JSON transport does not perturb
        // the signed bytes.
        assert!(decoded.verify("DEVICE-A", TEST_NOW).is_ok());
    }
}
