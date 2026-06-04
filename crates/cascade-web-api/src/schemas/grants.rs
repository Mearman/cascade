//! Grant schemas — listing and creation.

use cascade_engine::manage::{Capability, Scope};
use serde::{Deserialize, Serialize};

/// One capability-grant row.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct GrantView {
    /// The grant row id.
    pub id: i64,
    /// The device the grant authorises.
    pub grantee: String,
    /// The device that issued the grant.
    pub granted_by: String,
    /// The conferred capability.
    pub capability: Capability,
    /// The scope the capability applies over.
    pub scope: Scope,
    /// When the grant expires, if ever.
    pub expires: Option<chrono::DateTime<chrono::Utc>>,
}

/// `GET /v1/grants` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct GrantsResponse {
    /// Every grant row held on this node.
    pub grants: Vec<GrantView>,
}

/// `POST /v1/grants` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct GrantPost {
    /// The device to authorise.
    pub grantee: String,
    /// The capability to confer.
    pub capability: Capability,
    /// The scope to confer it over. A folder-scoped data verb's path is an
    /// operator-facing folder name, resolved to its canonical BEP id.
    pub scope: Scope,
    /// When the grant should expire, if ever.
    #[serde(default)]
    pub expires: Option<chrono::DateTime<chrono::Utc>>,
}
