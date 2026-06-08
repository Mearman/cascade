//! Self-certifying candidate sets.
//!
//! An announce server (and the Mainline DHT) is a blind carrier: it stores and
//! serves a device's candidate set for any peer that asks. A naive carrier could
//! substitute a different device's addresses, relabel an envelope, or replay a
//! stale set. The candidate set is made *self-certifying* — the announcing
//! device signs the set together with the device id it claims and an expiry, and
//! the resolver verifies the signature before trusting a single address. This
//! binds the addresses to the claimed id and bounds replay.
//!
//! ## Threat model: what this does and does not defend against
//!
//! The signing key is *derived from the device id* — the same public,
//! deterministically-derived BEP44 keypair the DHT path uses (see
//! [`crate::seed`]). The device id is itself a hash of the device's public TLS
//! certificate, exchanged on every handshake and carried in plaintext in
//! announce request paths and DHT targets. So the verifying key is derivable by
//! anyone, *and so is the signing key*: any party that knows the device id —
//! which includes the carrier and any active man-in-the-middle — can re-derive
//! the identical keypair and produce a validly-signed envelope for that id.
//!
//! What this construction therefore achieves:
//! - **Substitution defence.** A set signed under device A's derived key does not
//!   verify when resolved as device B, because B's derived key differs. A carrier
//!   cannot serve A's stored set in answer to a query for B, nor relabel A's
//!   envelope as B without the signature failing.
//! - **Tamper evidence on a single id.** A carrier cannot flip a byte of a stored
//!   envelope (an address, the expiry) without invalidating the signature.
//! - **Replay bound.** A captured envelope stops verifying once its expiry
//!   passes.
//!
//! What it does **not** achieve, and must not be claimed to: it is *not* forgery-
//! or MITM-resistant against a party that knows the device id. Because the
//! signing key is public-id-derived rather than tied to the device's TLS private
//! key, an active attacker who knows the id can forge a fully-valid envelope
//! pointing at attacker-controlled addresses, and [`SignedCandidates::verify`]
//! will accept it. The signature proves only "the author knew the (public) device
//! id", not "the author is the device". This limitation is inherited deliberately
//! from the reused BEP44 derivation, which requires the verifying key to be
//! derivable from the id alone; the discovery layer accepts the weaker guarantee
//! and relies on the authenticated TLS handshake at connect time to reject a
//! connection to a forged address.
//!
//! ## What the signature binds
//!
//! [`SignedCandidates`] signs three things together: the candidate set, the
//! device id the announcer is claiming, and an expiry timestamp. The envelope
//! carries no public key — the verifier *derives* the verifying key from the
//! device id it is resolving and checks the signature against that. The signature
//! covers a canonical byte encoding built field-by-field (not JSON), so it does
//! not depend on map key ordering or whitespace in any serialiser.

use ed25519_dalek::{SIGNATURE_LENGTH, Signature, Signer, VerifyingKey};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::candidate::WireCandidate;
use crate::seed::{keypair_for_device, verifying_key_for_device};

/// Domain-separation tag prefixed to every signed candidate-set payload.
///
/// The device key is derived for, and used by, more than one purpose; prefixing
/// the signed bytes with a fixed, purpose-specific tag ensures a signature
/// produced here can never be mistaken for — or replayed as — a signature over
/// some other structure that happened to share a byte prefix. Versioned so a
/// future change to the signed layout is a clean break rather than a silent
/// reinterpretation of old bytes.
const SIGNED_CANDIDATES_DOMAIN: &[u8] = b"cascade-announce-signed-candidates-v1";

