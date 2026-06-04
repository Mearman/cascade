//! Cross-resource schema pieces: pagination and the error envelope.

use serde::{Deserialize, Serialize};

/// Default page size when the caller does not pass `limit`.
pub const DEFAULT_PAGE_LIMIT: usize = 50;
/// Smallest accepted page size; smaller values clamp up to it.
pub const MIN_PAGE_LIMIT: usize = 1;
/// Largest accepted page size; larger values clamp down to it.
pub const MAX_PAGE_LIMIT: usize = 200;

/// Query parameters for paginated list routes.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Pagination {
    /// Requested page size (clamped to `[MIN_PAGE_LIMIT, MAX_PAGE_LIMIT]`).
    pub limit: Option<usize>,
    /// Opaque cursor from the previous page's `next_cursor`.
    pub cursor: Option<String>,
}

impl Pagination {
    /// The page size to use, clamped to the accepted range.
    #[must_use]
    pub fn clamped_limit(&self) -> usize {
        self.limit
            .unwrap_or(DEFAULT_PAGE_LIMIT)
            .clamp(MIN_PAGE_LIMIT, MAX_PAGE_LIMIT)
    }
}

/// The error envelope, as the PWA (and the contract test) parse it. The server
/// builds this shape in the request-id middleware; this type exists so the wire
/// shape is named and testable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ErrorBody {
    /// The single error object.
    pub error: ErrorDetail,
}

/// The body of an [`ErrorBody`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ErrorDetail {
    /// The stable machine-readable code.
    pub code: String,
    /// A human-readable message, not part of the contract.
    pub message: String,
    /// The per-request id.
    pub request_id: String,
    /// Structured per-code context; absent when empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}
