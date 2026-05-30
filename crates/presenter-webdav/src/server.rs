//! `WebDAV` HTTP server using `axum`.
//!
//! Serves files from the in-memory VFS item store and on-disk cache via
//! standard `WebDAV` methods. Binds to `127.0.0.1:0` (random port) so
//! macOS `mount_webdav` can connect without root.

use std::collections::HashMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use cascade_engine::types::{FileId, ItemId, VfsItem};
use tokio::net::TcpListener;

/// Build a response with an explicit empty body and Content-Length: 0.
///
/// Run an async future on a completely separate OS thread with its own
/// tokio runtime, blocking the current thread until it completes.
/// This avoids any `.await` on the axum runtime after body consumption,
/// working around a hyper 1.x bug where responses get stuck.
fn run_isolated_blocking<F, T>(future: F) -> anyhow::Result<T>
where
    F: std::future::Future<Output = anyhow::Result<T>> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => {
                let _ = tx.send(rt.block_on(future));
            }
            Err(e) => {
                let _ = tx.send(Err(anyhow::anyhow!("isolated runtime build failed: {e}")));
            }
        }
    });
    rx.recv()
        .map_err(|e| anyhow::anyhow!("isolated thread terminated without result: {e}"))?
}

/// Build a response with explicit Content-Length: 0.
fn empty_response(status: StatusCode) -> Response {
    let mut resp = Response::new(axum::body::Body::empty());
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
    resp
}

/// Shared state passed to all axum handlers.
#[derive(Clone)]
pub struct AppState {
    /// In-memory VFS items keyed by `ItemId` string.
    pub items: Arc<RwLock<HashMap<String, VfsItem>>>,
    /// On-disk cache directory.
    pub cache_dir: PathBuf,
    /// Backends for on-demand directory expansion.
    pub backends: Arc<tokio::sync::RwLock<Vec<Arc<dyn cascade_engine::backend::Backend>>>>,
    /// State DB for persisting expanded items.
    pub db: Option<Arc<cascade_engine::db::StateDb>>,
    /// Directories already expanded (by `ItemId` string), to avoid
    /// redundant API calls when Finder sends repeated PROPFINDs.
    pub expanded: Arc<RwLock<std::collections::HashSet<String>>>,
    /// Semaphore limiting concurrent backend API calls during expansion.
    pub expand_sem: Arc<tokio::sync::Semaphore>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState").finish_non_exhaustive()
    }
}

/// Running `WebDAV` server handle.
#[derive(Debug)]
pub struct WebDavServer {
    /// Port the server is listening on.
    port: u16,
    shutdown: tokio::sync::oneshot::Sender<()>,
}

impl WebDavServer {
    /// Start the `WebDAV` server on the given bind address.
    ///
    /// The address should typically be `127.0.0.1:0` for an OS-assigned port.
    ///
    /// # Errors
    ///
    /// Returns an error if the TCP listener cannot bind.
    pub async fn start(
        bind_addr: &str,
        items: Arc<RwLock<HashMap<String, VfsItem>>>,
        cache_dir: PathBuf,
        backends: Arc<tokio::sync::RwLock<Vec<Arc<dyn cascade_engine::backend::Backend>>>>,
        db: Option<Arc<cascade_engine::db::StateDb>>,
    ) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(bind_addr).await?;
        let port = listener.local_addr()?.port();

        let state = AppState {
            items,
            cache_dir,
            backends,
            db,
            expanded: Arc::new(RwLock::new(std::collections::HashSet::new())),
            expand_sem: Arc::new(tokio::sync::Semaphore::new(4)),
        };

        let app = Router::new().fallback(webdav_handler).with_state(state);

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            let result = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await;
            if let Err(e) = result {
                tracing::error!(error = %e, "WebDAV server error");
            }
        });

        tracing::info!(port, "WebDAV server started");

        Ok(Self {
            port,
            shutdown: shutdown_tx,
        })
    }

    /// Return the port the server is listening on.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Stop the `WebDAV` server.
    ///
    /// # Errors
    ///
    /// Returns an error if the shutdown signal cannot be sent.
    pub fn stop(self) -> anyhow::Result<()> {
        let _ = self.shutdown.send(());
        tracing::info!("WebDAV server stopped");
        Ok(())
    }
}

/// Main `WebDAV` request handler — dispatches by HTTP method.
async fn webdav_handler(State(state): State<AppState>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    let method = req.method().clone();

    tracing::debug!(method = %method, path = %path, "WebDAV request");

    let mut resp = match method {
        Method::GET => handle_get(&state, &path).await,
        Method::PUT => handle_put(&state, &path, req).await,
        Method::DELETE => handle_delete(&state, &path).await,
        m if m == Method::from_bytes(b"MKCOL").unwrap_or_default() => {
            handle_mkcol(&state, &path).await
        }
        m if m == Method::from_bytes(b"PROPFIND").unwrap_or_default() => {
            handle_propfind(&state, &path, req.headers()).await
        }
        m if m == Method::from_bytes(b"MOVE").unwrap_or_default() => {
            handle_move(&state, &path, req.headers()).await
        }
        m if m == Method::from_bytes(b"COPY").unwrap_or_default() => {
            handle_copy(&state, &path, req.headers()).await
        }
        Method::OPTIONS => handle_options(),
        _ => empty_response(StatusCode::METHOD_NOT_ALLOWED),
    };

    // Force Connection: close on all responses to work around axum 0.8
    // HTTP/1.1 keep-alive bug where responses after body consumption
    // are never flushed.
    resp.headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    resp
}

/// Handle `OPTIONS` — return `WebDAV` compliance headers.
fn handle_options() -> Response {
    let mut resp = empty_response(StatusCode::NO_CONTENT);
    resp.headers_mut().insert(
        header::ALLOW,
        HeaderValue::from_static("GET, PUT, DELETE, MKCOL, PROPFIND, MOVE, COPY, OPTIONS, HEAD"),
    );
    resp.headers_mut().insert(
        header::HeaderName::from_static("dav"),
        HeaderValue::from_static("1, 2"),
    );
    resp
}

