//! Grant routes — list, create, and revoke capability grants.
//!
//! Create and revoke require `grant:admin` over the scope, enforced through the
//! same [`authorises`](cascade_engine::manage::authorises) path the BEP plane
//! runs (via [`Session::require`]). The F1 canonical-folder rule, the F4
//! node-wide-data-verb bar, and the dangerous-capability bar are all applied
//! before any row is written, and every mutation is audited with the verified
//! bearer as actor and the request id recorded in the command column.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use cascade_engine::db::AuditEntry;
use cascade_engine::manage::{Capability, DeviceId, Grant, Scope};
use chrono::Utc;

use crate::auth::Session;
use crate::error::{ApiError, ErrorCode};
use crate::request_id::RequestId;
use crate::routes::resolve_folder_name;
use crate::schemas::grants::{GrantPost, GrantView, GrantsResponse};
use crate::state::AppState;

/// Register the grant routes.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/grants", get(list).post(create))
        .route("/v1/grants/{id}", axum::routing::delete(remove))
}

/// `GET /v1/grants` — capability: any verified session.
async fn list(
    State(state): State<AppState>,
    _session: Session,
) -> Result<Json<GrantsResponse>, ApiError> {
    let grants = state
        .engine
        .db()
        .list_grants()
        .map_err(|e| ApiError::internal(format!("could not list grants: {e}")))?
        .into_iter()
        .map(|record| GrantView {
            id: record.id,
            grantee: record.grant.grantee.as_str().to_owned(),
            granted_by: record.grant.granted_by.as_str().to_owned(),
            capability: record.grant.capability,
            scope: record.grant.scope,
            expires: record.grant.expires,
        })
        .collect();
    Ok(Json(GrantsResponse { grants }))
}

/// Resolve the stored scope for `POST /v1/grants`, applying the F4 and
/// dangerous-capability bars and the F1 canonical-folder rule.
fn resolve_grant_scope(
    state: &AppState,
    capability: Capability,
    scope: &Scope,
) -> Result<Scope, ApiError> {
    if scope.is_node_wide() {
        if capability.is_data_verb() {
            return Err(ApiError::new(
                ErrorCode::DataVerbNodeWideForbidden,
                format!(
                    "capability `{}` cannot be granted over a node-wide scope; name an explicit \
                     folder (data verbs are folder-scoped, not node-wide)",
                    capability.as_wire()
                ),
            ));
        }
        if capability.is_dangerous() {
            return Err(ApiError::unprocessable(format!(
                "capability `{}` cannot be granted over a node-wide scope; name an explicit folder",
                capability.as_wire()
            )));
        }
    }

    // A data verb's folder path is an operator-facing name resolved to its
    // canonical BEP id (the F1 fix), so the stored scope lands in the namespace
    // the runtime gate consults.
    if capability.is_data_verb()
        && let Scope::Folder { path } = scope
    {
        let folder_id = resolve_folder_name(state, path)?;
        return Ok(Scope::folder(folder_id));
    }
    Ok(scope.clone())
}

/// `POST /v1/grants` — capability: `grant:admin` over the scope.
async fn create(
    State(state): State<AppState>,
    session: Session,
    request_id: RequestId,
    Json(body): Json<GrantPost>,
) -> Result<impl IntoResponse, ApiError> {
    let scope = resolve_grant_scope(&state, body.capability, &body.scope)?;
    session.require(&state, Capability::GrantAdmin, &scope)?;

    let grant = Grant {
        grantee: DeviceId::new(body.grantee),
        capability: body.capability,
        scope: scope.clone(),
        granted_by: session.caller().clone(),
        expires: body.expires,
    };
    let id = state
        .engine
        .db()
        .insert_grant(&grant)
        .map_err(|e| ApiError::internal(format!("could not insert grant: {e}")))?;

    audit_grant_admin(
        &state,
        &session,
        &scope,
        &format!(
            "grant add {} to {} over {scope:?} [request {}]",
            grant.capability.as_wire(),
            grant.grantee,
            request_id.0
        ),
    )?;

    Ok((
        StatusCode::CREATED,
        Json(GrantView {
            id,
            grantee: grant.grantee.as_str().to_owned(),
            granted_by: grant.granted_by.as_str().to_owned(),
            capability: grant.capability,
            scope: grant.scope,
            expires: grant.expires,
        }),
    ))
}

/// `DELETE /v1/grants/{id}` — capability: `grant:admin` over the grant's scope.
async fn remove(
    State(state): State<AppState>,
    session: Session,
    request_id: RequestId,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    // Authorise against the scope of the row that will actually be mutated,
    // never a caller-advertised scope — the same rule the BEP `GrantRevoke`
    // path follows.
    let scope = state
        .engine
        .db()
        .grant_scope(id)
        .map_err(|e| ApiError::internal(format!("could not resolve grant {id}: {e}")))?
        .ok_or_else(|| ApiError::not_found(format!("no grant with id {id}")))?;
    session.require(&state, Capability::GrantAdmin, &scope)?;

    let removed = state
        .engine
        .db()
        .revoke_grant(id)
        .map_err(|e| ApiError::internal(format!("could not revoke grant {id}: {e}")))?;
    if !removed {
        return Err(ApiError::not_found(format!("no grant with id {id}")));
    }
    audit_grant_admin(
        &state,
        &session,
        &scope,
        &format!("grant revoke {id} [request {}]", request_id.0),
    )?;
    Ok(StatusCode::NO_CONTENT)
}

/// Append an `allowed` audit row for a grant-admin mutation, stamping the
/// verified bearer as actor and recording the request id in the command column.
pub(crate) fn audit_grant_admin(
    state: &AppState,
    session: &Session,
    scope: &Scope,
    command: &str,
) -> Result<(), ApiError> {
    state
        .engine
        .db()
        .append_audit(&AuditEntry {
            timestamp: Utc::now(),
            actor_device: session.caller().clone(),
            capability: Capability::GrantAdmin,
            scope: scope.clone(),
            command: command.to_owned(),
            outcome: "allowed".to_owned(),
        })
        .map_err(|e| ApiError::internal(format!("could not record audit row: {e}")))?;
    Ok(())
}
