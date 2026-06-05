#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::string_slice
    )
)]
//! Google Drive backend.
//!
//! Uses the Drive API v3 with `OAuth2` device code flow.
//! Full read/write support: upload, create directory, trash, move/rename.
//! Change detection via the Changes API (cursor-based).

pub mod auth;
pub mod client;
pub mod model;
pub mod token_store;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use cascade_engine::backend::{Backend, BackendError};
use cascade_engine::types::{Change, Cursor, FileEntry, FileId, ItemId, Quota};
use tokio::sync::{Mutex, RwLock};

use auth::AuthTokens;
use client::{DriveClient, ListQuery};
#[cfg(not(feature = "portable"))]
use token_store::PlatformTokenStore;
use token_store::TokenStore;

/// Create a Google Drive backend from config (native build).
///
/// Config keys expected:
/// - `client_id` — Google `OAuth2` client ID
/// - `client_secret` — Google `OAuth2` client secret
/// - `account` — account identifier for Keychain storage (defaults to "default")
///
/// Optional keys used in integration tests:
/// - `base_url` — override the Drive API base URL (e.g. a local mock server)
/// - `upload_url` — override the Drive upload API URL
/// - `token_url` — override the `OAuth2` token endpoint (refresh/exchange)
/// - `access_token` — pre-populate an access token, bypassing Keychain lookup
/// - `refresh_token` — pre-populate a refresh token so the refresh path is reachable
/// - `expires_in_secs` — seconds until the pre-populated token expires (default 24h)
#[cfg(not(feature = "portable"))]
pub fn create_backend(config: &toml::Value) -> anyhow::Result<Box<dyn Backend>> {
    create_backend_with_store(config, Arc::new(PlatformTokenStore))
}

/// Portable stub for `create_backend` — always returns an error.
///
/// When the `portable` feature is active, the backend requires an explicit
/// `HttpClient`. Use [`create_backend_with_store_and_http`] instead.
#[cfg(feature = "portable")]
pub fn create_backend(_config: &toml::Value) -> anyhow::Result<Box<dyn Backend>> {
    Err(anyhow::anyhow!(
        "the Google Drive backend's `portable` feature requires an explicit HttpClient — \
         use `create_backend_with_store_and_http`"
    ))
}

/// Build a backend with an injected [`TokenStore`] (native build).
///
/// Identical to [`create_backend`] but lets the caller supply the persistence
/// backing for refreshed tokens. Integration tests use this to substitute an
/// in-memory store so the token-refresh path can be exercised without writing
/// to the host Keychain or config directory.
#[cfg(not(feature = "portable"))]
pub fn create_backend_with_store(
    config: &toml::Value,
    token_store: Arc<dyn TokenStore>,
) -> anyhow::Result<Box<dyn Backend>> {
    let (oauth, drive, initial_tokens, instance_id) = parse_gdrive_config(config);

    Ok(Box::new(GdriveBackend {
        drive,
        oauth,
        account: config
            .get("account")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string(),
        instance_id,
        token_store,
        tokens: Arc::new(Mutex::new(initial_tokens)),
        shared_drive_ids: Arc::new(RwLock::new(HashSet::new())),
        folder_drive_ids: Arc::new(RwLock::new(HashMap::new())),
        my_drive_root_id: Arc::new(RwLock::new(None)),
        trashed_ids: Arc::new(RwLock::new(HashSet::new())),
    }))
}

/// Build a backend with an injected [`TokenStore`] and HTTP client (portable build).
///
/// Use this function when the `portable` feature is active. The `http` argument
/// supplies the `HttpClient` implementation used for both Drive API calls and
/// the `OAuth2` token-refresh path.
#[cfg(feature = "portable")]
pub fn create_backend_with_store_and_http(
    config: &toml::Value,
    token_store: Arc<dyn TokenStore>,
    http: Arc<dyn cascade_engine::portable::HttpClient>,
) -> anyhow::Result<Box<dyn Backend>> {
    let (oauth, drive_native, initial_tokens, instance_id) =
        parse_gdrive_config_portable(config, Arc::clone(&http))?;

    Ok(Box::new(GdriveBackend {
        drive: drive_native,
        oauth,
        account: config
            .get("account")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string(),
        instance_id,
        token_store,
        tokens: Arc::new(Mutex::new(initial_tokens)),
        http,
        shared_drive_ids: Arc::new(RwLock::new(HashSet::new())),
        folder_drive_ids: Arc::new(RwLock::new(HashMap::new())),
        my_drive_root_id: Arc::new(RwLock::new(None)),
        trashed_ids: Arc::new(RwLock::new(HashSet::new())),
    }))
}

