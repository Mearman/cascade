//! Portable IO contracts — the boundary that keeps the engine's core logic
//! free of any host-only runtime or storage dependency.
//!
//! The engine wants to compile to `wasm32` and run in a browser, but several
//! of its dependencies do not: tokio (the async runtime), rusqlite (`SQLite`),
//! reqwest (HTTP), and `std::fs` (the filesystem). The fix is to express every
//! one of those concerns as a trait here and depend only on the trait. A
//! native build supplies adapters wrapping tokio/rusqlite/reqwest/`std::fs`; a
//! wasm build supplies adapters over the browser's equivalents (the JS event
//! loop, `IndexedDB`, `fetch`, the File System Access API). Which adapter is
//! bound is a composition concern at the edge, never a branch inside the core.
//!
//! Four concerns live here:
//!
//! - [`RuntimeHandle`] — spawning, blocking work, and timers (replaces a
//!   `tokio::runtime::Handle`).
//! - [`StateStorage`] — the persistent state surface (replaces direct
//!   [`crate::db::StateDb`] calls). Every method is asynchronous and trades in
//!   serialisable domain types only, so the same contract holds whether the
//!   backing store is a local `SQLite` file or a remote/async key-value store.
//! - [`HttpClient`] — request/response HTTP (replaces reqwest).
//! - [`FileSystem`] — directory and file IO (replaces `std::fs` and walkdir).
//!
//! Errors are modelled with [`thiserror`] per concern ([`StorageError`],
//! [`HttpError`], [`FsError`], [`JoinError`]) rather than `anyhow`, so a caller
//! can match on a category without downcasting and an adapter has a fixed,
//! target-independent vocabulary to map its native errors into.
//!
//! No impls live here yet — this module defines the contracts only. The record
//! types the storage contract trades in ([`crate::db::BackendRecord`] and
//! friends) are plain serialisable structs that carry no rusqlite types, so
//! referencing them here keeps the boundary clean.

// Native adapters bind the contracts below to tokio/rusqlite/reqwest/`std::fs`.
// They compile only for the native build: a `--features portable` build (wasm
// or otherwise) drops them and supplies its own adapters over the browser's
// equivalents instead.
#[cfg(feature = "native")]
pub mod native;

// WASM IO adapters (HttpClient via fetch, FileSystem stub) for the wasm32
// target. The companion wasm module (RuntimeHandle, StateStorage) is added
// separately.
#[cfg(all(target_arch = "wasm32", feature = "portable"))]
pub mod wasm_io;

#[cfg(feature = "p2p")]
use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

#[cfg(feature = "p2p")]
use crate::db::TokenRecord;
use crate::db::{
    AuditEntry, AuditRecord, BackendRecord, DirtyFileRecord, ExplicitControlRecord, GrantRecord,
    LifecyclePolicyRecord, MaxFileLengthRecord, PeerRecord, PinRuleRecord, QuarantineRecord,
};
#[cfg(feature = "p2p")]
use crate::manage::token::CapabilityToken;
use crate::manage::{Grant, Scope};
use crate::types::{CacheState, Cursor, FileEntry, ItemId};

/// A boxed, sendable future — the portable spelling of "some async work that
/// yields `T`". Used where a trait must name a future type without committing
/// to a concrete runtime's future.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

// ─────────────────────────── Runtime ───────────────────────────

/// Error returned when a value spawned on the blocking pool could not be run to
/// completion — for example because the task panicked or the runtime shut down
/// before it finished.
#[derive(Debug, thiserror::Error)]
#[error("spawned task failed to complete: {0}")]
pub struct JoinError(pub String);

/// A handle to a value being computed off the async path (a runtime's blocking
/// pool on native, or resolved inline where no such pool exists).
///
/// Awaiting the handle yields the task's result, or a [`JoinError`] if the task
/// could not run to completion. The handle is runtime-agnostic: a native
/// adapter wraps a `tokio::task::JoinHandle`, a single-threaded or wasm adapter
/// resolves the work immediately and yields it back.
pub struct JoinHandle<R> {
    inner: BoxFuture<Result<R, JoinError>>,
}

impl<R> JoinHandle<R> {
    /// Wrap a future yielding the task's result as a join handle.
    #[must_use]
    pub const fn new(inner: BoxFuture<Result<R, JoinError>>) -> Self {
        Self { inner }
    }
}

