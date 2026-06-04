//! Bearer-token authentication and session-class derivation.
//!
//! There is one credential: the signed
//! [`CapabilityToken`] presented in
//! `Authorization: Bearer <base64-of-token-json>`, paired with a mandatory
//! `X-Cascade-Bearer-Device` header naming the device the bearer claims to be.
//! The token is verified the same way the BEP dispatcher verifies it — signed by
//! this node (or a chain rooting in it), unexpired, not revoked, bearer matching
//! the header — and then every per-request authorisation re-runs
//! [`authorises`], the *same* decision the
//! BEP path runs. There is no second credential format and no second
//! authorisation path.

use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use cascade_engine::manage::token::MAX_TOKEN_JSON_BYTES;
use cascade_engine::manage::{
    Capability, CapabilityToken, DeviceId, Grant, ManageGrantStore, Scope, TokenClaims,
    TokenVerifyError, authorises,
};
use chrono::Utc;
use data_encoding::BASE64;
use serde::Serialize;

use crate::error::{ApiError, ErrorCode};
use crate::state::AppState;

/// The mandatory header naming the device a bearer claims to be — the HTTP
/// stand-in for the device id the BEP transport authenticates by mutual TLS.
pub const BEARER_DEVICE_HEADER: &str = "x-cascade-bearer-device";

/// The session class a verified token derives. A UI signal only: the server
/// enforces the underlying [`authorises`] check on every request, never a
/// class-based bypass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionClass {
    /// The token was issued directly by this node, or its bearer is this node.
    Owner,
    /// The bearer is a device this node already knows (a grantee or a peer).
    NamedUser,
    /// The bearer is otherwise unknown; only the verified claims apply.
    Bearer,
}

impl SessionClass {
    /// The wire string (`owner`, `named_user`, `bearer`).
    #[must_use]
    pub const fn wire(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::NamedUser => "named_user",
            Self::Bearer => "bearer",
        }
    }
}

/// A verified session — the output of authenticating a request.
#[derive(Debug, Clone)]
pub struct Session {
    /// The derived session class (a UI signal).
    pub class: SessionClass,
    /// The verified leaf-token claims. The principal for authorisation is
    /// [`TokenClaims::bearer`].
    pub claims: TokenClaims,
    /// This node's device id, for denormalising into the session response.
    pub node_device_id: DeviceId,
}

impl Session {
    /// The principal every authorisation decision is made against — the verified
    /// bearer.
    #[must_use]
    pub const fn caller(&self) -> &DeviceId {
        &self.claims.bearer
    }

    /// Re-run the management-plane authorisation for `needed` over `target`,
    /// folding the session's token-carried grant into the on-node grant set —
    /// the exact path the BEP dispatcher takes.
    ///
    /// Distinguishes `403 forbidden` (the caller holds the capability but not
    /// over this scope) from `401 unauthorised` (the caller does not hold the
    /// capability at all), per the contract's status table.
    pub fn require(
        &self,
        state: &AppState,
        needed: Capability,
        target: &Scope,
    ) -> Result<(), ApiError> {
        let now = Utc::now();
        let mut grants = state
            .engine
            .manage_grants()
            .map_err(|e| ApiError::internal(format!("could not read grants: {e}")))?;
        // The token the caller presented is a portable grant; fold it in exactly
        // as `verify_presented_token` does on the BEP path.
        grants.push(self.claims.to_grant());

        if authorises(&grants, self.caller(), needed, target, now) {
            return Ok(());
        }

        // The caller has the capability somewhere (over a scope this target is
        // not within) → forbidden; otherwise the capability is absent →
        // unauthorised.
        let holds_capability = grants.iter().any(|grant: &Grant| {
            grant.grantee == *self.caller() && grant.capability == needed && !grant.is_expired(now)
        });
        if holds_capability {
            Err(ApiError::forbidden(format!(
                "caller holds {} but not over {target:?}",
                needed.as_wire()
            )))
        } else {
            Err(ApiError::unauthorised(format!(
                "caller's verified claims do not satisfy the required capability {}",
                needed.as_wire()
            )))
        }
    }
}

/// Map a [`ManageGrantStore`] trait —
/// already implemented by the engine — to read the revoked-token set without a
/// per-token database round-trip during verification.
fn read_revoked(state: &AppState) -> Result<std::collections::HashSet<String>, ApiError> {
    state
        .engine
        .manage_revoked_token_ids()
        .map_err(|e| ApiError::internal(format!("could not read token revocation list: {e}")))
}

