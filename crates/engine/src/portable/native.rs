//! Native adapters for the portable IO contracts.
//!
//! Each adapter here binds one [`super`] trait to the host-only crate it was
//! abstracting over:
//!
//! - [`TokioRuntimeHandle`] → `tokio::runtime::Handle` ([`super::RuntimeHandle`]).
//! - [`SqliteStorage`] → [`crate::db::StateDb`] (rusqlite) ([`super::StateStorage`]).
//! - [`ReqwestClient`] → `reqwest::Client` ([`super::HttpClient`]).
//! - [`StdFileSystem`] → `std::fs` ([`super::FileSystem`]).
//!
//! The module compiles only for the native build. A `--features portable`
//! build (wasm or otherwise) drops it and supplies adapters over the browser's
//! equivalents behind the very same contracts, so nothing downstream of the
//! traits has to know which adapter is bound.
//!
//! [`StateDb`] is synchronous and guards a single
//! `rusqlite` connection behind a mutex, so every storage call is offloaded to
//! the runtime's blocking pool via [`super::RuntimeHandle::spawn_blocking`]
//! rather than run on the async path. The filesystem adapter offloads its
//! `std::fs` calls the same way, through `tokio::task::spawn_blocking`.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::{
    BoxFuture, FileSystem, FsDirEntry, FsError, HeaderMap, HttpClient, HttpError, HttpResponse,
    JoinError, JoinHandle, RuntimeHandle, StateStorage, StorageError,
};
#[cfg(feature = "p2p")]
use crate::db::TokenRecord;
use crate::db::{
    AuditEntry, AuditRecord, BackendRecord, DirtyFileRecord, ExplicitControlRecord, GrantRecord,
    LifecyclePolicyRecord, PeerRecord, PinRuleRecord, QuarantineRecord, StateDb,
};
#[cfg(feature = "p2p")]
use crate::manage::token::CapabilityToken;
use crate::manage::{Grant, Scope};
use crate::types::{CacheState, Cursor, FileEntry, ItemId};
use std::collections::HashSet;
use std::time::Duration;

// ─────────────────────────── Runtime ───────────────────────────

/// [`RuntimeHandle`] backed by a tokio runtime handle.
#[derive(Clone)]
pub struct TokioRuntimeHandle(tokio::runtime::Handle);

impl TokioRuntimeHandle {
    /// Wrap an existing tokio runtime handle.
    #[must_use]
    pub const fn new(handle: tokio::runtime::Handle) -> Self {
        Self(handle)
    }

    /// Capture the handle of the runtime the caller is currently running on.
    ///
    /// # Panics
    ///
    /// Panics if called outside the context of a tokio runtime, mirroring
    /// `tokio::runtime::Handle::current`.
    #[must_use]
    pub fn current() -> Self {
        Self(tokio::runtime::Handle::current())
    }
}

impl std::fmt::Debug for TokioRuntimeHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokioRuntimeHandle").finish_non_exhaustive()
    }
}

impl RuntimeHandle for TokioRuntimeHandle {
    fn spawn(&self, fut: BoxFuture<()>) {
        // The detached task owns itself; the join handle is deliberately
        // discarded (its result is `()`), so drop it explicitly rather than
        // leave tokio's `#[must_use]` handle unused.
        drop(self.0.spawn(fut));
    }

    fn spawn_blocking<F, R>(&self, f: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let handle = self.0.spawn_blocking(f);
        JoinHandle::new(Box::pin(async move {
            handle.await.map_err(|e| JoinError(e.to_string()))
        }))
    }

    fn sleep(&self, duration: Duration) -> BoxFuture<()> {
        Box::pin(tokio::time::sleep(duration))
    }
}

// ─────────────────────────── State storage ───────────────────────────

/// [`StateStorage`] backed by the synchronous [`StateDb`].
///
/// Generic over the [`RuntimeHandle`] so the blocking `rusqlite` work runs on
/// whichever runtime the engine is composed with, never on the async path.
pub struct SqliteStorage<R: RuntimeHandle> {
    db: Arc<StateDb>,
    runtime: R,
}

impl<R: RuntimeHandle> SqliteStorage<R> {
    /// Bind a [`StateDb`] to a runtime handle.
    pub const fn new(db: Arc<StateDb>, runtime: R) -> Self {
        Self { db, runtime }
    }