impl<R> std::fmt::Debug for JoinHandle<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinHandle").finish_non_exhaustive()
    }
}

impl<R> Future for JoinHandle<R> {
    type Output = Result<R, JoinError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // `Pin<Box<_>>` is `Unpin`, so the sole field is `Unpin` and `get_mut`
        // is sound without any unsafe pin projection.
        self.get_mut().inner.as_mut().poll(cx)
    }
}

/// Abstraction over an async runtime's handle — spawning detached work,
/// offloading blocking work, and sleeping.
///
/// `Clone` lets a handle be cheaply copied into the many tasks that need to
/// schedule more work; the trait is therefore consumed generically
/// (`R: RuntimeHandle`) rather than as a trait object.
pub trait RuntimeHandle: Send + Sync + Clone + 'static {
    /// Spawn a detached future onto the runtime. Its output is discarded; use
    /// [`Self::spawn_blocking`] when the result is needed.
    fn spawn(&self, fut: BoxFuture<()>);

    /// Offload a blocking, synchronous computation so it does not stall the
    /// async path. The returned [`JoinHandle`] resolves to the computation's
    /// result.
    fn spawn_blocking<F, R>(&self, f: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static;

    /// Complete after `duration` has elapsed.
    fn sleep(&self, duration: Duration) -> BoxFuture<()>;
}

// ─────────────────────────── State storage ───────────────────────────

/// A persistent-state failure, categorised so callers can react without
/// knowing which backing store produced it.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// The backing store could not be reached or opened.
    #[error("storage unavailable: {0}")]
    Unavailable(String),
    /// A stored value could not be decoded back into its domain type — a
    /// corrupt row, an unknown enum tag, an invalid timestamp.
    #[error("stored data is corrupt: {0}")]
    Corruption(String),
    /// The write violated a store constraint (a uniqueness or foreign-key
    /// rule), for example re-issuing an existing token id.
    #[error("constraint violation: {0}")]
    Constraint(String),
    /// A value could not be serialised for storage or deserialised from it.
    #[error("serialisation failed: {0}")]
    Serialisation(String),
}

/// The engine's persistent state surface, as a backing-store-independent
/// contract.
///
/// Mirrors the public method set of [`crate::db::StateDb`] but names no
/// rusqlite type and is asynchronous throughout, so a local `SQLite` file and a
/// remote/async key-value store satisfy the same trait.
#[async_trait]
pub trait StateStorage: Send + Sync {
    // ── File operations ──

    /// Insert or replace a file entry.
    async fn upsert_file(&self, entry: &FileEntry) -> Result<(), StorageError>;

    /// Fetch a file entry by id, or `None` if absent.
    async fn get_file(&self, id: &ItemId) -> Result<Option<FileEntry>, StorageError>;

    /// Delete a file entry by id.
    async fn delete_file(&self, id: &ItemId) -> Result<(), StorageError>;

    /// Delete a file or directory and every descendant.
    async fn delete_subtree(&self, root_id: &ItemId) -> Result<(), StorageError>;

    /// Update the cache state of a file.
    async fn update_cache_state(&self, id: &ItemId, state: CacheState) -> Result<(), StorageError>;

    /// Read the cache state of a file, or `None` if the file is unknown.
    async fn get_cache_state(&self, id: &ItemId) -> Result<Option<CacheState>, StorageError>;

    // ── Sync cursor operations ──

    /// Store the sync cursor for a backend.
    async fn set_cursor(&self, backend_id: &str, cursor: &Cursor) -> Result<(), StorageError>;

    /// Read the sync cursor for a backend, or `None` if none is stored.
    async fn get_cursor(&self, backend_id: &str) -> Result<Option<Cursor>, StorageError>;

    // ── Backend registration ──

    /// Register (or replace) a backend.
    async fn register_backend(
        &self,
        id: &str,
        backend_type: &str,
        display_name: &str,
        mount_path: Option<&str>,
        config: Option<&str>,
    ) -> Result<(), StorageError>;

    /// Remove a registered backend by id. Returns `true` if a row was removed.
    async fn remove_backend(&self, id: &str) -> Result<bool, StorageError>;

    /// List every registered backend.
    async fn list_backends(&self) -> Result<Vec<BackendRecord>, StorageError>;

    // ── Pin rule operations ──

    /// Add (or replace) a pin rule.
    async fn add_pin_rule(
        &self,
        path_glob: &str,
        recursive: bool,
        conditions: Option<&str>,
    ) -> Result<(), StorageError>;

