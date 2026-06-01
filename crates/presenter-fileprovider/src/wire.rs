//! Wire types for inbound File Provider RPC calls.
//!
//! The Swift File Provider extension sends each call as a JSON `Request`
//! and expects a JSON `Response`. The request shape matches the engine's
//! shared [`cascade_engine::protocol::Request`]. The response shape extends
//! it with a structured `error` object so the Swift side can map errors
//! to specific `NSFileProviderError` cases.
//!
//! Each handler module defines its own params/result types and re-exports
//! them here.

use cascade_engine::types::SyncCursor;
use serde::{Deserialize, Serialize};

use crate::items::FileProviderItem;

/// Inbound RPC method names.
///
/// The Swift side issues calls keyed by these strings; the server dispatches
/// on them. Kept as `&'static str` constants to ensure both sides use the
/// same spelling — adding a new method means adding a constant here and a
/// branch in the dispatcher.
pub mod methods {
    pub const GET_ITEM: &str = "getItem";
    pub const ENUMERATE_ITEMS: &str = "enumerateItems";
    pub const FETCH_CONTENTS: &str = "fetchContents";
    pub const IMPORT_DOCUMENT: &str = "importDocument";
    pub const CREATE_DIRECTORY: &str = "createDirectory";
    pub const DELETE_ITEM: &str = "deleteItem";
    pub const MOVE_ITEM: &str = "moveItem";
    pub const CURRENT_SYNC_CURSOR: &str = "currentSyncCursor";
    pub const ENUMERATE_CHANGES: &str = "enumerateChanges";
}

/// Structured error returned in an [`RpcResponse`].
///
/// `code` is one of the well-known machine-readable strings defined by
/// [`crate::handlers::ErrorCode`]; `message` is a free-form human-readable
/// string suitable for logging on the Swift side.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RpcError {
    pub code: String,
    pub message: String,
}

/// Inbound RPC response envelope.
///
/// Mirrors [`cascade_engine::protocol::Response`] but uses a structured
/// `error` object so the Swift side can switch on `code`. Successful
/// responses carry `result`; failed responses carry `error`. Exactly one
/// of the two is present.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl RpcResponse {
    #[must_use]
    pub const fn ok(id: u32, result: serde_json::Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }

    #[must_use]
    pub const fn err(id: u32, error: RpcError) -> Self {
        Self {
            id,
            result: None,
            error: Some(error),
        }
    }
}

// ---------------------------------------------------------------------------
// getItem
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct GetItemParams {
    pub id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GetItemResult {
    pub item: FileProviderItem,
}

// ---------------------------------------------------------------------------
// enumerateItems
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct EnumerateItemsParams {
    pub parent_id: String,
    #[serde(default)]
    pub page: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnumerateItemsResult {
    pub items: Vec<FileProviderItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_page: Option<String>,
}

// ---------------------------------------------------------------------------
// fetchContents
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct FetchContentsParams {
    pub id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FetchContentsResult {
    pub path: String,
}

// ---------------------------------------------------------------------------
// importDocument
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct ImportDocumentParams {
    pub source_url: String,
    pub parent_id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub existing_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportDocumentResult {
    pub item: FileProviderItem,
}

// ---------------------------------------------------------------------------
// createDirectory
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct CreateDirectoryParams {
    pub name: String,
    pub parent_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateDirectoryResult {
    pub item: FileProviderItem,
}

// ---------------------------------------------------------------------------
// deleteItem
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct DeleteItemParams {
    pub id: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct DeleteItemResult {}

// ---------------------------------------------------------------------------
// moveItem
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct MoveItemParams {
    pub id: String,
    pub new_parent_id: String,
    pub new_name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MoveItemResult {
    pub item: FileProviderItem,
}

// ---------------------------------------------------------------------------
// currentSyncCursor
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct CurrentSyncCursorParams {
    pub parent_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CurrentSyncCursorResult {
    pub cursor: SyncCursor,
}

// ---------------------------------------------------------------------------
// enumerateChanges
// ---------------------------------------------------------------------------

/// Parameters for the `enumerateChanges` RPC.
///
/// `since_cursor` is the opaque cursor the Swift File Provider extension
/// last persisted (the one the engine handed back via either
/// `currentSyncCursor` or a prior `enumerateChanges` call). `None` — or a
/// cursor the engine no longer recognises — means "start fresh": the
/// engine returns every current child as `added_or_modified` and emits a
/// new cursor describing the snapshot it just took.
#[derive(Debug, Clone, Deserialize)]
pub struct EnumerateChangesParams {
    pub parent_id: String,
    #[serde(default)]
    pub since_cursor: Option<SyncCursor>,
}

/// Result of the `enumerateChanges` RPC.
///
/// `added_or_modified` carries items that are present in the current view
/// but were either absent from the prior snapshot or have a different
/// metadata tuple. `deleted` carries the IDs of items present in the
/// prior snapshot that no longer exist. `new_cursor` is the cursor the
/// caller must echo on the next `enumerateChanges` call to receive an
/// incremental delta — passing back a stale or unknown cursor falls
/// through to the "first call" behaviour.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnumerateChangesResult {
    pub added_or_modified: Vec<FileProviderItem>,
    pub deleted: Vec<String>,
    pub new_cursor: SyncCursor,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rpc_response_ok_serialises_without_error_field() {
        let response = RpcResponse::ok(1, json!({"a": 1}));
        let encoded = serde_json::to_value(&response).unwrap();
        assert_eq!(encoded["id"], 1);
        assert_eq!(encoded["result"], json!({"a": 1}));
        assert!(encoded.get("error").is_none());
    }

    #[test]
    fn rpc_response_err_serialises_without_result_field() {
        let response = RpcResponse::err(
            7,
            RpcError {
                code: "not_found".to_string(),
                message: "missing".to_string(),
            },
        );
        let encoded = serde_json::to_value(&response).unwrap();
        assert_eq!(encoded["id"], 7);
        assert_eq!(encoded["error"]["code"], "not_found");
        assert_eq!(encoded["error"]["message"], "missing");
        assert!(encoded.get("result").is_none());
    }

    #[test]
    fn enumerate_items_params_accepts_missing_page() {
        let params: EnumerateItemsParams =
            serde_json::from_value(json!({"parent_id": "gdrive:root"})).unwrap();
        assert_eq!(params.parent_id, "gdrive:root");
        assert!(params.page.is_none());
    }

    #[test]
    fn current_sync_cursor_params_deserialises() {
        let params: CurrentSyncCursorParams =
            serde_json::from_value(json!({"parent_id": "gdrive:root"})).unwrap();
        assert_eq!(params.parent_id, "gdrive:root");
    }

    #[test]
    fn current_sync_cursor_result_round_trips() {
        let result = CurrentSyncCursorResult {
            cursor: SyncCursor::new(vec![1, 2, 3, 4]),
        };
        let encoded = serde_json::to_value(&result).unwrap();
        // SyncCursor encodes as base64url-no-pad string.
        assert!(encoded["cursor"].is_string());
        let decoded: CurrentSyncCursorResult = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.cursor.as_bytes(), &[1, 2, 3, 4]);
    }
}
