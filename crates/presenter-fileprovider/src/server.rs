//! Unix domain socket server that dispatches inbound File Provider RPC
//! calls to a [`crate::handlers::FileProviderHandlers`] implementation.
//!
//! The Swift `ActionHandler` opens a fresh socket per call, writes one
//! length-prefixed JSON [`cascade_engine::protocol::Request`], reads back
//! one length-prefixed [`crate::wire::RpcResponse`], and closes. The
//! server mirrors that shape: accept a connection, read one request,
//! dispatch, write one response, drop the connection.
//!
//! The server task is spawned with [`FileProviderServer::serve`]. It runs
//! until the cancel signal fires or the listener is dropped.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use cascade_engine::protocol::{Request, encode_message};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;

/// File mode applied to the RPC socket after binding.
///
/// `0o600` — owner read/write only. The socket carries privileged engine
/// RPCs (full filesystem read/write); anyone with connect access can issue
/// them, so the socket must never be readable by other local users. The
/// daemon's process umask is not relied upon — we set the mode explicitly
/// after `bind`.
const SOCKET_MODE: u32 = 0o600;

use crate::handlers::{FileProviderHandlers, HandlerError, HandlerResult};
use crate::items::FileProviderItem;
use crate::wire::{
    CreateDirectoryParams, CreateDirectoryResult, CurrentSyncCursorParams, CurrentSyncCursorResult,
    DeleteItemParams, DeleteItemResult, EnumerateItemsParams, EnumerateItemsResult,
    FetchContentsParams, FetchContentsResult, GetItemParams, GetItemResult, ImportDocumentParams,
    ImportDocumentResult, MoveItemParams, MoveItemResult, RpcError, RpcResponse, methods,
};

/// Server that listens on a Unix domain socket and dispatches each
/// incoming [`Request`] to a [`FileProviderHandlers`].
#[derive(Debug)]
pub struct FileProviderServer {
    socket_path: PathBuf,
    handlers: Arc<dyn FileProviderHandlers>,
}

impl FileProviderServer {
    pub fn new(socket_path: impl Into<PathBuf>, handlers: Arc<dyn FileProviderHandlers>) -> Self {
        Self {
            socket_path: socket_path.into(),
            handlers,
        }
    }

