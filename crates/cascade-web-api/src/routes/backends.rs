//! Backend route — the PWA's folder picker.

use axum::Json;
use axum::extract::State;
use axum::routing::get;
use axum::Router;
use cascade_engine::manage::{Capability, Scope};

use crate::auth::Session;
use crate::error::ApiError;
use crate::routes::backend_folder_id;
use crate::schemas::backends::{BackendView, BackendsResponse};
use crate::state::AppState;

/// Register the backends route.
pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/backends", get(list))
}

/// `GET /v1/backends` — capability: `status:read`.
async fn list(
    State(state): State<AppState>,
    session: Session,
) -> Result<Json<BackendsResponse>, ApiError> {
    session.require(&state, Capability::StatusRead, &Scope::Node)?;
    let backends = state
        .engine
        .db()
        .list_backends()
        .map_err(|e| ApiError::internal(format!("could not list backends: {e}")))?
        .into_iter()
        .map(|record| BackendView {
            folder_id: backend_folder_id(&record),
            id: record.id,
            backend_type: record.backend_type,
            display_name: record.display_name,
            mount_path: record.mount_path,
        })
        .collect();
    Ok(Json(BackendsResponse { backends }))
}
