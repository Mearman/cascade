//! `HMAC` handshake verification.
//!
//! The first binary `WebSocket` frame from each client carries the device
//! identifier, the session identifier (echoed for binding), and an
//! `HMAC-SHA256` tag over `device_id || session_id` keyed by the shared
//! secret. The server verifies the tag in constant time and admits the
//! client into the rendezvous pool only on success.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use thiserror::Error;

use crate::config::SHARED_SECRET_LEN;

/// Wire-format version for the handshake frame. Bumped on incompatible
/// changes; current servers reject any other value.
pub const HANDSHAKE_VERSION: u8 = 1;

/// `HMAC-SHA256` tag length in bytes.
pub const HMAC_TAG_LEN: usize = 32;

/// Maximum permitted length of a device identifier inside the handshake.
///
/// The Cascade device ID is a base32-encoded `SHA-256` (52 chars / 32 bytes),
/// so 128 bytes is generous without inviting megabyte-sized "device IDs".
pub const MAX_DEVICE_ID_LEN: usize = 128;

/// Maximum permitted length of the echoed session ID. The path component on
/// the `WebSocket` URL imposes a similar bound but we cap it explicitly here
/// to reject pathological inputs early.
pub const MAX_SESSION_ID_LEN: usize = 256;

/// A successfully parsed and verified handshake frame.
#[derive(Debug, Clone)]
pub struct VerifiedHandshake {
    /// Device identifier the client claims, as provided in the handshake.
    pub device_id: String,
    /// Session ID echoed by the client. Must match the URL path component.
    pub session_id: String,
}

/// Reasons an inbound handshake frame may be rejected.
#[derive(Debug, Clone, Copy, Error)]
pub enum HandshakeError {
    #[error("handshake frame truncated: expected at least {expected} bytes, got {actual}")]
    Truncated { expected: usize, actual: usize },

    #[error("unsupported handshake version: {0}")]
    UnsupportedVersion(u8),

    #[error("device identifier too long: {0} bytes (max {MAX_DEVICE_ID_LEN})")]
    DeviceIdTooLong(usize),

    #[error("session identifier too long: {0} bytes (max {MAX_SESSION_ID_LEN})")]
    SessionIdTooLong(usize),

    #[error("device identifier is not valid UTF-8")]
    DeviceIdNotUtf8,

    #[error("session identifier is not valid UTF-8")]
    SessionIdNotUtf8,

    #[error("session identifier in handshake does not match URL path")]
    SessionIdMismatch,

    #[error("HMAC tag verification failed")]
    BadHmac,

    #[error("HMAC initialisation failed")]
    HmacInit,
}

/// Build a handshake frame (used by tests and any reference client).
///
/// Layout:
/// ```text
/// | version (u8)       = HANDSHAKE_VERSION
/// | device_id_len (u16 BE)
/// | device_id bytes
/// | session_id_len (u16 BE)
/// | session_id bytes
/// | hmac (HMAC_TAG_LEN bytes)
/// ```
pub fn encode_handshake(
    device_id: &str,
    session_id: &str,
    shared_secret: &[u8; SHARED_SECRET_LEN],
) -> Result<Vec<u8>, HandshakeError> {
    let device_id_len = u16::try_from(device_id.len())
        .map_err(|_| HandshakeError::DeviceIdTooLong(device_id.len()))?;
    if usize::from(device_id_len) > MAX_DEVICE_ID_LEN {
        return Err(HandshakeError::DeviceIdTooLong(device_id.len()));
    }
    let session_id_len = u16::try_from(session_id.len())
        .map_err(|_| HandshakeError::SessionIdTooLong(session_id.len()))?;
    if usize::from(session_id_len) > MAX_SESSION_ID_LEN {
        return Err(HandshakeError::SessionIdTooLong(session_id.len()));
    }

    let tag = compute_tag(device_id.as_bytes(), session_id.as_bytes(), shared_secret)?;

    let mut frame = Vec::with_capacity(
        1 + 2 + device_id.len() + 2 + session_id.len() + HMAC_TAG_LEN,
    );
    frame.push(HANDSHAKE_VERSION);
    frame.extend_from_slice(&device_id_len.to_be_bytes());
    frame.extend_from_slice(device_id.as_bytes());
    frame.extend_from_slice(&session_id_len.to_be_bytes());
    frame.extend_from_slice(session_id.as_bytes());
    frame.extend_from_slice(&tag);
    Ok(frame)
}

