//! Shared `HMAC-SHA256` client-authentication primitive.
//!
//! Two cascade rendezvous surfaces authenticate writers with a shared secret and
//! an `HMAC-SHA256` tag: the relay-server's byte-pipe handshake (which binds
//! `device_id || session_id`) and the announce directory's write path (which
//! binds `device_id || body`). Both compute and verify the tag the same way, in
//! constant time, against the same 256-bit secret. That common machinery lives
//! here — [`compute_tag`] and [`verify_tag`] take an ordered list of byte
//! segments and key the MAC with the shared secret — so neither surface
//! re-implements the HMAC, and both stay byte-compatible.
//!
//! The announce-directory write authentication is defined on top of that
//! primitive by [`announce_write_tag`] and [`verify_announce_write`]: the client
//! sends the tag in the [`ANNOUNCE_AUTH_HEADER`] header alongside the
//! `POST /announce/<device_id>` request, computed over the device id and the
//! exact request body. The server recomputes the tag over the path's device id
//! and the body it received and rejects the write unless they match in constant
//! time. Authenticating the writer does not require trusting the stored blob: the
//! [`crate::signing::SignedCandidates`] envelope is self-certifying on read. The
//! HMAC simply gates *who* may write to the soft-state directory, and binding the
//! body into the tag stops a man-in-the-middle swapping the stored blob for a
//! different (even validly-signed) one in flight.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use thiserror::Error;

/// Length of the shared secret in bytes (256-bit `HMAC` key).
pub const SHARED_SECRET_LEN: usize = 32;

/// `HMAC-SHA256` tag length in bytes.
pub const HMAC_TAG_LEN: usize = 32;

/// HTTP header carrying the hex-encoded announce write-authentication tag.
///
/// The client sets this on `POST /announce/<device_id>`; the server reads it,
/// recomputes the tag over the path device id and the request body, and rejects
/// the write on mismatch.
pub const ANNOUNCE_AUTH_HEADER: &str = "x-cascade-announce-auth";

type HmacSha256 = Hmac<Sha256>;

/// Reasons computing or verifying a tag may fail.
#[derive(Debug, Clone, Copy, Error)]
pub enum AuthError {
    /// The HMAC could not be initialised from the shared secret. `Hmac` accepts
    /// a key of any length, so for a fixed-width secret this is unreachable in
    /// practice; it is surfaced rather than unwrapped so the verify/compute path
    /// never panics.
    #[error("HMAC initialisation failed")]
    HmacInit,

    /// The supplied authentication header was not valid hexadecimal or was the
    /// wrong length for an [`HMAC_TAG_LEN`]-byte tag.
    #[error("malformed authentication tag")]
    MalformedTag,
}

fn new_mac(secret: &[u8; SHARED_SECRET_LEN]) -> Result<HmacSha256, AuthError> {
    HmacSha256::new_from_slice(secret).map_err(|_| AuthError::HmacInit)
}

/// Compute the `HMAC-SHA256` tag over `segments`, keyed by `secret`.
///
/// The segments are fed to the MAC in order; the caller chooses what they bind
/// (the relay handshake binds `device_id` then `session_id`; the announce write
/// binds `device_id` then the request body). Returning the fixed-width tag keeps
/// callers from re-deriving the length.
pub fn compute_tag(
    secret: &[u8; SHARED_SECRET_LEN],
    segments: &[&[u8]],
) -> Result<[u8; HMAC_TAG_LEN], AuthError> {
    let mut mac = new_mac(secret)?;
    for segment in segments {
        mac.update(segment);
    }
    let bytes = mac.finalize().into_bytes();
    let mut out = [0u8; HMAC_TAG_LEN];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// Verify `candidate` against the tag computed over `segments`, in constant time.
///
/// Uses [`hmac::Mac::verify_slice`], which compares in constant time and so does
/// not leak how many leading bytes matched. Returns `Ok(true)` only on an exact
/// match; a wrong-length candidate, a wrong secret, or any other mismatch yields
/// `Ok(false)`. The only `Err` is an HMAC initialisation failure.
pub fn verify_tag(
    secret: &[u8; SHARED_SECRET_LEN],
    segments: &[&[u8]],
    candidate: &[u8],
) -> Result<bool, AuthError> {
    let mut mac = new_mac(secret)?;
    for segment in segments {
        mac.update(segment);
    }
    Ok(mac.verify_slice(candidate).is_ok())
}

/// Decode a 64-character hexadecimal shared secret.
///
/// The secret is exchanged out of band as hex (matching the relay-server's
/// config); decoding rejects a wrong-length or non-hex value loudly rather than
/// silently truncating or padding.
pub fn parse_shared_secret_hex(hex_secret: &str) -> Result<[u8; SHARED_SECRET_LEN], AuthError> {
    let bytes = decode_hex(hex_secret).ok_or(AuthError::MalformedTag)?;
    <[u8; SHARED_SECRET_LEN]>::try_from(bytes.as_slice()).map_err(|_| AuthError::MalformedTag)
}

/// Compute the announce write-authentication tag binding `device_id` and `body`.
///
/// The client sets the hex of this on [`ANNOUNCE_AUTH_HEADER`] when posting a
/// registration. Binding the body (not just the id) means a man-in-the-middle
/// cannot swap the stored blob for a different one without the recomputed tag
/// failing.
pub fn announce_write_tag(
    secret: &[u8; SHARED_SECRET_LEN],
    device_id: &str,
    body: &[u8],
) -> Result<[u8; HMAC_TAG_LEN], AuthError> {
    compute_tag(secret, &[device_id.as_bytes(), body])
}

/// Verify a hex-encoded announce write tag against `device_id` and `body`.
///
/// `header_value` is the raw [`ANNOUNCE_AUTH_HEADER`] value as received. It is
/// decoded from hex (a malformed value is rejected, not silently treated as a
/// mismatch, so the caller can distinguish a broken client from a wrong secret),
/// then compared in constant time against the tag recomputed over the path device
/// id and the received body.
pub fn verify_announce_write(
    secret: &[u8; SHARED_SECRET_LEN],
    device_id: &str,
    body: &[u8],
    header_value: &str,
) -> Result<bool, AuthError> {
    let candidate = decode_hex(header_value).ok_or(AuthError::MalformedTag)?;
    if candidate.len() != HMAC_TAG_LEN {
        return Err(AuthError::MalformedTag);
    }
    verify_tag(secret, &[device_id.as_bytes(), body], &candidate)
}

/// Lower-case hex encoding of a tag, for setting [`ANNOUNCE_AUTH_HEADER`].
#[must_use]
pub fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // `{:02x}` formats a single byte as two lower-case hex digits; the
        // width and radix are fixed, so the output length is always `2 * len`.
        out.push(nibble_hex(byte >> 4));
        out.push(nibble_hex(byte & 0x0f));
    }
    out
}

