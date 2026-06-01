//! Handler trait for inbound File Provider RPC calls.
//!
//! The trait abstracts the seven RPC entry points the Swift File Provider
//! extension issues. The production implementation
//! ([`crate::engine_handlers::EngineHandlers`]) routes calls through the
//! Cascade engine; test stubs implement the same trait against in-memory
//! state so the server can be exercised without spinning up a real engine.
//!
//! Errors are returned as a typed [`HandlerError`] carrying both a
//! machine-readable [`ErrorCode`] and a human-readable message. The server
//! turns each into an [`crate::wire::RpcError`] on the wire.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::items::FileProviderItem;

/// Stable error codes returned by the handler trait.
///
/// Each variant maps to a specific `NSFileProviderError` case on the Swift
/// side. The string form is the wire identifier — the Swift extension
/// switches on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The requested item does not exist. Maps to `.noSuchItem`.
    NotFound,
    /// The operation was rejected by policy. Maps to `.notAuthenticated`.
    PermissionDenied,
    /// A name collision prevents the operation. Maps to `.filenameCollision`.
    AlreadyExists,
    /// Any other engine error. Maps to a generic error on Swift's side;
    /// the message is intended for logging only.
    Internal,
}

impl ErrorCode {
    /// Wire-format identifier (`snake_case`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::PermissionDenied => "permission_denied",
            Self::AlreadyExists => "already_exists",
            Self::Internal => "internal",
        }
    }
}

/// Error returned by every handler method.
///
/// The server turns this into an [`crate::wire::RpcError`] before writing
/// the response. The `message` field is free-form; callers may surface it
/// in logs but the Swift side keys behaviour off `code`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandlerError {
    pub code: ErrorCode,
    pub message: String,
}

impl HandlerError {
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }

    #[must_use]
    pub fn permission_denied(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::PermissionDenied, message)
    }

    #[must_use]
    pub fn already_exists(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::AlreadyExists, message)
    }

    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }
}

impl std::fmt::Display for HandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for HandlerError {}

/// Convert an [`anyhow::Error`] coming back from the engine into a
/// `HandlerError`. Backend errors that downcast to
/// [`cascade_engine::backend::BackendError`] are mapped to their natural
/// code; anything else collapses to [`ErrorCode::Internal`].
impl From<anyhow::Error> for HandlerError {
    fn from(err: anyhow::Error) -> Self {
        use cascade_engine::backend::BackendError;
        if let Some(backend_error) = err.downcast_ref::<BackendError>() {
            return match backend_error {
                BackendError::NotFound(msg) => Self::not_found(msg.clone()),
                BackendError::Forbidden(msg) | BackendError::ReadOnly(msg) => {
                    Self::permission_denied(msg.clone())
                }
                BackendError::Conflict(msg) => Self::already_exists(msg.clone()),
            };
        }
        Self::internal(err.to_string())
    }
}

/// Result alias for handler methods.
pub type HandlerResult<T> = Result<T, HandlerError>;

/// Trait implemented by anything that can answer the seven inbound RPC
/// methods the Swift File Provider extension issues.
///
/// The server is generic over this trait, so tests can substitute an
/// in-memory stub for the real engine-backed implementation.
#[async_trait]
pub trait FileProviderHandlers: Send + Sync + std::fmt::Debug {
    /// Look up an item by ID.
    async fn get_item(&self, id: &str) -> HandlerResult<FileProviderItem>;

    /// Enumerate children of `parent_id`. `page` is an opaque cursor; an
    /// implementation that does not paginate ignores it and returns
    /// `next_page = None`.
    async fn enumerate_items(
        &self,
        parent_id: &str,
        page: Option<&str>,
    ) -> HandlerResult<EnumerateOutput>;

    /// Materialise the file content for `id` to local disk and return the
    /// resulting path. Implementations should be idempotent — calling
    /// again for the same `id` may return the same path.
    async fn fetch_contents(&self, id: &str) -> HandlerResult<PathBuf>;

    /// Import bytes from `source_url` into the engine under `parent_id`.
    /// `existing_id` set means overwrite; otherwise create a new file.
    /// When `name` is `None`, the implementation may derive it from the
    /// source URL's filename.
    async fn import_document(
        &self,
        source_url: &str,
        parent_id: &str,
        name: Option<&str>,
        existing_id: Option<&str>,
    ) -> HandlerResult<FileProviderItem>;

    /// Create a new directory named `name` under `parent_id`.
    async fn create_directory(
        &self,
        name: &str,
        parent_id: &str,
    ) -> HandlerResult<FileProviderItem>;

    /// Delete an item by ID.
    async fn delete_item(&self, id: &str) -> HandlerResult<()>;

    /// Move/rename an item.
    async fn move_item(
        &self,
        id: &str,
        new_parent_id: &str,
        new_name: &str,
    ) -> HandlerResult<FileProviderItem>;
}

/// Return value for [`FileProviderHandlers::enumerate_items`].
#[derive(Debug, Clone)]
pub struct EnumerateOutput {
    pub items: Vec<FileProviderItem>,
    pub next_page: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use cascade_engine::backend::BackendError;

    #[test]
    fn error_code_strings_are_stable() {
        assert_eq!(ErrorCode::NotFound.as_str(), "not_found");
        assert_eq!(ErrorCode::PermissionDenied.as_str(), "permission_denied");
        assert_eq!(ErrorCode::AlreadyExists.as_str(), "already_exists");
        assert_eq!(ErrorCode::Internal.as_str(), "internal");
    }

    #[test]
    fn anyhow_not_found_maps_to_not_found_code() {
        let err: anyhow::Error = BackendError::NotFound("file.txt".to_string()).into();
        let handler_err: HandlerError = err.into();
        assert_eq!(handler_err.code, ErrorCode::NotFound);
        assert!(handler_err.message.contains("file.txt"));
    }

    #[test]
    fn anyhow_forbidden_maps_to_permission_denied() {
        let err: anyhow::Error = BackendError::Forbidden("read only".to_string()).into();
        let handler_err: HandlerError = err.into();
        assert_eq!(handler_err.code, ErrorCode::PermissionDenied);
    }

    #[test]
    fn anyhow_read_only_maps_to_permission_denied() {
        let err: anyhow::Error = BackendError::ReadOnly("bin".to_string()).into();
        let handler_err: HandlerError = err.into();
        assert_eq!(handler_err.code, ErrorCode::PermissionDenied);
    }

    #[test]
    fn anyhow_conflict_maps_to_already_exists() {
        let err: anyhow::Error = BackendError::Conflict("name taken".to_string()).into();
        let handler_err: HandlerError = err.into();
        assert_eq!(handler_err.code, ErrorCode::AlreadyExists);
    }

    #[test]
    fn anyhow_generic_maps_to_internal() {
        let err = anyhow!("network blew up");
        let handler_err: HandlerError = err.into();
        assert_eq!(handler_err.code, ErrorCode::Internal);
        assert!(handler_err.message.contains("network blew up"));
    }
}