    /// Offload a synchronous [`StateDb`] call to the runtime's blocking pool,
    /// mapping its [`anyhow::Error`] into a [`StorageError`] and a failed join
    /// into [`StorageError::Unavailable`].
    async fn run<T, F>(&self, f: F) -> Result<T, StorageError>
    where
        F: FnOnce(&StateDb) -> anyhow::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let db = Arc::clone(&self.db);
        self.runtime
            .spawn_blocking(move || f(db.as_ref()).map_err(map_storage_err))
            .await
            .map_err(|e| StorageError::Unavailable(e.0))?
    }
}

impl<R: RuntimeHandle> std::fmt::Debug for SqliteStorage<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteStorage").finish_non_exhaustive()
    }
}

/// Map a [`StateDb`] error into the portable [`StorageError`] vocabulary.
///
/// A `SQLite` constraint failure becomes [`StorageError::Constraint`] and a
/// serde failure becomes [`StorageError::Serialisation`]; anything else is
/// surfaced as [`StorageError::Unavailable`] rather than guessed at.
fn map_storage_err(err: anyhow::Error) -> StorageError {
    if let Some(rusqlite::Error::SqliteFailure(inner, _)) = err.downcast_ref::<rusqlite::Error>()
        && inner.code == rusqlite::ErrorCode::ConstraintViolation
    {
        return StorageError::Constraint(err.to_string());
    }
    if err.is::<serde_json::Error>() {
        return StorageError::Serialisation(err.to_string());
    }
    StorageError::Unavailable(err.to_string())
}

#[async_trait]
impl<R: RuntimeHandle> StateStorage for SqliteStorage<R> {
    // ── File operations ──

    async fn upsert_file(&self, entry: &FileEntry) -> Result<(), StorageError> {
        let entry = entry.clone();
        self.run(move |db| db.upsert_file(&entry)).await
    }

    async fn get_file(&self, id: &ItemId) -> Result<Option<FileEntry>, StorageError> {
        let id = id.clone();
        self.run(move |db| db.get_file(&id)).await
    }

    async fn delete_file(&self, id: &ItemId) -> Result<(), StorageError> {
        let id = id.clone();
        self.run(move |db| db.delete_file(&id)).await
    }

    async fn delete_subtree(&self, root_id: &ItemId) -> Result<(), StorageError> {
        let root_id = root_id.clone();
        self.run(move |db| db.delete_subtree(&root_id)).await
    }

    async fn update_cache_state(&self, id: &ItemId, state: CacheState) -> Result<(), StorageError> {
        let id = id.clone();
        self.run(move |db| db.update_cache_state(&id, state)).await
    }

    async fn get_cache_state(&self, id: &ItemId) -> Result<Option<CacheState>, StorageError> {
        let id = id.clone();
        self.run(move |db| db.get_cache_state(&id)).await
    }

    // ── Sync cursor operations ──

    async fn set_cursor(&self, backend_id: &str, cursor: &Cursor) -> Result<(), StorageError> {
        let backend_id = backend_id.to_owned();
        let cursor = cursor.clone();
        self.run(move |db| db.set_cursor(&backend_id, &cursor))
            .await
    }

    async fn get_cursor(&self, backend_id: &str) -> Result<Option<Cursor>, StorageError> {
        let backend_id = backend_id.to_owned();
        self.run(move |db| db.get_cursor(&backend_id)).await
    }

    // ── Backend registration ──

    async fn register_backend(
        &self,
        id: &str,
        backend_type: &str,
        display_name: &str,
        mount_path: Option<&str>,
        config: Option<&str>,
    ) -> Result<(), StorageError> {
        let id = id.to_owned();
        let backend_type = backend_type.to_owned();
        let display_name = display_name.to_owned();
        let mount_path = mount_path.map(ToOwned::to_owned);
        let config = config.map(ToOwned::to_owned);
        self.run(move |db| {
            db.register_backend(
                &id,
                &backend_type,
                &display_name,
                mount_path.as_deref(),
                config.as_deref(),
            )
        })
        .await
    }

    async fn remove_backend(&self, id: &str) -> Result<bool, StorageError> {
        let id = id.to_owned();
        self.run(move |db| db.remove_backend(&id)).await
    }

    async fn list_backends(&self) -> Result<Vec<BackendRecord>, StorageError> {
        self.run(StateDb::list_backends).await
    }

    // ── Pin rule operations ──

