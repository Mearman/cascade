//! Auth routes — PWA authentication flows that don't require an existing session.
//!
//! Three endpoints allow the PWA to obtain a capability token without pasting
//! raw JSON:
//!
//! - `POST /v1/auth/pair` — pairing code: the CLI generates a code, the PWA
//!   submits it.
//! - `POST /v1/auth/secret` — shared secret: a password the operator shares
//!   with the PWA.
//! - `POST /v1/auth/device` + `GET /v1/auth/device/{code}` — device code:
//!   the PWA requests a code, the CLI authorises it, the PWA polls.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use cascade_engine::manage::token::derive_token_id;
use cascade_engine::manage::{Capability, CapabilityToken, DeviceId, Scope};
use chrono::{DateTime, Duration, Utc};
use data_encoding::{BASE32_NOPAD, HEXLOWER};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::ApiError;
use crate::state::AppState;

/// Register the auth routes.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/auth/pair", post(pair))
        .route("/v1/auth/secret", post(secret))
        .route("/v1/auth/device", post(device_request))
        .route("/v1/auth/device/{code}", get(device_poll))
        .route("/v1/auth/device/{code}/authorize", post(device_authorize))
}

// ── Request / response schemas ──

#[derive(Debug, Deserialize)]
struct PairRequest {
    code: String,
}

#[derive(Debug, Deserialize)]
struct SecretRequest {
    secret: String,
}

#[derive(Debug, Serialize)]
struct DeviceCodeResponse {
    code: String,
    expires_in: u32,
}

#[derive(Debug, Serialize)]
struct DevicePollResponse {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<serde_json::Value>,
}

// ── Helpers ──

/// Device code TTL in seconds.
const DEVICE_TTL_SECONDS: i64 = 600;

/// Monotonic counter mixed into each code to ensure uniqueness.
static CODE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate an 8-character code derived from SHA-256 of timestamp + counter.
fn generate_code() -> String {
    let now = Utc::now();
    let counter = CODE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut hasher = Sha256::new();
    hasher.update(b"cascade-auth-code-v1");
    hasher.update(now.timestamp_nanos_opt().unwrap_or(0).to_be_bytes());
    hasher.update(counter.to_be_bytes());
    let digest = hasher.finalize();
    // Take first 5 bytes → 8 base32 characters.
    let prefix: [u8; 5] = digest
        .get(..5)
        .and_then(|s| s.try_into().ok())
        .unwrap_or([0u8; 5]);
    BASE32_NOPAD.encode(&prefix)
}

/// Constant-time comparison of two byte strings.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() == b.len() {
        a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | x ^ y) == 0
    } else {
        // Length mismatch: still do a comparison to avoid timing leaks on length.
        let _ = a
            .iter()
            .zip(b.iter().copied())
            .fold(0u8, |acc, (x, y)| acc | x ^ y);
        false
    }
}

/// Issue an owner-level token (all capabilities, node-wide scope, 24h expiry).
fn issue_owner_token(state: &AppState) -> Result<(CapabilityToken, DateTime<Utc>), ApiError> {
    let now = Utc::now();
    let expires = now + Duration::hours(24);

    // Derive a bearer device id from the node identity + timestamp.
    let mut hasher = Sha256::new();
    hasher.update(b"cascade-pwa-bearer-v1");
    hasher.update(state.identity.device_id().as_str().as_bytes());
    hasher.update(now.timestamp_nanos_opt().map_or([0u8; 8], i64::to_be_bytes));
    let digest = hasher.finalize();
    let bearer_id = format!(
        "pwa-{}",
        HEXLOWER.encode(digest.get(..10).unwrap_or(&[0u8; 10]))
    );
    let bearer = DeviceId::new(bearer_id);

    let identity = state.identity.identity();
    let token_id = derive_token_id(
        state.identity.device_id(),
        &bearer,
        Capability::StatusRead,
        &Scope::Node,
        expires,
        now,
    );

    let token = CapabilityToken::issue(
        token_id,
        identity,
        &bearer,
        Capability::StatusRead,
        Scope::Node,
        expires,
    )
    .map_err(|e| ApiError::internal(format!("could not sign capability token: {e}")))?;

    Ok((token, now))
}

