//! Token schemas — listing and issuance.

use cascade_engine::manage::{Capability, Scope};
use serde::{Deserialize, Serialize};

/// One row in the issued-token list.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TokenView {
    /// The token's stable id.
    pub token_id: String,
    /// The issuing device id.
    pub issuer: String,
    /// The bearer device id.
    pub bearer: String,
    /// The conferred capability.
    pub capability: Capability,
    /// The scope the capability applies over.
    pub scope: Scope,
    /// When the token expires.
    pub expires: chrono::DateTime<chrono::Utc>,
    /// When the token was issued.
    pub issued_at: chrono::DateTime<chrono::Utc>,
    /// Whether the token has been revoked.
    pub revoked: bool,
}

/// `GET /v1/tokens` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TokensResponse {
    /// Every token this node has issued.
    pub tokens: Vec<TokenView>,
}

/// `POST /v1/tokens` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TokenPost {
    /// The device the token is issued to.
    pub bearer: String,
    /// The capability to confer.
    pub capability: Capability,
    /// The scope to confer it over.
    pub scope: Scope,
    /// When the token should expire.
    pub expires: chrono::DateTime<chrono::Utc>,
}

/// `POST /v1/tokens/{id}/revoke` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TokenRevokeResponse {
    /// The revoked token id.
    pub token_id: String,
    /// When the revocation took effect.
    pub revoked_at: chrono::DateTime<chrono::Utc>,
}