    async fn add_pin_rule(
        &self,
        path_glob: &str,
        recursive: bool,
        conditions: Option<&str>,
    ) -> Result<(), StorageError> {
        let path_glob = path_glob.to_owned();
        let conditions = conditions.map(ToOwned::to_owned);
        self.run(move |db| db.add_pin_rule(&path_glob, recursive, conditions.as_deref()))
            .await
    }

    async fn remove_pin_rule(&self, path_glob: &str) -> Result<bool, StorageError> {
        let path_glob = path_glob.to_owned();
        self.run(move |db| db.remove_pin_rule(&path_glob)).await
    }

    async fn list_pin_rules(&self) -> Result<Vec<PinRuleRecord>, StorageError> {
        self.run(StateDb::list_pin_rules).await
    }

    // ── Lifecycle policy operations ──

    async fn add_lifecycle_policy(
        &self,
        path_glob: &str,
        max_age: Option<i64>,
        max_file_size: Option<i64>,
        priority: i32,
        conditions: Option<&str>,
    ) -> Result<(), StorageError> {
        let path_glob = path_glob.to_owned();
        let conditions = conditions.map(ToOwned::to_owned);
        self.run(move |db| {
            db.add_lifecycle_policy(
                &path_glob,
                max_age,
                max_file_size,
                priority,
                conditions.as_deref(),
            )
        })
        .await
    }

    async fn list_lifecycle_policies(&self) -> Result<Vec<LifecyclePolicyRecord>, StorageError> {
        self.run(StateDb::list_lifecycle_policies).await
    }

    async fn remove_lifecycle_policy(&self, id: i64) -> Result<bool, StorageError> {
        self.run(move |db| db.remove_lifecycle_policy(id)).await
    }

    // ── Cache queries ──

    async fn list_files_by_cache_state(
        &self,
        state: CacheState,
    ) -> Result<Vec<FileEntry>, StorageError> {
        self.run(move |db| db.list_files_by_cache_state(state))
            .await
    }

    async fn list_all_files(&self) -> Result<Vec<FileEntry>, StorageError> {
        self.run(StateDb::list_all_files).await
    }

    async fn list_children(&self, parent_id: &str) -> Result<Vec<FileEntry>, StorageError> {
        let parent_id = parent_id.to_owned();
        self.run(move |db| db.list_children(&parent_id)).await
    }

    async fn cache_size(&self) -> Result<i64, StorageError> {
        self.run(StateDb::cache_size).await
    }

    // ── Dirty file operations ──

    async fn mark_dirty(&self, id: &ItemId) -> Result<(), StorageError> {
        let id = id.clone();
        self.run(move |db| db.mark_dirty(&id)).await
    }

    async fn clear_dirty(&self, id: &ItemId) -> Result<(), StorageError> {
        let id = id.clone();
        self.run(move |db| db.clear_dirty(&id)).await
    }

    async fn set_file_paths(
        &self,
        id: &ItemId,
        path: &str,
        local_path: &str,
    ) -> Result<(), StorageError> {
        let id = id.clone();
        let path = path.to_owned();
        let local_path = local_path.to_owned();
        self.run(move |db| db.set_file_paths(&id, &path, &local_path))
            .await
    }

    async fn is_dirty(&self, id: &ItemId) -> Result<Option<bool>, StorageError> {
        let id = id.clone();
        self.run(move |db| db.is_dirty(&id)).await
    }

    async fn list_dirty_files(&self) -> Result<Vec<DirtyFileRecord>, StorageError> {
        self.run(StateDb::list_dirty_files).await
    }

    async fn eviction_candidates(&self, limit: usize) -> Result<Vec<FileEntry>, StorageError> {
        self.run(move |db| db.eviction_candidates(limit)).await
    }

    // ── P2P operations ──

    async fn index_p2p_blocks(
        &self,
        file_id: &ItemId,
        block_hashes: &[[u8; 32]],
    ) -> Result<(), StorageError> {
        let file_id = file_id.clone();
        let block_hashes = block_hashes.to_vec();
        self.run(move |db| db.index_p2p_blocks(&file_id, &block_hashes))
            .await
    }

    async fn get_p2p_blocks(&self, file_id: &ItemId) -> Result<Vec<[u8; 32]>, StorageError> {
        let file_id = file_id.clone();
        self.run(move |db| db.get_p2p_blocks(&file_id)).await
    }