    /// Remove a pin rule by its path glob. Returns `true` if a row was removed.
    async fn remove_pin_rule(&self, path_glob: &str) -> Result<bool, StorageError>;

    /// List every pin rule.
    async fn list_pin_rules(&self) -> Result<Vec<PinRuleRecord>, StorageError>;

    // ── Lifecycle policy operations ──

    /// Add a lifecycle policy.
    async fn add_lifecycle_policy(
        &self,
        path_glob: &str,
        max_age: Option<i64>,
        max_file_size: Option<i64>,
        priority: i32,
        conditions: Option<&str>,
    ) -> Result<(), StorageError>;

    /// List lifecycle policies, ordered by priority descending.
    async fn list_lifecycle_policies(&self) -> Result<Vec<LifecyclePolicyRecord>, StorageError>;

    /// Remove a lifecycle policy by id. Returns `true` if a row was removed.
    async fn remove_lifecycle_policy(&self, id: i64) -> Result<bool, StorageError>;

    // ── Max file length rule operations ──

    /// Add a max file length rule.
    async fn add_max_file_length_rule(
        &self,
        path_glob: &str,
        max_bytes: u64,
        priority: i32,
        conditions: Option<&str>,
    ) -> Result<(), StorageError>;

    /// List max file length rules, ordered by priority descending.
    async fn list_max_file_length_rules(&self) -> Result<Vec<MaxFileLengthRecord>, StorageError>;

    /// Remove a max file length rule by id. Returns `true` if a row was removed.
    async fn remove_max_file_length_rule(&self, id: i64) -> Result<bool, StorageError>;

    // ── Cache queries ──

    /// List every file in a given cache state.
    async fn list_files_by_cache_state(
        &self,
        state: CacheState,
    ) -> Result<Vec<FileEntry>, StorageError>;

    /// List every file.
    async fn list_all_files(&self) -> Result<Vec<FileEntry>, StorageError>;

    /// List the immediate children of a directory by its parent id string.
    async fn list_children(&self, parent_id: &str) -> Result<Vec<FileEntry>, StorageError>;

    /// Total cache size — the summed size of cached and pinned files.
    async fn cache_size(&self) -> Result<i64, StorageError>;

    // ── Dirty file operations ──

    /// Mark a file as dirty (locally modified, pending upload).
    async fn mark_dirty(&self, id: &ItemId) -> Result<(), StorageError>;

    /// Clear a file's dirty flag (upload succeeded).
    async fn clear_dirty(&self, id: &ItemId) -> Result<(), StorageError>;

    /// Set the VFS path and on-disk path for a materialised file.
    async fn set_file_paths(
        &self,
        id: &ItemId,
        path: &str,
        local_path: &str,
    ) -> Result<(), StorageError>;

    /// Whether a file is dirty, or `None` if the file is unknown.
    async fn is_dirty(&self, id: &ItemId) -> Result<Option<bool>, StorageError>;

    /// List every dirty file, ordered by path.
    async fn list_dirty_files(&self) -> Result<Vec<DirtyFileRecord>, StorageError>;

    /// Eviction candidates: cached, non-pinned files ordered least-recently
    /// accessed first.
    async fn eviction_candidates(&self, limit: usize) -> Result<Vec<FileEntry>, StorageError>;

    // ── P2P operations ──

    /// Store the ordered block index for a file.
    async fn index_p2p_blocks(
        &self,
        file_id: &ItemId,
        block_hashes: &[[u8; 32]],
    ) -> Result<(), StorageError>;

    /// Read the ordered block hashes for a file.
    async fn get_p2p_blocks(&self, file_id: &ItemId) -> Result<Vec<[u8; 32]>, StorageError>;

    /// Store or update a known peer.
    async fn upsert_peer(
        &self,
        device_id: &str,
        address: &str,
        last_seen: DateTime<Utc>,
    ) -> Result<(), StorageError>;

    /// List every known peer.
    async fn list_peers(&self) -> Result<Vec<PeerRecord>, StorageError>;

    // ── Management-plane grant operations ──

    /// Insert a capability grant. Returns the assigned row id.
    async fn insert_grant(&self, grant: &Grant) -> Result<i64, StorageError>;

    /// List every grant in insertion order.
    async fn list_grants(&self) -> Result<Vec<GrantRecord>, StorageError>;

