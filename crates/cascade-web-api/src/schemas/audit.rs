//! Audit-log schemas.

use cascade_engine::manage::{Capability, Scope};
use serde::{Deserialize, Serialize};

/// One audit-log row.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AuditEntry {
    /// The monotonic row id (also the append order).
    pub id: i64,
    /// When the command was processed.
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// The device that issued the command.
    pub actor_device: String,
    /// The capability the command exercised.
    pub capability: Capability,
    /// The scope the command targeted.
    pub scope: Scope,
    /// A short human-readable summary of the command.
    pub command: String,
    /// The outcome (`allowed`, `denied`, `failed`).
    pub outcome: String,
    /// The per-request id, when the row was written by the HTTP layer.
    /// `null` for rows the audit table predates the contract for.
    pub request_id: Option<String>,
}

/// `GET /v1/audit` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AuditResponse {
    /// The audit entries for this page.
    pub entries: Vec<AuditEntry>,
    /// The cursor for the next page, or `null` when exhausted.
    pub next_cursor: Option<String>,
}
