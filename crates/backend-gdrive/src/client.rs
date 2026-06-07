//! HTTP client, rate limiting, retry for Google Drive API.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use cascade_engine::backend::BackendError;

use super::model::{
    AboutResponse, ChangesResponse, DriveFile, FileListResponse, SharedDriveListResponse,
};

/// Describes the kind of listing to perform against the Drive files.list endpoint.
#[derive(Debug)]
pub enum ListQuery {
    /// List the immediate children of a directory.
    ///
    /// `drive_id` must be set when the directory lives inside a shared drive so
    /// that Drive scopes the query with `corpora=drive&driveId=<id>`. Omit for
    /// My Drive folders (uses `corpora=user`).
    ChildrenOf {
        parent_id: String,
        drive_id: Option<String>,
    },
    /// Items shared directly with the authenticated user (`sharedWithMe=true`).
    SharedWithMe,
    /// Items currently in the user's Bin (`trashed=true`).
    Trashed,
    /// Immediate children of a folder that is itself trashed.
    ChildrenOfTrashed { parent_id: String },
}

// ── Internal response type ───────────────────────────────────────────────────

/// Normalised HTTP response shared between the native (reqwest) and portable
/// (`HttpClient` trait) paths. Callers parse JSON from `body` directly rather
/// than calling `.json()` on a streaming response.
#[derive(Debug)]
pub struct DriveHttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

// ── Error helper ─────────────────────────────────────────────────────────────

/// Map a Drive API HTTP error status to a typed `BackendError` where relevant.
#[must_use]
pub fn drive_api_error(context: &str, status: u16, body: String) -> anyhow::Error {
    let msg = format!("{context} (HTTP {status}): {body}");
    match status {
        403 => BackendError::Forbidden(msg).into(),
        404 => BackendError::NotFound(msg).into(),
        409 => BackendError::Conflict(msg).into(),
        _ => anyhow::Error::msg(msg),
    }
}

// ── URL helper ────────────────────────────────────────────────────────────────

/// Append URL-encoded query parameters to `base_url`.
///
/// If `params` is empty, `base_url` is returned unchanged. If `base_url`
/// already contains a `?`, the new parameters are appended with `&`.
fn build_query_url(base_url: &str, params: &[(&str, &str)]) -> String {
    if params.is_empty() {
        return base_url.to_string();
    }
    let qs: String = params
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    if base_url.contains('?') {
        format!("{base_url}&{qs}")
    } else {
        format!("{base_url}?{qs}")
    }
}

// ── Rate limiter ──────────────────────────────────────────────────────────────

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

// ── DriveClient struct ────────────────────────────────────────────────────────

/// Google Drive API HTTP client.
///
/// Every Drive API call and `OAuth2` refresh routes through a single injected
/// [`HttpClient`](cascade_engine::portable::HttpClient). Native builds inject a
/// single long-lived pooled `reqwest::Client` (wrapped in
/// `cascade_engine::portable::native::ReqwestClient`); portable builds inject
/// their own adapter. There is exactly one client per `DriveClient`, reused for
/// the client's whole lifetime — the daemon owns one and shares it across all
/// Drive backends, so the pooled connection driver lives on the daemon's stable
/// main runtime and is never stranded.
pub struct DriveClient {
    rate_limiter: RateLimiter,
    pub(crate) base_url: String,
    pub(crate) upload_url: String,
    /// Injected HTTP client, used for every Drive API request and `OAuth2`
    /// refresh.
    http: std::sync::Arc<dyn cascade_engine::portable::HttpClient>,
}

impl std::fmt::Debug for DriveClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriveClient")
            .field("base_url", &self.base_url)
            .field("upload_url", &self.upload_url)
            .finish_non_exhaustive()
    }
}

// ── Constructors and HTTP helpers ─────────────────────────────────────────────

impl DriveClient {
    /// Construct a client with custom base URLs and an injected HTTP client.
    ///
    /// The daemon builds a single long-lived pooled `reqwest::Client`, wraps it
    /// in `cascade_engine::portable::native::ReqwestClient::from_client`, and
    /// injects it here so all Drive API calls and `OAuth2` refreshes share one
    /// pooled client on a stable runtime. Portable builds inject their own
    /// `HttpClient` because `reqwest` is unavailable.
    #[must_use]
    pub fn with_http_client(
        base_url: String,
        upload_url: String,
        http: std::sync::Arc<dyn cascade_engine::portable::HttpClient>,
    ) -> Self {
        Self {
            rate_limiter: RateLimiter::new(10_000),
            base_url,
            upload_url,
            http,
        }
    }

