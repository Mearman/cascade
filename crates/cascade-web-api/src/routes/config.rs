//! Config-push route — merges a `.cascade` fragment over the engine, scoped to
//! the target folder.

use axum::Json;
use axum::extract::State;
use axum::routing::post;
use axum::Router;
use cascade_engine::manage::{Capability, ManageCommandExecutor, Scope};
use cascade_p2p::protocol::ManageConfigFormat;

use crate::auth::Session;
use crate::error::ApiError;
use crate::schemas::config::{ConfigFormat, ConfigPushPost, ConfigPushResponse};
use crate::state::AppState;

/// Register the config-push route.
pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/config/push", post(push))
}

/// Map the wire format to the engine's `ManageConfigFormat`.
const fn to_manage_format(format: ConfigFormat) -> ManageConfigFormat {
    match format {
        ConfigFormat::Gitignore => ManageConfigFormat::Gitignore,
        ConfigFormat::Toml => ManageConfigFormat::Toml,
        ConfigFormat::Yaml => ManageConfigFormat::Yaml,
        ConfigFormat::Json => ManageConfigFormat::Json,
    }
}

/// `POST /v1/config/push` — capability: `config:push` over the target folder.
async fn push(
    State(state): State<AppState>,
    session: Session,
    Json(body): Json<ConfigPushPost>,
) -> Result<Json<ConfigPushResponse>, ApiError> {
    session.require(
        &state,
        Capability::ConfigPush,
        &Scope::folder(body.folder.clone()),
    )?;
    let summary = state
        .engine
        .manage_config_push(to_manage_format(body.format), &body.folder, &body.body)
        .await
        .map_err(|e| ApiError::unprocessable(format!("config push rejected: {e}")))?;
    Ok(Json(ConfigPushResponse { summary }))
}
