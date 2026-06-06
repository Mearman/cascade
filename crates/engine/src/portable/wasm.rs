//! WASM adapters for the portable IO contracts.
//!
//! Each adapter here binds one [`super`] trait to the browser's equivalent:
//!
//! - [`WasmRuntimeHandle`] → browser event loop (`wasm_bindgen_futures`)
//!   ([`super::RuntimeHandle`]). Only available on `wasm32`.
//! - [`WasmStateStorage`] → in-memory `HashMap` store
//!   ([`super::StateStorage`]). Available on all targets so it can be tested
//!   natively.
//!
//! The in-memory storage is correct for the WASM engine's current usage: the
//! PWA does not need persistent Rust-side state yet (it uses `IndexedDB` on the
//! JS side for auth tokens). A proper IndexedDB-backed store can replace this
//! later.

use std::collections::HashMap;
use std::sync::Mutex;
#[cfg(target_arch = "wasm32")]
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

#[cfg(target_arch = "wasm32")]
use super::{BoxFuture, JoinHandle, RuntimeHandle};
use super::{StateStorage, StorageError};
use crate::db::{
    AuditEntry, AuditRecord, BackendRecord, DirtyFileRecord, ExplicitControlRecord, GrantRecord,
    LifecyclePolicyRecord, MaxFileLengthRecord, PeerRecord, PinRuleRecord, QuarantineRecord,
};
use crate::manage::{Grant, Scope};
use crate::types::{CacheState, Cursor, FileEntry, ItemId};

// ─────────────────────────── Runtime ───────────────────────────

/// [`RuntimeHandle`] backed by the browser's JS event loop.
///
/// WASM is single-threaded. `spawn` uses `wasm_bindgen_futures::spawn_local`,
/// `spawn_blocking` runs synchronously (there is no thread pool to offload to),
/// and `sleep` resolves via `window.setTimeout`.
#[cfg(target_arch = "wasm32")]
#[derive(Clone, Copy, Debug)]
pub struct WasmRuntimeHandle;

#[cfg(target_arch = "wasm32")]
impl RuntimeHandle for WasmRuntimeHandle {
    fn spawn(&self, fut: BoxFuture<()>) {
        wasm_bindgen_futures::spawn_local(fut);
    }

    fn spawn_blocking<F, R>(&self, f: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        // WASM is single-threaded; execute synchronously.
        let result = f();
        JoinHandle::new(Box::pin(async move { Ok(result) }))
    }

    fn sleep(&self, duration: Duration) -> BoxFuture<()> {
        let ms = duration.as_millis();
        // Promise.resolve immediately for zero-duration sleeps, avoiding
        // unnecessary interaction with the timer API.
        if ms == 0 {
            return Box::pin(async {});
        }
        let ms = i32::try_from(ms).unwrap_or(i32::MAX);
        Box::pin(async move {
            let promise = js_sys::Promise::new(&mut |resolve, _| {
                let window =
                    web_sys::window().expect("no window — sleep requires a browser environment");
                window
                    .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve.into(), ms)
                    .expect("setTimeout failed");
            });
            wasm_bindgen_futures::JsFuture::from(promise)
                .await
                .expect("setTimeout promise rejected");
        })
    }
}

// ─────────────────────────── State storage ───────────────────────────

/// In-memory state backing for the WASM adapter.
struct Inner {
    files: HashMap<String, FileEntry>,
    cache_states: HashMap<String, CacheState>,
    dirty_files: HashMap<String, bool>,
    file_paths: HashMap<String, (String, String)>,
    backends: Vec<BackendRecord>,
    cursors: HashMap<String, Cursor>,
    pin_rules: Vec<PinRuleRecord>,
    next_pin_rule_id: i64,
    lifecycle_policies: Vec<LifecyclePolicyRecord>,
    next_lifecycle_policy_id: i64,
    max_file_length_rules: Vec<MaxFileLengthRecord>,
    next_max_file_length_rule_id: i64,
    p2p_blocks: HashMap<String, Vec<[u8; 32]>>,
    peers: Vec<PeerRecord>,
    grants: Vec<GrantRecord>,
    next_grant_id: i64,
    audit: Vec<AuditRecord>,
    next_audit_id: i64,
    quarantine: Vec<QuarantineRecord>,
    explicit_control: Vec<ExplicitControlRecord>,
}

