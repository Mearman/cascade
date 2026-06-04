//! The single error envelope every handler returns.
//!
//! There is exactly one error shape on the wire (see the contract's "Error
//! envelope" section):
//!
//! ```json
//! { "error": { "code": "...", "message": "...", "request_id": "...",
//!              "details": { ... } } }
//! ```
//!
//! Handlers never construct an [`axum::http::StatusCode`] directly: they return
//! an [`ApiError`] whose [`ErrorCode`] maps to a status through the one table in
//! [`ErrorCode::status`]. The `request_id` is stamped by the
//! [`crate::request_id`] middleware, which finalises the body so a handler never
//! has to thread the id through itself.

use axum::Extension;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::Value;

/// The closed set of machine-readable error codes the PWA branches on.
///
/// Each code maps to exactly one HTTP status through [`Self::status`] and to its
/// stable wire string through [`Self::wire`]. The PWA branches on the wire
/// string, never on the human-readable message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// Token missing, malformed, signature bad, expired, revoked, or claims do
    /// not satisfy the route's required capability. HTTP 401.
    Unauthorised,
    /// The `X-Cascade-Bearer-Device` header did not match the verified token
    /// bearer (or was absent). HTTP 401.
    BearerMismatch,
    /// The presented token JSON exceeded `MAX_TOKEN_JSON_BYTES`. HTTP 413.
    TokenTooLarge,
    /// The presented token's delegation chain was deeper than
    /// `MAX_DELEGATION_DEPTH`. HTTP 400.
    ChainTooDeep,
    /// The caller holds the capability but not over the requested scope. HTTP
    /// 403.
    Forbidden,
    /// Path or resource does not exist. HTTP 404.
    NotFound,
    /// Optimistic-concurrency clash, duplicate key, or already-revoked token.
    /// HTTP 409.
    Conflict,
    /// Resource existed and was removed (for example an already-revoked token).
    /// HTTP 410.
    Gone,
    /// An `If-Match` / `If-None-Match` precondition failed. HTTP 412.
    PreconditionFailed,
    /// The request body exceeded `[web].max_body_bytes`. HTTP 413.
    PayloadTooLarge,
    /// The body parsed but failed domain validation. HTTP 422.
    Unprocessable,
    /// A `data:read` / `data:write` verb was requested over a node-wide scope
    /// (the F4 bar). HTTP 422.
    DataVerbNodeWideForbidden,
    /// A delegated token or grant tried to widen authority beyond its parent.
    /// HTTP 422.
    DelegationExceedsParent,
    /// A folder name did not resolve to a known P2P folder (the F1 bar). HTTP
    /// 422.
    UnknownFolder,
    /// The token-bucket rate limiter is exhausted. HTTP 429.
    RateLimited,
    /// An unexpected server error; details are suppressed in production. HTTP
    /// 500.
    Internal,
    /// The daemon is shutting down or the state database is unreadable. HTTP
    /// 503.
    Unavailable,
    /// A data-plane route was reached before the F3 readiness bit flipped. HTTP
    /// 503.
    DataPlaneNotReady,
    /// An upstream backend or engine call exceeded its budget. HTTP 504.
    Timeout,
}

