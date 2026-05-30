//! HTTP client, rate limiting, retry for Google Drive API.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use super::model::{AboutResponse, ChangesResponse, DriveFile, FileListResponse};

/// Token-bucket rate limiter for Google Drive API.
/// Allows ~10,000 requests per 100 seconds per user.
#[derive(Debug)]
pub struct RateLimiter {
    tokens: AtomicU32,
    max_tokens: u32,
    refill_rate: u32,
}

impl RateLimiter {
    #[must_use]
    pub const fn new(max_requests_per_100s: u32) -> Self {
        Self {
            tokens: AtomicU32::new(max_requests_per_100s),
            max_tokens: max_requests_per_100s,
            refill_rate: max_requests_per_100s / 100,
        }
    }

    /// Try to acquire a token. Returns true if successful.
    pub fn try_acquire(&self) -> bool {
        loop {
            let current = self.tokens.load(Ordering::Relaxed);
            if current == 0 {
                return false;
            }
            if self
                .tokens
                .compare_exchange_weak(current, current - 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
        }
    }

    /// Wait for a token to become available.
    pub async fn acquire(&self) {
        while !self.try_acquire() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Refill tokens (called periodically).
    pub fn refill(&self) {
        let current = self.tokens.load(Ordering::Relaxed);
        let new = (current + self.refill_rate).min(self.max_tokens);
        self.tokens.store(new, Ordering::Relaxed);
    }
}

/// Google Drive API HTTP client.
pub struct DriveClient {
    rate_limiter: RateLimiter,
    base_url: String,
    upload_url: String,
}

impl std::fmt::Debug for DriveClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriveClient")
            .field("base_url", &self.base_url)
            .field("upload_url", &self.upload_url)
            .finish_non_exhaustive()
    }
}

impl Default for DriveClient {
    fn default() -> Self {
        Self::new()
    }
}

impl DriveClient {
    #[must_use]
    pub fn new() -> Self {
        Self::with_urls(
            "https://www.googleapis.com/drive/v3".to_string(),
            "https://www.googleapis.com/upload/drive/v3".to_string(),
        )
    }