/// Map a 4-bit nibble (0..=15) to its lower-case hex digit.
const fn nibble_hex(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        // The caller only ever passes a masked or shifted nibble, so every value
        // here is 10..=15; the arm is total over the nibble domain.
        _ => (b'a' + (nibble - 10)) as char,
    }
}

/// Decode a lower- or upper-case hex string to bytes, or `None` if it is not
/// valid hex (odd length or a non-hex digit).
fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut idx = 0;
    while idx < bytes.len() {
        let hi = hex_value(*bytes.get(idx)?)?;
        let lo = hex_value(*bytes.get(idx + 1)?)?;
        out.push((hi << 4) | lo);
        idx += 2;
    }
    Some(out)
}

/// Map a single ASCII hex digit to its 0..=15 value, or `None` if it is not a
/// hex digit.
const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn secret() -> [u8; SHARED_SECRET_LEN] {
        let mut s = [0u8; SHARED_SECRET_LEN];
        for (idx, byte) in s.iter_mut().enumerate() {
            *byte = u8::try_from(idx).unwrap_or(0);
        }
        s
    }

    #[test]
    fn announce_write_tag_round_trips() {
        let body = br#"{"signed":{}}"#;
        let tag = announce_write_tag(&secret(), "DEVICE-A", body).unwrap();
        let header = encode_hex(&tag);
        assert!(verify_announce_write(&secret(), "DEVICE-A", body, &header).unwrap());
    }

    #[test]
    fn wrong_secret_fails_verification() {
        let body = br#"{"signed":{}}"#;
        let tag = announce_write_tag(&secret(), "DEVICE-A", body).unwrap();
        let header = encode_hex(&tag);
        let mut other = secret();
        other[0] ^= 0xFF;
        assert!(!verify_announce_write(&other, "DEVICE-A", body, &header).unwrap());
    }

    #[test]
    fn tampered_body_fails_verification() {
        let body = br#"{"signed":{"device_id":"DEVICE-A"}}"#;
        let tag = announce_write_tag(&secret(), "DEVICE-A", body).unwrap();
        let header = encode_hex(&tag);
        let tampered = br#"{"signed":{"device_id":"DEVICE-B"}}"#;
        assert!(!verify_announce_write(&secret(), "DEVICE-A", tampered, &header).unwrap());
    }

    #[test]
    fn wrong_device_id_fails_verification() {
        let body = br#"{"signed":{}}"#;
        let tag = announce_write_tag(&secret(), "DEVICE-A", body).unwrap();
        let header = encode_hex(&tag);
        assert!(!verify_announce_write(&secret(), "DEVICE-B", body, &header).unwrap());
    }

    #[test]
    fn malformed_header_is_an_error_not_a_silent_mismatch() {
        let body = br"{}";
        // Odd length.
        assert!(matches!(
            verify_announce_write(&secret(), "DEVICE-A", body, "abc"),
            Err(AuthError::MalformedTag)
        ));
        // Non-hex.
        assert!(matches!(
            verify_announce_write(&secret(), "DEVICE-A", body, &"z".repeat(HMAC_TAG_LEN * 2)),
            Err(AuthError::MalformedTag)
        ));
        // Right hex, wrong length.
        assert!(matches!(
            verify_announce_write(&secret(), "DEVICE-A", body, "00"),
            Err(AuthError::MalformedTag)
        ));
    }

    #[test]
    fn compute_tag_segments_are_order_sensitive() {
        let a = compute_tag(&secret(), &[b"foo", b"bar"]).unwrap();
        let b = compute_tag(&secret(), &[b"foobar"]).unwrap();
        // Concatenation is the same to the MAC, so these agree by construction —
        // the segment list is purely a convenience for the caller.
        assert_eq!(a, b);
        let c = compute_tag(&secret(), &[b"bar", b"foo"]).unwrap();
        assert_ne!(a, c);
    }

    #[test]
    fn parse_shared_secret_hex_round_trips() {
        let hex = encode_hex(&secret());
        assert_eq!(parse_shared_secret_hex(&hex).unwrap(), secret());
    }

    #[test]
    fn parse_shared_secret_hex_rejects_wrong_length_and_non_hex() {
        assert!(parse_shared_secret_hex(&"ab".repeat(31)).is_err());
        assert!(parse_shared_secret_hex(&"ab".repeat(33)).is_err());
        assert!(parse_shared_secret_hex(&"z".repeat(64)).is_err());
    }

    #[test]
    fn encode_hex_is_lower_case_and_fixed_width() {
        assert_eq!(encode_hex(&[0x00, 0xff, 0x0a, 0xb3]), "00ff0ab3");
    }
}