    /// The stored scope of the grant with the given row id, or `None` if no
    /// such grant exists.
    async fn grant_scope(&self, id: i64) -> Result<Option<Scope>, StorageError>;

    /// Revoke a grant by row id. Returns `true` if a row was removed.
    async fn revoke_grant(&self, id: i64) -> Result<bool, StorageError>;

    /// List only data-verb grants (`data:read` / `data:write`).
    async fn list_data_grants(&self) -> Result<Vec<GrantRecord>, StorageError>;

    // ── Management-plane audit operations ──

    /// Append an audit row. Returns the assigned row id. The audit log is
    /// append-only — there is deliberately no update or delete path.
    async fn append_audit(&self, entry: &AuditEntry) -> Result<i64, StorageError>;

    /// List audit rows in append order.
    async fn list_audit(&self) -> Result<Vec<AuditRecord>, StorageError>;

    // ── Capability-token operations ──

    /// Record an issued capability token. Re-issuing an existing token id is a
    /// constraint violation, never a silent overwrite.
    #[cfg(feature = "p2p")]
    async fn insert_token(
        &self,
        token: &CapabilityToken,
        issued_at: DateTime<Utc>,
    ) -> Result<(), StorageError>;

    /// List every issued token in issuance order.
    #[cfg(feature = "p2p")]
    async fn list_tokens(&self) -> Result<Vec<TokenRecord>, StorageError>;

    /// Add a token id to the append-only revocation list. Returns `true` if the
    /// id was newly revoked, `false` if it was already present.
    #[cfg(feature = "p2p")]
    async fn revoke_token(
        &self,
        token_id: &str,
        revoked_at: DateTime<Utc>,
    ) -> Result<bool, StorageError>;

    /// Whether a token id is on the revocation list.
    #[cfg(feature = "p2p")]
    async fn is_token_revoked(&self, token_id: &str) -> Result<bool, StorageError>;

    /// The full set of revoked token ids, for building an in-memory predicate.
    #[cfg(feature = "p2p")]
    async fn revoked_token_ids(&self) -> Result<HashSet<String>, StorageError>;

    // ── Data-receive quarantine operations ──

    /// Insert or replace a quarantine record.
    async fn upsert_quarantine(&self, record: &QuarantineRecord) -> Result<(), StorageError>;

    /// List quarantined rows for a `(folder, peer)` pair, ordered by path.
    async fn list_quarantine(
        &self,
        folder_id: &str,
        peer_device: &str,
    ) -> Result<Vec<QuarantineRecord>, StorageError>;

    /// Count quarantined rows for a `(folder, peer)` pair.
    async fn quarantine_count(
        &self,
        folder_id: &str,
        peer_device: &str,
    ) -> Result<u64, StorageError>;

    /// Prune every quarantined row for a `(folder, peer)` pair. Returns the
    /// number removed.
    async fn prune_quarantine(
        &self,
        folder_id: &str,
        peer_device: &str,
    ) -> Result<u64, StorageError>;

    // ── Data-plane explicit-control bit operations ──

    /// Record (or OR-merge) the explicit-control bit for a `(peer, folder)`
    /// pair.
    async fn record_data_explicit_control(
        &self,
        peer_device: &str,
        folder_id: &str,
        data_read: bool,
        data_write: bool,
        observed_at: DateTime<Utc>,
    ) -> Result<(), StorageError>;

    /// List every explicit-control row.
    async fn list_data_explicit_control(&self) -> Result<Vec<ExplicitControlRecord>, StorageError>;

    /// Clear the explicit-control bit for a `(peer, folder)` pair. Returns
    /// `true` if a row was removed.
    async fn clear_data_explicit_control(
        &self,
        peer_device: &str,
        folder_id: &str,
    ) -> Result<bool, StorageError>;
}

// ─────────────────────────── HTTP ───────────────────────────

/// An HTTP failure, categorised so callers can distinguish a bad URL from a
/// dropped connection without inspecting a host client's error type.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    /// The URL could not be parsed or was rejected before sending.
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    /// The connection could not be established or was lost in flight.
    #[error("connection error: {0}")]
    Connection(String),
    /// The request did not complete within its deadline.
    #[error("request timed out")]
    Timeout,
    /// The request failed for some other reason carried in the message.
    #[error("request failed: {0}")]
    Request(String),
}