    fn http() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(0)
            .http1_only()
            .build()
            .unwrap_or_default()
    }

    /// Construct a client with custom base URLs — used in integration tests
    /// to point at a mock server instead of the real Drive API.
    #[must_use]
    pub fn with_urls(base_url: String, upload_url: String) -> Self {
        Self {
            rate_limiter: RateLimiter::new(10_000),
            base_url,
            upload_url,
        }
    }

    /// GET request to Drive API with rate limiting and auth.
    async fn authenticated_get(
        &self,
        path: &str,
        token: &str,
        query: &[(&str, &str)],
    ) -> anyhow::Result<reqwest::Response> {
        self.rate_limiter.acquire().await;
        let url = format!("{}/{path}", self.base_url);
        let resp = Self::http()
            .get(&url)
            .bearer_auth(token)
            .query(query)
            .send()
            .await?;

        let status = resp.status();
        if status.is_client_error() || status.is_server_error() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Drive API error {status}: {body}");
        }
        Ok(resp)
    }

    /// Fetch a single file by ID.
    pub async fn get_file(&self, file_id: &str, token: &str) -> anyhow::Result<DriveFile> {
        let resp = self
            .authenticated_get(
                &format!("files/{file_id}"),
                token,
                &[
                    (
                        "fields",
                        "id,name,mimeType,parents,size,modifiedTime,md5Checksum,trashed",
                    ),
                    ("supportsAllDrives", "true"),
                ],
            )
            .await?;
        let file = resp.json::<DriveFile>().await?;
        Ok(file)
    }

    /// List files in a directory (children of a parent).
    pub async fn list_files(
        &self,
        parent_id: &str,
        token: &str,
        page_token: Option<&str>,
    ) -> anyhow::Result<FileListResponse> {
        let query = format!("'{parent_id}' in parents and trashed = false");
        let mut params = vec![
            ("q", query.as_str()),
            (
                "fields",
                "nextPageToken,files(id,name,mimeType,parents,size,modifiedTime,md5Checksum,trashed)",
            ),
            ("pageSize", "100"),
            ("supportsAllDrives", "true"),
            ("includeItemsFromAllDrives", "true"),
            ("orderBy", "name"),
        ];
        let owned_token;
        if let Some(pt) = page_token {
            owned_token = pt.to_string();
            params.push(("pageToken", owned_token.as_str()));
        }

        let resp = self.authenticated_get("files", token, &params).await?;
        let list = resp.json::<FileListResponse>().await?;
        Ok(list)
    }

    /// Search for a file by name within a specific parent directory.
    /// Returns at most one match (the first found).
    pub async fn find_file_in_parent(
        &self,
        name: &str,
        parent_id: &str,
        token: &str,
    ) -> anyhow::Result<Option<DriveFile>> {
        self.rate_limiter.acquire().await;
        let query = format!(
            "'{parent_id}' in parents and name = '{}' and trashed = false",
            name.replace('\\', "\\\\").replace('"', "\\\"")
        );
        let params = [
            ("q", query.as_str()),
            (
                "fields",
                "files(id,name,mimeType,parents,size,modifiedTime,md5Checksum,trashed)",
            ),
            ("pageSize", "1"),
            ("supportsAllDrives", "true"),
            ("includeItemsFromAllDrives", "true"),
        ];
        let resp = self.authenticated_get("files", token, &params).await?;
        let list = resp.json::<FileListResponse>().await?;
        Ok(list.files.into_iter().next())
    }

    /// Download file content.
    pub async fn download_content(
        &self,
        file_id: &str,
        token: &str,
    ) -> anyhow::Result<reqwest::Response> {
        self.rate_limiter.acquire().await;
        let url = format!("{}/files/{file_id}", self.base_url);
        let resp = Self::http()
            .get(&url)
            .bearer_auth(token)
            .query(&[("alt", "media"), ("supportsAllDrives", "true")])
            .send()
            .await?;

        let status = resp.status();
        if status.is_client_error() || status.is_server_error() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Drive download error {status}: {body}");
        }
        Ok(resp)
    }

    /// Fetch storage quota / about info.
    pub async fn get_about(&self, token: &str) -> anyhow::Result<AboutResponse> {
        let resp = self
            .authenticated_get("about", token, &[("fields", "storageQuota(limit,usage)")])
            .await?;
        let about = resp.json::<AboutResponse>().await?;
        Ok(about)
    }

    /// Get the initial start page token for the Changes stream.
    pub async fn get_start_page_token(&self, token: &str) -> anyhow::Result<String> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct StartPageToken {
            start_page_token: String,
        }

        let resp = self
            .authenticated_get(
                "changes/startPageToken",
                token,
                &[("supportsAllDrives", "true")],
            )
            .await?;
        let spt = resp.json::<StartPageToken>().await?;
        Ok(spt.start_page_token)
    }

    /// Fetch changes from the Drive API.
    pub async fn get_changes(
        &self,
        page_token: &str,
        token: &str,
    ) -> anyhow::Result<ChangesResponse> {
        let resp = self
            .authenticated_get("changes", token, &[
                ("pageToken", page_token),
                ("fields", "nextPageToken,newStartPageToken,changes(kind,fileId,removed,file(id,name,mimeType,parents,size,modifiedTime,md5Checksum,trashed))"),
                ("pageSize", "100"),
                ("supportsAllDrives", "true"),
                ("includeItemsFromAllDrives", "true"),
            ])
            .await?;
        let changes = resp.json::<ChangesResponse>().await?;
        Ok(changes)
    }

    // ── Write operations ──

    /// Upload file content (create or update).
    /// Uses the multipart upload endpoint for new files.
    pub async fn upload_file(
        &self,
        file_name: &str,
        parent_id: &str,
        data: &[u8],
        token: &str,
    ) -> anyhow::Result<DriveFile> {
        self.rate_limiter.acquire().await;

        // Multipart upload: metadata + content.
        let metadata = serde_json::json!({
            "name": file_name,
            "parents": [parent_id]
        });

        let boundary = "cascade_upload_boundary";
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice("Content-Type: application/json; charset=UTF-8\r\n\r\n".as_bytes());
        body.extend_from_slice(metadata.to_string().as_bytes());
        body.extend_from_slice(format!("\r\n--{boundary}\r\n").as_bytes());
        body.extend_from_slice("Content-Type: application/octet-stream\r\n\r\n".as_bytes());
        body.extend_from_slice(data);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let url = format!(
            "{}/files?uploadType=multipart&supportsAllDrives=true&fields=id,name,mimeType,parents,size,modifiedTime,md5Checksum,trashed",
            self.upload_url
        );
        let resp = Self::http()
            .post(&url)
            .bearer_auth(token)
            .header(
                "Content-Type",
                format!("multipart/related; boundary={boundary}"),
            )
            .body(body)
            .send()
            .await?;

        let status = resp.status();
        if status.is_client_error() || status.is_server_error() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Drive upload error {status}: {body}");
        }
        let file = resp.json::<DriveFile>().await?;
        Ok(file)
    }

    /// Update an existing file's content.
    pub async fn update_file(
        &self,
        file_id: &str,
        data: &[u8],
        token: &str,
    ) -> anyhow::Result<DriveFile> {
        self.rate_limiter.acquire().await;
        let url = format!(
            "{}/files/{file_id}?uploadType=media&supportsAllDrives=true&fields=id,name,mimeType,parents,size,modifiedTime,md5Checksum,trashed",
            self.upload_url
        );
        let resp = Self::http()
            .patch(&url)
            .bearer_auth(token)
            .header("Content-Type", "application/octet-stream")
            .body(data.to_vec())
            .send()
            .await?;

        let status = resp.status();
        if status.is_client_error() || status.is_server_error() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Drive update error {status}: {body}");
        }
        let file = resp.json::<DriveFile>().await?;
        Ok(file)
    }

    /// Create a directory.
    pub async fn create_directory(
        &self,
        name: &str,
        parent_id: &str,
        token: &str,
    ) -> anyhow::Result<DriveFile> {
        self.rate_limiter.acquire().await;
        let url = format!(
            "{}/files?supportsAllDrives=true&fields=id,name,mimeType,parents,size,modifiedTime,md5Checksum,trashed",
            self.base_url
        );
        let body = serde_json::json!({
            "name": name,
            "mimeType": "application/vnd.google-apps.folder",
            "parents": [parent_id]
        });
        let resp = Self::http()
            .post(&url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if status.is_client_error() || status.is_server_error() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Drive create_dir error {status}: {body}");
        }
        let file = resp.json::<DriveFile>().await?;
        Ok(file)
    }

    /// Trash (soft-delete) a file.
    pub async fn trash_file(&self, file_id: &str, token: &str) -> anyhow::Result<()> {
        self.rate_limiter.acquire().await;
        let url = format!(
            "{}/files/{file_id}?supportsAllDrives=true&fields=id",
            self.base_url
        );
        let resp = Self::http()
            .patch(&url)
            .bearer_auth(token)
            .json(&serde_json::json!({"trashed": true}))
            .send()
            .await?;

        let status = resp.status();
        if status.is_client_error() || status.is_server_error() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Drive trash error {status}: {body}");
        }
        Ok(())
    }

    /// Move a file to a new parent and/or rename it.
    pub async fn move_file(
        &self,
        file_id: &str,
        new_parent_id: &str,
        new_name: Option<&str>,
        token: &str,
    ) -> anyhow::Result<DriveFile> {
        self.rate_limiter.acquire().await;
        let url = format!(
            "{}/files/{file_id}?supportsAllDrives=true&fields=id,name,mimeType,parents,size,modifiedTime,md5Checksum,trashed",
            self.base_url
        );
        let mut body = serde_json::Map::new();
        if let Some(name) = new_name {
            body.insert(
                "name".to_string(),
                serde_json::Value::String(name.to_string()),
            );
        }
        let body = serde_json::Value::Object(body);
        // The addParents/removeParents params handle parent change.
        let resp = Self::http()
            .patch(&url)
            .bearer_auth(token)
            .query(&[
                ("addParents", new_parent_id),
                ("removeParents", ""), // API removes all current parents when addParents + removeParents specified
            ])
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if status.is_client_error() || status.is_server_error() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Drive move error {status}: {body}");
        }
        let file = resp.json::<DriveFile>().await?;
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_acquire_and_exhaust() {
        let limiter = RateLimiter::new(5);
        for _ in 0..5 {
            assert!(limiter.try_acquire());
        }
        assert!(!limiter.try_acquire());
    }

    #[test]
    fn rate_limiter_refill() {
        let limiter = RateLimiter::new(100);
        for _ in 0..100 {
            assert!(limiter.try_acquire());
        }
        assert!(!limiter.try_acquire());
        limiter.refill();
        // refill_rate = 100/100 = 1, so after refill we get 1 token back.
        assert!(limiter.try_acquire());
    }

    #[test]
    fn client_construction() {
        let _client = DriveClient::new();
    }
}
