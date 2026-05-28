//! Engine ↔ presenter wire protocol.
//!
//! JSON over Unix domain socket, length-prefixed.
//! Each message: 4-byte big-endian length + JSON body.

use serde::{Deserialize, Serialize};

/// A request from a presenter or CLI to the engine.
#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: u32,
    pub method: String,
    pub params: serde_json::Value,
}

/// A response from the engine.
#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub id: u32,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
}

impl Response {
    #[must_use] pub const fn ok(id: u32, result: serde_json::Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: u32, message: impl Into<String>) -> Self {
        Self {
            id,
            result: None,
            error: Some(message.into()),
        }
    }
}

/// Engine daemon status.
#[derive(Debug, Serialize, Deserialize)]
pub struct StatusInfo {
    pub running: bool,
    pub mount_point: Option<String>,
    pub backends: Vec<BackendStatus>,
    pub cache_stats: CacheStats,
}

/// Status of a single backend.
#[derive(Debug, Serialize, Deserialize)]
pub struct BackendStatus {
    pub id: String,
    pub backend_type: String,
    pub display_name: String,
    pub connected: bool,
}

/// Cache usage statistics.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct CacheStats {
    pub total_files: u64,
    pub online_files: u64,
    pub cached_files: u64,
    pub pinned_files: u64,
    pub cache_bytes: u64,
}

/// Encode a message with a 4-byte big-endian length prefix.
pub fn encode_message(msg: &impl Serialize) -> anyhow::Result<Vec<u8>> {
    let json = serde_json::to_vec(msg)?;
    let len = u32::try_from(json.len())
        .map_err(|e| anyhow::anyhow!("message too large to encode: {e}"))?
        .to_be_bytes();
    let mut out = Vec::with_capacity(4 + json.len());
    out.extend_from_slice(&len);
    out.extend_from_slice(&json);
    Ok(out)
}

/// Decode a length-prefixed message from a buffer.
/// Returns (`consumed_bytes`, `decoded_value`) or None if incomplete.
pub fn decode_message<T: for<'de> Deserialize<'de>>(
    buf: &[u8],
) -> anyhow::Result<Option<(usize, T)>> {
    let Some(header) = buf.get(..4) else {
        return Ok(None);
    };
    let b0 = header.first().copied().unwrap_or(0);
    let b1 = header.get(1).copied().unwrap_or(0);
    let b2 = header.get(2).copied().unwrap_or(0);
    let b3 = header.get(3).copied().unwrap_or(0);
    let len = usize::try_from(u32::from_be_bytes([b0, b1, b2, b3]))
        .unwrap_or(usize::MAX);
    let end = 4 + len;
    if buf.len() < end {
        return Ok(None);
    }
    let body = buf
        .get(4..end)
        .ok_or_else(|| anyhow::anyhow!("buffer slice out of range"))?;
    let value: T = serde_json::from_slice(body)?;
    Ok(Some((end, value)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_request() {
        let req = Request {
            id: 42,
            method: "getItem".to_string(),
            params: serde_json::json!({"id": "gdrive:abc123"}),
        };
        let encoded = encode_message(&req).unwrap();
        let (consumed, decoded): (usize, Request) = decode_message(&encoded).unwrap().unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded.id, 42);
        assert_eq!(decoded.method, "getItem");
    }

    #[test]
    fn decode_incomplete_returns_none() {
        let buf = [0u8, 0, 0, 10, 0, 0]; // claims 10 bytes but only 2 present
        let result: Option<(usize, Request)> = decode_message(&buf).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn response_ok_and_error() {
        let ok = Response::ok(1, serde_json::json!({"status": "alive"}));
        assert!(ok.result.is_some());
        assert!(ok.error.is_none());

        let err = Response::error(2, "not found");
        assert!(err.result.is_none());
        assert_eq!(err.error, Some("not found".to_string()));
    }
}
