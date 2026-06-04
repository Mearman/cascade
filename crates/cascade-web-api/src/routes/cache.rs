//! Cache routes — evict and warm, routed through the engine's command
//! executor.

use axum::Json;
use axum::extract::State;
use axum::routing::post;
use axum::Router;
use cascade_engine::manage::{Capability, ManageCommandExecutor, Scope};

use crate::auth::Session;
use crate::error::ApiError;
use crate::schemas::cache::{CacheResponse, CacheWarmPost};
use crate::state::AppState;

/// Register the cache routes.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/cache/evict", post(evict))
        .route("/v1/cache/warm", post(warm))
}

/// `POST /v1/cache/evict` — capability: `cache:manage`. No body.
async fn evict(
    State(state): State<AppState>,
    session: Session,
) -> Result<Json<CacheResponse>, ApiError> {
    session.require(&state, Capability::CacheManage, &Scope::Node)?;
    let summary = state
        .engine
        .manage_cache_evict()
        .await
        .map_err(|e| ApiError::internal(format!("cache eviction failed: {e}")))?;
    Ok(Json(CacheResponse { summary }))
}

/// `POST /v1/cache/warm` — capability: `cache:manage`.
async fn warm(
    State(state): State<AppState>,
    session: Session,
    Json(body): Json<CacheWarmPost>,
) -> Result<Json<CacheResponse>, ApiError> {
    session.require(&state, Capability::CacheManage, &Scope::Node)?;
    let summary = state
        .engine
        .manage_cache_warm(&body.path_glob)
        .await
        .map_err(|e| ApiError::internal(format!("cache warm failed: {e}")))?;
    Ok(Json(CacheResponse { summary }))
}
