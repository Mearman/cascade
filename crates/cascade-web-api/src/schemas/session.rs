//! Session response schema — the denormalised first-load view for the PWA.

use cascade_engine::manage::{Capability, Scope};
use serde::{Deserialize, Serialize};

/// `GET /v1/session` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionResponse {
    /// The verified session identity and class.
    pub session: SessionInfo,
    /// The verified token's claims, denormalised for display.
    pub token: TokenView,
    /// The denormalised abilities the PWA renders on first load. A UI hint
    /// only; the server re-checks every request.
    pub abilities: Abilities,
}

/// The verified session identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionInfo {
    /// The derived class (`owner`, `named_user`, `bearer`).
    pub class: String,
    /// This node's device id.
    pub node_device_id: String,
    /// The verified bearer device id.
    pub verified_bearer: String,
}

/// The verified session token's claims.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TokenView {
    /// The token's stable id.
    pub token_id: String,
    /// The issuing device id.
    pub issuer: String,
    /// The bearer device id.
    pub bearer: String,
    /// The conferred capability (`"status:read"`, …).
    pub capability: Capability,
    /// The scope the capability applies over.
    pub scope: Scope,
    /// When the token expires.
    pub expires: chrono::DateTime<chrono::Utc>,
    /// When the token was issued, when this node knows it (it issued the
    /// token); `null` for a token issued elsewhere.
    pub issued_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// The denormalised ability hints. The `*_manage` booleans are all-or-nothing;
/// `data_read` / `data_write` are arrays of folder prefixes (the canonical BEP
/// folder ids the F1 fix binds grant scope to).
///
/// The many booleans mirror the contract's per-capability ability flags exactly,
/// one field per capability — this is a flat wire schema, not a state machine,
/// so the `struct_excessive_bools` lint does not apply.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::struct_excessive_bools)]
pub struct Abilities {
    /// Whether the session can read status.
    pub status_read: bool,
    /// Whether the session can write pins.
    pub pin_write: bool,
    /// Whether the session can manage the cache.
    pub cache_manage: bool,
    /// Whether the session can push config.
    pub config_push: bool,
    /// Whether the session can set policies.
    pub policy_set: bool,
    /// Whether the session can manage backends.
    pub backend_manage: bool,
    /// Whether the session can control the daemon lifecycle.
    pub lifecycle_control: bool,
    /// Whether the session can administer grants.
    pub grant_admin: bool,
    /// The folder prefixes the session may read.
    pub data_read: Vec<String>,
    /// The folder prefixes the session may write.
    pub data_write: Vec<String>,
}