    async fn upsert_peer(
        &self,
        device_id: &str,
        address: &str,
        last_seen: DateTime<Utc>,
    ) -> Result<(), StorageError> {
        let device_id = device_id.to_owned();
        let address = address.to_owned();
        self.run(move |db| db.upsert_peer(&device_id, &address, last_seen))
            .await
    }

    async fn list_peers(&self) -> Result<Vec<PeerRecord>, StorageError> {
        self.run(StateDb::list_peers).await
    }

    // ── Management-plane grant operations ──

    async fn insert_grant(&self, grant: &Grant) -> Result<i64, StorageError> {
        let grant = grant.clone();
        self.run(move |db| db.insert_grant(&grant)).await
    }

    async fn list_grants(&self) -> Result<Vec<GrantRecord>, StorageError> {
        self.run(StateDb::list_grants).await
    }

    async fn grant_scope(&self, id: i64) -> Result<Option<Scope>, StorageError> {
        self.run(move |db| db.grant_scope(id)).await
    }

    async fn revoke_grant(&self, id: i64) -> Result<bool, StorageError> {
        self.run(move |db| db.revoke_grant(id)).await
    }

    async fn list_data_grants(&self) -> Result<Vec<GrantRecord>, StorageError> {
        self.run(StateDb::list_data_grants).await
    }

    // ── Management-plane audit operations ──

    async fn append_audit(&self, entry: &AuditEntry) -> Result<i64, StorageError> {
        let entry = entry.clone();
        self.run(move |db| db.append_audit(&entry)).await
    }

    async fn list_audit(&self) -> Result<Vec<AuditRecord>, StorageError> {
        self.run(StateDb::list_audit).await
    }

    // ── Capability-token operations ──

    #[cfg(feature = "p2p")]
    async fn insert_token(
        &self,
        token: &CapabilityToken,
        issued_at: DateTime<Utc>,
    ) -> Result<(), StorageError> {
        let token = token.clone();
        self.run(move |db| db.insert_token(&token, issued_at)).await
    }

    #[cfg(feature = "p2p")]
    async fn list_tokens(&self) -> Result<Vec<TokenRecord>, StorageError> {
        self.run(StateDb::list_tokens).await
    }

    #[cfg(feature = "p2p")]
    async fn revoke_token(
        &self,
        token_id: &str,
        revoked_at: DateTime<Utc>,
    ) -> Result<bool, StorageError> {
        let token_id = token_id.to_owned();
        self.run(move |db| db.revoke_token(&token_id, revoked_at))
            .await
    }

    #[cfg(feature = "p2p")]
    async fn is_token_revoked(&self, token_id: &str) -> Result<bool, StorageError> {
        let token_id = token_id.to_owned();
        self.run(move |db| db.is_token_revoked(&token_id)).await
    }

    #[cfg(feature = "p2p")]
    async fn revoked_token_ids(&self) -> Result<HashSet<String>, StorageError> {
        self.run(StateDb::revoked_token_ids).await
    }

    // ── Data-receive quarantine operations ──

    async fn upsert_quarantine(&self, record: &QuarantineRecord) -> Result<(), StorageError> {
        let record = record.clone();
        self.run(move |db| db.upsert_quarantine(&record)).await
    }

    async fn list_quarantine(
        &self,
        folder_id: &str,
        peer_device: &str,
    ) -> Result<Vec<QuarantineRecord>, StorageError> {
        let folder_id = folder_id.to_owned();
        let peer_device = peer_device.to_owned();
        self.run(move |db| db.list_quarantine(&folder_id, &peer_device))
            .await
    }

    async fn quarantine_count(
        &self,
        folder_id: &str,
        peer_device: &str,
    ) -> Result<u64, StorageError> {
        let folder_id = folder_id.to_owned();
        let peer_device = peer_device.to_owned();
        self.run(move |db| db.quarantine_count(&folder_id, &peer_device))
            .await
    }

    async fn prune_quarantine(
        &self,
        folder_id: &str,
        peer_device: &str,
    ) -> Result<u64, StorageError> {
        let folder_id = folder_id.to_owned();
        let peer_device = peer_device.to_owned();
        self.run(move |db| db.prune_quarantine(&folder_id, &peer_device))
            .await
    }

    // ── Data-plane explicit-control bit operations ──