impl ErrorCode {
    /// The stable wire string the PWA branches on.
    #[must_use]
    pub const fn wire(self) -> &'static str {
        match self {
            Self::Unauthorised => "unauthorised",
            Self::BearerMismatch => "bearer_mismatch",
            Self::TokenTooLarge => "token_too_large",
            Self::ChainTooDeep => "chain_too_deep",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::Gone => "gone",
            Self::PreconditionFailed => "precondition_failed",
            Self::PayloadTooLarge => "payload_too_large",
            Self::Unprocessable => "unprocessable",
            Self::DataVerbNodeWideForbidden => "data_verb_node_wide_forbidden",
            Self::DelegationExceedsParent => "delegation_exceeds_parent",
            Self::UnknownFolder => "unknown_folder",
            Self::RateLimited => "rate_limited",
            Self::Internal => "internal",
            Self::Unavailable => "unavailable",
            Self::DataPlaneNotReady => "data_plane_not_ready",
            Self::Timeout => "timeout",
        }
    }

    /// The single HTTP status mapping. The only place an `ErrorCode` becomes a
    /// status; no handler maps codes itself.
    #[must_use]
    pub const fn status(self) -> StatusCode {
        match self {
            Self::Unauthorised | Self::BearerMismatch => StatusCode::UNAUTHORIZED,
            Self::ChainTooDeep => StatusCode::BAD_REQUEST,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Conflict => StatusCode::CONFLICT,
            Self::Gone => StatusCode::GONE,
            Self::PreconditionFailed => StatusCode::PRECONDITION_FAILED,
            Self::TokenTooLarge | Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Unprocessable
            | Self::DataVerbNodeWideForbidden
            | Self::DelegationExceedsParent
            | Self::UnknownFolder => StatusCode::UNPROCESSABLE_ENTITY,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Unavailable | Self::DataPlaneNotReady => StatusCode::SERVICE_UNAVAILABLE,
            Self::Timeout => StatusCode::GATEWAY_TIMEOUT,
        }
    }
}

/// A structured error to return from a handler.
///
/// Construct one with the named constructors ([`ApiError::not_found`], …) rather
/// than building the fields by hand, so the message and details stay consistent.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct ApiError {
    /// The machine-readable code that fixes the HTTP status.
    pub code: ErrorCode,
    /// A human-readable message, safe to log; not part of the contract.
    pub message: String,
    /// Structured per-code context. Omitted from the envelope when absent.
    pub details: Option<Value>,
}

impl ApiError {
    /// Build an error with no structured details.
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
        }
    }

    /// Attach structured details to an error.
    #[must_use]
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    /// A `401 unauthorised`.
    #[must_use]
    pub fn unauthorised(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unauthorised, message)
    }

    /// A `403 forbidden` — the caller holds the capability but not over the
    /// requested scope.
    #[must_use]
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Forbidden, message)
    }

    /// A `404 not_found`.
    #[must_use]
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }

    /// A `422 unprocessable` — the body parsed but failed domain validation.
    #[must_use]
    pub fn unprocessable(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unprocessable, message)
    }

    /// A `503 unavailable`.
    #[must_use]
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unavailable, message)
    }

    /// A `503 data_plane_not_ready` — the F3 readiness bit has not yet flipped.
    #[must_use]
    pub fn data_plane_not_ready(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::DataPlaneNotReady, message)
    }

    /// A `500 internal`. The message is logged; the envelope carries a generic
    /// message in production builds.
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }
}

/// The error parts carried in the response extensions for the request-id
/// middleware to finalise.
///
/// The [`crate::request_id`] middleware reads this back out and builds the final
/// envelope with the request id folded in. Kept distinct from [`ApiError`] (the
/// handler-facing type) because it must be `Clone` for the response-extension
/// round-trip.
#[derive(Debug, Clone)]
pub struct ApiErrorPayload {
    /// The error code (fixes the status and the wire string).
    pub code: ErrorCode,
    /// The human-readable message.
    pub message: String,
    /// Optional structured details.
    pub details: Option<Value>,
}

impl IntoResponse for ApiError {
    /// Produce a response carrying the status and an [`ApiErrorPayload`] in the
    /// response extensions. The body is finalised — request id folded in — by
    /// the [`crate::request_id`] middleware that wraps the whole router. Internal
    /// errors are logged here, where the real message is still in hand, and the
    /// outward message is genericised so server internals never leak.
    fn into_response(self) -> Response {
        if self.code == ErrorCode::Internal {
            tracing::error!(target: "cascade::web", message = %self.message, "internal API error");
        }
        let message = if self.code == ErrorCode::Internal {
            "an internal server error occurred".to_owned()
        } else {
            self.message
        };
        let payload = ApiErrorPayload {
            code: self.code,
            message,
            details: self.details,
        };
        (self.code.status(), Extension(payload)).into_response()
    }
}
