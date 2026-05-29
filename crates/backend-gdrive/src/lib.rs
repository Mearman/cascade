//! Google Drive backend.
//!
//! Uses the Drive API v3 with `OAuth2` device code flow.
//! Full read/write support: upload, create directory, trash, move/rename.
//! Change detection via the Changes API (cursor-based).

pub mod auth;
pub mod client;
pub mod model;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use cascade_engine::backend::Backend;
use cascade_engine::types::{Change, Cursor, FileEntry, ItemId, Quota};
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;

use auth::AuthTokens;
use client::DriveClient;

/// Create a Google Drive backend from config.
///
/// Config keys expected:
/// - `client_id` — Google `OAuth2` client ID
/// - `client_secret` — Google `OAuth2` client secret
/// - `account` — account identifier for Keychain storage (defaults to "default")
///
/// Optional keys used in integration tests:
/// - `base_url` — override the Drive API base URL (e.g. a local mock server)
/// - `upload_url` — override the Drive upload API URL
/// - `access_token` — pre-populate an access token, bypassing Keychain lookup
pub fn create_backend(config: &toml::Value) -> anyhow::Result<Box<dyn Backend>> {
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

    let initial_tokens = config
        .get("access_token")
        .and_then(|v| v.as_str())
        .map(|token| auth::AuthTokens {
            access_token: token.to_string(),
            refresh_token: String::new(),
            expires_at: chrono::Utc::now() + chrono::Duration::hours(24),
        });

    let instance_id = format!("gdrive-{account}");

    Ok(Box::new(GdriveBackend {
        drive,
        oauth: auth::OAuthConfig {
            client_id,
            client_secret,
        },
        account,
        instance_id,
        tokens: Arc::new(RwLock::new(initial_tokens)),
    }))
}

/// Google Drive backend implementation.
#[derive(Debug)]
pub struct GdriveBackend {
    drive: DriveClient,
    oauth: auth::OAuthConfig,
    account: String,
    /// Per-instance backend ID, e.g. "gdrive-personal".
    instance_id: String,
    tokens: Arc<RwLock<Option<AuthTokens>>>,
}

impl GdriveBackend {
    /// Get a valid access token, refreshing if necessary.
    async fn access_token(&self) -> anyhow::Result<String> {
        let mut tokens = self.tokens.write().await;

        // Try loading from Keychain if we don't have tokens yet.
        if tokens.is_none() {
            *tokens = auth::load_tokens(&self.account)?;
        }

        let tokens = tokens.as_mut().ok_or_else(|| {
            anyhow::anyhow!("Not authenticated. Run `cascade backend auth gdrive`")
        })?;

        // Refresh if expired.
        if tokens.is_expired() {
            let http = reqwest::Client::new();
            let refreshed =
                auth::refresh_access_token(&http, &self.oauth, &tokens.refresh_token).await?;
            auth::save_tokens(&self.account, &refreshed)?;
            *tokens = refreshed;
        }

        Ok(tokens.access_token.clone())
    }

    /// List immediate children of the Drive root directory.
    /// Used for initial sync to populate top-level items quickly.
    async fn list_root_children(&self, token: &str) -> anyhow::Result<Vec<Change>> {
        let mut all_changes = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let resp = self
                .drive
                .list_files("root", token, page_token.as_deref())
                .await?;

            for file in resp.files {
                if let Some(entry) = file.to_file_entry(&self.instance_id) {
                    all_changes.push(Change::Created(entry));
                }
            }

            match resp.next_page_token {
                Some(next) => page_token = Some(next),
                None => break,
            }
        }

        tracing::info!(backend = %self.instance_id, count = all_changes.len(), "listed root children");

        Ok(all_changes)
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

        // No cursor → initial sync: list root-level children only.
        let Some(cursor) = cursor else {
            let root_changes = self.list_root_children(&token).await?;
            let start_token = self.drive.get_start_page_token(&token).await?;
            return Ok((root_changes, Cursor(start_token)));
        };

        let page_token = cursor.0.clone();
        let mut all_changes = Vec::new();
        let mut current_token = page_token;