    async fn record_data_explicit_control(
        &self,
        peer_device: &str,
        folder_id: &str,
        data_read: bool,
        data_write: bool,
        observed_at: DateTime<Utc>,
    ) -> Result<(), StorageError> {
        let peer_device = peer_device.to_owned();
        let folder_id = folder_id.to_owned();
        self.run(move |db| {
            db.record_data_explicit_control(
                &peer_device,
                &folder_id,
                data_read,
                data_write,
                observed_at,
            )
        })
        .await
    }

    async fn list_data_explicit_control(&self) -> Result<Vec<ExplicitControlRecord>, StorageError> {
        self.run(StateDb::list_data_explicit_control).await
    }

    async fn clear_data_explicit_control(
        &self,
        peer_device: &str,
        folder_id: &str,
    ) -> Result<bool, StorageError> {
        let peer_device = peer_device.to_owned();
        let folder_id = folder_id.to_owned();
        self.run(move |db| db.clear_data_explicit_control(&peer_device, &folder_id))
            .await
    }
}

// ─────────────────────────── HTTP ───────────────────────────

/// [`HttpClient`] backed by a `reqwest::Client`.
#[derive(Debug, Clone, Default)]
pub struct ReqwestClient {
    inner: reqwest::Client,
}

impl ReqwestClient {
    /// A client with reqwest's default configuration.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: reqwest::Client::new(),
        }
    }

    /// Wrap a pre-configured `reqwest::Client` (custom timeouts, proxies, TLS
    /// roots, and so on).
    #[must_use]
    pub const fn from_client(client: reqwest::Client) -> Self {
        Self { inner: client }
    }
}

/// Translate the portable [`HeaderMap`] into reqwest's, preserving order and
/// repeats. A header name or value reqwest rejects becomes
/// [`HttpError::Request`] rather than being silently dropped.
fn to_reqwest_headers(headers: &HeaderMap) -> Result<reqwest::header::HeaderMap, HttpError> {
    let mut out = reqwest::header::HeaderMap::with_capacity(headers.as_pairs().len());
    for (name, value) in headers.as_pairs() {
        let header_name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| HttpError::Request(format!("invalid header name '{name}': {e}")))?;
        let header_value = reqwest::header::HeaderValue::from_str(value)
            .map_err(|e| HttpError::Request(format!("invalid value for header '{name}': {e}")))?;
        out.append(header_name, header_value);
    }
    Ok(out)
}

/// Categorise a reqwest error into the portable [`HttpError`] vocabulary.
fn map_http_err(err: &reqwest::Error) -> HttpError {
    if err.is_timeout() {
        HttpError::Timeout
    } else if err.is_connect() {
        HttpError::Connection(err.to_string())
    } else if err.is_builder() {
        HttpError::InvalidUrl(err.to_string())
    } else {
        HttpError::Request(err.to_string())
    }
}