impl Inner {
    fn new() -> Self {
        Self {
            files: HashMap::new(),
            cache_states: HashMap::new(),
            dirty_files: HashMap::new(),
            file_paths: HashMap::new(),
            backends: Vec::new(),
            cursors: HashMap::new(),
            pin_rules: Vec::new(),
            next_pin_rule_id: 1,
            lifecycle_policies: Vec::new(),
            next_lifecycle_policy_id: 1,
            max_file_length_rules: Vec::new(),
            next_max_file_length_rule_id: 1,
            p2p_blocks: HashMap::new(),
            peers: Vec::new(),
            grants: Vec::new(),
            next_grant_id: 1,
            audit: Vec::new(),
            next_audit_id: 1,
            quarantine: Vec::new(),
            explicit_control: Vec::new(),
        }
    }
}

/// [`StateStorage`] backed by an in-memory `HashMap` store.
///
/// Suitable for WASM environments where persistent state is managed on the JS
/// side (`IndexedDB`). All operations are synchronous internally but wrapped in
/// async to satisfy the trait contract. The `Mutex` satisfies `Send + Sync`
/// even though WASM is single-threaded.
pub struct WasmStateStorage {
    inner: Mutex<Inner>,
}

impl WasmStateStorage {
    /// Create a new empty in-memory state store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::new()),
        }
    }
}