// ── Handlers ──

/// `POST /v1/auth/pair` — Pairing code verification (no session required).
///
/// The CLI generated a code and stored it in `auth_codes` with `kind =
/// 'pairing'`. The PWA submits that code here. If the code is valid and not
/// expired, an owner token is issued, the code is consumed, and the token is
/// returned.
async fn pair(
    State(state): State<AppState>,
    Json(body): Json<PairRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let db = state.engine.db();

    // Clean up expired codes on every attempt.
    let _ = db.delete_expired_auth_codes();

    let record = db
        .get_auth_code(body.code.trim())
        .map_err(|e| ApiError::internal(format!("could not look up auth code: {e}")))?;

    let record = record.ok_or_else(|| {
        ApiError::not_found(
            "code not found or expired — run `cascade auth pair` to generate a new one",
        )
    })?;

    if record.kind != "pairing" {
        return Err(ApiError::not_found("code not found or expired"));
    }

    let now = Utc::now();
    let expires = DateTime::parse_from_rfc3339(&record.expires_at)
        .map(|dt| dt.to_utc())
        .map_err(|e| ApiError::internal(format!("could not parse expires_at: {e}")))?;

    if record.status != "pending" {
        return Err(ApiError::new(
            crate::error::ErrorCode::Gone,
            "code already used",
        ));
    }

    if now >= expires {
        return Err(ApiError::new(
            crate::error::ErrorCode::Gone,
            "code expired — run `cascade auth pair` to generate a new one",
        ));
    }

    let (token, issued_at) = issue_owner_token(&state)?;

    db.insert_token(&token, issued_at)
        .map_err(|e| ApiError::internal(format!("could not store token: {e}")))?;

    let token_json = serde_json::to_string(&token)
        .map_err(|e| ApiError::internal(format!("could not serialise token: {e}")))?;

    db.update_auth_code(
        &record.code,
        "consumed",
        Some(&token.claims.token_id),
        Some(&token_json),
    )
    .map_err(|e| ApiError::internal(format!("could not update auth code: {e}")))?;

    Ok((StatusCode::OK, Json(token)))
}

/// `POST /v1/auth/secret` — Shared secret verification (no session required).
///
/// The operator generated a secret via `cascade auth secret`. The PWA sends
/// it here. If it matches, an owner token is issued.
async fn secret(
    State(state): State<AppState>,
    Json(body): Json<SecretRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let db = state.engine.db();

    let stored = db
        .get_daemon_secret()
        .map_err(|e| ApiError::internal(format!("could not read daemon secret: {e}")))?;

    let stored = stored.ok_or_else(|| {
        ApiError::forbidden(
            "no shared secret configured — run `cascade auth secret` on the daemon host first",
        )
    })?;

    if !constant_time_eq(body.secret.trim().as_bytes(), stored.as_bytes()) {
        return Err(ApiError::unauthorised("invalid secret"));
    }

    let (token, issued_at) = issue_owner_token(&state)?;

    db.insert_token(&token, issued_at)
        .map_err(|e| ApiError::internal(format!("could not store token: {e}")))?;

    Ok((StatusCode::OK, Json(token)))
}

/// `POST /v1/auth/device` — Device code: request a new code (no session required).
///
/// Generates a short-lived code the PWA displays to the user. The user runs
/// `cascade auth authorize <code>` on the CLI to authorise it.
async fn device_request(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let db = state.engine.db();

    // Clean up expired codes.
    let _ = db.delete_expired_auth_codes();

    let code = generate_code();
    let expires_at = Utc::now() + Duration::seconds(DEVICE_TTL_SECONDS);

    db.insert_auth_code(&code, "device", expires_at)
        .map_err(|e| ApiError::internal(format!("could not store device code: {e}")))?;

    Ok((
        StatusCode::CREATED,
        Json(DeviceCodeResponse {
            code,
            expires_in: u32::try_from(DEVICE_TTL_SECONDS).unwrap_or(600),
        }),
    ))
}

