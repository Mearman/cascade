//! Session routes — the verified session view and a stateless logout.

use axum::Json;
use axum::extract::State;
use axum::routing::{get, post};
use axum::Router;
use cascade_engine::manage::{Capability, Grant, ManageGrantStore, Scope, TokenClaims};
use chrono::Utc;

use crate::auth::Session;
use crate::error::ApiError;
use crate::schemas::session::{Abilities, SessionInfo, SessionResponse, TokenView};
use crate::state::AppState;

/// Register the session routes.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/session", get(session))
        .route("/v1/session/revoke", post(revoke))
}

/// `GET /v1/session` — capability: any verified session.
async fn session(
    State(state): State<AppState>,
    session: Session,
) -> Result<Json<SessionResponse>, ApiError> {
    let abilities = compute_abilities(&state, &session)?;
    Ok(Json(build_response(&state, &session, abilities)?))
}

/// `POST /v1/session/revoke` — capability: any verified session.
///
/// The daemon holds no server-side session, so this returns the same response
/// shape with the abilities zeroed; the PWA clears its local copy and
/// redirects to login. Not a security boundary.
async fn revoke(
    State(state): State<AppState>,
    session: Session,
) -> Result<Json<SessionResponse>, ApiError> {
    Ok(Json(build_response(&state, &session, Abilities {
        status_read: false,
        pin_write: false,
        cache_manage: false,
        config_push: false,
        policy_set: false,
        backend_manage: false,
        lifecycle_control: false,
        grant_admin: false,
        data_read: Vec::new(),
        data_write: Vec::new(),
    })?))
}

/// Build the session response, denormalising the verified claims and abilities.
fn build_response(
    state: &AppState,
    session: &Session,
    abilities: Abilities,
) -> Result<SessionResponse, ApiError> {
    Ok(SessionResponse {
        session: SessionInfo {
            class: session.class.wire().to_owned(),
            node_device_id: session.node_device_id.as_str().to_owned(),
            verified_bearer: session.claims.bearer.as_str().to_owned(),
        },
        token: token_view(state, &session.claims)?,
        abilities,
    })
}

/// Build the token view, looking up the issuance instant when this node issued
/// the token.
fn token_view(state: &AppState, claims: &TokenClaims) -> Result<TokenView, ApiError> {
    let issued_at = state
        .engine
        .db()
        .list_tokens()
        .map_err(|e| ApiError::internal(format!("could not read issued tokens: {e}")))?
        .into_iter()
        .find(|record| record.token.claims.token_id == claims.token_id)
        .map(|record| record.issued_at);

    Ok(TokenView {
        token_id: claims.token_id.clone(),
        issuer: claims.issuer.as_str().to_owned(),
        bearer: claims.bearer.as_str().to_owned(),
        capability: claims.capability,
        scope: claims.scope.clone(),
        expires: claims.expires,
        issued_at,
    })
}

/// Compute the denormalised abilities: the `*_manage` booleans plus the
/// per-folder `data_read` / `data_write` prefix lists. A UI hint derived from
/// the bearer's grants and the presented token, never trusted by the server for
/// a decision.
fn compute_abilities(state: &AppState, session: &Session) -> Result<Abilities, ApiError> {
    let now = Utc::now();
    let mut grants = state
        .engine
        .manage_grants()
        .map_err(|e| ApiError::internal(format!("could not read grants: {e}")))?;
    grants.push(session.claims.to_grant());

    let caller = session.caller();
    let holds = |needed: Capability| -> bool {
        grants.iter().any(|grant: &Grant| {
            grant.grantee == *caller && grant.capability == needed && !grant.is_expired(now)
        })
    };

    let folder_prefixes = |needed: Capability| -> Vec<String> {
        let mut prefixes: Vec<String> = grants
            .iter()
            .filter(|grant| {
                grant.grantee == *caller && grant.capability == needed && !grant.is_expired(now)
            })
            .filter_map(|grant| match &grant.scope {
                Scope::Folder { path } => Some(path.clone()),
                Scope::Node => None,
            })
            .collect();
        prefixes.sort_unstable();
        prefixes.dedup();
        prefixes
    };

    Ok(Abilities {
        status_read: holds(Capability::StatusRead),
        pin_write: holds(Capability::PinWrite),
        cache_manage: holds(Capability::CacheManage),
        config_push: holds(Capability::ConfigPush),
        policy_set: holds(Capability::PolicySet),
        backend_manage: holds(Capability::BackendManage),
        lifecycle_control: holds(Capability::LifecycleControl),
        grant_admin: holds(Capability::GrantAdmin),
        data_read: folder_prefixes(Capability::DataRead),
        data_write: folder_prefixes(Capability::DataWrite),
    })
}