        // Fetch all pages.
        loop {
            let resp = self.drive.get_changes(&current_token, &token).await?;

            for change in resp.changes {
                if change.removed.unwrap_or(false) {
                    // For deletions, we need a FileEntry with what we know.
                    // The change may or may not include the file metadata.
                    if let Some(file) = change.file {
                        if let Some(entry) = file.to_file_entry(&self.instance_id) {
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
                } else if let Some(file) = change.file
                    && let Some(entry) = file.to_file_entry(&self.instance_id)
                {
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

        // For root, use "root".
        if path_str == "/" || path_str.is_empty() {
            let file = self.drive.get_file("root", &token).await?;
            return file
                .to_file_entry(&self.instance_id)
                .ok_or_else(|| anyhow::anyhow!("Root folder returned trashed file"));
        }

        // Walk the path components.
        let components: Vec<&str> = path
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .filter(|s| !s.is_empty() && *s != "/")
            .collect();

        let mut current_id = "root".to_string();
        for component in &components {
            let children = self.drive.list_files(&current_id, &token, None).await?;
            let found = children.files.iter().find(|f| f.name == *component);

            match found {
                Some(f) => {
                    f.id.clone_into(&mut current_id);
                }
                None => anyhow::bail!("Path not found: {path_str}"),
            }
        }

        let file = self.drive.get_file(&current_id, &token).await?;
        file.to_file_entry(&self.instance_id)
            .ok_or_else(|| anyhow::anyhow!("File not found: {path_str}"))
    }

    async fn download(
        &self,
        file: &FileEntry,
        writer: &mut (dyn tokio::io::AsyncWrite + Unpin + Send),
    ) -> anyhow::Result<()> {
        let token = self.access_token().await?;
        let remote_id = file.id.native_id();

        let resp = self.drive.download_content(remote_id, &token).await?;

        // Read the full response body and write it out.
        let bytes = resp.bytes().await?;
        writer.write_all(&bytes).await?;
        writer.flush().await?;

        tracing::debug!(file = %file.id, size = file.size.unwrap_or(0), "downloaded");
        Ok(())
    }

    async fn upload(
        &self,
        path: &Path,
        reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
        parent_id: &cascade_engine::types::FileId,
    ) -> anyhow::Result<FileEntry> {
        let token = self.access_token().await?;

        // Read all data from the reader.
        let mut data = Vec::<u8>::new();
        tokio::io::AsyncReadExt::read_to_end(reader, &mut data).await?;

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("untitled");

        let file = self
            .drive
            .upload_file(file_name, parent_id.native_id(), &data, &token)
            .await?;

        file.to_file_entry(&self.instance_id)
            .ok_or_else(|| anyhow::anyhow!("upload returned trashed file"))
    }

    async fn create_dir(&self, path: &Path) -> anyhow::Result<FileEntry> {
        let token = self.access_token().await?;

        let dir_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("New Folder");

        // Resolve parent directory.
        let parent = path.parent().unwrap_or_else(|| Path::new("/"));
        let parent_id = if parent == Path::new("") || parent == Path::new("/") {
            "root".to_string()
        } else {
            let parent_entry = self.metadata(parent).await?;
            parent_entry.id.native_id().to_string()
        };

        let file = self
            .drive
            .create_directory(dir_name, &parent_id, &token)
            .await?;

        file.to_file_entry(&self.instance_id)
            .ok_or_else(|| anyhow::anyhow!("create_dir returned trashed file"))
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
        let dst_parent_id = if dst_parent == Path::new("") || dst_parent == Path::new("/") {
            "root".to_string()
        } else {
            let parent_entry = self.metadata(dst_parent).await?;
            parent_entry.id.native_id().to_string()
        };

        let new_name = dst.file_name().and_then(|n| n.to_str());

        let file = self
            .drive
            .move_file(file_id, &dst_parent_id, new_name, &token)
            .await?;

        file.to_file_entry(&self.instance_id)
            .ok_or_else(|| anyhow::anyhow!("move returned trashed file"))
    }

    async fn poll_interval(&self) -> Option<Duration> {
        #[allow(unknown_lints, clippy::duration_suboptimal_units)]
        Some(Duration::from_secs(60))
    }

    async fn list_children(&self, parent_native_id: &str) -> anyhow::Result<Vec<FileEntry>> {
        let token = self.access_token().await?;
        let mut all_entries = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let resp = self
                .drive
                .list_files(parent_native_id, &token, page_token.as_deref())
                .await?;

            for file in resp.files {
                if let Some(entry) = file.to_file_entry(&self.instance_id) {
                    all_entries.push(entry);
                }
            }

            match resp.next_page_token {
                Some(next) => page_token = Some(next),
                None => break,
            }
        }

        Ok(all_entries)
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
