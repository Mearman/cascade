//! File and folder routes — directory listings, entry metadata, file content,
//! search, and the subtree archive.
//!
//! Folder-metadata routes (`children`, `entries`, `search`) require
//! `status:read` over the folder. File-content routes
//! (`/v1/files/{folder}/entries/{path}`) and the `archive` stream require
//! `data:read` (read) or `data:write` (write) and are gated by the F3
//! data-plane readiness bit — every one returns `503 data_plane_not_ready`
//! until the data plane comes up. An unknown `{folder}` is `404 not_found`.

use std::io::Write as _;
use std::path::{Path as FsPath, PathBuf};

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use cascade_engine::manage::{Capability, Scope};
use cascade_engine::types::{Change, DirEntry, FileEntry as EngineFileEntry, FileId};
use serde::Deserialize;

use crate::auth::Session;
use crate::error::{ApiError, ErrorCode};
use crate::routes::{decode_cursor, encode_cursor, require_data_plane_ready, require_known_folder};
use crate::schemas::common::{DEFAULT_PAGE_LIMIT, MAX_PAGE_LIMIT, MIN_PAGE_LIMIT};
use crate::schemas::files::{EntryKind, FileEntry, FolderChildren, SearchResponse};
use crate::state::AppState;

/// The default content type for a file whose backend reports no MIME type.
const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";

/// Register the file and folder routes.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/folders/{folder}/children", get(children))
        .route("/v1/folders/{folder}/entries/{*path}", get(folder_entry))
        .route("/v1/folders/{folder}/search", get(search))
        .route("/v1/folders/{folder}/archive", get(archive))
        .route(
            "/v1/files/{folder}/entries/{*path}",
            get(get_file)
                .head(head_file)
                .put(put_file)
                .delete(delete_file),
        )
}

/// The VFS path base for a canonical folder id (`p2p-<name>` → `<name>`), the
/// prefix the engine mounts the backend at.
fn folder_base(folder_id: &str) -> PathBuf {
    PathBuf::from(folder_id.strip_prefix("p2p-").unwrap_or(folder_id))
}

/// Query parameters for `children`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
struct ChildrenQuery {
    #[serde(default)]
    path: Option<String>,
    limit: Option<usize>,
    cursor: Option<String>,
}

/// Query parameters for `search`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
struct SearchQuery {
    q: String,
    limit: Option<usize>,
}

/// Map a VFS [`DirEntry`] to the schema [`FileEntry`], with its path joined onto
/// the listed directory.
fn dir_entry_to_schema(parent_path: &str, entry: &DirEntry) -> FileEntry {
    let path = if parent_path.is_empty() {
        entry.name.clone()
    } else {
        format!("{}/{}", parent_path.trim_end_matches('/'), entry.name)
    };
    FileEntry {
        name: entry.name.clone(),
        path,
        kind: if entry.is_dir {
            EntryKind::Dir
        } else {
            EntryKind::File
        },
        size: None,
        mtime: None,
        etag: None,
    }
}

/// Map an engine [`FileEntry`](EngineFileEntry) to the schema view.
fn engine_entry_to_schema(path: &str, entry: &EngineFileEntry) -> FileEntry {
    FileEntry {
        name: entry.name.clone(),
        path: path.to_owned(),
        kind: if entry.is_dir {
            EntryKind::Dir
        } else {
            EntryKind::File
        },
        size: entry.size,
        mtime: entry.mod_time,
        etag: entry.hash.clone(),
    }
}

