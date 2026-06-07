//! Portable WebRTC configuration and error types.
//!
//! [`WebRtcConfig`] and [`WebRtcError`] are the crate's portable surface:
//! they compile and are testable on native targets. The `#[serde(rename_all =
//! "camelCase")]` annotation ensures the wire form matches the TypeScript
//! interface at `apps/web/src/wasm/webrtc.ts:14-18`.

use serde::{Deserialize, Serialize};

/// A failure crossing the WebRTC bridge.
#[derive(Debug, thiserror::Error)]
pub enum WebRtcError {
    /// A JavaScript-side call rejected or threw. The message carries the JS
    /// error text where one was available.
    #[error("WebRTC bridge call failed: {0}")]
    Js(String),
}

/// Configuration for establishing a peer connection.
///
/// The role is implied by `session_id`: `None` makes this peer the initiator
/// (it generates a session id and sends the SDP offer); `Some` makes it the
/// responder for an existing session.
///
/// The serde form is camelCase to match the TypeScript `WebRtcConfig` interface
/// at `apps/web/src/wasm/webrtc.ts`. `session_id` is omitted when `None` so
/// the JS side can distinguish initiator (`config.sessionId === undefined`) from
/// responder (`config.sessionId !== undefined`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebRtcConfig {
    /// WebSocket URL of the relay used for signalling.
    pub relay_url: String,
    /// STUN server URLs for ICE candidate gathering.
    pub stun_servers: Vec<String>,
    /// The rendezvous session id, or `None` to act as the initiator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

impl WebRtcConfig {
    /// Serialise the config to the canonical wire form consumed by the
    /// JavaScript bridge.
    ///
    /// This is the single portable definition of the boundary shape; the wasm
    /// shim turns the resulting `Value` into a `JsValue` via
    /// `serde_json::to_string` + `js_sys::JSON::parse`.
    ///
    /// # Errors
    ///
    /// Returns `WebRtcError::Js` if serialisation fails (in practice this is
    /// unreachable for this struct, but the workspace denies `expect_used` in
    /// non-test code).
    pub fn to_wire_json(&self) -> Result<serde_json::Value, WebRtcError> {
        serde_json::to_value(self).map_err(|error| WebRtcError::Js(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::{WebRtcConfig, WebRtcError};

    fn initiator_config() -> WebRtcConfig {
        WebRtcConfig {
            relay_url: "wss://relay.example/ws".into(),
            stun_servers: vec!["stun:stun.l.google.com:19302".into()],
            session_id: None,
        }
    }

    fn responder_config() -> WebRtcConfig {
        WebRtcConfig {
            relay_url: "wss://relay.example/ws".into(),
            stun_servers: vec!["stun:stun.l.google.com:19302".into()],
            session_id: Some("abc-123".into()),
        }
    }

    /// Initiator config omits `sessionId` entirely (not null) so that
    /// `config.sessionId === undefined` in JS correctly selects the initiator
    /// role. (webrtc.ts:62 branches on this.)
    #[test]
    fn wire_shape_initiator_omits_session_id() {
        let value = initiator_config().to_wire_json().unwrap();
        assert_eq!(
            value.get("relayUrl").unwrap().as_str().unwrap(),
            "wss://relay.example/ws"
        );
        assert!(
            value.get("sessionId").is_none(),
            "initiator config must not emit a sessionId key"
        );
    }

    /// Responder config must include `sessionId` so the JS bridge can route
    /// this peer to the correct in-progress session.
    #[test]
    fn wire_shape_responder_includes_session_id() {
        let value = responder_config().to_wire_json().unwrap();
        assert_eq!(value.get("sessionId").unwrap().as_str().unwrap(), "abc-123");
        assert!(value.get("relayUrl").is_some());
        assert!(value.get("stunServers").is_some());
    }

    /// The wire keys must be camelCase (`relayUrl`, `stunServers`) to match the
    /// TypeScript `WebRtcConfig` interface. Guards the `#[serde(rename_all =
    /// "camelCase")]` annotation.
    #[test]
    fn wire_shape_uses_camel_case_keys() {
        let value = initiator_config().to_wire_json().unwrap();
        assert!(
            value.get("relayUrl").is_some(),
            "expected camelCase key 'relayUrl'"
        );
        assert!(
            value.get("stunServers").is_some(),
            "expected camelCase key 'stunServers'"
        );
        assert!(
            value.get("relay_url").is_none(),
            "snake_case key 'relay_url' must not appear in wire form"
        );
        assert!(
            value.get("stun_servers").is_none(),
            "snake_case key 'stun_servers' must not appear in wire form"
        );
    }

    /// An empty STUN server list must serialise to `[]`, not be omitted, because
    /// `config.stunServers.map(...)` at webrtc.ts:60 requires an array to be
    /// present.
    #[test]
    fn wire_shape_empty_stun_servers_is_empty_array() {
        let config = WebRtcConfig {
            relay_url: "wss://relay.example/ws".into(),
            stun_servers: vec![],
            session_id: None,
        };
        let value = config.to_wire_json().unwrap();
        let stun = value
            .get("stunServers")
            .expect("stunServers must be present");
        assert!(
            stun.as_array().unwrap().is_empty(),
            "empty stunServers must serialise to []"
        );
    }

    /// The wire form must faithfully round-trip: deserialising it back must
    /// produce the original `WebRtcConfig`.
    #[test]
    fn config_round_trips_through_serde() {
        for config in [initiator_config(), responder_config()] {
            let wire = config.to_wire_json().unwrap();
            let recovered: WebRtcConfig = serde_json::from_value(wire).unwrap();
            assert_eq!(config, recovered);
        }
    }

    /// The `#[error(...)]` format string on `WebRtcError::Js` must include the
    /// message text so callers surfacing the error to JavaScript see a
    /// human-readable string.
    #[test]
    fn error_display_carries_message() {
        let error = WebRtcError::Js("reflect rejected".into());
        assert_eq!(
            error.to_string(),
            "WebRTC bridge call failed: reflect rejected"
        );
    }
}