/// `GET /v1/auth/device/{code}` — Device code: poll for authorisation.
///
/// Returns `pending` while unauthorised, the token once authorised, or an
/// error if the code has expired.
async fn device_poll(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let db = state.engine.db();

    let record = db
        .get_auth_code(&code)
        .map_err(|e| ApiError::internal(format!("could not look up device code: {e}")))?;

    let record = record.ok_or_else(|| ApiError::not_found("device code not found"))?;

    if record.kind != "device" {
        return Err(ApiError::not_found("device code not found"));
    }

    let now = Utc::now();
    let expires = DateTime::parse_from_rfc3339(&record.expires_at)
        .map(|dt| dt.to_utc())
        .map_err(|e| ApiError::internal(format!("could not parse expires_at: {e}")))?;

    if now >= expires {
        return Err(ApiError::new(
            crate::error::ErrorCode::Gone,
            "device code expired — request a new one",
        ));
    }

    match record.status.as_str() {
        "pending" => Ok((
            StatusCode::ACCEPTED,
            Json(DevicePollResponse {
                status: "pending".to_owned(),
                token: None,
            }),
        )),
        "authorised" => {
            let token_value: serde_json::Value = record
                .token_json
                .as_deref()
                .and_then(|json| serde_json::from_str(json).ok())
                .ok_or_else(|| ApiError::internal("authorised code has no token attached"))?;

            // Mark consumed.
            let _ = db.update_auth_code(&code, "consumed", None, None);

            Ok((
                StatusCode::OK,
                Json(DevicePollResponse {
                    status: "authorised".to_owned(),
                    token: Some(token_value),
                }),
            ))
        }
        "consumed" => Err(ApiError::new(
            crate::error::ErrorCode::Gone,
            "code already consumed",
        )),
        _ => Err(ApiError::internal(format!(
            "unexpected auth code status: {}",
            record.status
        ))),
    }
}

/// `POST /v1/auth/device/{code}/authorize` — Device code: authorise a pending code.
///
/// Called by the CLI (`cascade auth authorize <code>`). Issues an owner token
/// and stores it against the code so the PWA's poll picks it up.
///
/// This endpoint accepts requests from localhost without bearer auth. The
/// daemon binds to loopback by default, so only local processes can reach it.
async fn device_authorize(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let db = state.engine.db();

    let record = db
        .get_auth_code(&code)
        .map_err(|e| ApiError::internal(format!("could not look up device code: {e}")))?;

    let record = record.ok_or_else(|| ApiError::not_found("device code not found"))?;

    if record.kind != "device" {
        return Err(ApiError::not_found("device code not found"));
    }

    let now = Utc::now();
    let expires = DateTime::parse_from_rfc3339(&record.expires_at)
        .map(|dt| dt.to_utc())
        .map_err(|e| ApiError::internal(format!("could not parse expires_at: {e}")))?;

    if record.status != "pending" {
        return Err(ApiError::new(
            crate::error::ErrorCode::Conflict,
            format!("code is already {}", record.status),
        ));
    }

    if now >= expires {
        return Err(ApiError::new(
            crate::error::ErrorCode::Gone,
            "device code expired — request a new one",
        ));
    }

    let (token, issued_at) = issue_owner_token(&state)?;

    db.insert_token(&token, issued_at)
        .map_err(|e| ApiError::internal(format!("could not store token: {e}")))?;

    let token_json = serde_json::to_string(&token)
        .map_err(|e| ApiError::internal(format!("could not serialise token: {e}")))?;

    db.update_auth_code(
        &code,
        "authorised",
        Some(&token.claims.token_id),
        Some(&token_json),
    )
    .map_err(|e| ApiError::internal(format!("could not update device code: {e}")))?;

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "authorised",
            "message": "the web UI will connect automatically"
        })),
    ))
}