/// `GET /v1/folders/{folder}/children` — capability: `status:read`.
async fn children(
    State(state): State<AppState>,
    session: Session,
    Path(folder): Path<String>,
    Query(query): Query<ChildrenQuery>,
) -> Result<Json<FolderChildren>, ApiError> {
    session.require(
        &state,
        Capability::StatusRead,
        &Scope::folder(folder.clone()),
    )?;
    require_known_folder(&state, &folder)?;

    let sub_path = query.path.clone().unwrap_or_default();
    let listing = read_dir(&state, &folder, &sub_path).await?;

    let limit = query
        .limit
        .unwrap_or(DEFAULT_PAGE_LIMIT)
        .clamp(MIN_PAGE_LIMIT, MAX_PAGE_LIMIT);
    let offset = query
        .cursor
        .as_deref()
        .map(decode_cursor)
        .transpose()?
        .map_or(0, |id| usize::try_from(id).unwrap_or(0));

    let mut entries: Vec<FileEntry> = listing
        .iter()
        .map(|entry| dir_entry_to_schema(&sub_path, entry))
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    let page: Vec<FileEntry> = entries.iter().skip(offset).take(limit).cloned().collect();
    let next_offset = offset.saturating_add(page.len());
    let next_cursor = (next_offset < entries.len())
        .then(|| encode_cursor(i64::try_from(next_offset).unwrap_or(i64::MAX)));

    Ok(Json(FolderChildren {
        folder,
        path: sub_path,
        entries: page,
        next_cursor,
    }))
}

/// `GET /v1/folders/{folder}/entries/{path}` — capability: `status:read`.
async fn folder_entry(
    State(state): State<AppState>,
    session: Session,
    Path((folder, path)): Path<(String, String)>,
) -> Result<Json<FileEntry>, ApiError> {
    session.require(
        &state,
        Capability::StatusRead,
        &Scope::folder(folder.clone()),
    )?;
    require_known_folder(&state, &folder)?;
    let entry = metadata(&state, &folder, &path).await?;
    Ok(Json(engine_entry_to_schema(&path, &entry)))
}