/// Buffer a reqwest response into the portable [`HttpResponse`]. A response
/// header whose value is not valid UTF-8 cannot be represented by the
/// string-based [`HeaderMap`] contract, so it fails loudly as
/// [`HttpError::Request`].
async fn into_response(response: reqwest::Response) -> Result<HttpResponse, HttpError> {
    let status = response.status().as_u16();
    let mut headers = HeaderMap::new();
    for (name, value) in response.headers() {
        let value = value.to_str().map_err(|e| {
            HttpError::Request(format!("response header '{name}' is not valid UTF-8: {e}"))
        })?;
        headers.insert(name.as_str(), value);
    }
    let body = response
        .bytes()
        .await
        .map_err(|e| map_http_err(&e))?
        .to_vec();
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

/// Send a built request and buffer its response.
async fn execute(request: reqwest::RequestBuilder) -> Result<HttpResponse, HttpError> {
    let response = request.send().await.map_err(|e| map_http_err(&e))?;
    into_response(response).await
}

#[async_trait]
impl HttpClient for ReqwestClient {
    async fn get(&self, url: &str, headers: HeaderMap) -> Result<HttpResponse, HttpError> {
        let request = self.inner.get(url).headers(to_reqwest_headers(&headers)?);
        execute(request).await
    }

    async fn post(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Vec<u8>,
    ) -> Result<HttpResponse, HttpError> {
        let request = self
            .inner
            .post(url)
            .headers(to_reqwest_headers(&headers)?)
            .body(body);
        execute(request).await
    }

    async fn put(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Vec<u8>,
    ) -> Result<HttpResponse, HttpError> {
        let request = self
            .inner
            .put(url)
            .headers(to_reqwest_headers(&headers)?)
            .body(body);
        execute(request).await
    }

    async fn delete(&self, url: &str, headers: HeaderMap) -> Result<HttpResponse, HttpError> {
        let request = self
            .inner
            .delete(url)
            .headers(to_reqwest_headers(&headers)?);
        execute(request).await
    }

    async fn head(&self, url: &str, headers: HeaderMap) -> Result<HttpResponse, HttpError> {
        let request = self.inner.head(url).headers(to_reqwest_headers(&headers)?);
        execute(request).await
    }

    async fn patch(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Vec<u8>,
    ) -> Result<HttpResponse, HttpError> {
        let request = self
            .inner
            .patch(url)
            .headers(to_reqwest_headers(&headers)?)
            .body(body);
        execute(request).await
    }
}

// ─────────────────────────── Filesystem ───────────────────────────

/// [`FileSystem`] backed by `std::fs`, with blocking calls offloaded to
/// `tokio::task::spawn_blocking`.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdFileSystem;

/// Categorise a `std::io::Error` against the path it concerned into the
/// portable [`FsError`] vocabulary.
fn map_fs_err(err: &std::io::Error, path: &Path) -> FsError {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::NotFound => FsError::NotFound(path.to_path_buf()),
        ErrorKind::PermissionDenied => FsError::PermissionDenied(path.to_path_buf()),
        ErrorKind::AlreadyExists => FsError::AlreadyExists(path.to_path_buf()),
        ErrorKind::NotADirectory => FsError::NotADirectory(path.to_path_buf()),
        _ => FsError::Other(format!("{}: {err}", path.display())),
    }
}

/// Offload a blocking filesystem closure, mapping a failed join into
/// [`FsError::Other`].
async fn spawn_fs<T, F>(f: F) -> Result<T, FsError>
where
    F: FnOnce() -> Result<T, FsError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| FsError::Other(format!("filesystem task failed: {e}")))?
}

/// List a directory's immediate entries (the blocking body of
/// [`FileSystem::read_dir`]).
fn read_dir_blocking(path: &Path) -> Result<Vec<FsDirEntry>, FsError> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(path).map_err(|e| map_fs_err(&e, path))? {
        let entry = entry.map_err(|e| map_fs_err(&e, path))?;
        let entry_path = entry.path();
        let metadata = entry.metadata().map_err(|e| map_fs_err(&e, &entry_path))?;
        let is_dir = metadata.is_dir();
        entries.push(FsDirEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            path: entry_path,
            is_dir,
            size: if is_dir { 0 } else { metadata.len() },
        });
    }
    Ok(entries)
}

#[async_trait]
impl FileSystem for StdFileSystem {
    async fn read_dir(&self, path: &Path) -> Result<Vec<FsDirEntry>, FsError> {
        let path = path.to_path_buf();
        spawn_fs(move || read_dir_blocking(&path)).await
    }

    async fn read_file(&self, path: &Path) -> Result<Vec<u8>, FsError> {
        let path = path.to_path_buf();
        spawn_fs(move || std::fs::read(&path).map_err(|e| map_fs_err(&e, &path))).await
    }

    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), FsError> {
        let path = path.to_path_buf();
        let data = data.to_vec();
        spawn_fs(move || std::fs::write(&path, &data).map_err(|e| map_fs_err(&e, &path))).await
    }

    async fn create_dir(&self, path: &Path) -> Result<(), FsError> {
        let path = path.to_path_buf();
        spawn_fs(move || std::fs::create_dir_all(&path).map_err(|e| map_fs_err(&e, &path))).await
    }

    async fn remove_file(&self, path: &Path) -> Result<(), FsError> {
        let path = path.to_path_buf();
        spawn_fs(move || std::fs::remove_file(&path).map_err(|e| map_fs_err(&e, &path))).await
    }

    async fn remove_dir(&self, path: &Path) -> Result<(), FsError> {
        let path = path.to_path_buf();
        spawn_fs(move || std::fs::remove_dir_all(&path).map_err(|e| map_fs_err(&e, &path))).await
    }

    async fn exists(&self, path: &Path) -> Result<bool, FsError> {
        let path = path.to_path_buf();
        spawn_fs(move || std::fs::exists(&path).map_err(|e| map_fs_err(&e, &path))).await
    }
}

