//! Health, readiness, and bundle-manifest routes.
//!
//! `/v1/health` and `/v1/bundle` are the only unauthenticated routes.
//! `/v1/ready` requires any verified session and reports the F3 data-plane bit,
//! returning `503` until that bit flips.

use axum::Json;
use axum::extract::State;
use axum::routing::get;
use axum::{Router, response::IntoResponse};

use crate::auth::Session;
use crate::error::ApiError;
use crate::schemas::health::{BundleResponse, HealthResponse, ReadyResponse};
use crate::state::AppState;

/// Register the health, readiness, and bundle routes.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/ready", get(ready))
        .route("/v1/bundle", get(bundle))
}

/// `GET /v1/health` — capability: none.
async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_owned(),
        version: state.bind.version.clone(),
        node_device_id: state.identity.device_id().as_str().to_owned(),
    })
}

/// `GET /v1/ready` — capability: any verified session.
///
/// `503 unavailable` while starting up, while the F3 data-plane bit is unset, or
/// when the state database is unreadable; the specific reason is in `details`.
async fn ready(
    State(state): State<AppState>,
    _session: Session,
) -> Result<Json<ReadyResponse>, ApiError> {
    // The state database backs readiness; an unreadable database is a hard
    // `unavailable`, not a silent "no backends".
    let status = state.engine.status().await;
    let started_at = state.readiness.started_at();

    if !state.readiness.data_plane_ready() {
        return Err(
            ApiError::unavailable("the data plane is not yet ready").with_details(
                serde_json::json!({
                    "reason": "data_plane_not_ready",
                    "data_plane_ready": false,
                    "started_at": started_at,
                }),
            ),
        );
    }

    Ok(Json(ReadyResponse {
        ready: true,
        data_plane_ready: true,
        backends: status.backends,
        started_at,
    }))
}

/// `GET /v1/bundle` — capability: none. The public PWA shell manifest.
async fn bundle(State(state): State<AppState>) -> impl IntoResponse {
    Json(BundleResponse {
        bundle_url: state.bind.bundle_url.clone(),
        api_base_url: format!("http://{}", state.bind.bind),
        version: state.bind.version.clone(),
        build_sha: state.bind.build_sha.clone(),
    })
}
