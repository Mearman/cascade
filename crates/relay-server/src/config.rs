//! Relay server configuration.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};

/// Length of the shared secret in bytes (256-bit `HMAC` key).
pub const SHARED_SECRET_LEN: usize = 32;

/// Runtime configuration for a relay server instance.
#[derive(Debug, Clone, Copy)]
pub struct RelayConfig {
    /// Address the byte-pipe listener binds to.
    pub bind: SocketAddr,
    /// 32-byte shared secret used to verify the `HMAC` handshake.
    pub shared_secret: [u8; SHARED_SECRET_LEN],
    /// How long the first peer of a session may wait before the server
    /// times it out and disconnects.
    pub session_timeout: Duration,
    /// Maximum number of in-flight sessions (paired or parked).
    pub max_sessions: u32,
    /// Optional address for the `/metrics` `HTTP` endpoint.
    pub metrics_bind: Option<SocketAddr>,
}

impl RelayConfig {
    /// Decode a 64-character hexadecimal shared secret.
    pub fn parse_shared_secret(hex_secret: &str) -> Result<[u8; SHARED_SECRET_LEN]> {
        let bytes = hex::decode(hex_secret).context("shared secret must be valid hexadecimal")?;
        if bytes.len() != SHARED_SECRET_LEN {
            return Err(anyhow!(
                "shared secret must be {SHARED_SECRET_LEN} bytes ({} hex chars), got {} bytes",
                SHARED_SECRET_LEN * 2,
                bytes.len()
            ));
        }
        let mut secret = [0u8; SHARED_SECRET_LEN];
        secret.copy_from_slice(&bytes);
        Ok(secret)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_hex_secret() {
        let hex_secret = "0".repeat(64);
        let secret = RelayConfig::parse_shared_secret(&hex_secret).unwrap();
        assert_eq!(secret, [0u8; SHARED_SECRET_LEN]);
    }

    #[test]
    fn rejects_wrong_length_secret() {
        let too_short = "ab".repeat(31);
        assert!(RelayConfig::parse_shared_secret(&too_short).is_err());
        let too_long = "ab".repeat(33);
        assert!(RelayConfig::parse_shared_secret(&too_long).is_err());
    }

    #[test]
    fn rejects_non_hex_secret() {
        let not_hex = "z".repeat(64);
        assert!(RelayConfig::parse_shared_secret(&not_hex).is_err());
    }
}