/// `GET /v1/folders/{folder}/search` — capability: `status:read`. Substring
/// match on entry name.
async fn search(
    State(state): State<AppState>,
    session: Session,
    Path(folder): Path<String>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<SearchResponse>, ApiError> {
    session.require(
        &state,
        Capability::StatusRead,
        &Scope::folder(folder.clone()),
    )?;
    require_known_folder(&state, &folder)?;

    let limit = query
        .limit
        .unwrap_or(DEFAULT_PAGE_LIMIT)
        .clamp(MIN_PAGE_LIMIT, MAX_PAGE_LIMIT);
    let needle = query.q.to_lowercase();
    let listing = read_dir(&state, &folder, "").await?;
    let entries: Vec<FileEntry> = listing
        .iter()
        .filter(|entry| entry.name.to_lowercase().contains(&needle))
        .take(limit)
        .map(|entry| dir_entry_to_schema("", entry))
        .collect();

    Ok(Json(SearchResponse {
        folder,
        query: query.q,
        entries,
        next_cursor: None,
    }))
}

/// `GET /v1/files/{folder}/entries/{path}` — capability: `data:read`.
async fn get_file(
    State(state): State<AppState>,
    session: Session,
    headers: HeaderMap,
    Path((folder, path)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    fetch_file(&state, &session, &folder, &path, true, &headers).await
}

/// `HEAD /v1/files/{folder}/entries/{path}` — capability: `data:read`.
async fn head_file(
    State(state): State<AppState>,
    session: Session,
    headers: HeaderMap,
    Path((folder, path)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    fetch_file(&state, &session, &folder, &path, false, &headers).await
}

/// Shared read path for `GET` and `HEAD`: gate, authorise, fetch metadata, and
/// (for `GET`) the content.
async fn fetch_file(
    state: &AppState,
    session: &Session,
    folder: &str,
    path: &str,
    include_body: bool,
    _headers: &HeaderMap,
) -> Result<Response, ApiError> {
    require_data_plane_ready(state)?;
    session.require(
        state,
        Capability::DataRead,
        &Scope::folder(folder.to_owned()),
    )?;
    require_known_folder(state, folder)?;

    let entry = metadata(state, folder, path).await?;
    if entry.is_dir {
        return Err(ApiError::unprocessable("path is a directory, not a file"));
    }

    let content_type = entry
        .mime_type
        .clone()
        .unwrap_or_else(|| DEFAULT_CONTENT_TYPE.to_owned());
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type);
    if let Some(hash) = &entry.hash {
        builder = builder.header(header::ETAG, format!("\"{hash}\""));
    }

    let body = if include_body {
        let bytes = download(state, folder, &entry).await?;
        Body::from(bytes)
    } else {
        Body::empty()
    };
    builder
        .body(body)
        .map_err(|e| ApiError::internal(format!("could not build file response: {e}")))
}

/// `PUT /v1/files/{folder}/entries/{path}` — capability: `data:write`.
async fn put_file(
    State(state): State<AppState>,
    session: Session,
    headers: HeaderMap,
    Path((folder, path)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    require_data_plane_ready(&state)?;
    session.require(
        &state,
        Capability::DataWrite,
        &Scope::folder(folder.clone()),
    )?;
    require_known_folder(&state, &folder)?;

    if body.len() > state.bind.max_body_bytes {
        return Err(ApiError::new(
            ErrorCode::PayloadTooLarge,
            format!(
                "request body {} bytes exceeds the maximum {}",
                body.len(),
                state.bind.max_body_bytes
            ),
        ));
    }

    let base = folder_base(&folder);
    let rel = base.join(path.trim_start_matches('/'));
    let vfs = state.engine.vfs();
    let (backend, backend_rel) = {
        let tree = vfs
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (backend, backend_rel) = tree.resolve(&rel);
        (backend.clone(), backend_rel)
    };

    let written = match backend.metadata(&backend_rel).await {
        Ok(existing) if existing.is_dir => {
            return Err(ApiError::unprocessable("path is a directory, not a file"));
        }
        Ok(existing) => {
            check_if_match(&headers, existing.hash.as_deref())?;
            backend
                .update(&FileId(existing.id.0.clone()), &body)
                .await
                .map_err(|e| ApiError::internal(format!("could not write file: {e}")))?
        }
        Err(_) => {
            // New file: address it under its parent directory.
            let parent_rel = backend_rel
                .parent()
                .map(FsPath::to_path_buf)
                .unwrap_or_default();
            let parent = backend
                .metadata(&parent_rel)
                .await
                .map_err(|e| ApiError::not_found(format!("parent directory not found: {e}")))?;
            backend
                .upload(&backend_rel, &body, &FileId(parent.id.0.clone()))
                .await
                .map_err(|e| ApiError::internal(format!("could not write file: {e}")))?
        }
    };

    Ok((
        StatusCode::OK,
        Json(engine_entry_to_schema(&path, &written)),
    )
        .into_response())
}

/// `DELETE /v1/files/{folder}/entries/{path}` — capability: `data:write`.
async fn delete_file(
    State(state): State<AppState>,
    session: Session,
    Path((folder, path)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    require_data_plane_ready(&state)?;
    session.require(
        &state,
        Capability::DataWrite,
        &Scope::folder(folder.clone()),
    )?;
    require_known_folder(&state, &folder)?;

    let entry = metadata(&state, &folder, &path).await?;
    let base = folder_base(&folder);
    let rel = base.join(path.trim_start_matches('/'));
    let vfs = state.engine.vfs();
    let backend = {
        let tree = vfs
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (backend, _) = tree.resolve(&rel);
        backend.clone()
    };
    backend
        .delete(&entry)
        .await
        .map_err(|e| ApiError::internal(format!("could not delete file: {e}")))?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /v1/folders/{folder}/archive` — capability: `data:read`. Streams a
/// gzipped tar of the subtree.
async fn archive(
    State(state): State<AppState>,
    session: Session,
    Path(folder): Path<String>,
) -> Result<Response, ApiError> {
    require_data_plane_ready(&state)?;
    session.require(&state, Capability::DataRead, &Scope::folder(folder.clone()))?;
    require_known_folder(&state, &folder)?;

    // Collect (path, bytes) for every file in the subtree, then build the tar.
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    collect_subtree(&state, &folder, "", &mut files).await?;

    let gz = build_targz(&files)
        .map_err(|e| ApiError::internal(format!("could not build archive: {e}")))?;

    let filename = format!("{folder}.tar.gz");
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/gzip")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .body(Body::from(gz))
        .map_err(|e| ApiError::internal(format!("could not build archive response: {e}")))
}

/// List a directory within a folder via the VFS.
///
/// Mirrors `VfsTree::read_dir` but takes the `std::sync::RwLock` guard only
/// briefly — to clone the owning backend and the child mount names — then drops
/// it before awaiting the backend, so the handler future stays `Send`. (The
/// guard is `!Send`, so it can never be held across an `.await`.)
async fn read_dir(
    state: &AppState,
    folder: &str,
    sub_path: &str,
) -> Result<Vec<DirEntry>, ApiError> {
    let path = folder_base(folder).join(sub_path.trim_start_matches('/'));
    let vfs = state.engine.vfs();
    let (backend, child_dirs) = {
        let tree = vfs
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (backend, _) = tree.resolve(&path);
        let child_dirs: Vec<String> = tree
            .children()
            .iter()
            .filter(|(prefix, _)| prefix.parent() == Some(path.as_path()))
            .filter_map(|(prefix, _)| {
                prefix
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .collect();
        (backend.clone(), child_dirs)
    };

    let (changes, _) = backend
        .changes(None)
        .await
        .map_err(|e| ApiError::internal(format!("could not list directory: {e}")))?;
    let mut entries: Vec<DirEntry> = changes
        .into_iter()
        .filter_map(|change| match change {
            Change::Created(entry) => Some(DirEntry {
                name: entry.name,
                is_dir: entry.is_dir,
            }),
            _ => None,
        })
        .collect();
    for dir in child_dirs {
        if !entries.iter().any(|entry| entry.name == dir) {
            entries.push(DirEntry::dir(dir));
        }
    }
    Ok(entries)
}

/// Fetch a single entry's metadata within a folder via the VFS.
async fn metadata(state: &AppState, folder: &str, path: &str) -> Result<EngineFileEntry, ApiError> {
    let rel = folder_base(folder).join(path.trim_start_matches('/'));
    let vfs = state.engine.vfs();
    let (backend, backend_rel) = {
        let tree = vfs
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (backend, backend_rel) = tree.resolve(&rel);
        (backend.clone(), backend_rel)
    };
    backend
        .metadata(&backend_rel)
        .await
        .map_err(|_| ApiError::not_found(format!("no entry at {path}")))
}

/// Download a file's content via the owning backend.
async fn download(
    state: &AppState,
    folder: &str,
    entry: &EngineFileEntry,
) -> Result<Vec<u8>, ApiError> {
    let rel = folder_base(folder);
    let vfs = state.engine.vfs();
    let backend = {
        let tree = vfs
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (backend, _) = tree.resolve(&rel);
        backend.clone()
    };
    let buf = backend
        .download(entry)
        .await
        .map_err(|e| ApiError::internal(format!("could not download file: {e}")))?;
    Ok(buf)
}

/// Recursively collect every file's content under a folder subtree.
async fn collect_subtree(
    state: &AppState,
    folder: &str,
    sub_path: &str,
    out: &mut Vec<(String, Vec<u8>)>,
) -> Result<(), ApiError> {
    let entries = read_dir(state, folder, sub_path).await?;
    for entry in entries {
        let child_path = if sub_path.is_empty() {
            entry.name.clone()
        } else {
            format!("{}/{}", sub_path.trim_end_matches('/'), entry.name)
        };
        if entry.is_dir {
            Box::pin(collect_subtree(state, folder, &child_path, out)).await?;
        } else {
            let meta = metadata(state, folder, &child_path).await?;
            let bytes = download(state, folder, &meta).await?;
            out.push((child_path, bytes));
        }
    }
    Ok(())
}

/// Build a gzipped tar from a list of (path, content) pairs.
fn build_targz(files: &[(String, Vec<u8>)]) -> std::io::Result<Vec<u8>> {
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::default(),
    ));
    for (path, bytes) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, path, bytes.as_slice())?;
    }
    let encoder = builder.into_inner()?;
    let mut buffer = encoder.finish()?;
    buffer.flush()?;
    Ok(buffer)
}

/// Enforce an `If-Match` precondition against the current etag, when present.
fn check_if_match(headers: &HeaderMap, current_hash: Option<&str>) -> Result<(), ApiError> {
    let Some(if_match) = headers.get(header::IF_MATCH) else {
        return Ok(());
    };
    let requested = if_match
        .to_str()
        .map_err(|_| ApiError::unprocessable("If-Match header is not valid text"))?
        .trim()
        .trim_matches('"');
    if current_hash == Some(requested) {
        Ok(())
    } else {
        Err(ApiError::new(
            ErrorCode::PreconditionFailed,
            "If-Match etag does not match the current entry",
        ))
    }
}