impl Default for WasmStateStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for WasmStateStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmStateStorage").finish_non_exhaustive()
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl StateStorage for WasmStateStorage {
    // ── File operations ──

    async fn upsert_file(&self, entry: &FileEntry) -> Result<(), StorageError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        inner.files.insert(entry.id.0.clone(), entry.clone());
        inner
            .cache_states
            .insert(entry.id.0.clone(), CacheState::Online);
        Ok(())
    }

    async fn get_file(&self, id: &ItemId) -> Result<Option<FileEntry>, StorageError> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        Ok(inner.files.get(&id.0).cloned())
    }

    async fn delete_file(&self, id: &ItemId) -> Result<(), StorageError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        inner.files.remove(&id.0);
        inner.cache_states.remove(&id.0);
        inner.dirty_files.remove(&id.0);
        inner.file_paths.remove(&id.0);
        Ok(())
    }

    async fn delete_subtree(&self, root_id: &ItemId) -> Result<(), StorageError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        // Collect all IDs in the subtree rooted at root_id. Take from the
        // front of the queue (swap to back then pop) to avoid indexing.
        let mut queue: Vec<String> = vec![root_id.0.clone()];
        let mut to_remove: Vec<String> = Vec::new();
        while let Some(parent) = queue.pop() {
            for file in inner.files.values() {
                if file.parent_id.0 == parent
                    && !to_remove.contains(&file.id.0)
                    && !queue.contains(&file.id.0)
                {
                    queue.push(file.id.0.clone());
                }
            }
            to_remove.push(parent);
        }
        for id in &to_remove {
            inner.files.remove(id);
            inner.cache_states.remove(id);
            inner.dirty_files.remove(id);
            inner.file_paths.remove(id);
        }
        Ok(())
    }

    async fn update_cache_state(&self, id: &ItemId, state: CacheState) -> Result<(), StorageError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        inner.cache_states.insert(id.0.clone(), state);
        Ok(())
    }

    async fn get_cache_state(&self, id: &ItemId) -> Result<Option<CacheState>, StorageError> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        // Return the stored cache state only if the file exists.
        if inner.files.contains_key(&id.0) {
            Ok(inner.cache_states.get(&id.0).copied())
        } else {
            Ok(None)
        }
    }

    // ── Sync cursor operations ──

    async fn set_cursor(&self, backend_id: &str, cursor: &Cursor) -> Result<(), StorageError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        inner.cursors.insert(backend_id.to_owned(), cursor.clone());
        Ok(())
    }

    async fn get_cursor(&self, backend_id: &str) -> Result<Option<Cursor>, StorageError> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        Ok(inner.cursors.get(backend_id).cloned())
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
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        let record = BackendRecord {
            id: id.to_owned(),
            backend_type: backend_type.to_owned(),
            display_name: display_name.to_owned(),
            mount_path: mount_path.map(ToOwned::to_owned),
            config: config.map(ToOwned::to_owned),
        };
        // Deduplicate by id: remove existing then push the new record.
        inner.backends.retain(|b| b.id != id);
        inner.backends.push(record);
        Ok(())
    }

    async fn remove_backend(&self, id: &str) -> Result<bool, StorageError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        let before = inner.backends.len();
        inner.backends.retain(|b| b.id != id);
        Ok(inner.backends.len() < before)
    }

    async fn list_backends(&self) -> Result<Vec<BackendRecord>, StorageError> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        Ok(inner.backends.clone())
    }

    // ── Pin rule operations ──

    async fn add_pin_rule(
        &self,
        path_glob: &str,
        recursive: bool,
        conditions: Option<&str>,
    ) -> Result<(), StorageError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        let id = inner.next_pin_rule_id;
        inner.next_pin_rule_id += 1;
        inner.pin_rules.push(PinRuleRecord {
            id,
            path_glob: path_glob.to_owned(),
            recursive,
            conditions: conditions.map(ToOwned::to_owned),
        });
        Ok(())
    }

    async fn remove_pin_rule(&self, path_glob: &str) -> Result<bool, StorageError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        let before = inner.pin_rules.len();
        inner.pin_rules.retain(|r| r.path_glob != path_glob);
        Ok(inner.pin_rules.len() < before)
    }

    async fn list_pin_rules(&self) -> Result<Vec<PinRuleRecord>, StorageError> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        Ok(inner.pin_rules.clone())
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
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        let id = inner.next_lifecycle_policy_id;
        inner.next_lifecycle_policy_id += 1;
        inner.lifecycle_policies.push(LifecyclePolicyRecord {
            id,
            path_glob: path_glob.to_owned(),
            max_age,
            max_file_size,
            priority,
            conditions: conditions.map(ToOwned::to_owned),
        });
        Ok(())
    }

    async fn list_lifecycle_policies(&self) -> Result<Vec<LifecyclePolicyRecord>, StorageError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        // Ordered by priority descending, matching the SQLite behaviour.
        inner
            .lifecycle_policies
            .sort_by_key(|r| std::cmp::Reverse(r.priority));
        Ok(inner.lifecycle_policies.clone())
    }

    async fn remove_lifecycle_policy(&self, id: i64) -> Result<bool, StorageError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        let before = inner.lifecycle_policies.len();
        inner.lifecycle_policies.retain(|p| p.id != id);
        Ok(inner.lifecycle_policies.len() < before)
    }

    // ── Max file length rule operations ──

    async fn add_max_file_length_rule(
        &self,
        path_glob: &str,
        max_bytes: u64,
        priority: i32,
        conditions: Option<&str>,
    ) -> Result<(), StorageError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        let id = inner.next_max_file_length_rule_id;
        inner.next_max_file_length_rule_id += 1;
        inner.max_file_length_rules.push(MaxFileLengthRecord {
            id,
            path_glob: path_glob.to_owned(),
            max_bytes,
            priority,
            conditions: conditions.map(ToOwned::to_owned),
        });
        Ok(())
    }

    async fn list_max_file_length_rules(&self) -> Result<Vec<MaxFileLengthRecord>, StorageError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        // Ordered by priority descending, matching the SQLite behaviour.
        inner
            .max_file_length_rules
            .sort_by_key(|r| std::cmp::Reverse(r.priority));
        Ok(inner.max_file_length_rules.clone())
    }

    async fn remove_max_file_length_rule(&self, id: i64) -> Result<bool, StorageError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        let before = inner.max_file_length_rules.len();
        inner.max_file_length_rules.retain(|r| r.id != id);
        Ok(inner.max_file_length_rules.len() < before)
    }

    // ── Cache queries ──

    async fn list_files_by_cache_state(
        &self,
        state: CacheState,
    ) -> Result<Vec<FileEntry>, StorageError> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        let files: Vec<FileEntry> = inner
            .files
            .values()
            .filter(|f| inner.cache_states.get(&f.id.0) == Some(&state))
            .cloned()
            .collect();
        Ok(files)
    }

    async fn list_all_files(&self) -> Result<Vec<FileEntry>, StorageError> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        Ok(inner.files.values().cloned().collect())
    }

    async fn list_children(&self, parent_id: &str) -> Result<Vec<FileEntry>, StorageError> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        let children: Vec<FileEntry> = inner
            .files
            .values()
            .filter(|f| f.parent_id.0 == parent_id)
            .cloned()
            .collect();
        Ok(children)
    }

    async fn cache_size(&self) -> Result<i64, StorageError> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        let total: i64 = inner
            .files
            .values()
            .filter(|f| {
                inner.cache_states.get(&f.id.0) == Some(&CacheState::Cached)
                    || inner.cache_states.get(&f.id.0) == Some(&CacheState::Pinned)
            })
            .map(|f| i64::try_from(f.size.unwrap_or(0)).unwrap_or(i64::MAX))
            .sum();
        Ok(total)
    }

    // ── Dirty file operations ──

    async fn mark_dirty(&self, id: &ItemId) -> Result<(), StorageError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        inner.dirty_files.insert(id.0.clone(), true);
        Ok(())
    }

    async fn clear_dirty(&self, id: &ItemId) -> Result<(), StorageError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        inner.dirty_files.insert(id.0.clone(), false);
        Ok(())
    }

    async fn set_file_paths(
        &self,
        id: &ItemId,
        path: &str,
        local_path: &str,
    ) -> Result<(), StorageError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        inner
            .file_paths
            .insert(id.0.clone(), (path.to_owned(), local_path.to_owned()));
        Ok(())
    }

    async fn is_dirty(&self, id: &ItemId) -> Result<Option<bool>, StorageError> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        // Return None if the file does not exist.
        if inner.files.contains_key(&id.0) {
            Ok(Some(inner.dirty_files.get(&id.0).copied().unwrap_or(false)))
        } else {
            Ok(None)
        }
    }

    async fn list_dirty_files(&self) -> Result<Vec<DirtyFileRecord>, StorageError> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        let mut records: Vec<DirtyFileRecord> = inner
            .dirty_files
            .iter()
            .filter(|(_, dirty)| **dirty)
            .filter_map(|(id_str, _)| {
                let file = inner.files.get(id_str)?;
                let (path, local_path) = inner.file_paths.get(id_str).map_or_else(
                    || (file.name.clone(), None),
                    |(p, lp)| (p.clone(), Some(lp.clone())),
                );
                Some(DirtyFileRecord {
                    id: file.id.clone(),
                    backend_id: file.id.backend_id().to_owned(),
                    path,
                    parent_id: file.parent_id.clone(),
                    name: file.name.clone(),
                    is_dir: file.is_dir,
                    size: file.size,
                    mime_type: file.mime_type.clone(),
                    mod_time: file.mod_time,
                    remote_hash: file.hash.clone(),
                    local_path,
                })
            })
            .collect();
        // Ordered by path, matching SQLite behaviour.
        records.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(records)
    }

    async fn eviction_candidates(&self, limit: usize) -> Result<Vec<FileEntry>, StorageError> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        let candidates: Vec<FileEntry> = inner
            .files
            .values()
            .filter(|f| {
                inner.cache_states.get(&f.id.0) == Some(&CacheState::Cached)
                    && inner.dirty_files.get(&f.id.0).copied() != Some(true)
            })
            .take(limit)
            .cloned()
            .collect();
        Ok(candidates)
    }

    // ── P2P operations ──

    async fn index_p2p_blocks(
        &self,
        file_id: &ItemId,
        block_hashes: &[[u8; 32]],
    ) -> Result<(), StorageError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        inner
            .p2p_blocks
            .insert(file_id.0.clone(), block_hashes.to_vec());
        Ok(())
    }

    async fn get_p2p_blocks(&self, file_id: &ItemId) -> Result<Vec<[u8; 32]>, StorageError> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        Ok(inner
            .p2p_blocks
            .get(&file_id.0)
            .cloned()
            .unwrap_or_default())
    }

    async fn upsert_peer(
        &self,
        device_id: &str,
        address: &str,
        last_seen: DateTime<Utc>,
    ) -> Result<(), StorageError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        if let Some(peer) = inner.peers.iter_mut().find(|p| p.device_id == device_id) {
            peer.addresses = Some(address.to_owned());
            peer.last_seen = Some(last_seen);
            peer.online = true;
        } else {
            inner.peers.push(PeerRecord {
                device_id: device_id.to_owned(),
                name: None,
                addresses: Some(address.to_owned()),
                last_seen: Some(last_seen),
                online: true,
            });
        }
        Ok(())
    }

    async fn list_peers(&self) -> Result<Vec<PeerRecord>, StorageError> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        Ok(inner.peers.clone())
    }

    // ── Management-plane grant operations ──

    async fn insert_grant(&self, grant: &Grant) -> Result<i64, StorageError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        let id = inner.next_grant_id;
        inner.next_grant_id += 1;
        inner.grants.push(GrantRecord {
            id,
            grant: grant.clone(),
        });
        Ok(id)
    }

    async fn list_grants(&self) -> Result<Vec<GrantRecord>, StorageError> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        Ok(inner.grants.clone())
    }

    async fn grant_scope(&self, id: i64) -> Result<Option<Scope>, StorageError> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        Ok(inner
            .grants
            .iter()
            .find(|g| g.id == id)
            .map(|g| g.grant.scope.clone()))
    }

    async fn revoke_grant(&self, id: i64) -> Result<bool, StorageError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        let before = inner.grants.len();
        inner.grants.retain(|g| g.id != id);
        Ok(inner.grants.len() < before)
    }

    async fn list_data_grants(&self) -> Result<Vec<GrantRecord>, StorageError> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        Ok(inner
            .grants
            .iter()
            .filter(|g| g.grant.capability.is_data_verb())
            .cloned()
            .collect())
    }

    // ── Management-plane audit operations ──

    async fn append_audit(&self, entry: &AuditEntry) -> Result<i64, StorageError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        let id = inner.next_audit_id;
        inner.next_audit_id += 1;
        inner.audit.push(AuditRecord {
            id,
            entry: entry.clone(),
        });
        Ok(id)
    }

    async fn list_audit(&self) -> Result<Vec<AuditRecord>, StorageError> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        Ok(inner.audit.clone())
    }

    // ── Capability-token operations (cfg p2p) ──
    // The WASM build does not enable the p2p feature, so these methods are
    // unreachable. They are still required by the trait definition; the
    // implementations return empty results.

    #[cfg(feature = "p2p")]
    async fn insert_token(
        &self,
        _token: &crate::manage::token::CapabilityToken,
        _issued_at: DateTime<Utc>,
    ) -> Result<(), StorageError> {
        Err(StorageError::Unavailable(
            "token storage is not supported on wasm".to_owned(),
        ))
    }

    #[cfg(feature = "p2p")]
    async fn list_tokens(&self) -> Result<Vec<crate::db::TokenRecord>, StorageError> {
        Ok(vec![])
    }

    #[cfg(feature = "p2p")]
    async fn revoke_token(
        &self,
        _token_id: &str,
        _revoked_at: DateTime<Utc>,
    ) -> Result<bool, StorageError> {
        Ok(false)
    }

    #[cfg(feature = "p2p")]
    async fn is_token_revoked(&self, _token_id: &str) -> Result<bool, StorageError> {
        Ok(false)
    }

    #[cfg(feature = "p2p")]
    async fn revoked_token_ids(&self) -> Result<std::collections::HashSet<String>, StorageError> {
        Ok(std::collections::HashSet::new())
    }

    // ── Data-receive quarantine operations ──

    async fn upsert_quarantine(&self, record: &QuarantineRecord) -> Result<(), StorageError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        // Upsert by (folder_id, peer_device, path).
        if let Some(existing) = inner.quarantine.iter_mut().find(|q| {
            q.folder_id == record.folder_id
                && q.peer_device == record.peer_device
                && q.path == record.path
        }) {
            *existing = record.clone();
        } else {
            inner.quarantine.push(record.clone());
        }
        Ok(())
    }

    async fn list_quarantine(
        &self,
        folder_id: &str,
        peer_device: &str,
    ) -> Result<Vec<QuarantineRecord>, StorageError> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        let mut results: Vec<QuarantineRecord> = inner
            .quarantine
            .iter()
            .filter(|q| q.folder_id == folder_id && q.peer_device == peer_device)
            .cloned()
            .collect();
        results.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(results)
    }

    async fn quarantine_count(
        &self,
        folder_id: &str,
        peer_device: &str,
    ) -> Result<u64, StorageError> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        let count = u64::try_from(
            inner
                .quarantine
                .iter()
                .filter(|q| q.folder_id == folder_id && q.peer_device == peer_device)
                .count(),
        )
        .unwrap_or(0);
        Ok(count)
    }

    async fn prune_quarantine(
        &self,
        folder_id: &str,
        peer_device: &str,
    ) -> Result<u64, StorageError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        let before = inner.quarantine.len();
        inner
            .quarantine
            .retain(|q| !(q.folder_id == folder_id && q.peer_device == peer_device));
        let removed = u64::try_from(before - inner.quarantine.len()).unwrap_or(0);
        Ok(removed)
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
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        if let Some(existing) = inner
            .explicit_control
            .iter_mut()
            .find(|r| r.peer_device == peer_device && r.folder_id == folder_id)
        {
            // OR-merge, matching SQLite ON CONFLICT behaviour.
            existing.data_read = existing.data_read || data_read;
            existing.data_write = existing.data_write || data_write;
            if observed_at > existing.observed_at {
                existing.observed_at = observed_at;
            }
        } else {
            inner.explicit_control.push(ExplicitControlRecord {
                peer_device: peer_device.to_owned(),
                folder_id: folder_id.to_owned(),
                data_read,
                data_write,
                observed_at,
            });
        }
        Ok(())
    }

    async fn list_data_explicit_control(&self) -> Result<Vec<ExplicitControlRecord>, StorageError> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        let mut records = inner.explicit_control.clone();
        records.sort_by(|a, b| {
            a.peer_device
                .cmp(&b.peer_device)
                .then_with(|| a.folder_id.cmp(&b.folder_id))
        });
        Ok(records)
    }

    async fn clear_data_explicit_control(
        &self,
        peer_device: &str,
        folder_id: &str,
    ) -> Result<bool, StorageError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        let before = inner.explicit_control.len();
        inner
            .explicit_control
            .retain(|r| !(r.peer_device == peer_device && r.folder_id == folder_id));
        Ok(inner.explicit_control.len() < before)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::portable::StateStorage;
    use crate::types::ItemId;

    /// Helper to build a file entry for testing.
    fn file_entry(id: &ItemId, parent_id: &ItemId, name: &str) -> FileEntry {
        FileEntry::file(id.clone(), parent_id.clone(), name.to_owned())
    }

    #[test]
    fn wasm_storage_round_trips_a_file() {
        let storage = WasmStateStorage::new();
        let id = ItemId::new("b1", "file-1");
        let parent = ItemId::new("b1", "root");
        let entry = file_entry(&id, &parent, "report.txt");

        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            storage
                .register_backend("b1", "local", "Local", None, None)
                .await
                .expect("register");
            storage.upsert_file(&entry).await.expect("upsert");
            let fetched = storage.get_file(&id).await.expect("get");
            assert_eq!(fetched, Some(entry));

            storage.delete_file(&id).await.expect("delete");
            assert_eq!(storage.get_file(&id).await.expect("get after delete"), None);
        });
    }

    #[test]
    fn wasm_storage_registers_and_lists_backends() {
        let storage = WasmStateStorage::new();
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            storage
                .register_backend("b1", "local", "Local", None, None)
                .await
                .expect("register");
            let backends = storage.list_backends().await.expect("list");
            assert_eq!(backends.len(), 1);
            assert_eq!(backends.first().map(|b| b.id.as_str()), Some("b1"));
        });
    }

    #[test]
    fn wasm_storage_reports_missing_cache_state_as_none() {
        let storage = WasmStateStorage::new();
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let missing = ItemId::new("b1", "absent");
            assert_eq!(
                storage.get_cache_state(&missing).await.expect("get state"),
                None
            );
        });
    }

    #[test]
    fn wasm_storage_updates_cache_state() {
        let storage = WasmStateStorage::new();
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            storage
                .register_backend("b1", "local", "Local", None, None)
                .await
                .expect("register");
            let id = ItemId::new("b1", "file-2");
            let parent = ItemId::new("b1", "root");
            let entry = file_entry(&id, &parent, "data.bin");
            storage.upsert_file(&entry).await.expect("upsert");

            storage
                .update_cache_state(&id, CacheState::Cached)
                .await
                .expect("update");
            assert_eq!(
                storage.get_cache_state(&id).await.expect("get state"),
                Some(CacheState::Cached)
            );
        });
    }

    #[test]
    fn wasm_storage_manages_dirty_files() {
        let storage = WasmStateStorage::new();
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            storage
                .register_backend("b1", "local", "Local", None, None)
                .await
                .expect("register");
            let id = ItemId::new("b1", "file-3");
            let parent = ItemId::new("b1", "root");
            let entry = file_entry(&id, &parent, "dirty.txt");
            storage.upsert_file(&entry).await.expect("upsert");

            // Initially not dirty.
            assert_eq!(storage.is_dirty(&id).await.expect("is_dirty"), Some(false));
            assert!(storage.list_dirty_files().await.expect("list").is_empty());

            // Mark dirty.
            storage.mark_dirty(&id).await.expect("mark");
            assert_eq!(storage.is_dirty(&id).await.expect("is_dirty"), Some(true));
            let dirty = storage.list_dirty_files().await.expect("list");
            assert_eq!(dirty.len(), 1);

            // Clear dirty.
            storage.clear_dirty(&id).await.expect("clear");
            assert_eq!(storage.is_dirty(&id).await.expect("is_dirty"), Some(false));
        });
    }

    #[test]
    fn wasm_storage_round_trips_cursors() {
        let storage = WasmStateStorage::new();
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            assert!(storage.get_cursor("b1").await.expect("get").is_none());
            storage
                .set_cursor("b1", &Cursor("token123".to_owned()))
                .await
                .expect("set");
            assert_eq!(
                storage
                    .get_cursor("b1")
                    .await
                    .expect("get")
                    .map(|c| c.0.clone()),
                Some("token123".to_owned())
            );
        });
    }

    #[test]
    fn wasm_storage_manages_pin_rules() {
        let storage = WasmStateStorage::new();
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            storage
                .add_pin_rule("/work/**", true, None)
                .await
                .expect("add");
            let rules = storage.list_pin_rules().await.expect("list");
            assert_eq!(rules.len(), 1);
            assert_eq!(rules[0].path_glob, "/work/**");

            assert!(storage.remove_pin_rule("/work/**").await.expect("remove"));
            assert!(storage.list_pin_rules().await.expect("list").is_empty());
        });
    }
}
