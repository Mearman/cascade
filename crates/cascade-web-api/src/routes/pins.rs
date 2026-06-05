//! Pin routes — list, create, and delete pin rules over the engine's
//! `pin_rules` table.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get};
use cascade_engine::manage::{Capability, Scope};

use crate::auth::Session;
use crate::error::ApiError;
use crate::schemas::pins::{PinPost, PinView, PinsResponse};
use crate::state::AppState;

/// Register the pin routes.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/pins", get(list).post(create))
        .route("/v1/pins/{id}", delete(remove))
}

/// Read the current pin rules as views.
async fn pin_views(state: &AppState) -> Result<Vec<PinView>, ApiError> {
    Ok(state
        .engine
        .list_pins()
        .await
        .map_err(|e| ApiError::internal(format!("could not list pins: {e}")))?
        .into_iter()
        .map(|record| PinView {
            id: record.id,
            path_glob: record.path_glob,
            recursive: record.recursive,
        })
        .collect())
}

/// `GET /v1/pins` — capability: `status:read`.
async fn list(
    State(state): State<AppState>,
    session: Session,
) -> Result<Json<PinsResponse>, ApiError> {
    session.require(&state, Capability::StatusRead, &Scope::Node)?;
    Ok(Json(PinsResponse {
        pins: pin_views(&state).await?,
    }))
}

/// `POST /v1/pins` — capability: `pin:write`.
async fn create(
    State(state): State<AppState>,
    session: Session,
    Json(body): Json<PinPost>,
) -> Result<impl IntoResponse, ApiError> {
    session.require(&state, Capability::PinWrite, &Scope::Node)?;
    state
        .engine
        .pin(&body.path_glob, body.recursive)
        .await
        .map_err(|e| ApiError::internal(format!("could not create pin: {e}")))?;

    // Return the created rule by finding the highest-id row matching the glob,
    // since `pin` does not return the inserted id.
    let created = pin_views(&state)
        .await?
        .into_iter()
        .filter(|view| view.path_glob == body.path_glob)
        .max_by_key(|view| view.id)
        .ok_or_else(|| ApiError::internal("pin was created but could not be read back"))?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// `DELETE /v1/pins/{id}` — capability: `pin:write`.
async fn remove(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    session.require(&state, Capability::PinWrite, &Scope::Node)?;

    // The engine removes a pin by its glob, so resolve the id to its glob first.
    let target = pin_views(&state)
        .await?
        .into_iter()
        .find(|view| view.id == id);
    let Some(view) = target else {
        return Err(ApiError::not_found(format!("no pin rule with id {id}")));
    };
    let removed = state
        .engine
        .db()
        .remove_pin_rule(&view.path_glob)
        .map_err(|e| ApiError::internal(format!("could not remove pin: {e}")))?;
    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found(format!("no pin rule with id {id}")))
    }
}