/// Handle `PROPFIND` — return `WebDAV` XML metadata for a resource.
async fn handle_propfind(state: &AppState, path: &str, headers: &HeaderMap) -> Response {
    let depth = headers
        .get("Depth")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("1");

    let normalised = normalise_path(path);

    // Root listing: show each backend as a top-level directory.
    if normalised == "/" {
        let items = read_items(&state.items).await;
        let mut backends: Vec<String> = items
            .values()
            .filter_map(|item| item.id.0.split(':').next().map(String::from))
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        backends.sort();
        let xml = build_root_response(&backends);
        return (
            StatusCode::MULTI_STATUS,
            [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
            xml,
        )
            .into_response();
    }

    // Find the target item (block scope to drop the guard).
    let target = {
        let items = read_items(&state.items).await;
        items
            .values()
            .find(|item| resolve_full_path(item, &items) == normalised)
            .cloned()
    };

    // Find children, expanding directories on demand if empty.
    let children: Vec<VfsItem> = if depth == "0" {
        Vec::new()
    } else if let Some(ref t) = target {
        let target_id = t.id.0.clone();
        let cached: Vec<VfsItem> = {
            let items = read_items(&state.items).await;
            items
                .values()
                .filter(|item| item.parent_id.0 == target_id)
                .cloned()
                .collect()
        };

        if cached.is_empty() && t.is_dir {
            // Try DB first, fall back to API.
            if hydrate_children_from_db(state, &target_id).await == 0 {
                expand_directory(state, &t.id).await;
            }
            let items = read_items(&state.items).await;
            items
                .values()
                .filter(|item| item.parent_id.0 == target_id)
                .cloned()
                .collect()
        } else {
            cached
        }
    } else {
        let backend_prefix = normalised.trim_start_matches('/').trim_end_matches('/');
        let root_id = format!("{backend_prefix}:root");
        let cached: Vec<VfsItem> = {
            let items = read_items(&state.items).await;
            items
                .values()
                .filter(|item| item.parent_id.0 == root_id)
                .cloned()
                .collect()
        };

        if cached.is_empty() {
            // Try DB first. The Google Drive API may store root children
            // under the real folder ID (e.g. "0APRsmt7LhxCIUk9PVA")
            // rather than the alias "root", so fall back to resolving
            // the actual root ID from the DB.
            let hydrated = hydrate_children_from_db(state, &root_id).await;
            let effective_root = if hydrated == 0 {
                if let Some(db) = &state.db {
                    if let Some(real_root) = resolve_root_id_from_db(db, backend_prefix) {
                        let n = hydrate_children_from_db(state, &real_root).await;
                        if n > 0 { Some(real_root) } else { None }
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                Some(root_id.clone())
            };

            if effective_root.is_none() {
                expand_root(state, backend_prefix).await;
            }

            // Re-read after hydration or API expansion. The effective root
            // may still be None if the API returned children under a real
            // folder ID rather than "root" — discover it from the items.
            let items = read_items(&state.items).await;
            let match_root = effective_root.unwrap_or_else(|| {
                // Find the most common parent_id among items with this
                // backend prefix that aren't in the expanded set.
                let prefix_colon = format!("{backend_prefix}:");
                let mut counts: std::collections::HashMap<String, usize> =
                    std::collections::HashMap::new();
                for item in items.values() {
                    if item.parent_id.0.starts_with(&prefix_colon) {
                        *counts.entry(item.parent_id.0.clone()).or_insert(0) += 1;
                    }
                }
                counts
                    .into_iter()
                    .max_by_key(|(_, c)| *c)
                    .map_or_else(|| root_id.clone(), |(id, _)| id)
            });
            items
                .values()
                .filter(|item| item.parent_id.0 == match_root)
                .cloned()
                .collect()
        } else {
            cached
        }
    };

    // Build response.
    let items = read_items(&state.items).await;
    let child_refs: Vec<&VfsItem> = children.iter().collect();
    let xml = build_propfind_response(&normalised, target.as_ref(), &child_refs, &items);
    (
        StatusCode::MULTI_STATUS,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        xml,
    )
        .into_response()
}

/// Handle `GET` — return file contents from the cache.
async fn handle_get(state: &AppState, path: &str) -> Response {
    let normalised = normalise_path(path);

    let (item_id, backend_id) = {
        let items = state.items.read().await;
        let found = items.values().find(|item| {
            let ip = resolve_full_path(item, &items);
            ip == normalised || ip == path
        });
        tracing::debug!(path = %normalised, found = found.is_some(), items_count = items.len(), "GET lookup");
        match found {
            Some(item) => (item.id.0.clone(), item.id.backend_id().to_string()),
            None => return empty_response(StatusCode::NOT_FOUND),
        }
    };

    // Try local cache first.
    let cache_path = state.cache_dir.join(safe_filename(&item_id));
    if let Ok(data) = tokio::fs::read(&cache_path).await {
        let mut response = Response::new(Body::from(data));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        return response;
    }

    // Fall back to backend download.
    let backend = {
        let backends = state.backends.read().await;
        backends.iter().find(|b| b.id() == backend_id).cloned()
    };
    let Some(backend) = backend else {
        return empty_response(StatusCode::NOT_FOUND);
    };

    // Find the FileEntry for this item.
    let file_entry = {
        let items = state.items.read().await;
        items
            .get(&item_id)
            .map(cascade_engine::types::FileEntry::from)
    };
    let Some(file_entry) = file_entry else {
        return empty_response(StatusCode::NOT_FOUND);
    };

    // Download to a cache file, then serve it.
    let _ = tokio::fs::create_dir_all(&state.cache_dir).await;
    let download_cache = state.cache_dir.join(safe_filename(&item_id));
    let mut file = match tokio::fs::File::create(&download_cache).await {
        Ok(f) => f,
        Err(e) => {
            tracing::error!(error = %e, "failed to create cache file");
            return empty_response(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    match backend.download(&file_entry, &mut file).await {
        Ok(()) => {
            drop(file);
            tokio::fs::read(&download_cache).await.map_or_else(
                |_| empty_response(StatusCode::INTERNAL_SERVER_ERROR),
                |data| {
                    let mut response = Response::new(Body::from(data));
                    response.headers_mut().insert(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/octet-stream"),
                    );
                    response
                },
            )
        }
        Err(e) => {
            tracing::warn!(error = %e, "backend download failed");
            let _ = tokio::fs::remove_file(&download_cache).await;
            empty_response(StatusCode::NOT_FOUND)
        }
    }
}

/// Handle `PUT` — store file contents to the cache.
async fn handle_put(state: &AppState, path: &str, req: Request) -> Response {
    let normalised = normalise_path(path);

    // Parse backend prefix and relative path.
    let parts: Vec<&str> = normalised.trim_start_matches('/').split('/').collect();
    let Some((&backend_id, relative)) = parts.split_first() else {
        return empty_response(StatusCode::BAD_REQUEST);
    };
    let relative_path = relative.join("/");
    let relative_path = Path::new(&relative_path);

    // Find the backend.
    let backend = {
        let backends = state.backends.read().await;
        backends.iter().find(|b| b.id() == backend_id).cloned()
    };
    let Some(backend) = backend else {
        return empty_response(StatusCode::NOT_FOUND);
    };

    let bytes = axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap_or_default();

    // Resolve the parent directory's native ID.
    let parent_id = if relative.is_empty() {
        cascade_engine::types::FileId(format!("{backend_id}:root"))
    } else {
        let parent_segments: Vec<&str> = if relative.len() > 1 {
            relative
                .get(..relative.len().saturating_sub(1))
                .map_or_else(Vec::new, ToOwned::to_owned)
        } else {
            vec![]
        };
        let parent_normalised = if parent_segments.is_empty() {
            format!("/{backend_id}/")
        } else {
            format!("/{backend_id}/{}", parent_segments.join("/"))
        };
        let parent_normalised = normalise_path(&parent_normalised);

        // Try in-memory store first.
        let found_in_items = {
            let items = state.items.read().await;
            items
                .values()
                .find(|item| {
                    let ip = resolve_full_path(item, &items);
                    ip == parent_normalised
                })
                .map(|p| cascade_engine::types::FileId(p.id.0.clone()))
        };

        if let Some(id) = found_in_items {
            tracing::debug!(parent = %id.0, sought = %parent_normalised, "parent found in items");
            id
        } else {
            // Fall back to backend metadata to resolve the parent.
            let parent_path_str = parent_segments.join("/");
            let parent_path = Path::new(&parent_path_str);
            tracing::debug!(path = %parent_normalised, "parent NOT found in items, trying backend metadata");
            if let Ok(parent_entry) = backend.metadata(parent_path).await {
                tracing::debug!(parent = %parent_entry.id.0, "parent found via backend");
                cascade_engine::types::FileId(parent_entry.id.0)
            } else {
                tracing::debug!(
                    path = %parent_normalised,
                    "parent not found via items or backend, uploading to root"
                );
                cascade_engine::types::FileId(format!("{backend_id}:root"))
            }
        }
    };

    let mut cursor = std::io::Cursor::new(bytes.clone());

    // Check if a file with the same name already exists in the parent directory.
    let existing_file_id = {
        let items = state.items.read().await;
        let file_normalised = normalise_path(path);
        items
            .values()
            .find(|item| {
                let ip = resolve_full_path(item, &items);
                ip == file_normalised
            })
            .map(|item| cascade_engine::types::FileId(item.id.0.clone()))
    };

    let relative_path_owned = relative_path.to_path_buf();
    let result = tokio::task::block_in_place(|| {
        run_isolated_blocking(async move {
            if let Some(file_id) = existing_file_id {
                backend.update(&file_id, &mut cursor).await
            } else {
                backend
                    .upload(&relative_path_owned, &mut cursor, &parent_id)
                    .await
            }
        })
    });

    match result {
        Ok(entry) => {
            if let Some(db) = &state.db {
                let _ = db.upsert_file(&entry);
            }
            let key = entry.id.0.clone();
            let vfs_item = VfsItem::from(entry);
            let cache_path = state.cache_dir.join(safe_filename(&key));
            if let Some(parent) = cache_path.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            let _ = tokio::fs::write(&cache_path, &bytes).await;
            {
                let mut items = state.items.write().await;
                items.insert(key, vfs_item);
            }
            empty_response(StatusCode::CREATED)
        }
        Err(e) => {
            tracing::warn!(error = %e, "backend upload failed");
            empty_response(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Handle `DELETE` — remove a file or directory.
async fn handle_delete(state: &AppState, path: &str) -> Response {
    let normalised = normalise_path(path);

    let (item_id, backend_id) = {
        let items = state.items.read().await;
        let found = items.values().find(|item| {
            let ip = resolve_full_path(item, &items);
            ip == normalised || ip == path
        });
        match found {
            Some(item) => (item.id.0.clone(), item.id.backend_id().to_string()),
            None => return empty_response(StatusCode::NOT_FOUND),
        }
    };

    // Find the backend.
    let backend = {
        let backends = state.backends.read().await;
        backends.iter().find(|b| b.id() == backend_id).cloned()
    };
    let Some(backend) = backend else {
        return empty_response(StatusCode::NOT_FOUND);
    };

    // Build a FileEntry for the backend.
    let file_entry = {
        let items = state.items.read().await;
        items
            .get(&item_id)
            .map(cascade_engine::types::FileEntry::from)
    };
    let Some(file_entry) = file_entry else {
        return empty_response(StatusCode::NOT_FOUND);
    };

    match backend.delete(&file_entry).await {
        Ok(()) => {
            // Remove from state DB.
            if let Some(db) = &state.db {
                let _ = db.delete_file(&ItemId(item_id.clone()));
            }
            // Remove from cache and in-memory store.
            let cache_path = state.cache_dir.join(safe_filename(&item_id));
            let _ = tokio::fs::remove_file(&cache_path).await;
            let mut items = state.items.write().await;
            items.remove(&item_id);
            empty_response(StatusCode::NO_CONTENT)
        }
        Err(e) => {
            tracing::warn!(error = %e, "backend delete failed");
            empty_response(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Handle `MKCOL` — create a directory.
async fn handle_mkcol(state: &AppState, path: &str) -> Response {
    let normalised = normalise_path(path);

    // Check if it already exists.
    {
        let items = state.items.read().await;
        let exists = items.values().any(|item| {
            let ip = resolve_full_path(item, &items);
            ip == normalised || ip == path
        });
        if exists {
            return empty_response(StatusCode::METHOD_NOT_ALLOWED);
        }
    }

    // Parse backend prefix and relative path.
    let parts: Vec<&str> = normalised.trim_start_matches('/').split('/').collect();
    let Some((&backend_id, relative)) = parts.split_first() else {
        return empty_response(StatusCode::BAD_REQUEST);
    };
    let relative_path = relative.join("/");
    let relative_path = Path::new(&relative_path);

    // Find the backend.
    let backend = {
        let backends = state.backends.read().await;
        backends.iter().find(|b| b.id() == backend_id).cloned()
    };
    let Some(backend) = backend else {
        return empty_response(StatusCode::NOT_FOUND);
    };

    // Resolve the parent directory ID from the in-memory store if possible.
    // This avoids a Drive API round-trip that would fail for freshly created
    // parents not yet indexed by the Drive listing.
    let dir_name = relative
        .last()
        .copied()
        .unwrap_or("New Folder");
    let parent_segments = relative
        .get(..relative.len().saturating_sub(1))
        .unwrap_or(&[]);
    let parent_found_in_items = if parent_segments.is_empty() {
        // Parent is the backend root.
        Some(cascade_engine::types::FileId(
            format!("{backend_id}:root"),
        ))
    } else {
        let parent_normalised = normalise_path(&format!(
            "/{backend_id}/{}",
            parent_segments.join("/")
        ));
        let items = state.items.read().await;
        items
            .values()
            .find(|item| {
                let ip = resolve_full_path(item, &items);
                ip == parent_normalised
            })
            .map(|p| cascade_engine::types::FileId(p.id.0.clone()))
    };

    let create_result = if let Some(ref parent_id) = parent_found_in_items {
        tracing::debug!(parent = %parent_id.0, dir = dir_name, "MKCOL: using parent ID from items");
        backend.create_dir_with_parent(dir_name, parent_id).await
    } else {
        tracing::debug!(path = %relative_path.display(), "MKCOL: parent not in items, using path walk");
        backend.create_dir(relative_path).await
    };

    match create_result {
        Ok(entry) => {
            // Persist to state DB.
            if let Some(db) = &state.db {
                let _ = db.upsert_file(&entry);
            }
            let mut items = state.items.write().await;
            let key = entry.id.0.clone();
            let vfs_item = VfsItem::from(entry);
            items.insert(key, vfs_item);
            empty_response(StatusCode::CREATED)
        }
        Err(e) => {
            tracing::warn!(error = %e, "backend create_dir failed");
            empty_response(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Handle `MOVE` — rename or move a resource.
async fn handle_move(state: &AppState, src_path: &str, headers: &HeaderMap) -> Response {
    let dest = headers
        .get("Destination")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let src_normalised = normalise_path(src_path);
    let dest_normalised = normalise_path(dest);

    // Resolve source item and destination parent from items map.
    let (src_key, backend_id, new_name) = {
        let items = state.items.read().await;
        let Some((src_key, src_item)) = items.iter().find(|(_, item)| {
            let ip = resolve_full_path(item, &items);
            ip == src_normalised || ip == src_path
        }) else {
            return empty_response(StatusCode::NOT_FOUND);
        };
        let src_key = src_key.clone();
        let backend_id = src_item.id.backend_id().to_string();

        // Destination filename is the last path component.
        let new_name = dest_normalised
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_string();

        (src_key, backend_id, new_name)
    };

    // Resolve destination parent from items map.
    let dest_parent_id = {
        let dest_parent_path = dest_normalised
            .trim_end_matches('/')
            .rsplit_once('/')
            .map_or("", |(parent, _)| parent);
        let dest_parent_normalised = normalise_path(dest_parent_path);
        let items = state.items.read().await;
        items
            .values()
            .find(|item| {
                let ip = resolve_full_path(item, &items);
                ip == dest_parent_normalised
            })
            .map(|item| FileId(item.id.0.clone()))
    };

    // Find the backend.
    let backend = {
        let backends = state.backends.read().await;
        backends.iter().find(|b| b.id() == backend_id).cloned()
    };
    let Some(backend) = backend else {
        return empty_response(StatusCode::NOT_FOUND);
    };

    let result = if let Some(ref parent_id) = dest_parent_id {
        backend
            .move_by_id(&FileId(src_key.clone()), parent_id, &new_name)
            .await
    } else {
        let src_parts: Vec<&str> = src_normalised.trim_start_matches('/').split('/').collect();
        let dest_parts: Vec<&str> = dest_normalised.trim_start_matches('/').split('/').collect();
        let src_relative = src_parts.get(1..).map(|p| p.join("/")).unwrap_or_default();
        let dest_relative = dest_parts.get(1..).map(|p| p.join("/")).unwrap_or_default();
        backend
            .move_entry(Path::new(&src_relative), Path::new(&dest_relative))
            .await
    };

    match result {
        Ok(entry) => {
            // Persist to state DB: delete old, insert new.
            if let Some(db) = &state.db {
                let _ = db.delete_file(&ItemId(src_key.clone()));
                let _ = db.upsert_file(&entry);
            }
            let mut items = state.items.write().await;
            items.remove(&src_key);
            items.insert(entry.id.0.clone(), VfsItem::from(entry));
            empty_response(StatusCode::CREATED)
        }
        Err(e) => {
            tracing::warn!(error = %e, "backend move failed");
            empty_response(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Handle `COPY` — copy a resource.
async fn handle_copy(state: &AppState, src_path: &str, headers: &HeaderMap) -> Response {
    let dest = headers
        .get("Destination")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let src_normalised = normalise_path(src_path);
    let dest_normalised = normalise_path(dest);

    let mut items = state.items.write().await;

    // Find source item.
    let src_item = items
        .values()
        .find(|item| {
            let ip = resolve_full_path(item, &items);
            ip == src_normalised || ip == src_path
        })
        .cloned();

    let Some(src_item) = src_item else {
        return empty_response(StatusCode::NOT_FOUND);
    };

    // Create a copy with a new ID.
    let dest_parts: Vec<&str> = dest_normalised.trim_start_matches('/').split('/').collect();
    let new_name = dest_parts.last().copied().unwrap_or("").to_string();
    let new_id_str = dest_normalised.trim_start_matches('/').replace('/', ":");

    let copy = VfsItem {
        id: cascade_engine::types::ItemId::new("webdav", &new_id_str),
        parent_id: src_item.parent_id,
        name: new_name,
        is_dir: src_item.is_dir,
        size: src_item.size,
        mod_time: src_item.mod_time,
        cache_state: src_item.cache_state,
        mime_type: src_item.mime_type,
    };
    items.insert(copy.id.0.clone(), copy);

    empty_response(StatusCode::CREATED)
}

// ---------------------------------------------------------------------------
// XML response generation
// ---------------------------------------------------------------------------

/// Build a PROPFIND multistatus XML response.
#[must_use]
pub fn build_root_response(backends: &[String]) -> String {
    let mut responses = String::new();
    responses.push_str(
        "<D:response>\
         <D:href>/</D:href>\
         <D:propstat>\
         <D:prop>\
         <D:resourcetype><D:collection/></D:resourcetype>\
         </D:prop>\
         <D:status>HTTP/1.1 200 OK</D:status>\
         </D:propstat>\
         </D:response>",
    );
    for backend in backends {
        // SAFETY: write! to String is infallible.
        #[allow(clippy::expect_used)]
        let _ = write!(
            responses,
            "<D:response>\
             <D:href>/{}/</D:href>\
             <D:propstat>\
             <D:prop>\
             <D:resourcetype><D:collection/></D:resourcetype>\
             </D:prop>\
             <D:status>HTTP/1.1 200 OK</D:status>\
             </D:propstat>\
             </D:response>",
            xml_escape(backend),
        );
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <D:multistatus xmlns:D=\"DAV:\">\
         {responses}\
         </D:multistatus>"
    )
}

#[must_use]
#[allow(clippy::implicit_hasher)]
pub fn build_propfind_response(
    href: &str,
    target: Option<&VfsItem>,
    children: &[&VfsItem],
    items: &std::collections::HashMap<String, VfsItem>,
) -> String {
    let mut responses = String::new();
    let root_href = xml_escape(href);

    if let Some(item) = target {
        responses.push_str(&build_response_element(&root_href, item));
    } else {
        // SAFETY: write! to String is infallible.
        #[allow(clippy::expect_used)]
        let _ = write!(
            responses,
            "<D:response>\
             <D:href>{root_href}</D:href>\
             <D:propstat>\
             <D:prop>\
             <D:resourcetype><D:collection/></D:resourcetype>\
             </D:prop>\
             <D:status>HTTP/1.1 200 OK</D:status>\
             </D:propstat>\
             </D:response>",
        );
    }

    for child in children {
        let child_path = resolve_full_path(child, items);
        let href_suffix = if child.is_dir { "/" } else { "" };
        let child_href = xml_escape(&format!("{child_path}{href_suffix}"));
        responses.push_str(&build_response_element_escaped(&child_href, child));
    }

    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <D:multistatus xmlns:D=\"DAV:\">\
         {responses}\
         </D:multistatus>"
    )
}

/// Build a single `<D:response>` element for a VFS item.
fn build_response_element(href: &str, item: &VfsItem) -> String {
    build_response_element_escaped(&xml_escape(href), item)
}

/// Build a single `<D:response>` element with a pre-escaped href.
fn build_response_element_escaped(href: &str, item: &VfsItem) -> String {
    let resource_type = if item.is_dir {
        "<D:resourcetype><D:collection/></D:resourcetype>".to_string()
    } else {
        "<D:resourcetype/>".to_string()
    };

    let content_length = item
        .size
        .map(|s| format!("<D:getcontentlength>{s}</D:getcontentlength>"))
        .unwrap_or_default();

    let last_modified = item.mod_time.map_or_else(
        || "Thu, 01 Jan 1970 00:00:00 GMT".to_string(),
        |t| t.format("%a, %d %b %Y %H:%M:%S GMT").to_string(),
    );

    let content_type = item
        .mime_type
        .as_deref()
        .map(|m| format!("<D:getcontenttype>{}</D:getcontenttype>", xml_escape(m)))
        .unwrap_or_default();

    format!(
        "<D:response>\
         <D:href>{href}</D:href>\
         <D:propstat>\
         <D:prop>\
         {resource_type}\
         <D:getlastmodified>{last_modified}</D:getlastmodified>\
         {content_length}\
         {content_type}\
         </D:prop>\
         <D:status>HTTP/1.1 200 OK</D:status>\
         </D:propstat>\
         </D:response>"
    )
}

/// XML-escape special characters.
#[must_use]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compute a display path for a VFS item.
/// Converts the `ItemId` to a slash-separated path.
#[must_use]
pub fn item_path(item: &VfsItem) -> String {
    let id = &item.id.0;
    if id.starts_with('/') {
        id.clone()
    } else if let Some((backend, _file_id)) = id.split_once(':') {
        format!("/{backend}/{}", item.name)
    } else {
        format!("/{}", item.name)
    }
}

/// Try to populate children from the state database.
/// Returns the number of items loaded (zero if no DB or no results).
async fn hydrate_children_from_db(state: &AppState, parent_id: &str) -> usize {
    let Some(db) = &state.db else {
        return 0;
    };
    let Ok(entries) = db.list_children(parent_id) else {
        return 0;
    };
    if entries.is_empty() {
        return 0;
    }
    let count = entries.len();
    let mut items = state.items.write().await;
    for entry in entries {
        let key = entry.id.0.clone();
        items.insert(key, VfsItem::from(entry));
    }
    count
}

/// Resolve the real root folder ID for a backend from the state database.
///
/// The Google Drive API may return `"root"` or the actual folder ID
/// (e.g. `0APRsmt7LhxCIUk9PVA`) as the parent. When `list_children`
/// finds nothing under `{prefix}:root`, this function queries the DB
/// for children whose parent is a top-level directory (appears in both
/// the id and `parent_id` columns) with the backend prefix.
fn resolve_root_id_from_db(db: &cascade_engine::db::StateDb, prefix: &str) -> Option<String> {
    let all = db.list_all_files().ok()?;
    let prefix_colon = format!("{prefix}:");

    // Collect all IDs for this backend into a set.
    let ids: std::collections::HashSet<&String> = all
        .iter()
        .filter(|e| e.id.0.starts_with(&prefix_colon))
        .map(|e| &e.id.0)
        .collect();

    // A root-level child is one whose parent_id is in the id set
    // (i.e. the parent is a known directory) AND the parent has
    // at least one child. Find all such parent IDs, then pick
    // the one that is NOT itself a child of another directory
    // in the same backend (i.e. its own parent_id is just the
    // prefix with no real native ID, or it's not in the ids set).
    let mut root_candidates: std::collections::HashSet<&String> = std::collections::HashSet::new();
    for entry in &all {
        if entry.id.0.starts_with(&prefix_colon) && ids.contains(&entry.parent_id.0) {
            root_candidates.insert(&entry.parent_id.0);
        }
    }

    // A root candidate whose own parent_id is NOT in the id set
    // must be the top-level root (its parent is the virtual root
    // or the "root" alias which may not be stored as an id).
    let real_roots: Vec<&String> = root_candidates
        .iter()
        .filter(|id| {
            // Look up this candidate's own parent_id.
            all.iter()
                .find(|e| &e.id.0 == **id)
                .is_none_or(|e| !ids.contains(&e.parent_id.0))
        })
        .copied()
        .collect();

    if real_roots.len() == 1 {
        return real_roots.into_iter().next().cloned();
    }
    None
}

/// Resolve the full path for an item by walking its parent chain.
/// Produces paths like `/gdrive/huggingface_hub/cli/command.py`
/// instead of the flat `/gdrive/command.py`.
fn resolve_full_path(item: &VfsItem, items: &std::collections::HashMap<String, VfsItem>) -> String {
    let mut parts = vec![item.name.clone()];
    let mut current_parent = item.parent_id.0.clone();

    // Walk up the parent chain until we hit a root-level item
    // (one whose parent_id is just the backend prefix like "gdrive:root"
    // or whose parent_id has no ":" separator).
    let mut seen = std::collections::HashSet::new();
    while let Some(parent) = items.get(&current_parent) {
        if !seen.insert(current_parent.clone()) {
            break; // cycle detected
        }
        parts.push(parent.name.clone());
        current_parent = parent.parent_id.0.clone();
    }

    // The remaining current_parent should be like "gdrive:root" or "gdrive:".
    // Extract the backend prefix.
    let backend = current_parent
        .split_once(':')
        .map_or("", |(prefix, _)| prefix);

    parts.reverse();
    if backend.is_empty() {
        format!("/{}", parts.join("/"))
    } else {
        format!("/{backend}/{}", parts.join("/"))
    }
}

/// Normalise a URL path for consistent matching.
/// Removes trailing slashes, ensures no double slashes.
async fn read_items(
    items: &Arc<RwLock<HashMap<String, VfsItem>>>,
) -> tokio::sync::RwLockReadGuard<'_, HashMap<String, VfsItem>> {
    items.read().await
}

/// On-demand expansion: fetch children of a directory from its backend.
async fn expand_directory(state: &AppState, item_id: &ItemId) {
    // Skip if already expanded.
    {
        let expanded = state.expanded.read().await;
        if expanded.contains(&item_id.0) {
            return;
        }
    }

    let backend_id = item_id.backend_id();
    let native_id = item_id.native_id();

    let backend = {
        let backends = state.backends.read().await;
        backends.iter().find(|b| b.id() == backend_id).cloned()
    };
    let Some(backend) = backend else {
        tracing::debug!(backend = %backend_id, "no backend found for expansion");
        return;
    };

    // Acquire semaphore — limits concurrent API calls.
    // SAFETY: the semaphore is owned by AppState and never closed,
    // so acquire() is infallible at runtime.
    #[allow(clippy::expect_used)]
    let _permit = state
        .expand_sem
        .acquire()
        .await
        .expect("expand semaphore should not be closed");

    // Double-check after acquiring the permit (another request may have
    // expanded this while we waited).
    {
        let expanded = state.expanded.read().await;
        if expanded.contains(&item_id.0) {
            return;
        }
    }

    tracing::info!(native_id = %native_id, "expanding directory");

    match backend.list_children(native_id).await {
        Ok(entries) => {
            let mut items = state.items.write().await;
            for entry in entries {
                let key = entry.id.0.clone();
                let vfs_item = VfsItem::from(entry);
                if let Some(db) = &state.db {
                    let file_entry = cascade_engine::types::FileEntry::from(&vfs_item);
                    if let Err(e) = db.upsert_file(&file_entry) {
                        tracing::debug!(error = %e, "failed to persist expanded item");
                    }
                }
                items.insert(key, vfs_item);
            }
            // Mark as expanded.
            state.expanded.write().await.insert(item_id.0.clone());
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to expand directory");
        }
    }
}

/// On-demand expansion: list root children for a virtual backend directory.
async fn expand_root(state: &AppState, backend_prefix: &str) {
    let root_key = format!("{backend_prefix}:root");

    // Skip if already expanded.
    {
        let expanded = state.expanded.read().await;
        if expanded.contains(&root_key) {
            return;
        }
    }

    let backend = {
        let backends = state.backends.read().await;
        backends.iter().find(|b| b.id() == backend_prefix).cloned()
    };
    let Some(backend) = backend else {
        tracing::debug!(backend = %backend_prefix, "no backend found for root expansion");
        return;
    };

    // Acquire semaphore — limits concurrent API calls.
    // SAFETY: the semaphore is owned by AppState and never closed,
    // so acquire() is infallible at runtime.
    #[allow(clippy::expect_used)]
    let _permit = state
        .expand_sem
        .acquire()
        .await
        .expect("expand semaphore should not be closed");

    // Double-check after acquiring the permit.
    {
        let expanded = state.expanded.read().await;
        if expanded.contains(&root_key) {
            return;
        }
    }

    tracing::info!(backend = %backend_prefix, "expanding backend root");

    match backend.list_children("root").await {
        Ok(entries) => {
            let mut items = state.items.write().await;
            for entry in entries {
                let key = entry.id.0.clone();
                let vfs_item = VfsItem::from(entry);
                if let Some(db) = &state.db {
                    let file_entry = cascade_engine::types::FileEntry::from(&vfs_item);
                    if let Err(e) = db.upsert_file(&file_entry) {
                        tracing::debug!(error = %e, "failed to persist expanded item");
                    }
                }
                items.insert(key, vfs_item);
            }
            // Mark as expanded.
            state.expanded.write().await.insert(root_key);
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to expand backend root");
        }
    }
}

fn normalise_path(path: &str) -> String {
    // Strip scheme+authority from absolute URIs (e.g. WebDAV Destination headers
    // arrive as "http://host:port/path" per RFC 4918).
    let path = path
        .split_once("://")
        .and_then(|(_, rest)| rest.find('/').and_then(|i| rest.get(i..)))
        .unwrap_or(path);
    let p = path.trim_end_matches('/');
    if p.is_empty() {
        "/".to_string()
    } else if p.starts_with('/') {
        p.to_string()
    } else {
        format!("/{p}")
    }
}

/// Sanitise an `ItemId` into a filesystem-safe filename.
#[must_use]
fn safe_filename(id: &str) -> String {
    id.replace([':', '/', '\\'], "_")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use cascade_engine::types::{CacheState, ItemId};

    fn make_item(name: &str, is_dir: bool) -> VfsItem {
        VfsItem {
            id: ItemId::new("gdrive", name),
            parent_id: ItemId::new("gdrive", ""),
            name: name.to_string(),
            is_dir,
            size: None,
            mod_time: None,
            cache_state: CacheState::Online,
            mime_type: None,
        }
    }

    #[test]
    fn propfind_xml_root_collection() {
        let empty = HashMap::new();
        let xml = build_propfind_response("/", None, &[], &empty);
        assert!(xml.contains("<?xml version=\"1.0\""));
        assert!(xml.contains("DAV:"));
        assert!(xml.contains("<D:collection/>"));
        assert!(xml.contains("HTTP/1.1 200 OK"));
    }

    #[test]
    fn propfind_xml_with_file() {
        let item = make_item("test.txt", false);
        let empty = HashMap::new();
        let xml = build_propfind_response("/gdrive/test.txt", Some(&item), &[], &empty);
        // Item has size=None, so no content-length element.
        assert!(!xml.contains("<D:collection/>"));
        assert!(xml.contains("/gdrive/test.txt"));
    }

    #[test]
    fn propfind_xml_with_file_with_size() {
        let mut item = make_item("test.txt", false);
        item.size = Some(1024);
        let empty = HashMap::new();
        let xml = build_propfind_response("/gdrive/test.txt", Some(&item), &[], &empty);
        assert!(xml.contains("<D:getcontentlength>1024</D:getcontentlength>"));
        assert!(!xml.contains("<D:collection/>"));
        assert!(xml.contains("/gdrive/test.txt"));
    }

    #[test]
    fn propfind_xml_with_directory() {
        let item = make_item("Documents", true);
        let empty = HashMap::new();
        let xml = build_propfind_response("/gdrive/Documents", Some(&item), &[], &empty);
        assert!(xml.contains("<D:collection/>"));
        assert!(xml.contains("/gdrive/Documents"));
    }

    #[test]
    fn propfind_xml_with_children() {
        let parent = make_item("Documents", true);
        let child = make_item("readme.txt", false);
        let empty = HashMap::new();
        let xml = build_propfind_response("/gdrive/Documents", Some(&parent), &[&child], &empty);
        assert!(xml.contains("<D:collection/>"));
        assert!(xml.contains("readme.txt"));
        // Two responses — parent + child.
        assert_eq!(xml.matches("<D:response>").count(), 2);
    }

    #[test]
    fn xml_escape_handles_special_chars() {
        assert_eq!(xml_escape("a&b<c>d"), "a&amp;b&lt;c&gt;d");
    }

    #[test]
    fn normalise_path_removes_trailing_slash() {
        assert_eq!(normalise_path("/foo/bar/"), "/foo/bar");
    }

    #[test]
    fn normalise_path_empty_is_root() {
        assert_eq!(normalise_path(""), "/");
    }

    #[test]
    fn normalise_path_strips_absolute_uri() {
        assert_eq!(
            normalise_path("http://localhost:50217/gdrive-personal/a/b.txt"),
            "/gdrive-personal/a/b.txt"
        );
    }

    #[test]
    fn item_path_from_vfs_item() {
        let item = make_item("test.txt", false);
        assert_eq!(item_path(&item), "/gdrive/test.txt");
    }

    #[tokio::test]
    async fn server_starts_and_stops() {
        let items = Arc::new(RwLock::new(HashMap::new()));
        let cache_dir = tempfile::tempdir().unwrap();
        let server = WebDavServer::start(
            "127.0.0.1:0",
            items,
            cache_dir.path().to_path_buf(),
            Arc::new(tokio::sync::RwLock::new(Vec::new())),
            None,
        )
        .await
        .unwrap();
        assert!(server.port() > 0);
        server.stop().unwrap();
    }

    #[tokio::test]
    async fn server_propfind_returns_multistatus() {
        let items = Arc::new(RwLock::new(HashMap::new()));
        let cache_dir = tempfile::tempdir().unwrap();
        let server = WebDavServer::start(
            "127.0.0.1:0",
            items.clone(),
            cache_dir.path().to_path_buf(),
            Arc::new(tokio::sync::RwLock::new(Vec::new())),
            None,
        )
        .await
        .unwrap();
        let port = server.port();

        let client = reqwest::Client::new();
        let resp = client
            .request(
                reqwest::Method::from_bytes(b"PROPFIND").unwrap(),
                format!("http://127.0.0.1:{port}/"),
            )
            .header("Depth", "0")
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), reqwest::StatusCode::MULTI_STATUS);
        let body = resp.text().await.unwrap();
        assert!(body.contains("multistatus"));
        assert!(body.contains("DAV:"));

        server.stop().unwrap();
    }

    #[tokio::test]
    async fn server_get_returns_not_found_for_missing() {
        let items = Arc::new(RwLock::new(HashMap::new()));
        let cache_dir = tempfile::tempdir().unwrap();
        let server = WebDavServer::start(
            "127.0.0.1:0",
            items,
            cache_dir.path().to_path_buf(),
            Arc::new(tokio::sync::RwLock::new(Vec::new())),
            None,
        )
        .await
        .unwrap();
        let port = server.port();

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://127.0.0.1:{port}/nonexistent"))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
        server.stop().unwrap();
    }

    #[tokio::test]
    #[ignore = "requires a registered backend to route write operations"]
    async fn server_put_and_get_roundtrip() {
        let items = Arc::new(RwLock::new(HashMap::new()));
        let cache_dir = tempfile::tempdir().unwrap();
        let server = WebDavServer::start(
            "127.0.0.1:0",
            items,
            cache_dir.path().to_path_buf(),
            Arc::new(tokio::sync::RwLock::new(Vec::new())),
            None,
        )
        .await
        .unwrap();
        let port = server.port();

        let client = reqwest::Client::new();

        // PUT a file.
        let resp = client
            .put(format!("http://127.0.0.1:{port}/test.txt"))
            .body(b"hello world".to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

        // GET it back.
        let resp = client
            .get(format!("http://127.0.0.1:{port}/test.txt"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body = resp.bytes().await.unwrap();
        assert_eq!(&*body, b"hello world");

        server.stop().unwrap();
    }

    #[tokio::test]
    #[ignore = "requires a registered backend to route write operations"]
    async fn server_mkcol_creates_directory() {
        let items = Arc::new(RwLock::new(HashMap::new()));
        let cache_dir = tempfile::tempdir().unwrap();
        let server = WebDavServer::start(
            "127.0.0.1:0",
            items.clone(),
            cache_dir.path().to_path_buf(),
            Arc::new(tokio::sync::RwLock::new(Vec::new())),
            None,
        )
        .await
        .unwrap();
        let port = server.port();

        let client = reqwest::Client::new();
        let resp = client
            .request(
                reqwest::Method::from_bytes(b"MKCOL").unwrap(),
                format!("http://127.0.0.1:{port}/newdir"),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

        // Verify it appears in items.
        let items_guard = items.read().await;
        assert!(
            items_guard
                .values()
                .any(|item| item.name == "newdir" && item.is_dir)
        );

        server.stop().unwrap();
    }

    #[tokio::test]
    #[ignore = "requires a registered backend to route write operations"]
    async fn server_delete_removes_item() {
        let items = Arc::new(RwLock::new(HashMap::new()));
        let cache_dir = tempfile::tempdir().unwrap();
        let server = WebDavServer::start(
            "127.0.0.1:0",
            items.clone(),
            cache_dir.path().to_path_buf(),
            Arc::new(tokio::sync::RwLock::new(Vec::new())),
            None,
        )
        .await
        .unwrap();
        let port = server.port();

        let client = reqwest::Client::new();

        // PUT a file first.
        client
            .put(format!("http://127.0.0.1:{port}/todelete.txt"))
            .body(b"data".to_vec())
            .send()
            .await
            .unwrap();

        // DELETE it.
        let resp = client
            .delete(format!("http://127.0.0.1:{port}/todelete.txt"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);

        server.stop().unwrap();
    }

    #[tokio::test]
    async fn server_options_returns_dav_header() {
        let items = Arc::new(RwLock::new(HashMap::new()));
        let cache_dir = tempfile::tempdir().unwrap();
        let server = WebDavServer::start(
            "127.0.0.1:0",
            items,
            cache_dir.path().to_path_buf(),
            Arc::new(tokio::sync::RwLock::new(Vec::new())),
            None,
        )
        .await
        .unwrap();
        let port = server.port();

        let client = reqwest::Client::new();
        let resp = client
            .request(
                reqwest::Method::OPTIONS,
                format!("http://127.0.0.1:{port}/"),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
        let dav_header = resp.headers().get("dav").unwrap();
        assert_eq!(dav_header, "1, 2");

        server.stop().unwrap();
    }
}
