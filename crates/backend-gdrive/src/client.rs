//! HTTP client, rate limiting, retry for Google Drive API.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use super::model::{AboutResponse, ChangesResponse, DriveFile, FileListResponse};

/// Token-bucket rate limiter for Google Drive API.
/// Allows ~10,000 requests per 100 seconds per user.
pub struct RateLimiter {
    tokens: AtomicU32,
    max_tokens: u32,
    refill_rate: u32,
}

impl RateLimiter {
    pub fn new(max_requests_per_100s: u32) -> Self {
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
    http: reqwest::Client,
    rate_limiter: RateLimiter,
    base_url: String,
    upload_url: String,
}

impl DriveClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            rate_limiter: RateLimiter::new(10_000),
            base_url: "https://www.googleapis.com/drive/v3".to_string(),
            upload_url: "https://www.googleapis.com/upload/drive/v3".to_string(),
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
        let resp = self
            .http
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
            .authenticated_get(&format!("files/{file_id}"), token, &[
                ("fields", "id,name,mimeType,parents,size,modifiedTime,md5Checksum,trashed"),
                ("supportsAllDrives", "true"),
            ])
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
            ("fields", "nextPageToken,files(id,name,mimeType,parents,size,modifiedTime,md5Checksum,trashed)"),
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

        let resp = self
            .authenticated_get("files", token, &params)
            .await?;
        let list = resp.json::<FileListResponse>().await?;
        Ok(list)
    }

    /// Download file content.
    pub async fn download_content(
        &self,
        file_id: &str,
        token: &str,
    ) -> anyhow::Result<reqwest::Response> {
        self.rate_limiter.acquire().await;
        let url = format!("{}/files/{file_id}", self.base_url);
        let resp = self
            .http
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
            .authenticated_get("about", token, &[
                ("fields", "storageQuota(limit,usage)"),
            ])
            .await?;
        let about = resp.json::<AboutResponse>().await?;
        Ok(about)
    }

    /// Get the initial start page token for the Changes stream.
    pub async fn get_start_page_token(&self, token: &str) -> anyhow::Result<String> {
        let resp = self
            .authenticated_get("changes/startPageToken", token, &[
                ("supportsAllDrives", "true"),
            ])
            .await?;
        #[derive(serde::Deserialize)]
        struct StartPageToken {
            start_page_token: String,
        }
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