/// Decode and parse the bearer credential into a [`CapabilityToken`].
fn parse_bearer_token(parts: &Parts) -> Result<CapabilityToken, ApiError> {
    let header = parts
        .headers
        .get(AUTHORIZATION)
        .ok_or_else(|| ApiError::unauthorised("missing Authorization header"))?;
    let value = header
        .to_str()
        .map_err(|_| ApiError::unauthorised("Authorization header is not valid text"))?;
    let credential = value
        .strip_prefix("Bearer ")
        .ok_or_else(|| ApiError::unauthorised("Authorization header must be a Bearer credential"))?
        .trim();

    // Guard the encoded length before allocating the decode buffer: base64
    // expands the JSON by 4/3, so anything beyond twice the JSON ceiling cannot
    // be a valid in-bounds token.
    if credential.len() > MAX_TOKEN_JSON_BYTES.saturating_mul(2) {
        return Err(ApiError::new(
            ErrorCode::TokenTooLarge,
            "presented capability token exceeds the maximum length",
        ));
    }

    let json = BASE64
        .decode(credential.as_bytes())
        .map_err(|_| ApiError::unauthorised("Bearer credential is not valid base64"))?;
    if json.len() > MAX_TOKEN_JSON_BYTES {
        return Err(ApiError::new(
            ErrorCode::TokenTooLarge,
            format!(
                "presented capability token exceeds the maximum length ({} > {MAX_TOKEN_JSON_BYTES} bytes)",
                json.len()
            ),
        ));
    }

    serde_json::from_slice::<CapabilityToken>(&json)
        .map_err(|e| ApiError::unauthorised(format!("could not parse capability token: {e}")))
}

/// Map a token verification failure to the contract's status codes.
fn verify_error_to_api(error: &TokenVerifyError) -> ApiError {
    match error {
        TokenVerifyError::BearerMismatch { .. } => ApiError::new(
            ErrorCode::BearerMismatch,
            format!("presented capability token rejected: {error}"),
        ),
        TokenVerifyError::ChainTooDeep { max, .. } => ApiError::new(
            ErrorCode::ChainTooDeep,
            format!("presented capability token chain exceeds maximum depth {max}"),
        ),
        other => ApiError::unauthorised(format!("presented capability token rejected: {other}")),
    }
}

/// Derive the session class from the verified claims and what the node knows
/// about the bearer. A pure UI signal; it confers no authority.
fn derive_class(state: &AppState, claims: &TokenClaims, node: &DeviceId) -> SessionClass {
    // Owner: the token was issued directly by this node (the leaf issuer is the
    // node), or the bearer is the node itself.
    if claims.issuer == *node || claims.bearer == *node {
        return SessionClass::Owner;
    }
    // NamedUser: the bearer is a device this node already knows — named in a
    // grant row (as grantee or granter) or present in the peer list.
    if bearer_is_known(state, &claims.bearer) {
        SessionClass::NamedUser
    } else {
        SessionClass::Bearer
    }
}

/// Whether the bearer appears in a grant row or the peer list.
fn bearer_is_known(state: &AppState, bearer: &DeviceId) -> bool {
    let db = state.engine.db();
    if let Ok(grants) = db.list_grants()
        && grants
            .iter()
            .any(|record| record.grant.grantee == *bearer || record.grant.granted_by == *bearer)
    {
        return true;
    }
    if let Ok(peers) = db.list_peers()
        && peers.iter().any(|peer| peer.device_id == bearer.as_str())
    {
        return true;
    }
    false
}

impl FromRequestParts<AppState> for Session {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = parse_bearer_token(parts)?;

        // The mandatory bearer-device header is the HTTP stand-in for the
        // TLS-authenticated device id the BEP path uses as `connected_device`.
        let connected_device = parts
            .headers
            .get(BEARER_DEVICE_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(|value| DeviceId::new(value.to_owned()))
            .ok_or_else(|| {
                ApiError::unauthorised(format!("{BEARER_DEVICE_HEADER} header is required"))
            })?;

        let node_device_id = state.identity.device_id().clone();
        let revoked = read_revoked(state)?;
        let is_revoked = |id: &str| revoked.contains(id);

        let claims = token
            .verify(&node_device_id, &connected_device, Utc::now(), &is_revoked)
            .map_err(|e| verify_error_to_api(&e))?
            .clone();

        let class = derive_class(state, &claims, &node_device_id);
        Ok(Self {
            class,
            claims,
            node_device_id,
        })
    }
}