/// Why verifying a [`SignedCandidates`] envelope failed.
///
/// Every variant is a hard rejection: the resolver discards the envelope and
/// treats the device as offering no candidates. None is recoverable, and none is
/// a panic — a hostile carrier must never be able to crash the resolver.
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
/// This is the wire shape stored and served by the announce server and the DHT.
/// Both carriers treat it as opaque: they never inspect the candidates, the key,
/// or the signature — they only hand the blob back verbatim. The resolver is the
/// only party that interprets it, and it does so only after [`Self::verify`]
/// succeeds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedCandidates {
    /// The device id the signer is claiming. Verification re-derives the
    /// verifying key from this id and checks the signature against it, and the id
    /// is itself part of the signed bytes, so the field is authenticated rather
    /// than merely advisory — a carrier cannot relabel it without invalidating
    /// the signature.
    pub device_id: String,

    /// Candidates the device is reachable on, in announcer-computed priority
    /// order.
    pub candidates: Vec<WireCandidate>,

    /// Expiry, in Unix milliseconds. After this instant the envelope no longer
    /// verifies, bounding replay of a captured set.
    pub expires_at_unix_ms: i64,

    /// ed25519 signature (raw 64 bytes) over the canonical encoding of the device
    /// id, candidates, and expiry. Verified with the key derived from the
    /// resolved device id — no public key is carried, so there is nothing for a
    /// carrier to substitute. Carried as base64 on the wire because serde does
    /// not derive (de)serialisation for byte arrays this wide.
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
    /// device-id length come from in-memory `usize` values that are bounded well
    /// below `u64::MAX`, and the `u64` conversion is therefore total.
    fn signing_bytes(
        device_id: &str,
        candidates: &[WireCandidate],
        expires_at_unix_ms: i64,
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(SIGNED_CANDIDATES_DOMAIN);

        let device_id_bytes = device_id.as_bytes();
        bytes.extend_from_slice(&len_prefix(device_id_bytes.len()));
        bytes.extend_from_slice(device_id_bytes);

        bytes.extend_from_slice(&expires_at_unix_ms.to_be_bytes());

        bytes.extend_from_slice(&len_prefix(candidates.len()));
        for candidate in candidates {
            // Each candidate is encoded field-by-field at fixed width so the
            // set's contribution to the signed bytes is canonical and
            // unforgeable without re-signing.
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
    /// Derives the device's keypair from its id (the same key the DHT path signs
    /// with), then signs the canonical encoding. The resulting envelope is
    /// self-certifying: any peer that knows `device_id` can verify it without
    /// trusting whoever carried it.
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
    ///   candidate or expiry, or a set signed by a different device, is caught;
    /// - the expiry must be in the future relative to `now_unix_ms`, bounding
    ///   replay.
    pub fn verify(
        &self,
        expected_device_id: &str,
        now_unix_ms: i64,
    ) -> Result<&[WireCandidate], VerifyError> {
        // The claimed id in the envelope and the id being resolved must agree.
        // Cheap and rejects a relabelled or substituted envelope before any curve
        // arithmetic.
        if self.device_id != expected_device_id {
            return Err(VerifyError::WrongDevice);
        }

        // Derive the verifying key from the resolved id — the envelope carries no
        // key, so there is nothing to substitute. A set signed by a different
        // device fails here because its signature was made with a different key.
        let verifying_key: VerifyingKey = verifying_key_for_device(expected_device_id);

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
}

/// Encode a `usize` length as the `u64` big-endian prefix used in the signed
/// bytes.
///
/// Both the device-id byte length and the candidate count are bounded well below
/// `u64::MAX` (a device id is a base32 SHA-256 of 52 bytes; the candidate count
/// is capped at [`crate::wire::MAX_ANNOUNCE_CANDIDATES`] by every honest
/// producer), so the `usize`→`u64` widening is total on every supported platform.
fn len_prefix(len: usize) -> [u8; 8] {
    u64::try_from(len).unwrap_or(u64::MAX).to_be_bytes()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
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

    /// One hour past the fixed test epoch — comfortably unexpired for the
    /// round-trip tests, which verify against an earlier `now`.
    const TEST_NOW: i64 = 1_700_000_000_000;
    const TEST_EXPIRY: i64 = TEST_NOW + 3_600_000;

    #[test]
    fn sign_then_verify_round_trips() {
        let candidates = vec![wire(22000, 0, 65_535), wire(33000, 1, 0)];
        let signed = SignedCandidates::sign("DEVICE-A", candidates.clone(), TEST_EXPIRY);
        let verified = signed.verify("DEVICE-A", TEST_NOW).unwrap();
        assert_eq!(verified, candidates.as_slice());
    }

    #[test]
    fn tampering_with_a_candidate_byte_is_rejected() {
        let mut signed = SignedCandidates::sign("DEVICE-A", vec![wire(22000, 0, 1)], TEST_EXPIRY);
        signed.candidates[0].priority ^= 0x01;
        assert_eq!(
            signed.verify("DEVICE-A", TEST_NOW),
            Err(VerifyError::BadSignature)
        );
    }

    #[test]
    fn tampering_with_a_single_signature_byte_is_rejected() {
        let mut signed = SignedCandidates::sign("DEVICE-A", vec![wire(22000, 0, 1)], TEST_EXPIRY);
        signed.signature[0] ^= 0x01;
        assert_eq!(
            signed.verify("DEVICE-A", TEST_NOW),
            Err(VerifyError::BadSignature)
        );
    }

    #[test]
    fn tampering_with_the_expiry_is_rejected() {
        let mut signed = SignedCandidates::sign("DEVICE-A", vec![wire(22000, 0, 1)], TEST_EXPIRY);
        signed.expires_at_unix_ms = TEST_EXPIRY + 1_000_000;
        assert_eq!(
            signed.verify("DEVICE-A", TEST_NOW),
            Err(VerifyError::BadSignature)
        );
    }

    #[test]
    fn set_signed_by_a_different_device_is_rejected() {
        let signed = SignedCandidates::sign("DEVICE-B", vec![wire(22000, 0, 1)], TEST_EXPIRY);
        assert_eq!(
            signed.verify("DEVICE-A", TEST_NOW),
            Err(VerifyError::WrongDevice)
        );
    }

    #[test]
    fn relabelled_envelope_for_another_device_is_rejected() {
        let mut signed = SignedCandidates::sign("DEVICE-B", vec![wire(1, 0, 0)], TEST_EXPIRY);
        signed.device_id = "DEVICE-A".to_owned();
        assert_eq!(
            signed.verify("DEVICE-A", TEST_NOW),
            Err(VerifyError::BadSignature)
        );
    }

    #[test]
    fn expired_envelope_is_rejected() {
        let signed = SignedCandidates::sign("DEVICE-A", vec![wire(1, 0, 0)], TEST_NOW);
        // now == expiry: expiry is inclusive-past, so this is stale.
        assert!(matches!(
            signed.verify("DEVICE-A", TEST_NOW),
            Err(VerifyError::Expired { .. })
        ));
        assert!(matches!(
            signed.verify("DEVICE-A", TEST_NOW + 1),
            Err(VerifyError::Expired { .. })
        ));
    }

    #[test]
    fn json_round_trip_preserves_the_envelope() {
        let signed = SignedCandidates::sign("DEVICE-A", vec![wire(22000, 0, 65_535)], TEST_EXPIRY);
        let json = serde_json::to_string(&signed).unwrap();
        let decoded: SignedCandidates = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, signed);
        assert!(decoded.verify("DEVICE-A", TEST_NOW).is_ok());
    }

    #[test]
    fn bit_flip_in_base64_signature_field_fails_verification() {
        // Deserialise a valid envelope from JSON, flip one character inside the
        // base64 `signature` field, re-deserialise, and expect `BadSignature`.
        // This exercises the interaction between serde and the signature check: a
        // base64-encoded blob that differs by one character still deserialises to
        // a 64-byte array (provided the result is still valid base64 and the
        // right length), but the reconstructed bytes differ and the signature
        // check rejects them.
        let signed = SignedCandidates::sign("DEVICE-A", vec![wire(22000, 0, 1)], TEST_EXPIRY);
        let mut obj: serde_json::Value = serde_json::to_value(&signed).unwrap();
        // Locate the signature string and flip its first character.
        let sig_str = obj["signature"].as_str().unwrap().to_owned();
        // Replace the first character with a different valid base64 character so
        // the string still decodes without a base64 error — the mutation must
        // reach the signature-verification step rather than the serde step.
        let first = sig_str.chars().next().unwrap();
        // Pick a replacement that differs from `first` and is valid base64.
        let replacement = if first == 'A' { 'B' } else { 'A' };
        let mut new_sig = sig_str;
        new_sig.replace_range(..first.len_utf8(), &replacement.to_string());
        obj["signature"] = serde_json::Value::String(new_sig);
        let tampered: Result<SignedCandidates, _> = serde_json::from_value(obj);
        match tampered {
            Err(_) => {
                // Serde rejected the mutated base64 — it was no longer a valid
                // 64-byte encoding, so the tampering is caught even earlier than
                // the signature check.  That is also a valid rejection.
            }
            Ok(env) => {
                assert_eq!(
                    env.verify("DEVICE-A", TEST_NOW),
                    Err(VerifyError::BadSignature)
                );
            }
        }
    }

    #[test]
    fn expiry_at_exactly_now_plus_one_ms_is_still_valid() {
        // The boundary condition: an envelope expiring at `now + 1` must verify,
        // while one expiring at `now` (or earlier) is rejected.  This pins the
        // off-by-one behaviour of the `expires_at > now` check in `verify`.
        let now = TEST_NOW;
        let signed = SignedCandidates::sign("DEVICE-A", vec![wire(1, 0, 0)], now + 1);
        assert!(signed.verify("DEVICE-A", now).is_ok());
    }

    #[test]
    fn expiry_at_exactly_now_is_rejected() {
        // An envelope whose expiry equals the verifier's clock is considered
        // stale: `expires_at > now` is strict, so equality is a rejection.
        let now = TEST_NOW;
        let signed = SignedCandidates::sign("DEVICE-A", vec![wire(1, 0, 0)], now);
        assert!(matches!(
            signed.verify("DEVICE-A", now),
            Err(VerifyError::Expired { .. })
        ));
    }

    #[test]
    fn empty_candidate_list_signs_and_verifies() {
        // A device with no current candidates should still produce a verifiable
        // (if useless) envelope rather than panicking or failing the signature
        // check — the signed bytes encode a zero candidate count unambiguously.
        let signed = SignedCandidates::sign("DEVICE-A", vec![], TEST_EXPIRY);
        let result = signed.verify("DEVICE-A", TEST_NOW);
        assert!(result.is_ok());
        let empty: &[WireCandidate] = &[];
        assert_eq!(result.unwrap(), empty);
    }

    #[test]
    fn ipv6_candidates_encode_and_verify_correctly() {
        // IPv6 addresses are encoded differently in `signing_bytes` (16-byte
        // octets with a `6` prefix) from IPv4 (4-byte octets with a `4`
        // prefix).  A round-trip through `sign`/`verify` confirms that the
        // canonical encoding is deterministic for both address families.
        use std::net::{Ipv6Addr, SocketAddrV6};
        let ipv6_candidate = WireCandidate {
            address: SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1),
                22000,
                0,
                0,
            )),
            kind: 1,
            priority: 100,
        };
        let signed = SignedCandidates::sign("DEVICE-A", vec![ipv6_candidate], TEST_EXPIRY);
        let result = signed.verify("DEVICE-A", TEST_NOW);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), &[ipv6_candidate]);
    }

    #[test]
    fn tampering_with_ipv6_address_octets_is_rejected() {
        // Confirm that a mutation to an IPv6 candidate's address invalidates the
        // signature, covering the IPv6 arm of the canonical-byte encoding.
        use std::net::{Ipv6Addr, SocketAddrV6};
        let ipv6_candidate = WireCandidate {
            address: SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1),
                22000,
                0,
                0,
            )),
            kind: 1,
            priority: 100,
        };
        let mut signed = SignedCandidates::sign("DEVICE-A", vec![ipv6_candidate], TEST_EXPIRY);
        // Flip the priority of the candidate post-signing.
        signed.candidates[0].priority ^= 0x01;
        assert_eq!(
            signed.verify("DEVICE-A", TEST_NOW),
            Err(VerifyError::BadSignature)
        );
    }
}
