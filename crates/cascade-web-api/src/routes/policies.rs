//! Lifecycle-policy routes — list, create, and delete policies over the
//! engine's `lifecycle_policies` table.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get};
use axum::Router;
use cascade_engine::manage::{Capability, Scope};

use crate::auth::Session;
use crate::error::ApiError;
use crate::schemas::policies::{PoliciesResponse, PolicyPost, PolicyView};
use crate::state::AppState;

/// Register the policy routes.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/policies", get(list).post(create))
        .route("/v1/policies/{id}", delete(remove))
}

/// Read the current lifecycle policies as views.
fn policy_views(state: &AppState) -> Result<Vec<PolicyView>, ApiError> {
    Ok(state
        .engine
        .db()
        .list_lifecycle_policies()
        .map_err(|e| ApiError::internal(format!("could not list policies: {e}")))?
        .into_iter()
        .map(|record| PolicyView {
            id: record.id,
            path_glob: record.path_glob,
            max_age_secs: record.max_age,
            max_file_size: record.max_file_size,
            priority: record.priority,
        })
        .collect())
}

/// `GET /v1/policies` — capability: `status:read`.
async fn list(
    State(state): State<AppState>,
    session: Session,
) -> Result<Json<PoliciesResponse>, ApiError> {
    session.require(&state, Capability::StatusRead, &Scope::Node)?;
    Ok(Json(PoliciesResponse {
        policies: policy_views(&state)?,
    }))
}

/// `POST /v1/policies` — capability: `policy:set`.
async fn create(
    State(state): State<AppState>,
    session: Session,
    Json(body): Json<PolicyPost>,
) -> Result<impl IntoResponse, ApiError> {
    session.require(&state, Capability::PolicySet, &Scope::Node)?;
    state
        .engine
        .policy_set(
            &body.path_glob,
            body.max_age_secs,
            body.max_file_size,
            body.priority,
        )
        .map_err(|e| ApiError::internal(format!("could not set policy: {e}")))?;

    let created = policy_views(&state)?
        .into_iter()
        .filter(|view| view.path_glob == body.path_glob)
        .max_by_key(|view| view.id)
        .ok_or_else(|| ApiError::internal("policy was set but could not be read back"))?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// `DELETE /v1/policies/{id}` — capability: `policy:set`.
async fn remove(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    session.require(&state, Capability::PolicySet, &Scope::Node)?;
    let removed = state
        .engine
        .db()
        .remove_lifecycle_policy(id)
        .map_err(|e| ApiError::internal(format!("could not remove policy: {e}")))?;
    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found(format!("no lifecycle policy with id {id}")))
    }
}