    /// Socket path the server listens on.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Bind the listener and serve until `cancel` fires.
    ///
    /// The socket path is removed before binding if a stale file exists, so
    /// the daemon can restart cleanly after a crash. The parent directory is
    /// created on demand.
    ///
    /// # Security
    ///
    /// The socket file is chmoded to `0o600` (owner read/write only)
    /// immediately after binding. The socket carries privileged engine RPCs
    /// — any connecting client can issue filesystem operations — so the
    /// permissions must restrict access to the owning user. Any future
    /// change to the bind sequence must preserve this invariant; rely on
    /// explicit `set_permissions`, not the process umask.
    pub async fn serve(&self, mut cancel: watch::Receiver<bool>) -> Result<()> {
        if let Some(parent) = self.socket_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("create socket parent directory {}", parent.display()))?;
        }
        if self.socket_path.exists() {
            tokio::fs::remove_file(&self.socket_path)
                .await
                .with_context(|| format!("remove stale socket {}", self.socket_path.display()))?;
        }

        let listener = UnixListener::bind(&self.socket_path)
            .with_context(|| format!("bind socket {}", self.socket_path.display()))?;
        tokio::fs::set_permissions(
            &self.socket_path,
            std::fs::Permissions::from_mode(SOCKET_MODE),
        )
        .await
        .with_context(|| {
            format!(
                "chmod socket {} to {SOCKET_MODE:#o}",
                self.socket_path.display()
            )
        })?;
        tracing::info!(socket = %self.socket_path.display(), "file provider server listening");

        loop {
            tokio::select! {
                accept = listener.accept() => {
                    match accept {
                        Ok((stream, _)) => {
                            let handlers = Arc::clone(&self.handlers);
                            tokio::spawn(async move {
                                if let Err(error) = serve_connection(stream, handlers).await {
                                    tracing::warn!(%error, "file provider connection failed");
                                }
                            });
                        }
                        Err(error) => {
                            tracing::warn!(%error, "file provider accept failed");
                        }
                    }
                }
                _ = cancel.changed() => {
                    if *cancel.borrow() {
                        tracing::info!("file provider server shutting down");
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Read one length-prefixed request, dispatch, and write one response.
async fn serve_connection(
    mut stream: UnixStream,
    handlers: Arc<dyn FileProviderHandlers>,
) -> Result<()> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .context("read request length")?;
    let body_len = u32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; body_len];
    stream
        .read_exact(&mut body)
        .await
        .context("read request body")?;

    let request: Request = serde_json::from_slice(&body).context("decode request body")?;
    let response = dispatch(handlers.as_ref(), &request).await;

    let encoded = encode_message(&response).context("encode response")?;
    stream.write_all(&encoded).await.context("write response")?;
    stream.flush().await.context("flush response")?;
    Ok(())
}

/// Route a request to the appropriate handler and build a [`RpcResponse`].
pub async fn dispatch(handlers: &dyn FileProviderHandlers, request: &Request) -> RpcResponse {
    let id = request.id;
    let method = request.method.as_str();
    let params = request.params.clone();

    let outcome: HandlerResult<serde_json::Value> = match method {
        methods::GET_ITEM => handle_get_item(handlers, params).await,
        methods::ENUMERATE_ITEMS => handle_enumerate_items(handlers, params).await,
        methods::FETCH_CONTENTS => handle_fetch_contents(handlers, params).await,
        methods::IMPORT_DOCUMENT => handle_import_document(handlers, params).await,
        methods::CREATE_DIRECTORY => handle_create_directory(handlers, params).await,
        methods::DELETE_ITEM => handle_delete_item(handlers, params).await,
        methods::MOVE_ITEM => handle_move_item(handlers, params).await,
        methods::CURRENT_SYNC_CURSOR => handle_current_sync_cursor(handlers, params).await,
        other => Err(HandlerError::internal(format!(
            "unknown File Provider RPC method: {other}"
        ))),
    };

    match outcome {
        Ok(value) => RpcResponse::ok(id, value),
        Err(error) => RpcResponse::err(
            id,
            RpcError {
                code: error.code.as_str().to_string(),
                message: error.message,
            },
        ),
    }
}

fn parse_params<T: serde::de::DeserializeOwned>(
    method: &str,
    params: serde_json::Value,
) -> HandlerResult<T> {
    serde_json::from_value(params)
        .map_err(|error| HandlerError::internal(format!("invalid params for {method}: {error}")))
}

fn serialise_result<T: serde::Serialize>(
    method: &str,
    value: &T,
) -> HandlerResult<serde_json::Value> {
    serde_json::to_value(value).map_err(|error| {
        HandlerError::internal(format!("failed to encode {method} result: {error}"))
    })
}

async fn handle_get_item(
    handlers: &dyn FileProviderHandlers,
    params: serde_json::Value,
) -> HandlerResult<serde_json::Value> {
    let params: GetItemParams = parse_params(methods::GET_ITEM, params)?;
    let item: FileProviderItem = handlers.get_item(&params.id).await?;
    serialise_result(methods::GET_ITEM, &GetItemResult { item })
}

async fn handle_enumerate_items(
    handlers: &dyn FileProviderHandlers,
    params: serde_json::Value,
) -> HandlerResult<serde_json::Value> {
    let params: EnumerateItemsParams = parse_params(methods::ENUMERATE_ITEMS, params)?;
    let output = handlers
        .enumerate_items(&params.parent_id, params.page.as_deref())
        .await?;
    serialise_result(
        methods::ENUMERATE_ITEMS,
        &EnumerateItemsResult {
            items: output.items,
            next_page: output.next_page,
        },
    )
}

async fn handle_fetch_contents(
    handlers: &dyn FileProviderHandlers,
    params: serde_json::Value,
) -> HandlerResult<serde_json::Value> {
    let params: FetchContentsParams = parse_params(methods::FETCH_CONTENTS, params)?;
    let path = handlers.fetch_contents(&params.id).await?;
    serialise_result(
        methods::FETCH_CONTENTS,
        &FetchContentsResult {
            path: path.to_string_lossy().into_owned(),
        },
    )
}

async fn handle_import_document(
    handlers: &dyn FileProviderHandlers,
    params: serde_json::Value,
) -> HandlerResult<serde_json::Value> {
    let params: ImportDocumentParams = parse_params(methods::IMPORT_DOCUMENT, params)?;
    let item = handlers
        .import_document(
            &params.source_url,
            &params.parent_id,
            params.name.as_deref(),
            params.existing_id.as_deref(),
        )
        .await?;
    serialise_result(methods::IMPORT_DOCUMENT, &ImportDocumentResult { item })
}

async fn handle_create_directory(
    handlers: &dyn FileProviderHandlers,
    params: serde_json::Value,
) -> HandlerResult<serde_json::Value> {
    let params: CreateDirectoryParams = parse_params(methods::CREATE_DIRECTORY, params)?;
    let item = handlers
        .create_directory(&params.name, &params.parent_id)
        .await?;
    serialise_result(methods::CREATE_DIRECTORY, &CreateDirectoryResult { item })
}

async fn handle_delete_item(
    handlers: &dyn FileProviderHandlers,
    params: serde_json::Value,
) -> HandlerResult<serde_json::Value> {
    let params: DeleteItemParams = parse_params(methods::DELETE_ITEM, params)?;
    handlers.delete_item(&params.id).await?;
    serialise_result(methods::DELETE_ITEM, &DeleteItemResult {})
}

async fn handle_move_item(
    handlers: &dyn FileProviderHandlers,
    params: serde_json::Value,
) -> HandlerResult<serde_json::Value> {
    let params: MoveItemParams = parse_params(methods::MOVE_ITEM, params)?;
    let item = handlers
        .move_item(&params.id, &params.new_parent_id, &params.new_name)
        .await?;
    serialise_result(methods::MOVE_ITEM, &MoveItemResult { item })
}

async fn handle_current_sync_cursor(
    handlers: &dyn FileProviderHandlers,
    params: serde_json::Value,
) -> HandlerResult<serde_json::Value> {
    let params: CurrentSyncCursorParams = parse_params(methods::CURRENT_SYNC_CURSOR, params)?;
    let cursor = handlers.current_sync_cursor(&params.parent_id).await?;
    serialise_result(
        methods::CURRENT_SYNC_CURSOR,
        &CurrentSyncCursorResult { cursor },
    )
}

// Re-export so callers can build a server without pulling the
// `wire::methods` constants individually.
#[doc(inline)]
pub use crate::wire::methods as method_names;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::{EnumerateOutput, ErrorCode};
    use async_trait::async_trait;
    use cascade_engine::protocol::Request;
    use cascade_engine::types::{CacheState, SyncCursor};
    use serde_json::json;
    use std::sync::Mutex;

    /// In-memory stub handler used to exercise the dispatcher without an
    /// engine. Each method records its invocation so tests can assert it
    /// was reached with the expected parameters.
    #[derive(Debug, Default)]
    struct StubHandlers {
        calls: Mutex<Vec<String>>,
        force_not_found: bool,
    }

    impl StubHandlers {
        fn record(&self, method: &str) {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(method.to_string());
        }
    }

    fn sample_item(id: &str, parent_id: &str, name: &str) -> FileProviderItem {
        FileProviderItem {
            id: id.to_string(),
            parent_id: parent_id.to_string(),
            filename: name.to_string(),
            is_directory: false,
            size: Some(0),
            content_type: None,
            last_modified: None,
            cache_state: CacheState::Online,
        }
    }

    #[async_trait]
    impl FileProviderHandlers for StubHandlers {
        async fn get_item(&self, id: &str) -> HandlerResult<FileProviderItem> {
            self.record("get_item");
            if self.force_not_found {
                return Err(HandlerError::not_found(id.to_string()));
            }
            Ok(sample_item(id, "gdrive:root", "sample.txt"))
        }

        async fn enumerate_items(
            &self,
            parent_id: &str,
            _page: Option<&str>,
        ) -> HandlerResult<EnumerateOutput> {
            self.record("enumerate_items");
            Ok(EnumerateOutput {
                items: vec![sample_item("gdrive:a", parent_id, "a.txt")],
                next_page: None,
            })
        }

        async fn fetch_contents(&self, id: &str) -> HandlerResult<PathBuf> {
            self.record("fetch_contents");
            Ok(PathBuf::from(format!("/tmp/{id}")))
        }

        async fn import_document(
            &self,
            _source_url: &str,
            parent_id: &str,
            name: Option<&str>,
            _existing_id: Option<&str>,
        ) -> HandlerResult<FileProviderItem> {
            self.record("import_document");
            Ok(sample_item(
                "gdrive:new",
                parent_id,
                name.unwrap_or("imported.bin"),
            ))
        }

        async fn create_directory(
            &self,
            name: &str,
            parent_id: &str,
        ) -> HandlerResult<FileProviderItem> {
            self.record("create_directory");
            let mut item = sample_item("gdrive:dir", parent_id, name);
            item.is_directory = true;
            item.size = None;
            Ok(item)
        }

        async fn delete_item(&self, _id: &str) -> HandlerResult<()> {
            self.record("delete_item");
            Ok(())
        }

        async fn move_item(
            &self,
            id: &str,
            new_parent_id: &str,
            new_name: &str,
        ) -> HandlerResult<FileProviderItem> {
            self.record("move_item");
            Ok(sample_item(id, new_parent_id, new_name))
        }

        async fn current_sync_cursor(&self, _parent_id: &str) -> HandlerResult<SyncCursor> {
            self.record("current_sync_cursor");
            Ok(SyncCursor::new(vec![0xab, 0xcd, 0xef]))
        }
    }

    fn make_request(id: u32, method: &str, params: serde_json::Value) -> Request {
        Request {
            id,
            method: method.to_string(),
            params,
        }
    }

    #[tokio::test]
    async fn dispatch_get_item_routes_to_handler() {
        let handlers = StubHandlers::default();
        let request = make_request(1, methods::GET_ITEM, json!({"id": "gdrive:abc"}));

        let response = dispatch(&handlers, &request).await;

        assert_eq!(response.id, 1);
        assert!(response.error.is_none());
        let result = response.result.unwrap();
        assert_eq!(result["item"]["id"], "gdrive:abc");
        assert!(
            handlers
                .calls
                .lock()
                .unwrap()
                .contains(&"get_item".to_string())
        );
    }

    #[tokio::test]
    async fn dispatch_enumerate_items_returns_array() {
        let handlers = StubHandlers::default();
        let request = make_request(
            2,
            methods::ENUMERATE_ITEMS,
            json!({"parent_id": "gdrive:root"}),
        );

        let response = dispatch(&handlers, &request).await;

        let result = response.result.unwrap();
        let items = result["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["parent_id"], "gdrive:root");
    }

    #[tokio::test]
    async fn dispatch_fetch_contents_returns_path() {
        let handlers = StubHandlers::default();
        let request = make_request(3, methods::FETCH_CONTENTS, json!({"id": "gdrive:f"}));

        let response = dispatch(&handlers, &request).await;

        let result = response.result.unwrap();
        assert_eq!(result["path"], "/tmp/gdrive:f");
    }

    #[tokio::test]
    async fn dispatch_import_document_returns_item() {
        let handlers = StubHandlers::default();
        let request = make_request(
            4,
            methods::IMPORT_DOCUMENT,
            json!({
                "source_url": "/tmp/source.txt",
                "parent_id": "gdrive:root",
                "name": "imported.txt"
            }),
        );

        let response = dispatch(&handlers, &request).await;

        let result = response.result.unwrap();
        assert_eq!(result["item"]["filename"], "imported.txt");
    }

    #[tokio::test]
    async fn dispatch_create_directory_returns_directory_item() {
        let handlers = StubHandlers::default();
        let request = make_request(
            5,
            methods::CREATE_DIRECTORY,
            json!({"name": "Photos", "parent_id": "gdrive:root"}),
        );

        let response = dispatch(&handlers, &request).await;

        let result = response.result.unwrap();
        assert_eq!(result["item"]["filename"], "Photos");
        assert_eq!(result["item"]["is_directory"], true);
    }

    #[tokio::test]
    async fn dispatch_delete_item_returns_empty_object() {
        let handlers = StubHandlers::default();
        let request = make_request(6, methods::DELETE_ITEM, json!({"id": "gdrive:x"}));

        let response = dispatch(&handlers, &request).await;

        let result = response.result.unwrap();
        assert_eq!(result, json!({}));
    }

    #[tokio::test]
    async fn dispatch_current_sync_cursor_returns_cursor_string() {
        let handlers = StubHandlers::default();
        let request = make_request(
            12,
            methods::CURRENT_SYNC_CURSOR,
            json!({"parent_id": "gdrive:root"}),
        );

        let response = dispatch(&handlers, &request).await;

        assert!(response.error.is_none());
        let result = response.result.unwrap();
        // Cursor encodes as base64url-no-pad of [0xab, 0xcd, 0xef] = "q83v".
        assert_eq!(result["cursor"], "q83v");
    }

    #[tokio::test]
    async fn dispatch_move_item_returns_updated_item() {
        let handlers = StubHandlers::default();
        let request = make_request(
            7,
            methods::MOVE_ITEM,
            json!({
                "id": "gdrive:f",
                "new_parent_id": "gdrive:other",
                "new_name": "renamed.txt"
            }),
        );

        let response = dispatch(&handlers, &request).await;

        let result = response.result.unwrap();
        assert_eq!(result["item"]["parent_id"], "gdrive:other");
        assert_eq!(result["item"]["filename"], "renamed.txt");
    }

    #[tokio::test]
    async fn dispatch_unknown_method_returns_internal_error() {
        let handlers = StubHandlers::default();
        let request = make_request(8, "doesNotExist", json!({}));

        let response = dispatch(&handlers, &request).await;

        let error = response.error.unwrap();
        assert_eq!(error.code, ErrorCode::Internal.as_str());
        assert!(error.message.contains("doesNotExist"));
    }

    #[tokio::test]
    async fn dispatch_not_found_propagates_code() {
        let handlers = StubHandlers {
            force_not_found: true,
            ..StubHandlers::default()
        };
        let request = make_request(9, methods::GET_ITEM, json!({"id": "gdrive:missing"}));

        let response = dispatch(&handlers, &request).await;

        let error = response.error.unwrap();
        assert_eq!(error.code, ErrorCode::NotFound.as_str());
        assert_eq!(error.message, "gdrive:missing");
    }

    #[tokio::test]
    async fn dispatch_malformed_params_returns_internal_error() {
        let handlers = StubHandlers::default();
        let request = make_request(10, methods::GET_ITEM, json!({"wrong": "field"}));

        let response = dispatch(&handlers, &request).await;

        let error = response.error.unwrap();
        assert_eq!(error.code, ErrorCode::Internal.as_str());
        assert!(error.message.contains("invalid params"));
    }

    #[tokio::test]
    async fn server_chmods_socket_to_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("perms.sock");

        let handlers: Arc<dyn FileProviderHandlers> = Arc::new(StubHandlers::default());
        let server = FileProviderServer::new(&socket_path, handlers);

        let (cancel_tx, cancel_rx) = watch::channel(false);
        let probe_path = socket_path.clone();
        let handle = tokio::spawn(async move { server.serve(cancel_rx).await });

        // Wait for the socket to appear.
        for _ in 0..50 {
            if probe_path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(probe_path.exists(), "server failed to bind socket");

        let metadata = tokio::fs::metadata(&probe_path).await.unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "socket permission bits {mode:#o} should be 0o600 (owner read/write only)"
        );

        cancel_tx.send(true).unwrap();
        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn server_round_trips_a_request_over_a_socket() {
        use tokio::net::UnixStream as ClientStream;

        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("server.sock");

        let handlers: Arc<dyn FileProviderHandlers> = Arc::new(StubHandlers::default());
        let server = FileProviderServer::new(&socket_path, handlers);

        let (cancel_tx, cancel_rx) = watch::channel(false);
        let server_socket = socket_path.clone();
        let handle = tokio::spawn(async move { server.serve(cancel_rx).await });

        // Wait for the socket to appear.
        for _ in 0..50 {
            if server_socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(server_socket.exists(), "server failed to bind socket");

        // Connect and send a real request.
        let mut stream = ClientStream::connect(&server_socket).await.unwrap();
        let request = make_request(11, methods::GET_ITEM, json!({"id": "gdrive:hello"}));
        let encoded = encode_message(&request).unwrap();
        stream.write_all(&encoded).await.unwrap();

        // Read the response.
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await.unwrap();
        let body_len = u32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; body_len];
        stream.read_exact(&mut body).await.unwrap();
        let response: RpcResponse = serde_json::from_slice(&body).unwrap();

        assert_eq!(response.id, 11);
        let result = response.result.unwrap();
        assert_eq!(result["item"]["id"], "gdrive:hello");

        // Shut down the server.
        cancel_tx.send(true).unwrap();
        handle.abort();
        let _ = handle.await;
    }
}
