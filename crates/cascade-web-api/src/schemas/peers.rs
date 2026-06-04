//! Peer schemas.

use serde::{Deserialize, Serialize};

/// A per-folder directional grant or explicit-control state for a peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FolderDirection {
    /// The canonical BEP folder id.
    pub folder: String,
    /// Whether `data:read` applies for this folder.
    pub data_read: bool,
    /// Whether `data:write` applies for this folder.
    pub data_write: bool,
}

/// One peer, with its data-verb grants (F1) and explicit-control bits (F2).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PeerView {
    /// The peer's device id.
    pub device_id: String,
    /// The peer's friendly name, when known.
    pub name: Option<String>,
    /// Whether the peer is currently online.
    pub online: bool,
    /// When the peer was last seen, when known.
    pub last_seen: Option<chrono::DateTime<chrono::Utc>>,
    /// The peer's per-folder data-verb grants (the F1 grant columns).
    pub data_verb_grants: Vec<FolderDirection>,
    /// The folders the peer is pinned into explicit-control mode for (F2).
    pub explicit_control: Vec<FolderDirection>,
}

/// `GET /v1/peers` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PeersResponse {
    /// Every known peer.
    pub peers: Vec<PeerView>,
}
