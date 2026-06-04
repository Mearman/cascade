//! Share routes — the operator-facing view over data-verb grants.
//!
//! A share is a thin layer over the data-verb grant machinery: a posture maps
//! to `data:read` / `data:write` grants. Create and delete require `grant:admin`
//! over the folder; the F1 fix resolves the operator-facing folder name to its
//! canonical BEP id before any grant is stored.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use cascade_engine::manage::{Capability, DeviceId, Grant, Scope};

use crate::auth::Session;
use crate::error::ApiError;
use crate::request_id::RequestId;
use crate::routes::grants::audit_grant_admin;
use crate::routes::resolve_folder_name;
use crate::schemas::shares::{SharePost, SharePosture, ShareView, SharesResponse};
use crate::state::AppState;

/// Register the share routes.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/shares", get(list).post(create))
        .route("/v1/shares/{id}", axum::routing::delete(remove))
}

/// The per-(peer, folder) accumulation used to derive a share posture from the
/// contributing data-verb grant rows.
struct Aggregate {
    read: bool,
    write: bool,
    granted_by: String,
    expires: Option<chrono::DateTime<chrono::Utc>>,
    grant_ids: Vec<i64>,
}

/// Recover the operator-facing folder name from a canonical BEP folder id.
fn folder_name_of(folder_id: &str) -> String {
    folder_id
        .strip_prefix("p2p-")
        .unwrap_or(folder_id)
        .to_owned()
}

/// `GET /v1/shares` — capability: any verified session.
async fn list(
    State(state): State<AppState>,
    _session: Session,
) -> Result<Json<SharesResponse>, ApiError> {
    let records = state
        .engine
        .db()
        .list_data_grants()
        .map_err(|e| ApiError::internal(format!("could not list data grants: {e}")))?;

    // Aggregate by (peer, folder id) into a posture plus the contributing rows.
    let mut by_peer_folder: BTreeMap<(String, String), Aggregate> = BTreeMap::new();

    for record in records {
        let grant = record.grant;
        let Scope::Folder { path } = &grant.scope else {
            continue;
        };
        let key = (grant.grantee.as_str().to_owned(), path.clone());
        let entry = by_peer_folder.entry(key).or_insert_with(|| Aggregate {
            read: false,
            write: false,
            granted_by: grant.granted_by.as_str().to_owned(),
            expires: grant.expires,
            grant_ids: Vec::new(),
        });
        match grant.capability {
            Capability::DataRead => entry.read = true,
            Capability::DataWrite => entry.write = true,
            _ => {}
        }
        entry.grant_ids.push(record.id);
    }

    let shares = by_peer_folder
        .into_iter()
        .filter_map(|((peer, folder_id), aggregate)| {
            SharePosture::from_flags(aggregate.read, aggregate.write).map(|posture| ShareView {
                peer_device_id: peer,
                folder: folder_name_of(&folder_id),
                folder_id,
                posture,
                granted_by: aggregate.granted_by,
                expires: aggregate.expires,
                grant_ids: aggregate.grant_ids,
            })
        })
        .collect();

    Ok(Json(SharesResponse { shares }))
}

/// `POST /v1/shares` — capability: `grant:admin` over the folder.
async fn create(
    State(state): State<AppState>,
    session: Session,
    request_id: RequestId,
    Json(body): Json<SharePost>,
) -> Result<impl IntoResponse, ApiError> {
    let folder_id = resolve_folder_name(&state, &body.folder)?;
    let scope = Scope::folder(folder_id.clone());
    session.require(&state, Capability::GrantAdmin, &scope)?;

    let grantee = DeviceId::new(body.peer_device_id.clone());
    let mut verbs = Vec::new();
    if body.posture.grants_read() {
        verbs.push(Capability::DataRead);
    }
    if body.posture.grants_write() {
        verbs.push(Capability::DataWrite);
    }

    let mut grant_ids = Vec::new();
    for capability in verbs {
        let grant = Grant {
            grantee: grantee.clone(),
            capability,
            scope: scope.clone(),
            granted_by: session.caller().clone(),
            expires: body.expires,
        };
        let id = state
            .engine
            .db()
            .insert_grant(&grant)
            .map_err(|e| ApiError::internal(format!("could not insert share grant: {e}")))?;
        grant_ids.push(id);
    }

    audit_grant_admin(
        &state,
        &session,
        &scope,
        &format!(
            "share add {} to {} over {folder_id} [request {}]",
            body.posture.as_label(),
            body.peer_device_id,
            request_id.0
        ),
    )?;

    Ok((
        StatusCode::CREATED,
        Json(ShareView {
            peer_device_id: body.peer_device_id,
            folder: body.folder,
            folder_id,
            posture: body.posture,
            granted_by: session.caller().as_str().to_owned(),
            expires: body.expires,
            grant_ids,
        }),
    ))
}

/// `DELETE /v1/shares/{id}` — capability: `grant:admin` over the folder.
///
/// Revokes every data-verb grant for the peer and folder the row identifies, so
/// the posture drops to `none` atomically.
async fn remove(
    State(state): State<AppState>,
    session: Session,
    request_id: RequestId,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let db = state.engine.db();
    let records = db
        .list_data_grants()
        .map_err(|e| ApiError::internal(format!("could not list data grants: {e}")))?;

    let target = records
        .iter()
        .find(|record| record.id == id)
        .ok_or_else(|| ApiError::not_found(format!("no share grant with id {id}")))?;
    let grantee = target.grant.grantee.clone();
    let scope = target.grant.scope.clone();
    session.require(&state, Capability::GrantAdmin, &scope)?;

    // Revoke both directions for this peer and folder so the posture drops to
    // none in one operation.
    for record in &records {
        if record.grant.grantee == grantee && record.grant.scope == scope {
            db.revoke_grant(record.id)
                .map_err(|e| ApiError::internal(format!("could not revoke grant: {e}")))?;
        }
    }

    let folder_id = match &scope {
        Scope::Folder { path } => path.clone(),
        Scope::Node => "node".to_owned(),
    };
    audit_grant_admin(
        &state,
        &session,
        &scope,
        &format!("share revoke {grantee} over {folder_id} [request {}]", request_id.0),
    )?;
    Ok(StatusCode::NO_CONTENT)
}