/// Parse common config fields shared between native and portable backends.
///
/// Returns `(oauth_config, drive_client, initial_tokens, instance_id)`.
#[cfg(not(feature = "portable"))]
fn parse_gdrive_config(
    config: &toml::Value,
) -> (
    auth::OAuthConfig,
    DriveClient,
    Option<auth::AuthTokens>,
    String,
) {
    const DEFAULT_TOKEN_LIFETIME_HOURS: i64 = 24;

    let client_id = config
        .get("client_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let client_secret = config
        .get("client_secret")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let account = config
        .get("account")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();
    let token_url = config
        .get("token_url")
        .and_then(|v| v.as_str())
        .unwrap_or(auth::GOOGLE_TOKEN_URL)
        .to_string();

    let drive = match (
        config.get("base_url").and_then(|v| v.as_str()),
        config.get("upload_url").and_then(|v| v.as_str()),
    ) {
        (Some(base), Some(upload)) => DriveClient::with_urls(base.to_string(), upload.to_string()),
        (Some(base), None) => DriveClient::with_urls(
            base.to_string(),
            "https://www.googleapis.com/upload/drive/v3".to_string(),
        ),
        _ => DriveClient::new(),
    };

    let initial_tokens = build_initial_tokens(config, DEFAULT_TOKEN_LIFETIME_HOURS);
    let instance_id = format!("gdrive-{account}");
    let oauth = auth::OAuthConfig {
        client_id,
        client_secret,
        token_url,
    };

    (oauth, drive, initial_tokens, instance_id)
}

/// Parse common config fields for the portable build, constructing a
/// `DriveClient` with the supplied HTTP client.
#[cfg(feature = "portable")]
fn parse_gdrive_config_portable(
    config: &toml::Value,
    http: Arc<dyn cascade_engine::portable::HttpClient>,
) -> anyhow::Result<(
    auth::OAuthConfig,
    DriveClient,
    Option<auth::AuthTokens>,
    String,
)> {
    const DEFAULT_TOKEN_LIFETIME_HOURS: i64 = 24;

    let client_id = config
        .get("client_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let client_secret = config
        .get("client_secret")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let account = config
        .get("account")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();
    let token_url = config
        .get("token_url")
        .and_then(|v| v.as_str())
        .unwrap_or(auth::GOOGLE_TOKEN_URL)
        .to_string();

    let base_url = config
        .get("base_url")
        .and_then(|v| v.as_str())
        .unwrap_or("https://www.googleapis.com/drive/v3")
        .to_string();
    let upload_url = config
        .get("upload_url")
        .and_then(|v| v.as_str())
        .unwrap_or("https://www.googleapis.com/upload/drive/v3")
        .to_string();

    let drive = DriveClient::with_http_client(base_url, upload_url, http);

    let initial_tokens = build_initial_tokens(config, DEFAULT_TOKEN_LIFETIME_HOURS);
    let instance_id = format!("gdrive-{account}");
    let oauth = auth::OAuthConfig {
        client_id,
        client_secret,
        token_url,
    };

    Ok((oauth, drive, initial_tokens, instance_id))
}

/// Build optional pre-populated tokens from test-harness config keys.
fn build_initial_tokens(
    config: &toml::Value,
    default_lifetime_hours: i64,
) -> Option<auth::AuthTokens> {
    config
        .get("access_token")
        .and_then(|v| v.as_str())
        .map(|token| {
            let lifetime = config
                .get("expires_in_secs")
                .and_then(toml::Value::as_integer)
                .map_or_else(
                    || chrono::Duration::hours(default_lifetime_hours),
                    chrono::Duration::seconds,
                );
            let refresh_token = config
                .get("refresh_token")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            auth::AuthTokens {
                access_token: token.to_string(),
                refresh_token,
                expires_at: chrono::Utc::now() + lifetime,
            }
        })
}

/// Google Drive backend implementation.
#[derive(Debug)]
pub struct GdriveBackend {
    drive: DriveClient,
    oauth: auth::OAuthConfig,
    account: String,
    /// Per-instance backend ID, e.g. "gdrive-personal".
    instance_id: String,
    /// Durable persistence for refreshed tokens (Keychain/file in production,
    /// in-memory under test).
    token_store: Arc<dyn TokenStore>,
    tokens: Arc<Mutex<Option<AuthTokens>>>,
    /// Injected HTTP client used for the token-refresh path under the
    /// `portable` feature. Under the `native` feature a fresh unpooled
    /// `reqwest::Client` is built per refresh call instead.
    #[cfg(feature = "portable")]
    http: Arc<dyn cascade_engine::portable::HttpClient>,
    /// IDs of shared drives this user is a member of.
    /// Populated on first `list_children("__shared_drives")` call.
    shared_drive_ids: Arc<RwLock<HashSet<String>>>,
    /// Maps a folder's native ID to the shared drive it lives in.
    /// Populated incrementally as shared-drive directories are listed.
    folder_drive_ids: Arc<RwLock<HashMap<String, String>>>,
    /// The real Drive folder ID of the user's My Drive root, cached so
    /// items whose `parents` field references it can be rewritten to the
    /// `__mydrive` virtual view ID.
    my_drive_root_id: Arc<RwLock<Option<String>>>,
    /// Drive folder IDs known to be trashed.
    ///
    /// Populated by the Bin listing and by trashed-child listings so that
    /// `list_children` of a trashed folder switches to a `trashed=true`
    /// query (descendants of a trashed folder remain trashed in Drive).
    trashed_ids: Arc<RwLock<HashSet<String>>>,
}

impl GdriveBackend {
    /// Get a valid access token, refreshing if necessary.
    async fn access_token(&self) -> anyhow::Result<String> {
        // Fast path: check if token is still valid without holding the lock
        // across an await.
        {
            let tokens = self.tokens.lock().await;
            if let Some(t) = tokens.as_ref()
                && !t.is_expired()
            {
                return Ok(t.access_token.clone());
            }
        }

        // If we have no in-memory tokens yet, try the durable store before
        // taking the lock. The store load is itself an `.await`, so it must
        // happen without the guard held — holding a tokio MutexGuard across an
        // `.await` and then re-locking the same mutex deadlocks the task.
        let loaded = {
            let have_tokens = self.tokens.lock().await.is_some();
            if have_tokens {
                None
            } else {
                self.token_store.load(&self.account).await?
            }
        };

        // Slow path: acquire the lock, fold in anything we loaded, extract the
        // refresh token, then drop the guard before making any network calls.
        let refresh_token = {
            let mut guard = self.tokens.lock().await;

            // Adopt store-loaded tokens only if another task hasn't populated
            // them in the meantime.
            if guard.is_none() {
                *guard = loaded;
            }

            let token_ref = guard.as_mut().ok_or_else(|| {
                anyhow::anyhow!("Not authenticated. Run `cascade backend auth gdrive`")
            })?;

            // Another task may have refreshed while we waited for the lock.
            if !token_ref.is_expired() {
                return Ok(token_ref.access_token.clone());
            }

            token_ref.refresh_token.clone()
            // guard is dropped here — mutex released before the HTTP call.
        };

        // Perform the token refresh. Under native the refresh uses a fresh
        // unpooled HTTP/1.1-only client (the confirmed TLS deadlock workaround;
        // see `DriveClient::http` in client.rs). Under portable the injected
        // HttpClient is used instead.
        #[cfg(not(feature = "portable"))]
        let refreshed = {
            /// Per-request timeout for the `OAuth2` token-refresh call.
            const REFRESH_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
            let http = client::build_unpooled_http1_client(REFRESH_REQUEST_TIMEOUT)?;
            auth::refresh_access_token(&http, &self.oauth, &refresh_token).await?
        };
        #[cfg(feature = "portable")]
        let refreshed =
            auth::refresh_access_token(self.http.as_ref(), &self.oauth, &refresh_token).await?;
        self.token_store.save(&self.account, &refreshed).await?;

        let mut guard = self.tokens.lock().await;
        *guard = Some(refreshed);
        let access_token = guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("tokens unexpectedly empty after refresh"))?
            .access_token
            .clone();
        Ok(access_token)
    }

    /// Build a synthetic directory `FileEntry` with no cloud-side counterpart.
    fn make_synthetic_dir(
        backend_id: &str,
        native_id: &str,
        parent_native_id: &str,
        name: &str,
    ) -> FileEntry {
        FileEntry {
            id: ItemId::new(backend_id, native_id),
            parent_id: ItemId::new(backend_id, parent_native_id),
            name: name.to_string(),
            is_dir: true,
            size: None,
            mod_time: None,
            mime_type: Some("application/vnd.google-apps.folder".to_string()),
            hash: None,
        }
    }

    /// Return the four virtual top-level directories as `Change::Created` events
    /// for the initial sync snapshot. No Drive API call is needed.
    fn virtual_root_entries(&self) -> Vec<Change> {
        [
            ("__mydrive", "My Drive"),
            ("__shared_drives", "Shared drives"),
            ("__shared_with_me", "Shared with me"),
            ("__trash", "Bin"),
        ]
        .iter()
        .map(|(id, name)| {
            Change::Created(Self::make_synthetic_dir(
                &self.instance_id,
                id,
                "root",
                name,
            ))
        })
        .collect()
    }

    /// Look up a shared drive by display name, listing all shared drives if
    /// needed to populate the cache. Returns the shared drive's native ID.
    async fn resolve_shared_drive_id(
        &self,
        drive_name: &str,
        token: &str,
    ) -> anyhow::Result<String> {
        // Check the in-memory cache first (ids are keyed by name in the
        // shared_drive_ids set but we need name→id, so scan folder_drive_ids
        // which maps folder_id → drive_id; shared drive roots map to themselves).
        // Simplest: list shared drives and match by name, with no extra cache.
        let mut page_token: Option<String> = None;
        loop {
            let resp = self
                .drive
                .list_shared_drives(token, page_token.as_deref())
                .await?;
            for sd in &resp.drives {
                if sd.name == drive_name {
                    return Ok(sd.id.clone());
                }
            }
            match resp.next_page_token {
                Some(next) => page_token = Some(next),
                None => break,
            }
        }
        anyhow::bail!("Shared drive not found: {drive_name}")
    }

    /// Page through a `list_files` query, returning all entries.
    ///
    /// Items from shared-drive listings (`drive_id` set in the file's metadata)
    /// are recorded in `folder_drive_ids` so subsequent child lookups can scope
    /// their queries correctly.
    async fn list_files_all_pages(
        &self,
        query: &ListQuery,
        token: &str,
    ) -> anyhow::Result<Vec<FileEntry>> {
        let mut all = Vec::new();
        let mut page_token: Option<String> = None;
        let mut drive_id_updates: Vec<(String, String)> = Vec::new();

        loop {
            let resp = self
                .drive
                .list_files(query, token, page_token.as_deref())
                .await?;

            for file in &resp.files {
                // Record drive_id so nested folders can be listed with the
                // correct shared-drive scope.
                if let Some(did) = &file.drive_id
                    && file.mime_type == "application/vnd.google-apps.folder"
                {
                    drive_id_updates.push((file.id.clone(), did.clone()));
                }
            }

            for file in resp.files {
                if let Some(entry) = file.to_file_entry(&self.instance_id) {
                    all.push(entry);
                }
            }

            match resp.next_page_token {
                Some(next) => page_token = Some(next),
                None => break,
            }
        }

        if !drive_id_updates.is_empty() {
            let mut cache = self.folder_drive_ids.write().await;
            for (id, did) in drive_id_updates {
                cache.insert(id, did);
            }
        }

        Ok(all)
    }

    /// Resolve and cache the real Drive folder ID of the user's My Drive root.
    ///
    /// The `/files/root` endpoint resolves the special `root` alias to the
    /// actual folder ID. We cache it so changes-stream items whose `parents`
    /// reference this ID can be rewritten to the `__mydrive` virtual view.
    async fn my_drive_root(&self, token: &str) -> anyhow::Result<String> {
        {
            let cache = self.my_drive_root_id.read().await;
            if let Some(id) = cache.as_ref() {
                return Ok(id.clone());
            }
        }
        let file = self.drive.get_file("root", token).await?;
        let mut cache = self.my_drive_root_id.write().await;
        *cache = Some(file.id.clone());
        Ok(file.id)
    }

    /// Rewrite a parent ID that points at the real My Drive root to the
    /// `__mydrive` virtual view ID so the entry appears in the right place.
    fn rewrite_my_drive_parent(&self, entry: &mut FileEntry, my_drive_root: &str) {
        if entry.parent_id.native_id() == my_drive_root {
            entry.parent_id = ItemId::new(&self.instance_id, "__mydrive");
        }
    }

    /// Same as `list_files_all_pages` but for the Bin view.
    ///
    /// Uses `to_trash_entry` so all returned items carry `parent_id = __trash`
    /// regardless of their original Drive parent. Folder IDs are recorded
    /// in `trashed_ids` so subsequent `list_children` calls for those
    /// folders can switch to a trashed-aware query.
    async fn list_trash_all_pages(&self, token: &str) -> anyhow::Result<Vec<FileEntry>> {
        let mut all = Vec::new();
        let mut page_token: Option<String> = None;
        let query = ListQuery::Trashed;
        let mut new_trashed: Vec<String> = Vec::new();

        loop {
            let resp = self
                .drive
                .list_files(&query, token, page_token.as_deref())
                .await?;

            for file in &resp.files {
                if file.mime_type == "application/vnd.google-apps.folder" {
                    new_trashed.push(file.id.clone());
                }
            }
            for file in resp.files {
                all.push(file.to_trash_entry(&self.instance_id));
            }

            match resp.next_page_token {
                Some(next) => page_token = Some(next),
                None => break,
            }
        }

        if !new_trashed.is_empty() {
            let mut cache = self.trashed_ids.write().await;
            for id in new_trashed {
                cache.insert(id);
            }
        }

        Ok(all)
    }

    /// List the immediate children of a trashed folder, keeping their real
    /// parent ID (the folder we're inside) rather than rewriting to
    /// `__trash`. Folder children are added to `trashed_ids` so further
    /// descent works the same way.
    async fn list_trashed_children_all_pages(
        &self,
        parent_id: &str,
        token: &str,
    ) -> anyhow::Result<Vec<FileEntry>> {
        let mut all = Vec::new();
        let mut page_token: Option<String> = None;
        let query = ListQuery::ChildrenOfTrashed {
            parent_id: parent_id.to_string(),
        };
        let mut new_trashed: Vec<String> = Vec::new();

        loop {
            let resp = self
                .drive
                .list_files(&query, token, page_token.as_deref())
                .await?;

            for file in &resp.files {
                if file.mime_type == "application/vnd.google-apps.folder" {
                    new_trashed.push(file.id.clone());
                }
            }
            for file in resp.files {
                all.push(file.to_file_entry_keeping_trashed(&self.instance_id));
            }

            match resp.next_page_token {
                Some(next) => page_token = Some(next),
                None => break,
            }
        }

        if !new_trashed.is_empty() {
            let mut cache = self.trashed_ids.write().await;
            for id in new_trashed {
                cache.insert(id);
            }
        }

        Ok(all)
    }
}

#[async_trait]
impl Backend for GdriveBackend {
    fn id(&self) -> &str {
        &self.instance_id
    }

    fn display_name(&self) -> &'static str {
        "Google Drive"
    }

    async fn quota(&self) -> anyhow::Result<Option<Quota>> {
        let token = self.access_token().await?;
        let about = self.drive.get_about(&token).await?;

        let quota = about.storage_quota.map(|sq| {
            let total = sq.limit.as_ref().and_then(|v| v.parse::<u64>().ok());
            let used = sq.usage.as_ref().and_then(|v| v.parse::<u64>().ok());
            let available = total.zip(used).map(|(t, u)| t.saturating_sub(u));
            Quota {
                total,
                used,
                available,
            }
        });

        Ok(quota)
    }

    async fn changes(&self, cursor: Option<&Cursor>) -> anyhow::Result<(Vec<Change>, Cursor)> {
        let token = self.access_token().await?;

        // No cursor → initial sync: emit the four virtual root directories and
        // record a Changes start-page token so incremental polling works from
        // this point on. Real file content is loaded on-demand via list_children.
        let Some(cursor) = cursor else {
            let start_token = self.drive.get_start_page_token(&token).await?;
            return Ok((self.virtual_root_entries(), Cursor(start_token)));
        };

        let page_token = cursor.0.clone();
        let mut all_changes = Vec::new();
        let mut current_token = page_token;
        let my_drive_root = self.my_drive_root(&token).await?;

        // Fetch all pages.
        loop {
            let resp = self.drive.get_changes(&current_token, &token).await?;

            for change in resp.changes {
                if change.removed.unwrap_or(false) {
                    // For deletions, we need a FileEntry with what we know.
                    // The change may or may not include the file metadata.
                    if let Some(file) = change.file {
                        if let Some(mut entry) = file.to_file_entry(&self.instance_id) {
                            self.rewrite_my_drive_parent(&mut entry, &my_drive_root);
                            all_changes.push(Change::Deleted(entry));
                        }
                    } else if let Some(file_id) = change.file_id {
                        // Minimal FileEntry for the deleted file.
                        let entry = FileEntry {
                            id: ItemId::new(&self.instance_id, &file_id),
                            parent_id: ItemId::new(&self.instance_id, "unknown"),
                            name: String::new(),
                            is_dir: false,
                            size: None,
                            mod_time: None,
                            mime_type: None,
                            hash: None,
                        };
                        all_changes.push(Change::Deleted(entry));
                    }
                } else if let Some(file) = change.file {
                    // Trashed items must route to the Bin view, not be
                    // dropped — otherwise files the user trashes via
                    // drive.google.com (or our own trash_file call) stay
                    // stuck at their old location in the local cache.
                    let mut entry = if file.trashed {
                        file.to_trash_entry(&self.instance_id)
                    } else {
                        let Some(e) = file.to_file_entry(&self.instance_id) else {
                            continue;
                        };
                        e
                    };
                    self.rewrite_my_drive_parent(&mut entry, &my_drive_root);
                    all_changes.push(Change::Created(entry));
                }
            }

            if let Some(next) = resp.next_page_token {
                current_token = next;
            } else {
                let new_cursor = resp.new_start_page_token.unwrap_or(current_token);
                return Ok((all_changes, Cursor(new_cursor)));
            }
        }
    }

    async fn metadata(&self, path: &Path) -> anyhow::Result<FileEntry> {
        let token = self.access_token().await?;
        let path_str = path.to_string_lossy();

        if path_str == "/" || path_str.is_empty() {
            return Ok(Self::make_synthetic_dir(
                &self.instance_id,
                "root",
                "root",
                "Google Drive",
            ));
        }

        let all_components: Vec<&str> = path
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .filter(|s| !s.is_empty() && *s != "/")
            .collect();

        // Split at the first component to avoid indexing_slicing lint violations.
        let Some((first, remaining)) = all_components.split_first() else {
            anyhow::bail!("Empty path");
        };

        // Strip the virtual view prefix so we know which Drive root to walk
        // from, and return synthetic entries when the path resolves to a view
        // root itself.
        let (mut current_id, drive_id, walk_components) = match *first {
            "My Drive" => {
                if remaining.is_empty() {
                    return Ok(Self::make_synthetic_dir(
                        &self.instance_id,
                        "__mydrive",
                        "root",
                        "My Drive",
                    ));
                }
                ("root".to_string(), None::<String>, remaining)
            }
            "Shared drives" => {
                let Some((drive_name, path_rest)) = remaining.split_first() else {
                    return Ok(Self::make_synthetic_dir(
                        &self.instance_id,
                        "__shared_drives",
                        "root",
                        "Shared drives",
                    ));
                };
                let did = self.resolve_shared_drive_id(drive_name, &token).await?;
                if path_rest.is_empty() {
                    return Ok(Self::make_synthetic_dir(
                        &self.instance_id,
                        &did,
                        "__shared_drives",
                        drive_name,
                    ));
                }
                let start = did.clone();
                (start, Some(did), path_rest)
            }
            "Shared with me" => {
                let Some((name, path_rest)) = remaining.split_first() else {
                    return Ok(Self::make_synthetic_dir(
                        &self.instance_id,
                        "__shared_with_me",
                        "root",
                        "Shared with me",
                    ));
                };
                if !path_rest.is_empty() {
                    anyhow::bail!(
                        "nested paths under 'Shared with me' not supported in metadata yet"
                    );
                }
                let entries = self
                    .list_files_all_pages(&ListQuery::SharedWithMe, &token)
                    .await?;
                let found = entries.into_iter().find(|e| e.name == *name);
                return found.ok_or_else(|| anyhow::anyhow!("Path not found: {path_str}"));
            }
            "Bin" => {
                let Some((name, path_rest)) = remaining.split_first() else {
                    return Ok(Self::make_synthetic_dir(
                        &self.instance_id,
                        "__trash",
                        "root",
                        "Bin",
                    ));
                };

                // First level: find the named item in the Bin listing.
                let trash_entries = self.list_trash_all_pages(&token).await?;
                let first = trash_entries
                    .into_iter()
                    .find(|e| e.name == *name)
                    .ok_or_else(|| BackendError::NotFound(format!("Path not found: {path_str}")))?;

                if path_rest.is_empty() {
                    return Ok(first);
                }

                // Walk deeper using trashed-aware listings — descendants
                // of a trashed folder are themselves trashed in Drive.
                let mut current = first;
                for component in path_rest {
                    let children = self
                        .list_trashed_children_all_pages(current.id.native_id(), &token)
                        .await?;
                    current = children
                        .into_iter()
                        .find(|e| e.name == *component)
                        .ok_or_else(|| {
                            BackendError::NotFound(format!("Path not found: {path_str}"))
                        })?;
                }
                return Ok(current);
            }
            _ => {
                // Legacy path without a virtual view prefix — walk from Drive root,
                // treating first as the first component to resolve.
                (
                    "root".to_string(),
                    None::<String>,
                    all_components.as_slice(),
                )
            }
        };

        for component in walk_components {
            let q = ListQuery::ChildrenOf {
                parent_id: current_id.clone(),
                drive_id: drive_id.clone(),
            };
            let resp = self.drive.list_files(&q, &token, None).await?;
            let found = resp.files.iter().find(|f| f.name == *component);
            match found {
                Some(f) => current_id.clone_from(&f.id),
                None => anyhow::bail!("Path not found: {path_str}"),
            }
        }

        let file = self.drive.get_file(&current_id, &token).await?;
        file.to_file_entry(&self.instance_id)
            .ok_or_else(|| anyhow::anyhow!("File not found: {path_str}"))
    }

    async fn download(&self, file: &FileEntry) -> anyhow::Result<Vec<u8>> {
        let token = self.access_token().await?;
        let remote_id = file.id.native_id();

        let resp = self.drive.download_content(remote_id, &token).await?;

        tracing::debug!(file = %file.id, size = file.size.unwrap_or(0), "downloaded");
        Ok(resp.body)
    }

    async fn read_range(
        &self,
        file: &FileEntry,
        offset: u64,
        length: u32,
    ) -> anyhow::Result<Vec<u8>> {
        let token = self.access_token().await?;
        let remote_id = file.id.native_id();

        let bytes = self
            .drive
            .download_range(remote_id, &token, offset, length)
            .await?;

        tracing::debug!(
            file = %file.id,
            offset,
            length,
            returned = bytes.len(),
            "read range",
        );
        Ok(bytes)
    }

    async fn upload(
        &self,
        path: &Path,
        data: &[u8],
        parent_id: &cascade_engine::types::FileId,
    ) -> anyhow::Result<FileEntry> {
        let native_parent = parent_id.native_id();
        if native_parent == "__trash" {
            return Err(BackendError::ReadOnly(
                "Bin is read-only — restore files via `cascade backend restore`".to_string(),
            )
            .into());
        }
        if native_parent == "__shared_with_me" {
            return Err(BackendError::ReadOnly(
                "Cannot create files directly in 'Shared with me'".to_string(),
            )
            .into());
        }
        if native_parent == "__shared_drives" {
            return Err(BackendError::Forbidden(
                "Cannot create files at the 'Shared drives' root — pick a specific drive"
                    .to_string(),
            )
            .into());
        }

        let token = self.access_token().await?;

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("untitled");

        // `__mydrive` is the user's real Drive root — Drive's upload API
        // needs the actual folder ID, not the virtual view alias.
        let drive_parent = if native_parent == "__mydrive" {
            self.my_drive_root(&token).await?
        } else {
            native_parent.to_string()
        };

        let file = self
            .drive
            .upload_file(file_name, &drive_parent, data, &token)
            .await?;

        let mut entry = file
            .to_file_entry(&self.instance_id)
            .ok_or_else(|| anyhow::anyhow!("upload returned trashed file"))?;
        let my_drive_root = self.my_drive_root(&token).await?;
        self.rewrite_my_drive_parent(&mut entry, &my_drive_root);
        Ok(entry)
    }

    async fn update(
        &self,
        file_id: &cascade_engine::types::FileId,
        data: &[u8],
    ) -> anyhow::Result<FileEntry> {
        let token = self.access_token().await?;

        let native_id = file_id.native_id();
        let file = self.drive.update_file(native_id, data, &token).await?;

        file.to_file_entry(&self.instance_id)
            .ok_or_else(|| anyhow::anyhow!("update returned trashed file"))
    }

    async fn create_dir(&self, path: &Path) -> anyhow::Result<FileEntry> {
        let token = self.access_token().await?;

        let dir_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("New Folder");

        // Resolve parent directory. `__mydrive`/`__trash`/etc. need to be
        // translated to real Drive IDs or rejected before hitting the API.
        let parent = path.parent().unwrap_or_else(|| Path::new("/"));
        let parent_native = if parent == Path::new("") || parent == Path::new("/") {
            "root".to_string()
        } else {
            let parent_entry = self.metadata(parent).await?;
            parent_entry.id.native_id().to_string()
        };

        let drive_parent = match parent_native.as_str() {
            "__trash" => {
                return Err(BackendError::ReadOnly(
                    "Bin is read-only — restore files via `cascade backend restore`".to_string(),
                )
                .into());
            }
            "__shared_with_me" => {
                return Err(BackendError::ReadOnly(
                    "Cannot create directories directly in 'Shared with me'".to_string(),
                )
                .into());
            }
            "__shared_drives" => {
                return Err(BackendError::Forbidden(
                    "Cannot create directories at the 'Shared drives' root — pick a specific drive"
                        .to_string(),
                )
                .into());
            }
            "__mydrive" => self.my_drive_root(&token).await?,
            other => other.to_string(),
        };

        let file = self
            .drive
            .create_directory(dir_name, &drive_parent, &token)
            .await?;

        let mut entry = file
            .to_file_entry(&self.instance_id)
            .ok_or_else(|| anyhow::anyhow!("create_dir returned trashed file"))?;
        let my_drive_root = self.my_drive_root(&token).await?;
        self.rewrite_my_drive_parent(&mut entry, &my_drive_root);
        Ok(entry)
    }

    async fn create_dir_with_parent(
        &self,
        name: &str,
        parent_id: &cascade_engine::types::FileId,
    ) -> anyhow::Result<FileEntry> {
        let native_parent = parent_id.native_id();
        if native_parent == "__trash" {
            return Err(BackendError::ReadOnly(
                "Bin is read-only — restore files via `cascade backend restore`".to_string(),
            )
            .into());
        }
        if native_parent == "__shared_with_me" {
            return Err(BackendError::ReadOnly(
                "Cannot create directories directly in 'Shared with me'".to_string(),
            )
            .into());
        }
        if native_parent == "__shared_drives" {
            return Err(BackendError::Forbidden(
                "Cannot create directories at the 'Shared drives' root — pick a specific drive"
                    .to_string(),
            )
            .into());
        }
        let token = self.access_token().await?;

        let drive_parent = if native_parent == "__mydrive" {
            self.my_drive_root(&token).await?
        } else {
            native_parent.to_string()
        };

        let file = self
            .drive
            .create_directory(name, &drive_parent, &token)
            .await?;
        let mut entry = file
            .to_file_entry(&self.instance_id)
            .ok_or_else(|| anyhow::anyhow!("create_dir_with_parent returned trashed file"))?;
        let my_drive_root = self.my_drive_root(&token).await?;
        self.rewrite_my_drive_parent(&mut entry, &my_drive_root);
        Ok(entry)
    }

    async fn delete(&self, file: &FileEntry) -> anyhow::Result<()> {
        let token = self.access_token().await?;
        let file_id = file.id.native_id();
        self.drive.trash_file(file_id, &token).await
    }

    async fn move_entry(&self, src: &Path, dst: &Path) -> anyhow::Result<FileEntry> {
        let token = self.access_token().await?;

        // Resolve source file to get its ID.
        let src_entry = self.metadata(src).await?;
        let file_id = src_entry.id.native_id();

        // Resolve destination parent.
        let dst_parent = dst.parent().unwrap_or_else(|| Path::new("/"));
        let dst_parent_native = if dst_parent == Path::new("") || dst_parent == Path::new("/") {
            "root".to_string()
        } else {
            let parent_entry = self.metadata(dst_parent).await?;
            parent_entry.id.native_id().to_string()
        };

        // Destination is the Bin → map to a trash operation.
        if dst_parent_native == "__trash" {
            self.drive.trash_file(file_id, &token).await?;
            let file = self.drive.get_file(file_id, &token).await?;
            return Ok(file.to_trash_entry(&self.instance_id));
        }

        // Source currently trashed → untrash before moving.
        let src_drive_file = self.drive.get_file(file_id, &token).await?;
        if src_drive_file.trashed {
            self.drive.untrash_file(file_id, &token).await?;
        }

        let dst_parent_id = if dst_parent_native == "__mydrive" {
            self.my_drive_root(&token).await?
        } else {
            dst_parent_native
        };

        let new_name = dst.file_name().and_then(|n| n.to_str());

        let file = self
            .drive
            .move_file(
                file_id,
                &dst_parent_id,
                &src_drive_file.parents,
                new_name,
                &token,
            )
            .await?;

        let mut entry = file
            .to_file_entry(&self.instance_id)
            .ok_or_else(|| anyhow::anyhow!("move returned trashed file"))?;
        let my_drive_root = self.my_drive_root(&token).await?;
        self.rewrite_my_drive_parent(&mut entry, &my_drive_root);
        Ok(entry)
    }

    async fn move_by_id(
        &self,
        src_id: &FileId,
        dst_parent_id: &FileId,
        new_name: &str,
    ) -> anyhow::Result<FileEntry> {
        let token = self.access_token().await?;
        let src_native = src_id.native_id();
        let dst_native = dst_parent_id.native_id();

        // Move into Bin: maps to `trash_file` rather than a parent change.
        // The file may live anywhere — My Drive, a shared drive, or already
        // be trashed — but a destination of `__trash` always means trash it.
        if dst_native == "__trash" {
            self.drive.trash_file(src_native, &token).await?;
            let file = self.drive.get_file(src_native, &token).await?;
            return Ok(file.to_trash_entry(&self.instance_id));
        }

        // Disallow direct moves into the virtual roots that have no
        // single Drive-level parent.
        if dst_native == "__shared_drives" || dst_native == "__shared_with_me" {
            return Err(BackendError::Forbidden(format!(
                "cannot move directly into virtual directory '{dst_native}' — pick a real folder under it"
            ))
            .into());
        }

        // If the source is currently trashed, untrash it first. A drag out
        // of Bin is the natural "restore" gesture.
        let current = self.drive.get_file(src_native, &token).await?;
        if current.trashed {
            self.drive.untrash_file(src_native, &token).await?;
        }

        // `__mydrive` is the user's real Drive root — resolve once and use
        // the real folder ID with Drive's addParents/removeParents semantics.
        let drive_parent = if dst_native == "__mydrive" {
            self.my_drive_root(&token).await?
        } else {
            dst_native.to_string()
        };

        let file = self
            .drive
            .move_file(
                src_native,
                &drive_parent,
                &current.parents,
                Some(new_name),
                &token,
            )
            .await?;

        let mut entry = file
            .to_file_entry(&self.instance_id)
            .ok_or_else(|| anyhow::anyhow!("move returned trashed file"))?;
        let my_drive_root = self.my_drive_root(&token).await?;
        self.rewrite_my_drive_parent(&mut entry, &my_drive_root);
        Ok(entry)
    }

    async fn poll_interval(&self) -> Option<Duration> {
        #[allow(unknown_lints, clippy::duration_suboptimal_units)]
        Some(Duration::from_secs(60))
    }

    async fn list_children(&self, parent_native_id: &str) -> anyhow::Result<Vec<FileEntry>> {
        match parent_native_id {
            // Virtual root: return the four top-level view directories.
            "root" => Ok([
                ("__mydrive", "My Drive"),
                ("__shared_drives", "Shared drives"),
                ("__shared_with_me", "Shared with me"),
                ("__trash", "Bin"),
            ]
            .iter()
            .map(|(id, name)| Self::make_synthetic_dir(&self.instance_id, id, "root", name))
            .collect()),

            // My Drive: list personal Drive root, reparent items to __mydrive
            // so PROPFIND filtering is consistent (Drive returns the real folder
            // ID as parent, which differs from the alias used by the presenter).
            "__mydrive" => {
                let token = self.access_token().await?;
                let q = ListQuery::ChildrenOf {
                    parent_id: "root".to_string(),
                    drive_id: None,
                };
                let entries = self.list_files_all_pages(&q, &token).await?;
                Ok(entries
                    .into_iter()
                    .map(|mut e| {
                        e.parent_id = ItemId::new(&self.instance_id, "__mydrive");
                        e
                    })
                    .collect())
            }

            // Shared drives: return one directory per shared drive.
            "__shared_drives" => {
                let token = self.access_token().await?;
                let mut all = Vec::new();
                let mut page_token: Option<String> = None;
                let mut new_ids: Vec<String> = Vec::new();
                loop {
                    let resp = self
                        .drive
                        .list_shared_drives(&token, page_token.as_deref())
                        .await?;
                    for sd in &resp.drives {
                        new_ids.push(sd.id.clone());
                        all.push(sd.to_file_entry(&self.instance_id));
                    }
                    match resp.next_page_token {
                        Some(next) => page_token = Some(next),
                        None => break,
                    }
                }
                // Cache the shared drive IDs so nested lookups can scope their
                // queries with corpora=drive.
                if !new_ids.is_empty() {
                    let mut cache = self.shared_drive_ids.write().await;
                    for id in new_ids {
                        cache.insert(id);
                    }
                }
                Ok(all)
            }

            // Shared with me: flat listing reparented to __shared_with_me.
            "__shared_with_me" => {
                let token = self.access_token().await?;
                let q = ListQuery::SharedWithMe;
                let entries = self.list_files_all_pages(&q, &token).await?;
                Ok(entries
                    .into_iter()
                    .map(|mut e| {
                        e.parent_id = ItemId::new(&self.instance_id, "__shared_with_me");
                        e
                    })
                    .collect())
            }

            // Bin: trashed items, reparented to __trash.
            "__trash" => {
                let token = self.access_token().await?;
                self.list_trash_all_pages(&token).await
            }

            // Regular folder: resolve the shared-drive scope from caches and
            // list with the appropriate corpora. If the folder is itself
            // trashed, switch to a `trashed=true` query so its (also
            // trashed) descendants are listed instead of appearing empty.
            native_id => {
                let token = self.access_token().await?;
                let is_trashed = self.trashed_ids.read().await.contains(native_id);
                if is_trashed {
                    return self
                        .list_trashed_children_all_pages(native_id, &token)
                        .await;
                }

                let drive_id = {
                    // A shared drive root maps to itself as the drive_id.
                    let sd_ids = self.shared_drive_ids.read().await;
                    if sd_ids.contains(native_id) {
                        Some(native_id.to_string())
                    } else {
                        drop(sd_ids);
                        self.folder_drive_ids.read().await.get(native_id).cloned()
                    }
                };

                let q = ListQuery::ChildrenOf {
                    parent_id: native_id.to_string(),
                    drive_id,
                };
                self.list_files_all_pages(&q, &token).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_backend_from_config() {
        let config = toml::toml! {
            client_id = "test-id"
            client_secret = "test-secret"
            account = "test-account"
        };
        let backend = create_backend(&config.into()).unwrap();
        assert_eq!(backend.id(), "gdrive-test-account");
    }
}