#[cfg(test)]
mod tests {
    use super::{ReqwestClient, SqliteStorage, StdFileSystem, TokioRuntimeHandle};
    use crate::db::StateDb;
    use crate::portable::{FileSystem, FsError, HttpClient, StateStorage};
    use crate::types::{CacheState, FileEntry, ItemId};
    use std::sync::Arc;

    /// In-memory storage with one registered backend, `b1`, so file rows can
    /// satisfy the `files.backend_id` foreign key.
    async fn storage() -> SqliteStorage<TokioRuntimeHandle> {
        let db = StateDb::open_in_memory().expect("open in-memory db");
        let storage = SqliteStorage::new(Arc::new(db), TokioRuntimeHandle::current());
        storage
            .register_backend("b1", "local", "Local", None, None)
            .await
            .expect("register backend");
        storage
    }

    #[tokio::test]
    async fn sqlite_storage_round_trips_a_file() {
        let storage = storage().await;
        let id = ItemId::new("b1", "file-1");
        let entry = FileEntry::file(
            id.clone(),
            ItemId::new("b1", "root"),
            "report.txt".to_owned(),
        );

        storage.upsert_file(&entry).await.expect("upsert");
        let fetched = storage.get_file(&id).await.expect("get");
        assert_eq!(fetched, Some(entry));

        storage.delete_file(&id).await.expect("delete");
        assert_eq!(storage.get_file(&id).await.expect("get after delete"), None);
    }

    #[tokio::test]
    async fn sqlite_storage_registers_and_lists_backends() {
        let storage = storage().await;
        let backends = storage.list_backends().await.expect("list");
        assert_eq!(backends.len(), 1);
        assert_eq!(backends.first().map(|b| b.id.as_str()), Some("b1"));
    }

    #[tokio::test]
    async fn sqlite_storage_reports_missing_cache_state_as_none() {
        let storage = storage().await;
        let missing = ItemId::new("b1", "absent");
        assert_eq!(
            storage.get_cache_state(&missing).await.expect("get state"),
            None
        );
    }

    #[tokio::test]
    async fn sqlite_storage_updates_cache_state() {
        let storage = storage().await;
        let id = ItemId::new("b1", "file-2");
        let entry = FileEntry::file(id.clone(), ItemId::new("b1", "root"), "data.bin".to_owned());
        storage.upsert_file(&entry).await.expect("upsert");

        storage
            .update_cache_state(&id, CacheState::Cached)
            .await
            .expect("update state");
        assert_eq!(
            storage.get_cache_state(&id).await.expect("get state"),
            Some(CacheState::Cached)
        );
    }

    #[tokio::test]
    async fn std_filesystem_writes_reads_and_lists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fs = StdFileSystem;
        let file = dir.path().join("hello.txt");

        fs.write_file(&file, b"portable").await.expect("write");
        assert_eq!(fs.read_file(&file).await.expect("read"), b"portable");
        assert!(fs.exists(&file).await.expect("exists"));

        let entries = fs.read_dir(dir.path()).await.expect("read_dir");
        assert_eq!(entries.len(), 1);
        let entry = entries.first().expect("one entry");
        assert_eq!(entry.name, "hello.txt");
        assert!(!entry.is_dir);
        assert_eq!(entry.size, "portable".len() as u64);

        fs.remove_file(&file).await.expect("remove");
        assert!(!fs.exists(&file).await.expect("exists after remove"));
    }

    #[tokio::test]
    async fn std_filesystem_creates_nested_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fs = StdFileSystem;
        let nested = dir.path().join("a").join("b").join("c");

        fs.create_dir(&nested).await.expect("create_dir");
        assert!(fs.exists(&nested).await.expect("exists"));
    }

    #[tokio::test]
    async fn std_filesystem_maps_missing_path_to_not_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fs = StdFileSystem;
        let missing = dir.path().join("nope.txt");

        match fs.read_file(&missing).await {
            Err(FsError::NotFound(path)) => assert_eq!(path, missing),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reqwest_client_rejects_an_invalid_url() {
        let client = ReqwestClient::new();
        // A syntactically invalid URL is rejected at build time, surfacing as
        // an `HttpError` rather than a panic.
        let result = client
            .get("not a url", crate::portable::HeaderMap::new())
            .await;
        assert!(result.is_err());
    }
}
