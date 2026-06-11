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
use cascade_engine::backend::BackendError;
use cascade_engine::sync::mount_path::apply_mount_prefix;
use cascade_engine::types::{FileId, ItemId, VfsItem};
use tokio::net::TcpListener;

/// Map a backend error to an appropriate HTTP status code. Backends signal
/// permission / not-found / read-only / conflict via `BackendError`; anything
/// else falls back to 500 Internal Server Error.
fn backend_error_status(e: &anyhow::Error) -> StatusCode {
    e.downcast_ref::<BackendError>()
        .map_or(StatusCode::INTERNAL_SERVER_ERROR, |be| match be {
            BackendError::Forbidden(_) | BackendError::ReadOnly(_) => StatusCode::FORBIDDEN,
            BackendError::NotFound(_) => StatusCode::NOT_FOUND,
            BackendError::Conflict(_) => StatusCode::CONFLICT,
        })
}

/// Return true for filenames macOS generates as filesystem metadata that
/// shouldn't be persisted to a cloud backend.
///
/// `._*` `AppleDouble` files hold extended attributes (Finder tags, custom
/// icons, quarantine flags, …) that don't survive a round-trip to Drive
/// anyway; without filtering they pile up alongside every real file.
/// `.DS_Store` is per-folder Finder UI state. The other entries are
/// volume-level metadata directories Finder/Spotlight may try to create.
fn is_macos_junk(name: &str) -> bool {
    name.starts_with("._")
        || name == ".DS_Store"
        || name == ".Spotlight-V100"
        || name == ".Trashes"
        || name == ".fseventsd"
        || name == ".TemporaryItems"
        || name == ".DocumentRevisions-V100"
        || name == ".VolumeIcon.icns"
}

/// Extract the trailing path component for the junk-file check.
fn last_component(normalised_path: &str) -> &str {
    normalised_path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
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
    /// Backends for on-demand directory expansion, kept for backward
    /// compatibility with ID-based dispatch (e.g. `populate_cache`).
    pub backends: Arc<tokio::sync::RwLock<Vec<Arc<dyn cascade_engine::backend::Backend>>>>,
    /// Mount table: `(mount_prefix, backend)` pairs, longest-prefix first.
    ///
    /// Used by write handlers (`PUT`, `MKCOL`, `MOVE`) to route a
    /// VFS path to the correct backend without treating the first path
    /// segment as a `backend_id`. The root PROPFIND reads the prefixes
    /// to enumerate the top-level mount directories it should present.
    pub mounts: Arc<tokio::sync::RwLock<Vec<(PathBuf, Arc<dyn cascade_engine::backend::Backend>)>>>,
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
    /// `mounts` is the ordered mount table (longest-prefix first) that maps
    /// VFS path prefixes to backends. It drives root-directory listing and
    /// write-path routing. Pass an empty vec when no mounts are configured
    /// (e.g. in unit tests).
    ///
    /// # Errors
    ///
    /// Returns an error if the TCP listener cannot bind.
    pub async fn start(
        bind_addr: &str,
        items: Arc<RwLock<HashMap<String, VfsItem>>>,
        cache_dir: PathBuf,
        backends: Arc<tokio::sync::RwLock<Vec<Arc<dyn cascade_engine::backend::Backend>>>>,
        mounts: Arc<tokio::sync::RwLock<Vec<(PathBuf, Arc<dyn cascade_engine::backend::Backend>)>>>,
        db: Option<Arc<cascade_engine::db::StateDb>>,
    ) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(bind_addr).await?;
        let port = listener.local_addr()?.port();

        let state = AppState {
            items,
            cache_dir,
            backends,
            mounts,
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
        Method::GET => handle_get(&state, &path, req.headers()).await,
        Method::HEAD => handle_head(&state, &path).await,
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
        m if m == Method::from_bytes(b"LOCK").unwrap_or_default() => handle_lock(&path),
        m if m == Method::from_bytes(b"UNLOCK").unwrap_or_default() => {
            empty_response(StatusCode::NO_CONTENT)
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
        HeaderValue::from_static(
            "GET, PUT, DELETE, MKCOL, PROPFIND, MOVE, COPY, LOCK, UNLOCK, OPTIONS, HEAD",
        ),
    );
    resp.headers_mut().insert(
        header::HeaderName::from_static("dav"),
        HeaderValue::from_static("1, 2"),
    );
    resp
}

/// Handle `LOCK` — return a stub write lock so macOS can proceed with PUT.
///
/// We do not enforce locking; the token is synthetic and not stored.
/// UNLOCK always succeeds (204).  This is sufficient for a single-writer
/// client like macOS Finder / shell redirection.
fn handle_lock(path: &str) -> Response {
    let token = format!("urn:uuid:{}", uuid_v4());
    let href = xml_escape(path);
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <D:prop xmlns:D=\"DAV:\">\
           <D:lockdiscovery>\
             <D:activelock>\
               <D:locktype><D:write/></D:locktype>\
               <D:lockscope><D:exclusive/></D:lockscope>\
               <D:depth>0</D:depth>\
               <D:timeout>Second-3600</D:timeout>\
               <D:locktoken><D:href>{token}</D:href></D:locktoken>\
               <D:lockroot><D:href>{href}</D:href></D:lockroot>\
             </D:activelock>\
           </D:lockdiscovery>\
         </D:prop>"
    );
    let lock_token_header = format!("<{token}>");
    let mut resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
        .header(
            header::HeaderName::from_static("lock-token"),
            lock_token_header,
        )
        .body(Body::from(xml))
        .unwrap_or_else(|_| empty_response(StatusCode::INTERNAL_SERVER_ERROR));
    resp.headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    resp
}