/// Verify an inbound handshake frame.
///
/// `expected_session_id` is the session ID extracted from the `WebSocket`
/// URL path. The handshake's own echo of the session ID must match — this
/// prevents a malicious client from authenticating against one session and
/// being routed into another.
pub fn verify_handshake(
    frame: &[u8],
    expected_session_id: &str,
    shared_secret: &[u8; SHARED_SECRET_LEN],
) -> Result<VerifiedHandshake, HandshakeError> {
    let mut cursor = 0usize;

    let version = read_u8(frame, &mut cursor)?;
    if version != HANDSHAKE_VERSION {
        return Err(HandshakeError::UnsupportedVersion(version));
    }

    let device_id_len = usize::from(read_u16(frame, &mut cursor)?);
    if device_id_len > MAX_DEVICE_ID_LEN {
        return Err(HandshakeError::DeviceIdTooLong(device_id_len));
    }
    let device_id_bytes = read_slice(frame, &mut cursor, device_id_len)?;

    let session_id_len = usize::from(read_u16(frame, &mut cursor)?);
    if session_id_len > MAX_SESSION_ID_LEN {
        return Err(HandshakeError::SessionIdTooLong(session_id_len));
    }
    let session_id_bytes = read_slice(frame, &mut cursor, session_id_len)?;

    let tag_bytes = read_slice(frame, &mut cursor, HMAC_TAG_LEN)?;

    let device_id = std::str::from_utf8(device_id_bytes)
        .map_err(|_| HandshakeError::DeviceIdNotUtf8)?
        .to_owned();
    let session_id = std::str::from_utf8(session_id_bytes)
        .map_err(|_| HandshakeError::SessionIdNotUtf8)?
        .to_owned();

    if session_id != expected_session_id {
        return Err(HandshakeError::SessionIdMismatch);
    }

    if !verify_tag(
        device_id.as_bytes(),
        session_id.as_bytes(),
        tag_bytes,
        shared_secret,
    )? {
        return Err(HandshakeError::BadHmac);
    }

    Ok(VerifiedHandshake {
        device_id,
        session_id,
    })
}

type HmacSha256 = Hmac<Sha256>;

fn new_mac(secret: &[u8; SHARED_SECRET_LEN]) -> Result<HmacSha256, HandshakeError> {
    HmacSha256::new_from_slice(secret).map_err(|_| HandshakeError::HmacInit)
}

fn compute_tag(
    device_id: &[u8],
    session_id: &[u8],
    shared_secret: &[u8; SHARED_SECRET_LEN],
) -> Result<[u8; HMAC_TAG_LEN], HandshakeError> {
    let mut mac = new_mac(shared_secret)?;
    mac.update(device_id);
    mac.update(session_id);
    let bytes = mac.finalize().into_bytes();
    let mut out = [0u8; HMAC_TAG_LEN];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn verify_tag(
    device_id: &[u8],
    session_id: &[u8],
    candidate: &[u8],
    shared_secret: &[u8; SHARED_SECRET_LEN],
) -> Result<bool, HandshakeError> {
    let mut mac = new_mac(shared_secret)?;
    mac.update(device_id);
    mac.update(session_id);
    Ok(mac.verify_slice(candidate).is_ok())
}

fn read_u8(frame: &[u8], cursor: &mut usize) -> Result<u8, HandshakeError> {
    let byte = frame
        .get(*cursor)
        .copied()
        .ok_or(HandshakeError::Truncated {
            expected: *cursor + 1,
            actual: frame.len(),
        })?;
    *cursor += 1;
    Ok(byte)
}

fn read_u16(frame: &[u8], cursor: &mut usize) -> Result<u16, HandshakeError> {
    let end = *cursor + 2;
    let slice = frame.get(*cursor..end).ok_or(HandshakeError::Truncated {
        expected: end,
        actual: frame.len(),
    })?;
    let mut bytes = [0u8; 2];
    bytes.copy_from_slice(slice);
    *cursor = end;
    Ok(u16::from_be_bytes(bytes))
}

fn read_slice<'a>(
    frame: &'a [u8],
    cursor: &mut usize,
    len: usize,
) -> Result<&'a [u8], HandshakeError> {
    let end = *cursor + len;
    let slice = frame.get(*cursor..end).ok_or(HandshakeError::Truncated {
        expected: end,
        actual: frame.len(),
    })?;
    *cursor = end;
    Ok(slice)
}

#[cfg(test)]
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
    fn roundtrip_valid_handshake() {
        let frame = encode_handshake("device-A", "sess-1", &secret()).unwrap();
        let verified = verify_handshake(&frame, "sess-1", &secret()).unwrap();
        assert_eq!(verified.device_id, "device-A");
        assert_eq!(verified.session_id, "sess-1");
    }

    #[test]
    fn rejects_bad_hmac_with_wrong_secret() {
        let frame = encode_handshake("device-A", "sess-1", &secret()).unwrap();
        let mut other = secret();
        if let Some(b) = other.get_mut(0) {
            *b ^= 0xFF;
        }
        let err = verify_handshake(&frame, "sess-1", &other).unwrap_err();
        assert!(matches!(err, HandshakeError::BadHmac));
    }

    #[test]
    fn rejects_session_id_mismatch() {
        let frame = encode_handshake("device-A", "sess-1", &secret()).unwrap();
        let err = verify_handshake(&frame, "sess-2", &secret()).unwrap_err();
        assert!(matches!(err, HandshakeError::SessionIdMismatch));
    }

    #[test]
    fn rejects_truncated_frame() {
        let frame = encode_handshake("device-A", "sess-1", &secret()).unwrap();
        let truncated = frame.get(..frame.len() - 4).unwrap_or(&[]);
        let err = verify_handshake(truncated, "sess-1", &secret()).unwrap_err();
        assert!(matches!(err, HandshakeError::Truncated { .. }));
    }

    #[test]
    fn rejects_unsupported_version() {
        let mut frame = encode_handshake("device-A", "sess-1", &secret()).unwrap();
        if let Some(byte) = frame.get_mut(0) {
            *byte = HANDSHAKE_VERSION.wrapping_add(1);
        }
        let err = verify_handshake(&frame, "sess-1", &secret()).unwrap_err();
        assert!(matches!(err, HandshakeError::UnsupportedVersion(_)));
    }
}
