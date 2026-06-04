//! Token routes — list issued tokens, issue a new one, and revoke one.

use std::collections::HashSet;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use cascade_engine::manage::{Capability, CapabilityToken, DeviceId, Scope, derive_token_id};
use chrono::{DateTime, Utc};

use crate::auth::{Session, SessionClass};
use crate::error::{ApiError, ErrorCode};
use crate::schemas::tokens::{TokenPost, TokenRevokeResponse, TokenView, TokensResponse};
use crate::state::AppState;

/// Register the token routes.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/tokens", get(list).post(issue))
        .route("/v1/tokens/{id}/revoke", post(revoke))
}

/// `GET /v1/tokens` — capability: any verified session.
async fn list(
    State(state): State<AppState>,
    _session: Session,
) -> Result<Json<TokensResponse>, ApiError> {
    let db = state.engine.db();
    let revoked: HashSet<String> = db
        .revoked_token_ids()
        .map_err(|e| ApiError::internal(format!("could not read revocation list: {e}")))?;
    let tokens = db
        .list_tokens()
        .map_err(|e| ApiError::internal(format!("could not list tokens: {e}")))?
        .into_iter()
        .map(|record| {
            let claims = &record.token.claims;
            TokenView {
                token_id: claims.token_id.clone(),
                issuer: claims.issuer.as_str().to_owned(),
                bearer: claims.bearer.as_str().to_owned(),
                capability: claims.capability,
                scope: claims.scope.clone(),
                expires: claims.expires,
                issued_at: record.issued_at,
                revoked: revoked.contains(&claims.token_id),
            }
        })
        .collect();
    Ok(Json(TokensResponse { tokens }))
}

/// Whether the caller's verified claims contain the requested authority — the
/// `claims.contains` rule `CapabilityToken::delegate` enforces, applied to a
/// non-owner issuing a node-signed token.
fn caller_contains(
    session: &Session,
    capability: Capability,
    scope: &Scope,
    expires: DateTime<Utc>,
) -> bool {
    session.claims.capability == capability
        && session.claims.scope.covers(scope)
        && expires <= session.claims.expires
}

/// `POST /v1/tokens` — capability: the conferred capability, plus
/// grant-admin-equivalent authority. An owner may issue any token; any other
/// session may only confer authority it itself holds.
async fn issue(
    State(state): State<AppState>,
    session: Session,
    Json(body): Json<TokenPost>,
) -> Result<impl IntoResponse, ApiError> {
    // F4: a data verb is never issued over a node-wide scope.
    if body.capability.is_data_verb() && body.scope.is_node_wide() {
        return Err(ApiError::new(
            ErrorCode::DataVerbNodeWideForbidden,
            format!(
                "capability `{}` cannot be issued over a node-wide scope; name an explicit folder",
                body.capability.as_wire()
            ),
        ));
    }

    // Only an owner may mint authority beyond its own claims; anyone else is
    // bounded by what they hold.
    if session.class != SessionClass::Owner
        && !caller_contains(&session, body.capability, &body.scope, body.expires)
    {
        return Err(ApiError::new(
            ErrorCode::DelegationExceedsParent,
            "the issued token exceeds the authority the caller holds",
        ));
    }

    let now = Utc::now();
    let bearer = DeviceId::new(body.bearer);
    let identity = state.identity.identity();
    let token_id = derive_token_id(
        &session.node_device_id,
        &bearer,
        body.capability,
        &body.scope,
        body.expires,
        now,
    );
    let token = CapabilityToken::issue(
        token_id,
        identity,
        &bearer,
        body.capability,
        body.scope,
        body.expires,
    )
    .map_err(|e| ApiError::internal(format!("could not sign capability token: {e}")))?;

    state
        .engine
        .db()
        .insert_token(&token, now)
        .map_err(|e| ApiError::internal(format!("could not record issued token: {e}")))?;

    Ok((StatusCode::CREATED, Json(token)))
}

/// `POST /v1/tokens/{id}/revoke` — capability: `Owner`.
async fn revoke(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<String>,
) -> Result<Json<TokenRevokeResponse>, ApiError> {
    if session.class != SessionClass::Owner {
        return Err(ApiError::forbidden("only an owner session may revoke a token"));
    }
    let db = state.engine.db();
    if db
        .is_token_revoked(&id)
        .map_err(|e| ApiError::internal(format!("could not read revocation state: {e}")))?
    {
        return Err(ApiError::new(
            ErrorCode::Gone,
            format!("token {id} is already revoked"),
        ));
    }
    let now = Utc::now();
    let revoked = db
        .revoke_token(&id, now)
        .map_err(|e| ApiError::internal(format!("could not revoke token {id}: {e}")))?;
    if !revoked {
        return Err(ApiError::not_found(format!("no token with id {id}")));
    }
    Ok(Json(TokenRevokeResponse {
        token_id: id,
        revoked_at: now,
    }))
}
