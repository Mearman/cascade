//! Audit-log route — reads the same `manage_audit` table the BEP dispatcher
//! writes to.

use axum::Json;
use axum::extract::{Query, State};
use axum::routing::get;
use axum::Router;
use cascade_engine::manage::{Capability, Scope};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::auth::Session;
use crate::error::ApiError;
use crate::routes::{decode_cursor, encode_cursor};
use crate::schemas::audit::{AuditEntry, AuditResponse};
use crate::schemas::common::{DEFAULT_PAGE_LIMIT, MAX_PAGE_LIMIT, MIN_PAGE_LIMIT};
use crate::state::AppState;

/// Register the audit route.
pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/audit", get(list))
}

/// Query parameters for `GET /v1/audit`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
struct AuditQuery {
    since: Option<String>,
    limit: Option<usize>,
    cursor: Option<String>,
}

/// `GET /v1/audit` — capability: `status:read`.
async fn list(
    State(state): State<AppState>,
    session: Session,
    Query(query): Query<AuditQuery>,
) -> Result<Json<AuditResponse>, ApiError> {
    session.require(&state, Capability::StatusRead, &Scope::Node)?;

    let since = query
        .since
        .as_deref()
        .map(|raw| {
            DateTime::parse_from_rfc3339(raw)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|_| ApiError::unprocessable(format!("`since` is not an RFC 3339 timestamp: {raw}")))
        })
        .transpose()?;
    let after_id = query.cursor.as_deref().map(decode_cursor).transpose()?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_PAGE_LIMIT)
        .clamp(MIN_PAGE_LIMIT, MAX_PAGE_LIMIT);

    let records = state
        .engine
        .db()
        .list_audit()
        .map_err(|e| ApiError::internal(format!("could not read audit log: {e}")))?;

    let mut filtered: Vec<_> = records
        .into_iter()
        .filter(|record| after_id.is_none_or(|id| record.id > id))
        .filter(|record| since.is_none_or(|ts| record.entry.timestamp >= ts))
        .collect();
    filtered.sort_by_key(|record| record.id);

    let has_more = filtered.len() > limit;
    let page = filtered.into_iter().take(limit);
    let entries: Vec<AuditEntry> = page
        .map(|record| AuditEntry {
            id: record.id,
            timestamp: record.entry.timestamp,
            actor_device: record.entry.actor_device.as_str().to_owned(),
            capability: record.entry.capability,
            scope: record.entry.scope,
            command: record.entry.command,
            outcome: record.entry.outcome,
            // The audit table predates the contract's request_id column; HTTP
            // mutations embed their request id in the command text instead.
            request_id: None,
        })
        .collect();

    let next_cursor = if has_more {
        entries.last().map(|entry| encode_cursor(entry.id))
    } else {
        None
    };

    Ok(Json(AuditResponse {
        entries,
        next_cursor,
    }))
}