/// Generate a random UUID v4 (used for synthetic lock tokens).
fn uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        (t & 0xffff_ffff) as u32,
        (t >> 32 & 0xffff) as u16,
        (t >> 48 & 0x0fff) as u16,
        0x8000u16 | ((t >> 60 & 0x3fff) as u16),
        t >> 16 & 0xffff_ffff_ffff_u128,
    )
}

/// Handle `PROPFIND` — return `WebDAV` XML metadata for a resource.
async fn handle_propfind(state: &AppState, path: &str, headers: &HeaderMap) -> Response {
    let depth = headers
        .get("Depth")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("1");

    let normalised = normalise_path(path);

    // Root listing: show each explicitly-mounted backend as a top-level
    // directory, using the configured mount name rather than the backend ID.
    // A backend mounted at the empty prefix (the "/" case) owns no synthetic
    // directory; its root children appear directly under "/", reproducing the
    // single-backend-at-root path shape.
    if normalised == "/" {
        // The mount table separates explicit prefixes (synthetic top-level
        // directories) from the at-root backend (its children listed inline).
        let (mount_names, at_root_backend_id) = {
            let mounts = state.mounts.read().await;
            let mut names: Vec<String> = mounts
                .iter()
                .filter_map(|(prefix, _)| {
                    let s = prefix.to_string_lossy();
                    if s.is_empty() {
                        None
                    } else {
                        Some(s.into_owned())
                    }
                })
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            names.sort();
            let at_root = mounts
                .iter()
                .find(|(prefix, _)| prefix.as_os_str().is_empty())
                .map(|(_, backend)| backend.id().to_string());
            (names, at_root)
        };

        let mount_table_configured = {
            let mounts = state.mounts.read().await;
            !mounts.is_empty()
        };

        if mount_table_configured {
            // Expand the at-root backend's root so its children are present in
            // the item store, then list them inline alongside the synthetic
            // directories of every explicitly-mounted backend.
            let mut at_root_children: Vec<VfsItem> = Vec::new();
            if let Some(ref backend_id) = at_root_backend_id {
                let root_id = format!("{backend_id}:root");
                let is_expanded = state.expanded.read().await.contains(&root_id);
                if !is_expanded {
                    expand_root(state, backend_id).await;
                }
                let items = read_items(&state.items).await;
                at_root_children = items
                    .values()
                    .filter(|item| item.parent_id.0 == root_id)
                    .cloned()
                    .collect();
                if at_root_children.is_empty() {
                    drop(items);
                    hydrate_children_from_db(state, &root_id).await;
                    let items = read_items(&state.items).await;
                    at_root_children = items
                        .values()
                        .filter(|item| item.parent_id.0 == root_id)
                        .cloned()
                        .collect();
                }
            }
            let items = read_items(&state.items).await;
            let child_refs: Vec<&VfsItem> = at_root_children
                .iter()
                .filter(|c| !is_macos_junk(&c.name))
                .collect();
            let xml = build_root_listing(&mount_names, &child_refs, &items);
            return (
                StatusCode::MULTI_STATUS,
                [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
                xml,
            )
                .into_response();
        }

        // No mount table configured (e.g. unit tests that construct AppState
        // directly): fall back to deriving top-level directory names from the
        // backend-ID prefix of each item.
        let effective_names = {
            let items = read_items(&state.items).await;
            let mut backends: Vec<String> = items
                .values()
                .filter_map(|item| item.id.0.split(':').next().map(String::from))
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();
            backends.sort();
            backends
        };
        let xml = build_root_response(&effective_names);
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

    // If the path isn't in the VFS, return 404 unless it is a top-level
    // backend path (e.g. `/gdrive-personal`) which is a virtual root with
    // no corresponding VfsItem. Any deeper path that is absent must return
    // 404 so that macOS sends MKCOL / PUT rather than treating the missing
    // path as an existing empty directory.
    if target.is_none() {
        let component_count = normalised
            .trim_start_matches('/')
            .trim_end_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .count();
        if component_count != 1 {
            return empty_response(StatusCode::NOT_FOUND);
        }
    }

    // Find children, expanding directories on demand if not already
    // fully hydrated this session. Trusting the in-memory items map
    // alone is unsafe: a sync cycle may have inserted a handful of
    // items while the DB has hundreds. Use `state.expanded` as the
    // authoritative "this directory's children are fully cached"
    // signal, populating from DB or API on first access per session.
    let children: Vec<VfsItem> = if depth == "0" {
        Vec::new()
    } else if let Some(ref t) = target {
        let target_id = t.id.0.clone();
        let is_expanded = state.expanded.read().await.contains(&target_id);

        if !is_expanded && t.is_dir {
            let hydrated = hydrate_children_from_db(state, &target_id).await;
            if hydrated > 0 {
                state.expanded.write().await.insert(target_id.clone());
            } else {
                expand_directory(state, &t.id).await;
            }
        }

        let items = read_items(&state.items).await;
        items
            .values()
            .filter(|item| item.parent_id.0 == target_id)
            .cloned()
            .collect()
    } else {
        // target is None and the path is a top-level mount path (the
        // multi-component guard above already returned 404 for anything
        // deeper). Expand root-level children for the backend mounted here.
        let mount_name = normalised.trim_start_matches('/').trim_end_matches('/');

        // Resolve the backend for this mount name from the mount table, then
        // fall back to the old backend-ID approach for unit tests that do not
        // configure a mount table.
        let backend_id = {
            let mounts = state.mounts.read().await;
            mounts
                .iter()
                .find(|(prefix, _)| prefix.to_string_lossy() == mount_name)
                .map(|(_, backend)| backend.id().to_string())
        };
        let backend_id = backend_id.unwrap_or_else(|| mount_name.to_string());
        let root_id = format!("{backend_id}:root");

        // Expand the root on first access. expand_root normalises every
        // root-level item's parent_id to the alias, so a simple alias
        // filter below reliably covers all root children regardless of
        // what the backend API returned as the native parent ID.
        let is_expanded = state.expanded.read().await.contains(&root_id);
        if !is_expanded {
            expand_root(state, &backend_id).await;
        }

        // After expansion all root children use the alias. If the store
        // is still empty (e.g. completely fresh backend), try DB.
        let cached: Vec<VfsItem> = {
            let items = read_items(&state.items).await;
            items
                .values()
                .filter(|item| item.parent_id.0 == root_id)
                .cloned()
                .collect()
        };

        if cached.is_empty() {
            hydrate_children_from_db(state, &root_id).await;
            let items = read_items(&state.items).await;
            items
                .values()
                .filter(|item| item.parent_id.0 == root_id)
                .cloned()
                .collect()
        } else {
            cached
        }
    };

    // Build response. Hide any macOS metadata files that slipped into
    // the cache from before the upload guard existed; Finder will neither
    // try to re-create them nor offer to delete them when invisible.
    let items = read_items(&state.items).await;
    let child_refs: Vec<&VfsItem> = children
        .iter()
        .filter(|c| !is_macos_junk(&c.name))
        .collect();
    let xml = build_propfind_response(&normalised, target.as_ref(), &child_refs, &items);
    (
        StatusCode::MULTI_STATUS,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        xml,
    )
        .into_response()
}

/// Handle `GET` — return file contents from the cache.
async fn handle_get(state: &AppState, path: &str, headers: &HeaderMap) -> Response {
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

    let cache_path = state.cache_dir.join(safe_filename(&item_id));

    // Cache miss → download. We hold the response until the full file
    // lands in the cache, then stream it (or a Range of it) back.
    if tokio::fs::metadata(&cache_path).await.is_err()
        && let Err(resp) = populate_cache(state, &item_id, &backend_id, &cache_path).await
    {
        return resp;
    }

    serve_from_cache(&cache_path, headers).await
}

/// Read-only response (no body) for `HEAD`. Returns the Content-Length
/// from the items map so clients can stat without forcing a download.
async fn handle_head(state: &AppState, path: &str) -> Response {
    let normalised = normalise_path(path);
    let items = state.items.read().await;
    let Some(item) = items.values().find(|item| {
        let ip = resolve_full_path(item, &items);
        ip == normalised || ip == path
    }) else {
        return empty_response(StatusCode::NOT_FOUND);
    };
    let size = item.size.unwrap_or(0);
    let accept_ranges = HeaderValue::from_static("bytes");
    let content_type = HeaderValue::from_static("application/octet-stream");
    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = StatusCode::OK;
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, content_type);
    h.insert(header::ACCEPT_RANGES, accept_ranges);
    if let Ok(v) = HeaderValue::from_str(&size.to_string()) {
        h.insert(header::CONTENT_LENGTH, v);
    }
    resp
}

/// Generous upper-bound on a backend download. A hung request produces a
/// visible 504 with the item ID rather than wedging the handler forever.
/// This value is intentionally very large — a legitimate large-file download
/// should complete well under 60 s on any reasonable connection; if it hasn't
/// in that window the call has almost certainly stalled.
const BACKEND_DOWNLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(1);

/// Upper-bound on the isolated PUT backend call (upload or update).
const BACKEND_UPLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(2);

/// Download `item_id` from its backend into `cache_path`. Returns
/// `Err(response)` if the download failed so the caller can pass that
/// response through unmodified.
async fn populate_cache(
    state: &AppState,
    item_id: &str,
    backend_id: &str,
    cache_path: &std::path::Path,
) -> std::result::Result<(), Response> {
    let backend = {
        let backends = state.backends.read().await;
        backends.iter().find(|b| b.id() == backend_id).cloned()
    };
    let Some(backend) = backend else {
        return Err(empty_response(StatusCode::NOT_FOUND));
    };

    let file_entry = {
        let items = state.items.read().await;
        items
            .get(item_id)
            .map(cascade_engine::types::FileEntry::from)
    };
    let Some(file_entry) = file_entry else {
        return Err(empty_response(StatusCode::NOT_FOUND));
    };

    let _ = tokio::fs::create_dir_all(&state.cache_dir).await;

    // Log before-download without holding the span guard across `.await` —
    // `EnteredSpan` is not `Send`.
    tracing::debug_span!("webdav_backend_download", item_id, backend_id)
        .in_scope(|| tracing::debug!(item_id, backend_id, "before backend.download"));
    let download_result =
        tokio::time::timeout(BACKEND_DOWNLOAD_TIMEOUT, backend.download(&file_entry)).await;

    let download_result = match download_result {
        Err(_elapsed) => {
            tracing::error!(
                item_id,
                backend_id,
                timeout_secs = BACKEND_DOWNLOAD_TIMEOUT.as_secs(),
                "backend.download timed out — suspected pooled-connection hang"
            );
            return Err(empty_response(StatusCode::GATEWAY_TIMEOUT));
        }
        Ok(r) => r,
    };

    tracing::debug!(item_id, backend_id, "after backend.download");

    match download_result {
        Ok(data) => {
            tokio::fs::write(cache_path, &data).await.map_err(|e| {
                tracing::error!(error = %e, "failed to write cache file");
                empty_response(StatusCode::INTERNAL_SERVER_ERROR)
            })?;
            Ok(())
        }
        Err(e) => {
            let status = backend_error_status(&e);
            tracing::warn!(error = %e, ?status, "backend download failed");
            let _ = tokio::fs::remove_file(cache_path).await;
            Err(empty_response(
                if status == StatusCode::INTERNAL_SERVER_ERROR {
                    StatusCode::NOT_FOUND
                } else {
                    status
                },
            ))
        }
    }
}

/// Stream the cache file as the response body, honouring an optional
/// `Range: bytes=…` header. Returns 206 Partial Content for satisfied
/// ranges, 416 Range Not Satisfiable for invalid ones, or 200 OK for
/// full reads. Body is streamed via `ReaderStream` so we never hold the
/// file in memory.
async fn serve_from_cache(cache_path: &std::path::Path, headers: &HeaderMap) -> Response {
    use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
    use tokio_util::io::ReaderStream;

    let Ok(file) = tokio::fs::File::open(cache_path).await else {
        return empty_response(StatusCode::NOT_FOUND);
    };
    let Ok(meta) = file.metadata().await else {
        return empty_response(StatusCode::INTERNAL_SERVER_ERROR);
    };
    let total = meta.len();

    let range_header = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let parsed = range_header.as_deref().map(|r| parse_range(r, total));

    match parsed {
        // No Range header → full file, 200 OK.
        None => {
            let stream = ReaderStream::new(file);
            let mut resp = Response::new(Body::from_stream(stream));
            let h = resp.headers_mut();
            h.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
            h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
            if let Ok(v) = HeaderValue::from_str(&total.to_string()) {
                h.insert(header::CONTENT_LENGTH, v);
            }
            resp
        }
        // Invalid range → 416 with Content-Range: */<total>.
        Some(Err(())) => {
            let mut resp = empty_response(StatusCode::RANGE_NOT_SATISFIABLE);
            if let Ok(v) = HeaderValue::from_str(&format!("bytes */{total}")) {
                resp.headers_mut().insert(header::CONTENT_RANGE, v);
            }
            resp
        }
        // Satisfied range → 206 Partial Content streaming start..=end.
        Some(Ok((start, end))) => {
            let length = end - start + 1;
            let mut file = file;
            if file.seek(SeekFrom::Start(start)).await.is_err() {
                return empty_response(StatusCode::INTERNAL_SERVER_ERROR);
            }
            let stream = ReaderStream::new(file.take(length));
            let mut resp = Response::new(Body::from_stream(stream));
            *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
            let h = resp.headers_mut();
            h.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
            h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
            if let Ok(v) = HeaderValue::from_str(&length.to_string()) {
                h.insert(header::CONTENT_LENGTH, v);
            }
            if let Ok(v) = HeaderValue::from_str(&format!("bytes {start}-{end}/{total}")) {
                h.insert(header::CONTENT_RANGE, v);
            }
            resp
        }
    }
}

/// Parse an HTTP `Range: bytes=…` header value against a known content
/// length. Returns the inclusive byte range on success.
///
/// Accepts the three common forms:
/// - `bytes=N-M` — bytes N..=M
/// - `bytes=N-`  — bytes N..=end
/// - `bytes=-N`  — last N bytes
///
/// Multi-range requests (`bytes=0-99,200-299`) are not supported and
/// return `Err(())` — the caller will reply with 416. This is RFC 7233
/// compliant (servers MAY refuse multipart byteranges).
fn parse_range(header: &str, total: u64) -> std::result::Result<(u64, u64), ()> {
    if total == 0 {
        return Err(());
    }
    let spec = header.strip_prefix("bytes=").ok_or(())?;
    if spec.contains(',') {
        return Err(());
    }
    let (start_s, end_s) = spec.split_once('-').ok_or(())?;

    let (start, end) = match (start_s, end_s) {
        ("", "") => return Err(()),
        ("", n) => {
            // Suffix: last `n` bytes.
            let n: u64 = n.parse().map_err(|_| ())?;
            if n == 0 {
                return Err(());
            }
            let n = n.min(total);
            (total - n, total - 1)
        }
        (s, "") => {
            let start: u64 = s.parse().map_err(|_| ())?;
            (start, total - 1)
        }
        (s, e) => {
            let start: u64 = s.parse().map_err(|_| ())?;
            let end: u64 = e.parse().map_err(|_| ())?;
            (start, end.min(total - 1))
        }
    };

    if start > end || start >= total {
        return Err(());
    }
    Ok((start, end))
}

/// Handle `PUT` — store file contents to the cache.
async fn handle_put(state: &AppState, path: &str, req: Request) -> Response {
    let normalised = normalise_path(path);

    // Reject macOS filesystem metadata (AppleDouble sidecars, .DS_Store,
    // …). Returning 403 makes Finder skip the write silently rather than
    // polluting the cloud backend with sidecar files for every real file.
    if is_macos_junk(last_component(&normalised)) {
        tracing::debug!(path = %normalised, "rejecting macOS metadata write");
        return empty_response(StatusCode::FORBIDDEN);
    }

    // Resolve the backend by mount path and extract the backend-relative path.
    // The VFS path (without leading '/') is looked up in the mount table with
    // longest-prefix-first semantics so nested mounts route correctly.
    let vfs_path = normalised.trim_start_matches('/');
    let (backend, relative_str) = backend_for_path(state, vfs_path).await;
    let Some(backend) = backend else {
        return empty_response(StatusCode::NOT_FOUND);
    };
    let relative_parts: Vec<&str> = relative_str.split('/').filter(|s| !s.is_empty()).collect();
    let relative_path_str = relative_parts.join("/");
    let relative_path = Path::new(&relative_path_str);
    let backend_id = backend.id().to_string();

    let bytes = axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap_or_default();

    // Resolve the parent directory's native ID.
    let relative = relative_parts.as_slice();
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

    let parent_id_str = parent_id.0.clone();
    let relative_path_owned = relative_path.to_path_buf();
    let upload_bytes = bytes.clone();
    let upload_path_for_log = normalised.clone();
    tracing::debug!(
        path = %normalised,
        backend_id,
        existing = existing_file_id.is_some(),
        "before backend upload"
    );

    // The upload is awaited directly on the main runtime. Every backend holds
    // the daemon's single long-lived pooled `reqwest::Client`, whose connection
    // driver lives on this runtime and stays polled — so there is nothing to
    // isolate. (The earlier `run_isolated_blocking` workaround, which ran the
    // upload in a transient current-thread runtime, was the very thing that
    // could strand the driver; it has been removed.)
    let result: anyhow::Result<cascade_engine::types::FileEntry> = {
        let backend_result = if let Some(file_id) = existing_file_id {
            tokio::time::timeout(
                BACKEND_UPLOAD_TIMEOUT,
                backend.update(&file_id, &upload_bytes),
            )
            .await
        } else {
            tokio::time::timeout(
                BACKEND_UPLOAD_TIMEOUT,
                backend.upload(&relative_path_owned, &upload_bytes, &parent_id),
            )
            .await
        };
        match backend_result {
            Ok(r) => r,
            Err(_elapsed) => {
                tracing::error!(
                    path = %upload_path_for_log,
                    timeout_secs = BACKEND_UPLOAD_TIMEOUT.as_secs(),
                    "backend upload timed out"
                );
                Err(anyhow::anyhow!("backend upload timed out after 120 s"))
            }
        }
    };
    tracing::debug!(path = %normalised, "after backend upload");

    match result {
        Ok(mut entry) => {
            // Normalise the parent_id to the value we resolved from the items
            // store, so uploaded files appear in the correct PROPFIND listing
            // regardless of which parent ID the backend API returns.
            entry.parent_id = cascade_engine::types::ItemId(parent_id_str);
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
            let status = backend_error_status(&e);
            tracing::warn!(error = %e, ?status, "backend upload failed");
            empty_response(status)
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
            // Collect the deleted item and all its descendants from the in-memory
            // VFS so they can be evicted from both the DB and the store together.
            let to_remove: Vec<String> = {
                let items = state.items.read().await;
                let mut ids = vec![item_id.clone()];
                let mut queue = vec![item_id.clone()];
                while let Some(parent) = queue.pop() {
                    let children: Vec<String> = items
                        .values()
                        .filter(|item| item.parent_id.0 == parent)
                        .map(|item| item.id.0.clone())
                        .collect();
                    queue.extend(children.clone());
                    ids.extend(children);
                }
                ids
            };

            // Remove the whole subtree from the DB in one statement.
            if let Some(db) = &state.db {
                let _ = db.delete_subtree(&ItemId(item_id.clone()));
            }

            // Evict cache files and VFS entries for every removed ID.
            let mut items = state.items.write().await;
            for id in &to_remove {
                let cache_path = state.cache_dir.join(safe_filename(id));
                let _ = tokio::fs::remove_file(&cache_path).await;
                items.remove(id);
            }

            empty_response(StatusCode::NO_CONTENT)
        }
        Err(e) => {
            let status = backend_error_status(&e);
            tracing::warn!(error = %e, ?status, "backend delete failed");
            empty_response(status)
        }
    }
}

/// Handle `MKCOL` — create a directory.
async fn handle_mkcol(state: &AppState, path: &str) -> Response {
    let normalised = normalise_path(path);

    // Reject macOS volume-metadata directories (`.Spotlight-V100`,
    // `.Trashes`, `.fseventsd`, …) so Spotlight/Finder don't seed them
    // into every backend on first access.
    if is_macos_junk(last_component(&normalised)) {
        tracing::debug!(path = %normalised, "rejecting macOS metadata mkdir");
        return empty_response(StatusCode::FORBIDDEN);
    }

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

    // Resolve the backend by mount path.
    let vfs_path = normalised.trim_start_matches('/');
    let (backend, relative_str) = backend_for_path(state, vfs_path).await;
    let Some(backend) = backend else {
        return empty_response(StatusCode::NOT_FOUND);
    };
    let relative_parts: Vec<&str> = relative_str.split('/').filter(|s| !s.is_empty()).collect();
    let relative_path_str = relative_parts.join("/");
    let relative_path = Path::new(&relative_path_str);
    let backend_id = backend.id().to_string();
    let relative = relative_parts.as_slice();

    // Resolve the parent directory ID from the in-memory store if possible.
    // This avoids a Drive API round-trip that would fail for freshly created
    // parents not yet indexed by the Drive listing.
    let dir_name = relative.last().copied().unwrap_or("New Folder");
    let parent_segments = relative
        .get(..relative.len().saturating_sub(1))
        .unwrap_or(&[]);
    let parent_found_in_items = if parent_segments.is_empty() {
        // Parent is the backend root.
        Some(cascade_engine::types::FileId(format!("{backend_id}:root")))
    } else {
        let parent_normalised =
            normalise_path(&format!("/{backend_id}/{}", parent_segments.join("/")));
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
        Ok(mut entry) => {
            // Normalise the parent_id to match what we used to locate the
            // parent in the items store. Drive API returns the real folder ID
            // (e.g. "0APRsmt7...") even when we passed the "root" alias, so
            // the new item would end up in a different PROPFIND bucket than
            // its siblings unless we rewrite it here.
            if let Some(ref parent_id) = parent_found_in_items {
                entry.parent_id = cascade_engine::types::ItemId(parent_id.0.clone());
            }
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
            let status = backend_error_status(&e);
            tracing::warn!(error = %e, ?status, "backend create_dir failed");
            empty_response(status)
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
            let status = backend_error_status(&e);
            tracing::warn!(error = %e, ?status, "backend move failed");
            empty_response(status)
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
        // The path for a WebDAV COPY destination derives from the
        // destination URL; it defaults to the name at this phase.
        path: new_name.clone(),
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

/// Build the `PROPFIND` response for the neutral VFS root ("/").
///
/// Lists each explicitly-mounted backend as a synthetic top-level collection
/// (`mount_names`) and, for a backend mounted at the empty prefix (the "/"
/// case), the backend's own root children (`at_root_children`) inline. Both
/// sets appear directly under "/", reproducing the single-backend-at-root path
/// shape when only an at-root backend is configured.
#[must_use]
#[allow(clippy::implicit_hasher)]
pub fn build_root_listing(
    mount_names: &[String],
    at_root_children: &[&VfsItem],
    items: &std::collections::HashMap<String, VfsItem>,
) -> String {
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
    for name in mount_names {
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
            xml_escape(name),
        );
    }
    for child in at_root_children {
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
        // Emit a virtual collection entry. This is only reached for backend
        // root paths (e.g. `/gdrive-personal`) which have no corresponding
        // VfsItem — handle_propfind returns 404 for any deeper missing path.
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

/// Compute the `WebDAV` `href` path for a VFS item.
///
/// Returns the full VFS path with a leading `/`. The path is read directly
/// from `item.path` (the mount-prefixed VFS-absolute path stored by the sync
/// runner) rather than re-derived from the `ItemId`, so the presenter renders
/// by mount PATH and not by backend ID.
#[must_use]
pub fn item_path(item: &VfsItem) -> String {
    if item.path.starts_with('/') {
        item.path.clone()
    } else {
        format!("/{}", item.path)
    }
}

/// Resolve a VFS path to its owning backend using the mount table.
///
/// `vfs_path` is the path without a leading `/` (matching the
/// `VfsItem.path` / `files.path` representation). Longest-prefix match:
/// the first mount whose prefix is a leading component of `vfs_path` wins.
/// An at-root backend (empty prefix) is tried after all explicit prefixes.
///
/// Returns `(Some(backend), backend_relative_path)` on a match, or
/// `(None, vfs_path)` when no backend is found for this path.
async fn backend_for_path<'a>(
    state: &'a AppState,
    vfs_path: &'a str,
) -> (Option<Arc<dyn cascade_engine::backend::Backend>>, &'a str) {
    let mounts = state.mounts.read().await;
    let mut at_root_backend: Option<Arc<dyn cascade_engine::backend::Backend>> = None;

    for (prefix, backend) in mounts.iter() {
        let prefix_str = prefix.to_string_lossy();
        if prefix_str.is_empty() {
            // At-root backend is the final fallback; record it and keep going.
            at_root_backend = Some(backend.clone());
            continue;
        }
        if vfs_path == prefix_str.as_ref() {
            return (Some(backend.clone()), "");
        }
        let with_slash = format!("{prefix_str}/");
        if let Some(rest) = vfs_path.strip_prefix(with_slash.as_str()) {
            return (Some(backend.clone()), rest);
        }
    }

    if let Some(b) = at_root_backend {
        return (Some(b), vfs_path);
    }

    // No mount table configured (e.g. tests) — fall back to treating the
    // first path segment as a backend ID and looking up by ID.
    drop(mounts);
    let first_segment = vfs_path.split('/').next().unwrap_or("");
    let rest = vfs_path
        .strip_prefix(first_segment)
        .and_then(|s| s.strip_prefix('/'))
        .unwrap_or("");
    let backend = {
        let backends = state.backends.read().await;
        backends.iter().find(|b| b.id() == first_segment).cloned()
    };
    (backend, rest)
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

/// Resolve the `WebDAV` `href` path for a VFS item.
///
/// The `VfsItem.path` field carries the full mount-prefixed VFS-absolute path
/// (no leading slash) as written by the sync runner. This function prepends a
/// `/` to produce the `href` used in `PROPFIND` responses and route matching.
///
/// The `items` parameter is retained for API compatibility but is no longer
/// consulted — the path is complete in `item.path` so parent-chain walking is
/// unnecessary.
fn resolve_full_path(
    item: &VfsItem,
    _items: &std::collections::HashMap<String, VfsItem>,
) -> String {
    item_path(item)
}

/// Normalise a URL path for consistent matching.
/// Removes trailing slashes, ensures no double slashes.
async fn read_items(
    items: &Arc<RwLock<HashMap<String, VfsItem>>>,
) -> tokio::sync::RwLockReadGuard<'_, HashMap<String, VfsItem>> {
    items.read().await
}

/// Resolve the mount prefix a backend is mounted at, from the mount table.
///
/// Returns the `PathBuf` prefix the backend is mounted under (e.g. `personal`),
/// or an empty `PathBuf` when the backend is mounted at `/` or when no mount
/// table is configured (the unit-test path). An empty prefix makes
/// [`apply_mount_prefix`] a no-op, so expanded paths stay byte-identical to the
/// pre-refactor single-backend-at-root shape.
async fn mount_prefix_for_backend(state: &AppState, backend_id: &str) -> PathBuf {
    let mounts = state.mounts.read().await;
    mounts
        .iter()
        .find(|(_, backend)| backend.id() == backend_id)
        .map_or_else(PathBuf::new, |(prefix, _)| prefix.clone())
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

    // The expanded children sit directly beneath the parent directory, whose
    // own `VfsItem.path` is already the full, mount-prefixed VFS path. Join
    // each child's basename onto it so the children carry prefixed paths too,
    // mirroring `SyncRunner::repath_entry`. When the parent is genuinely
    // absent from the store (it should not be — `expand_directory` is only
    // reached for an item already resolved from the store), fall back to the
    // backend's mount prefix so root-level children still gain the prefix
    // rather than a bare basename.
    let parent_vfs_path = {
        let items = read_items(&state.items).await;
        items.get(&item_id.0).map(|item| item.path.clone())
    };

    match backend.list_children(native_id).await {
        Ok(entries) => {
            let prefix = mount_prefix_for_backend(state, backend_id).await;
            let mut items = state.items.write().await;
            for entry in entries {
                let key = entry.id.0.clone();
                let mut vfs_item = VfsItem::from(entry);
                vfs_item.path = match &parent_vfs_path {
                    Some(parent_path) if !parent_path.is_empty() => {
                        format!("{parent_path}/{}", vfs_item.name)
                    }
                    // No (or empty) parent path: place the child directly under
                    // the mount prefix. For a backend at `/` this is a no-op,
                    // yielding the bare basename as before.
                    _ => apply_mount_prefix(&prefix, &vfs_item.name),
                };
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

    let prefix = mount_prefix_for_backend(state, backend_prefix).await;

    match backend.list_children("root").await {
        Ok(entries) => {
            let mut items = state.items.write().await;
            for entry in entries {
                let key = entry.id.0.clone();
                let mut vfs_item = VfsItem::from(entry);
                // Root children sit directly under the backend's mount prefix.
                // Stamp the full, mount-prefixed VFS path so the WebDAV href
                // and URL matching agree with the sync runner's `files.path`.
                // A backend at `/` (empty prefix) yields the bare basename,
                // preserving the single-backend-at-root path shape.
                vfs_item.path = apply_mount_prefix(&prefix, &vfs_item.name);
                // Normalise the parent_id to the root alias. The Drive API
                // returns the real folder ID (e.g. "0APRsmt7...") as the
                // parent of root-level items, but the PROPFIND listing filters
                // by the alias so all root children must use it consistently.
                vfs_item.parent_id = cascade_engine::types::ItemId(root_key.clone());
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
    // Percent-decode so paths containing spaces and other non-ASCII characters
    // (e.g. "/gdrive-personal/My%20Drive") match the resolved item paths,
    // which are decoded UTF-8.
    let decoded =
        urlencoding::decode(path).map_or_else(|_| path.to_string(), std::borrow::Cow::into_owned);
    let p = decoded.trim_end_matches('/');
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
#[path = "server_tests.rs"]
mod tests;