    /// Construct a client pointing at the production Google Drive API with an
    /// injected HTTP client.
    #[must_use]
    pub fn new_with_http_client(
        http: std::sync::Arc<dyn cascade_engine::portable::HttpClient>,
    ) -> Self {
        Self::with_http_client(
            "https://www.googleapis.com/drive/v3".to_string(),
            "https://www.googleapis.com/upload/drive/v3".to_string(),
            http,
        )
    }

    /// Issue an authenticated GET, rate-limited. Returns `Err` for 4xx/5xx.
    pub(crate) async fn authenticated_get(
        &self,
        path: &str,
        token: &str,
        query: &[(&str, &str)],
    ) -> anyhow::Result<DriveHttpResponse> {
        use cascade_engine::portable::HeaderMap;

        self.rate_limiter.acquire().await;
        let url = build_query_url(&format!("{}/{path}", self.base_url), query);

        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {token}").as_str());

        let resp = self
            .http
            .get(&url, headers)
            .await
            .map_err(|e| anyhow::anyhow!("Drive GET failed: {e}"))?;

        if resp.status >= 400 {
            let body_str = String::from_utf8_lossy(&resp.body).into_owned();
            return Err(drive_api_error("Drive API error", resp.status, body_str));
        }
        Ok(DriveHttpResponse {
            status: resp.status,
            body: resp.body,
        })
    }

    /// Issue a request with a body (POST, PATCH, PUT), rate-limited.
    pub(crate) async fn authenticated_write(
        &self,
        method: &str,
        url: &str,
        extra_query: &[(&str, &str)],
        token: &str,
        body: Vec<u8>,
        content_type: &str,
    ) -> anyhow::Result<DriveHttpResponse> {
        use cascade_engine::portable::HeaderMap;

        self.rate_limiter.acquire().await;
        let full_url = build_query_url(url, extra_query);

        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {token}").as_str());
        headers.insert("content-type", content_type);

        let resp = match method {
            "POST" => self
                .http
                .post(&full_url, headers, body)
                .await
                .map_err(|e| anyhow::anyhow!("Drive POST failed: {e}"))?,
            "PATCH" => self
                .http
                .patch(&full_url, headers, body)
                .await
                .map_err(|e| anyhow::anyhow!("Drive PATCH failed: {e}"))?,
            "PUT" => self
                .http
                .put(&full_url, headers, body)
                .await
                .map_err(|e| anyhow::anyhow!("Drive PUT failed: {e}"))?,
            other => anyhow::bail!("unsupported write method for Drive backend: {other}"),
        };

        if resp.status >= 400 {
            let body_str = String::from_utf8_lossy(&resp.body).into_owned();
            return Err(drive_api_error("Drive API error", resp.status, body_str));
        }
        Ok(DriveHttpResponse {
            status: resp.status,
            body: resp.body,
        })
    }

    /// Issue a GET with an HTTP `Range` header, **without** error-checking the
    /// status (callers handle 416 specially).
    pub(crate) async fn authenticated_get_range(
        &self,
        url: &str,
        token: &str,
        range: &str,
        query: &[(&str, &str)],
    ) -> anyhow::Result<DriveHttpResponse> {
        use cascade_engine::portable::HeaderMap;

        self.rate_limiter.acquire().await;
        let full_url = build_query_url(url, query);

        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {token}").as_str());
        if !range.is_empty() {
            headers.insert("range", range);
        }

        let resp = self
            .http
            .get(&full_url, headers)
            .await
            .map_err(|e| anyhow::anyhow!("Drive range GET failed: {e}"))?;

        Ok(DriveHttpResponse {
            status: resp.status,
            body: resp.body,
        })
    }
}

// ── Native convenience constructors ───────────────────────────────────────────