/// A set of HTTP headers, preserving insertion order and allowing repeats
/// (`Set-Cookie` and friends). Names match case-insensitively on lookup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeaderMap {
    entries: Vec<(String, String)>,
}

impl HeaderMap {
    /// An empty header set.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Build a header set from a list of name/value pairs.
    #[must_use]
    pub const fn from_pairs(entries: Vec<(String, String)>) -> Self {
        Self { entries }
    }

    /// Append a header. Existing headers of the same name are kept, matching
    /// HTTP's multi-value semantics.
    pub fn insert(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.entries.push((name.into(), value.into()));
    }

    /// The first value for `name`, matched case-insensitively, or `None`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The header pairs in insertion order.
    #[must_use]
    pub const fn as_pairs(&self) -> &[(String, String)] {
        self.entries.as_slice()
    }
}

/// An HTTP response: status, headers, and a fully-buffered body.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// The numeric status code.
    pub status: u16,
    /// The response headers.
    pub headers: HeaderMap,
    /// The response body.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// Whether the status code is in the 2xx success range.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        self.status >= 200 && self.status < 300
    }
}

/// Abstraction over an HTTP client (replaces reqwest). Each method buffers the
/// full response; streaming bodies are out of scope for this contract.
#[async_trait]
pub trait HttpClient: Send + Sync + std::fmt::Debug {
    /// Issue a GET request.
    async fn get(&self, url: &str, headers: HeaderMap) -> Result<HttpResponse, HttpError>;

    /// Issue a POST request with a body.
    async fn post(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Vec<u8>,
    ) -> Result<HttpResponse, HttpError>;

    /// Issue a PUT request with a body.
    async fn put(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Vec<u8>,
    ) -> Result<HttpResponse, HttpError>;

    /// Issue a DELETE request.
    async fn delete(&self, url: &str, headers: HeaderMap) -> Result<HttpResponse, HttpError>;

    /// Issue a PATCH request with a body.
    async fn patch(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Vec<u8>,
    ) -> Result<HttpResponse, HttpError>;

    /// Issue a HEAD request. Returns status and headers; the body is always empty.
    async fn head(&self, url: &str, headers: HeaderMap) -> Result<HttpResponse, HttpError>;
}

// ─────────────────────────── Filesystem ───────────────────────────

/// A filesystem failure, categorised so callers can react to a missing path or
/// a permission denial without inspecting a host `io::Error`.
#[derive(Debug, thiserror::Error)]
pub enum FsError {
    /// The path does not exist.
    #[error("path not found: {0}")]
    NotFound(PathBuf),
    /// The caller lacks permission for the operation.
    #[error("permission denied: {0}")]
    PermissionDenied(PathBuf),
    /// The path already exists where the operation required it not to.
    #[error("already exists: {0}")]
    AlreadyExists(PathBuf),
    /// A directory operation targeted a non-directory (or vice versa).
    #[error("not a directory: {0}")]
    NotADirectory(PathBuf),
    /// The operation failed for some other reason carried in the message.
    #[error("filesystem error: {0}")]
    Other(String),
}

/// A single entry returned by [`FileSystem::read_dir`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsDirEntry {
    /// The final path component (the file or directory name).
    pub name: String,
    /// The full path to the entry.
    pub path: PathBuf,
    /// Whether the entry is a directory.
    pub is_dir: bool,
    /// The entry size in bytes (0 for directories).
    pub size: u64,
}

/// Abstraction over filesystem IO (replaces `std::fs` and walkdir). The native
/// adapter wraps `std::fs`/`tokio::fs`; a wasm adapter wraps the browser's File
/// System Access API.
#[async_trait]
pub trait FileSystem: Send + Sync {
    /// List the immediate entries of a directory.
    async fn read_dir(&self, path: &Path) -> Result<Vec<FsDirEntry>, FsError>;

    /// Read a file's full contents.
    async fn read_file(&self, path: &Path) -> Result<Vec<u8>, FsError>;

    /// Write `data` to a file, creating or truncating it.
    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), FsError>;

    /// Create a directory, including missing parents.
    async fn create_dir(&self, path: &Path) -> Result<(), FsError>;

    /// Remove a file.
    async fn remove_file(&self, path: &Path) -> Result<(), FsError>;

    /// Remove a directory and its contents.
    async fn remove_dir(&self, path: &Path) -> Result<(), FsError>;

    /// Whether a path exists.
    async fn exists(&self, path: &Path) -> Result<bool, FsError>;
}