/// Native-only constructors that build a default pooled `reqwest::Client`
/// (wrapped in `ReqwestClient`) for callers that do not inject their own — the
/// CLI's standalone Drive calls and the integration tests. The daemon injects a
/// shared client via [`DriveClient::with_http_client`] instead.
#[cfg(not(feature = "portable"))]
impl DriveClient {
    #[must_use]
    pub fn new() -> Self {
        Self::new_with_http_client(std::sync::Arc::new(
            cascade_engine::portable::native::ReqwestClient::new(),
        ))
    }

    /// Construct a client with custom base URLs — used in integration tests.
    #[must_use]
    pub fn with_urls(base_url: String, upload_url: String) -> Self {
        Self::with_http_client(
            base_url,
            upload_url,
            std::sync::Arc::new(cascade_engine::portable::native::ReqwestClient::new()),
        )
    }
}

#[cfg(not(feature = "portable"))]
impl Default for DriveClient {
    fn default() -> Self {
        Self::new()
    }
}

// ── Shared public API ──────────────────────────────────────────────────────────

impl DriveClient {
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
        let file = serde_json::from_slice::<DriveFile>(&resp.body)?;
        Ok(file)
    }

    /// List files using the given query strategy.
    pub async fn list_files(
        &self,
        query: &ListQuery,
        token: &str,
        page_token: Option<&str>,
    ) -> anyhow::Result<FileListResponse> {
        let q_str: String;
        let drive_id_str: String;
        let page_token_str: String;

        let mut params: Vec<(&str, &str)> = Vec::new();

        match query {
            ListQuery::ChildrenOf {
                parent_id,
                drive_id,
            } => {
                q_str = format!("'{parent_id}' in parents and trashed = false");
                params.push(("q", &q_str));
                if let Some(did) = drive_id {
                    drive_id_str = did.clone();
                    params.push(("corpora", "drive"));
                    params.push(("driveId", &drive_id_str));
                    params.push(("includeItemsFromAllDrives", "true"));
                    params.push(("supportsAllDrives", "true"));
                } else {
                    params.push(("corpora", "user"));
                    params.push(("supportsAllDrives", "true"));
                    params.push(("includeItemsFromAllDrives", "true"));
                }
            }
            ListQuery::SharedWithMe => {
                q_str = "sharedWithMe = true and trashed = false".to_string();
                params.push(("q", &q_str));
                params.push(("corpora", "user"));
            }
            ListQuery::Trashed => {
                q_str = "trashed = true".to_string();
                params.push(("q", &q_str));
                params.push(("corpora", "user"));
            }
            ListQuery::ChildrenOfTrashed { parent_id } => {
                q_str = format!("'{parent_id}' in parents and trashed = true");
                params.push(("q", &q_str));
                params.push(("corpora", "user"));
                params.push(("supportsAllDrives", "true"));
                params.push(("includeItemsFromAllDrives", "true"));
            }
        }

        params.push((
            "fields",
            "nextPageToken,files(id,name,mimeType,parents,size,modifiedTime,md5Checksum,trashed,driveId)",
        ));
        params.push(("pageSize", "100"));
        params.push(("orderBy", "name"));

        if let Some(pt) = page_token {
            page_token_str = pt.to_string();
            params.push(("pageToken", &page_token_str));
        }

        let resp = self.authenticated_get("files", token, &params).await?;
        let list = serde_json::from_slice::<FileListResponse>(&resp.body)?;
        Ok(list)
    }

    /// List shared drives the authenticated user is a member of.
    pub async fn list_shared_drives(
        &self,
        token: &str,
        page_token: Option<&str>,
    ) -> anyhow::Result<SharedDriveListResponse> {
        let page_token_str: String;
        let mut params: Vec<(&str, &str)> = vec![
            ("fields", "nextPageToken,drives(id,name)"),
            ("pageSize", "100"),
        ];
        if let Some(pt) = page_token {
            page_token_str = pt.to_string();
            params.push(("pageToken", &page_token_str));
        }
        let resp = self.authenticated_get("drives", token, &params).await?;
        let list = serde_json::from_slice::<SharedDriveListResponse>(&resp.body)?;
        Ok(list)
    }

    /// Search for a file by name within a specific parent directory.
    pub async fn find_file_in_parent(
        &self,
        name: &str,
        parent_id: &str,
        token: &str,
    ) -> anyhow::Result<Option<DriveFile>> {
        let query = format!(
            "'{parent_id}' in parents and name = '{}' and trashed = false",
            name.replace('\\', "\\\\").replace('"', "\\\"")
        );
        let params = [
            ("q", query.as_str()),
            (
                "fields",
                "files(id,name,mimeType,parents,size,modifiedTime,md5Checksum,trashed,driveId)",
            ),
            ("pageSize", "1"),
            ("supportsAllDrives", "true"),
            ("includeItemsFromAllDrives", "true"),
        ];
        let resp = self.authenticated_get("files", token, &params).await?;
        let list = serde_json::from_slice::<FileListResponse>(&resp.body)?;
        Ok(list.files.into_iter().next())
    }

    /// Download file content. Returns the full body.
    pub async fn download_content(
        &self,
        file_id: &str,
        token: &str,
    ) -> anyhow::Result<DriveHttpResponse> {
        let url = format!("{}/files/{file_id}", self.base_url);
        self.authenticated_get_range(
            &url,
            token,
            // No range restriction — download the whole file.
            // We re-use the range helper so the rate limiter fires once.
            "",
            &[("alt", "media"), ("supportsAllDrives", "true")],
        )
        .await
        .and_then(|resp| {
            if resp.status >= 400 {
                let body_str = String::from_utf8_lossy(&resp.body).into_owned();
                Err(drive_api_error(
                    "Drive download error",
                    resp.status,
                    body_str,
                ))
            } else {
                Ok(resp)
            }
        })
    }

    /// Download a byte range of a file's content.
    pub async fn download_range(
        &self,
        file_id: &str,
        token: &str,
        offset: u64,
        length: u32,
    ) -> anyhow::Result<Vec<u8>> {
        if length == 0 {
            return Ok(Vec::new());
        }

        let url = format!("{}/files/{file_id}", self.base_url);
        let end = offset.saturating_add(u64::from(length)).saturating_sub(1);
        let range_header = format!("bytes={offset}-{end}");

        let resp = self
            .authenticated_get_range(
                &url,
                token,
                &range_header,
                &[("alt", "media"), ("supportsAllDrives", "true")],
            )
            .await?;

        // 416: offset at or past EOF — return empty per the trait contract.
        if resp.status == 416 {
            return Ok(Vec::new());
        }

        if resp.status >= 400 {
            let body_str = String::from_utf8_lossy(&resp.body).into_owned();
            return Err(drive_api_error(
                "Drive range download error",
                resp.status,
                body_str,
            ));
        }

        let honoured_range = resp.status == 206;
        let bytes = resp.body;

        if honoured_range {
            return Ok(bytes);
        }

        // 200: server returned the full file; slice client-side.
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(bytes.len());
        let len = usize::try_from(length).unwrap_or(usize::MAX);
        let stop = start.saturating_add(len).min(bytes.len());
        Ok(bytes.get(start..stop).unwrap_or_default().to_vec())
    }

    /// Fetch storage quota / about info.
    pub async fn get_about(&self, token: &str) -> anyhow::Result<AboutResponse> {
        let resp = self
            .authenticated_get("about", token, &[("fields", "storageQuota(limit,usage)")])
            .await?;
        let about = serde_json::from_slice::<AboutResponse>(&resp.body)?;
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
        let spt = serde_json::from_slice::<StartPageToken>(&resp.body)?;
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
        let changes = serde_json::from_slice::<ChangesResponse>(&resp.body)?;
        Ok(changes)
    }

    // ── Write operations ──

    /// Upload file content (create new file via multipart upload).
    pub async fn upload_file(
        &self,
        file_name: &str,
        parent_id: &str,
        data: &[u8],
        token: &str,
    ) -> anyhow::Result<DriveFile> {
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

        let url = format!("{}/files", self.upload_url);
        let resp = self
            .authenticated_write(
                "POST",
                &url,
                &[
                    ("uploadType", "multipart"),
                    ("supportsAllDrives", "true"),
                    (
                        "fields",
                        "id,name,mimeType,parents,size,modifiedTime,md5Checksum,trashed",
                    ),
                ],
                token,
                body,
                &format!("multipart/related; boundary={boundary}"),
            )
            .await?;

        let file = serde_json::from_slice::<DriveFile>(&resp.body)?;
        Ok(file)
    }

    /// Update an existing file's content.
    pub async fn update_file(
        &self,
        file_id: &str,
        data: &[u8],
        token: &str,
    ) -> anyhow::Result<DriveFile> {
        let url = format!("{}/files/{file_id}", self.upload_url);
        let resp = self
            .authenticated_write(
                "PATCH",
                &url,
                &[
                    ("uploadType", "media"),
                    ("supportsAllDrives", "true"),
                    (
                        "fields",
                        "id,name,mimeType,parents,size,modifiedTime,md5Checksum,trashed",
                    ),
                ],
                token,
                data.to_vec(),
                "application/octet-stream",
            )
            .await?;

        let file = serde_json::from_slice::<DriveFile>(&resp.body)?;
        Ok(file)
    }

    /// Create a directory.
    pub async fn create_directory(
        &self,
        name: &str,
        parent_id: &str,
        token: &str,
    ) -> anyhow::Result<DriveFile> {
        let body = serde_json::json!({
            "name": name,
            "mimeType": "application/vnd.google-apps.folder",
            "parents": [parent_id]
        });
        let url = format!("{}/files", self.base_url);
        let resp = self
            .authenticated_write(
                "POST",
                &url,
                &[
                    ("supportsAllDrives", "true"),
                    (
                        "fields",
                        "id,name,mimeType,parents,size,modifiedTime,md5Checksum,trashed",
                    ),
                ],
                token,
                body.to_string().into_bytes(),
                "application/json",
            )
            .await?;

        let file = serde_json::from_slice::<DriveFile>(&resp.body)?;
        Ok(file)
    }

    /// Trash (soft-delete) a file.
    pub async fn trash_file(&self, file_id: &str, token: &str) -> anyhow::Result<()> {
        let url = format!("{}/files/{file_id}", self.base_url);
        let body = serde_json::json!({"trashed": true})
            .to_string()
            .into_bytes();
        self.authenticated_write(
            "PATCH",
            &url,
            &[("supportsAllDrives", "true"), ("fields", "id")],
            token,
            body,
            "application/json",
        )
        .await?;
        Ok(())
    }

    /// Restore a trashed file by clearing the `trashed` flag.
    pub async fn untrash_file(&self, file_id: &str, token: &str) -> anyhow::Result<()> {
        let url = format!("{}/files/{file_id}", self.base_url);
        let body = serde_json::json!({"trashed": false})
            .to_string()
            .into_bytes();
        self.authenticated_write(
            "PATCH",
            &url,
            &[("supportsAllDrives", "true"), ("fields", "id")],
            token,
            body,
            "application/json",
        )
        .await?;
        Ok(())
    }

    /// Move a file to a new parent and/or rename it.
    pub async fn move_file(
        &self,
        file_id: &str,
        new_parent_id: &str,
        remove_parents: &[String],
        new_name: Option<&str>,
        token: &str,
    ) -> anyhow::Result<DriveFile> {
        let url = format!("{}/files/{file_id}", self.base_url);

        let mut metadata = serde_json::Map::new();
        if let Some(name) = new_name {
            metadata.insert(
                "name".to_string(),
                serde_json::Value::String(name.to_string()),
            );
        }
        let body_json = serde_json::Value::Object(metadata);
        let remove_csv = remove_parents.join(",");

        let resp = self
            .authenticated_write(
                "PATCH",
                &url,
                &[
                    ("supportsAllDrives", "true"),
                    (
                        "fields",
                        "id,name,mimeType,parents,size,modifiedTime,md5Checksum,trashed",
                    ),
                    ("addParents", new_parent_id),
                    ("removeParents", remove_csv.as_str()),
                ],
                token,
                body_json.to_string().into_bytes(),
                "application/json",
            )
            .await?;

        let file = serde_json::from_slice::<DriveFile>(&resp.body)?;
        Ok(file)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

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
        assert!(limiter.try_acquire());
    }

    /// The native convenience constructor builds a `DriveClient` backed by a
    /// default pooled `reqwest::Client`. The daemon injects a shared client via
    /// `with_http_client` instead; both produce a fully-formed client.
    #[test]
    #[cfg(not(feature = "portable"))]
    fn client_construction() {
        let _client = DriveClient::new();
    }
}
