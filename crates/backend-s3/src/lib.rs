//! S3-compatible backend for Cascade.
//!
//! Supports AWS S3, `MinIO`, Backblaze B2, Cloudflare R2, and any other
//! S3-compatible API. Uses AWS Signature Version 4 signing via `reqwest`.
//! No heavy AWS SDK dependency — signing is implemented directly.

pub mod signing;

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use cascade_engine::backend::Backend;
use cascade_engine::types::{Change, Cursor, FileEntry, FileId, ItemId, Quota};
use chrono::{DateTime, Utc};
use signing::{SigningParams, build_canonical_query_string, sha256_hex, sign, uri_encode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ── Constants ────────────────────────────────────────────────────────────────

/// Maximum bytes buffered for a single `PutObject` upload.
///
/// S3 `PutObject` requires `Content-Length` upfront, so we must buffer the
/// entire payload in memory. Objects larger than 5 GB require multipart upload,
/// which is not yet implemented.
const MAX_UPLOAD_BYTES: usize = 5 * 1024 * 1024 * 1024;

// ── Config ───────────────────────────────────────────────────────────────────

/// Configuration for an S3-compatible backend.
#[derive(Debug, Clone)]
pub struct S3Config {
    /// Base endpoint URL, e.g. `"https://s3.amazonaws.com"` or a `MinIO` URL.
    pub endpoint: String,
    /// Bucket name.
    pub bucket: String,
    /// AWS region, e.g. `"us-east-1"`.
    pub region: String,
    /// AWS access key ID.
    pub access_key_id: String,
    /// AWS secret access key.
    pub secret_access_key: String,
    /// Optional key prefix / virtual folder (no leading or trailing slash).
    pub prefix: Option<String>,
}

// ── Backend struct ───────────────────────────────────────────────────────────

/// S3-compatible backend.
#[derive(Debug)]
pub struct S3Backend {
    config: S3Config,
    http: reqwest::Client,
    backend_id: String,
}

// ── Factory function ─────────────────────────────────────────────────────────

/// Create an S3 backend from a TOML config value.
///
/// Required keys:
/// - `endpoint` — base URL, e.g. `"https://s3.amazonaws.com"`
/// - `bucket` — bucket name
/// - `region` — AWS region
/// - `access_key_id` — AWS access key ID
/// - `secret_access_key` — AWS secret access key
///
/// Optional keys:
/// - `id` — unique backend identifier (default: `"s3"`)
/// - `prefix` — key prefix / virtual folder
pub fn create_backend(config: &toml::Value) -> anyhow::Result<Box<dyn Backend>> {
    let get_str = |key: &str| -> anyhow::Result<String> {
        config
            .get(key)
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("S3 backend requires '{key}' config"))
    };

    let endpoint = get_str("endpoint")?;
    let bucket = get_str("bucket")?;
    let region = get_str("region")?;
    let access_key_id = get_str("access_key_id")?;
    let secret_access_key = get_str("secret_access_key")?;

    let prefix = config
        .get("prefix")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);

    let backend_id = config
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("s3")
        .to_string();

    let s3_config = S3Config {
        endpoint,
        bucket,
        region,
        access_key_id,
        secret_access_key,
        prefix,
    };

    let http = reqwest::Client::new();

    Ok(Box::new(S3Backend {
        config: s3_config,
        http,
        backend_id,
    }))
}

// ── S3Backend helpers ────────────────────────────────────────────────────────

impl S3Backend {
    /// Build the S3 object key for a given path, incorporating the optional prefix.
    ///
    /// - `path` is treated as relative (any leading `/` is stripped).
    /// - If `prefix` is set the key is `{prefix}/{path}`.
    #[must_use]
    pub(crate) fn key_for_path(&self, path: &Path) -> String {
        let path_str = path.to_string_lossy();
        let stripped = path_str.trim_start_matches('/');
        self.config.prefix.as_deref().map_or_else(
            || stripped.to_string(),
            |prefix| format!("{prefix}/{stripped}"),
        )
    }

    /// Build the S3 prefix used when listing objects under `path`.
    ///
    /// Returns a string that ends with `/` (S3 list "directory" convention).
    fn list_prefix_for_path(&self, path: &Path) -> String {
        let path_str = path.to_string_lossy();
        let stripped = path_str.trim_start_matches('/').trim_end_matches('/');

        let base = self.config.prefix.as_deref().map_or_else(
            || stripped.to_string(),
            |prefix| {
                if stripped.is_empty() {
                    prefix.to_string()
                } else {
                    format!("{prefix}/{stripped}")
                }
            },
        );

        if base.is_empty() {
            String::new()
        } else {
            format!("{base}/")
        }
    }

    /// Build the S3 request URL for a given object key (no query string).
    fn object_url(&self, key: &str) -> String {
        let encoded = uri_encode(key, false);
        format!(
            "{}/{}/{}",
            self.config.endpoint, self.config.bucket, encoded
        )
    }

    /// Return the base URL for `ListObjectsV2` (no query string).
    fn list_base_url(&self) -> String {
        format!("{}/{}", self.config.endpoint, self.config.bucket)
    }

    /// Return the host portion of the endpoint URL (for the `Host` header and
    /// canonical headers used in `SigV4` signing).
    fn endpoint_host(&self) -> String {
        url::Url::parse(&self.config.endpoint)
            .ok()
            .and_then(|u| u.host_str().map(String::from))
            .unwrap_or_else(|| self.config.endpoint.clone())
    }

    /// Sign and execute an HTTP request, returning the response.
    ///
    /// `base_url` is the URL **without** a query string (e.g. `"https://s3.amazonaws.com/bucket/key"`).
    /// `query_params` is a slice of raw (unencoded) key-value pairs. The function
    /// encodes them exactly once — for both `SigV4` signing and the outgoing URL.
    async fn signed_request(
        &self,
        method: &str,
        base_url: &str,
        query_params: &[(&str, &str)],
        body: &[u8],
        extra_headers: &[(&str, &str)],
    ) -> anyhow::Result<reqwest::Response> {
        let parsed = url::Url::parse(base_url)
            .map_err(|e| anyhow::anyhow!("invalid URL {base_url}: {e}"))?;

        let uri_path = parsed.path();
        let payload_hash = sha256_hex(body);
        let host = self.endpoint_host();
        let now = Utc::now();

        let signing_params = SigningParams {
            method,
            uri_path,
            query_params,
            host: &host,
            payload_hash: &payload_hash,
            access_key_id: &self.config.access_key_id,
            secret_access_key: &self.config.secret_access_key,
            region: &self.config.region,
            service: "s3",
            now,
        };

        let signed = sign(&signing_params);

        // Build the full request URL: base + encoded query string (encoding once,
        // consistent with what SigV4 signed above).
        let request_url = if query_params.is_empty() {
            base_url.to_string()
        } else {
            let qs = build_canonical_query_string(query_params);
            format!("{base_url}?{qs}")
        };

        let mut req = self
            .http
            .request(method.parse::<reqwest::Method>()?, request_url)
            .header("x-amz-date", &signed.x_amz_date)
            .header("x-amz-content-sha256", &signed.x_amz_content_sha256)
            .header("authorization", &signed.authorization)
            .header("host", &host);

        for (name, value) in extra_headers {
            req = req.header(*name, *value);
        }

        if !body.is_empty() {
            req = req.body(body.to_vec());
        }

        let resp = req.send().await?;
        Ok(resp)
    }

    /// Check a response for error status and return its body as an error message
    /// if the status is not 2xx.
    async fn check_response(resp: reqwest::Response) -> anyhow::Result<reqwest::Response> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_else(|_| String::new());
        anyhow::bail!("S3 error {status}: {body}");
    }

    /// Build a `FileEntry` for an S3 object.
    fn object_entry(
        &self,
        key: &str,
        name: &str,
        size: u64,
        last_modified: Option<DateTime<Utc>>,
        parent_id: &ItemId,
    ) -> FileEntry {
        let id = ItemId::new(&self.backend_id, key);
        let mut entry =
            FileEntry::file(id, parent_id.clone(), name.to_string()).with_size(Some(size));
        entry.mod_time = last_modified;
        entry
    }
}

// ── XML parsing helpers ──────────────────────────────────────────────────────

/// Extract the text content of the first occurrence of `<tag>…</tag>` in `xml`.
///
/// Uses `split_once` on ASCII tag delimiters to avoid string-slice indexing.
fn xml_text<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let (_, after_open) = xml.split_once(open.as_str())?;
    let (content, _) = after_open.split_once(close.as_str())?;
    Some(content)
}

/// Iterate over the inner text of all `<tag>…</tag>` occurrences in `xml`,
/// calling `f` with each block (including the enclosing tags).
///
/// We split on `<tag>` to get the portions after each opening tag, then split
/// on `</tag>` to isolate the content — avoiding any string indexing.
fn for_each_xml_block<F>(xml: &str, tag: &str, mut f: F) -> anyhow::Result<()>
where
    F: FnMut(&str) -> anyhow::Result<()>,
{
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");

    let mut remaining = xml;
    while let Some((_, after_open)) = remaining.split_once(open.as_str()) {
        let (block, after_close) = after_open
            .split_once(close.as_str())
            .ok_or_else(|| anyhow::anyhow!("malformed XML: unclosed <{tag}>"))?;
        f(block)?;
        remaining = after_close;
    }
    Ok(())
}

/// Parse the XML body of a `ListObjectsV2` response into file and directory
/// entries.
///
/// Returns `(entries, next_continuation_token)`.
fn parse_list_response(
    xml: &str,
    backend_id: &str,
    list_prefix: &str,
    parent_id: &ItemId,
) -> anyhow::Result<(Vec<FileEntry>, Option<String>)> {
    let mut entries = Vec::new();

    // Parse <Contents> elements — these are regular objects.
    for_each_xml_block(xml, "Contents", |block| {
        let key = xml_text(block, "Key")
            .ok_or_else(|| anyhow::anyhow!("ListObjectsV2 <Contents> missing <Key>"))?;

        // Skip "directory" marker objects (keys ending in `/`).
        if !key.ends_with('/') {
            let size: u64 = xml_text(block, "Size")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);

            let last_modified = xml_text(block, "LastModified")
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Utc));

            // Strip the list prefix to get the bare name.
            let name = key
                .strip_prefix(list_prefix)
                .unwrap_or(key)
                .trim_end_matches('/');

            // Only include direct children (no `/` in the remainder).
            if !name.is_empty() && !name.contains('/') {
                let id = ItemId::new(backend_id, key);
                let mut entry =
                    FileEntry::file(id, parent_id.clone(), name.to_string()).with_size(Some(size));
                entry.mod_time = last_modified;
                entries.push(entry);
            }
        }
        Ok(())
    })?;

    // Parse <CommonPrefixes> elements — these are virtual directories.
    for_each_xml_block(xml, "CommonPrefixes", |block| {
        let prefix = xml_text(block, "Prefix")
            .ok_or_else(|| anyhow::anyhow!("ListObjectsV2 <CommonPrefixes> missing <Prefix>"))?;

        // Strip the list_prefix to get the bare directory name.
        let name = prefix
            .strip_prefix(list_prefix)
            .unwrap_or(prefix)
            .trim_end_matches('/');

        if !name.is_empty() && !name.contains('/') {
            let id = ItemId::new(backend_id, prefix);
            entries.push(FileEntry::dir(id, parent_id.clone(), name.to_string()));
        }
        Ok(())
    })?;

    // Check for a continuation token.
    let next_token = xml_text(xml, "NextContinuationToken").map(String::from);

    Ok((entries, next_token))
}

/// Parse the XML body of a flat (no-delimiter) `ListObjectsV2` response into
/// file entries.
///
/// Unlike [`parse_list_response`], this does not apply a depth filter — it
/// returns all objects regardless of how many path components they contain.
/// Used by [`S3Backend::list_all_objects`] for recursive change detection.
///
/// Returns `(entries, next_continuation_token)`.
fn parse_flat_list_response(
    xml: &str,
    backend_id: &str,
    prefix: &str,
    parent_id: &ItemId,
) -> anyhow::Result<(Vec<FileEntry>, Option<String>)> {
    let mut entries = Vec::new();

    for_each_xml_block(xml, "Contents", |block| {
        let key = xml_text(block, "Key")
            .ok_or_else(|| anyhow::anyhow!("ListObjectsV2 <Contents> missing <Key>"))?;

        // Skip "directory" marker objects (keys ending in `/`).
        if !key.ends_with('/') {
            let size: u64 = xml_text(block, "Size")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);

            let last_modified = xml_text(block, "LastModified")
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Utc));

            // Strip the prefix to get the path relative to the listing root.
            let relative = key.strip_prefix(prefix).unwrap_or(key);

            // The file name is the last path component.
            let name = relative
                .rsplit('/')
                .next()
                .filter(|n| !n.is_empty())
                .unwrap_or(relative);

            if !name.is_empty() {
                let id = ItemId::new(backend_id, key);
                let mut entry =
                    FileEntry::file(id, parent_id.clone(), name.to_string()).with_size(Some(size));
                entry.mod_time = last_modified;
                entries.push(entry);
            }
        }
        Ok(())
    })?;

    let next_token = xml_text(xml, "NextContinuationToken").map(String::from);

    Ok((entries, next_token))
}

/// Parse a `HeadObject` response into a `FileEntry`.
fn parse_head_response(
    headers: &reqwest::header::HeaderMap,
    key: &str,
    name: &str,
    parent_id: &ItemId,
    backend_id: &str,
) -> FileEntry {
    let size: u64 = headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let last_modified = headers
        .get("last-modified")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| DateTime::parse_from_rfc2822(s).ok())
        .map(|dt| dt.with_timezone(&Utc));

    let id = ItemId::new(backend_id, key);
    let mut entry = FileEntry::file(id, parent_id.clone(), name.to_string()).with_size(Some(size));
    entry.mod_time = last_modified;
    entry
}

// ── Backend implementation ───────────────────────────────────────────────────

#[async_trait]
impl Backend for S3Backend {
    fn id(&self) -> &str {
        &self.backend_id
    }

    fn display_name(&self) -> &str {
        &self.config.bucket
    }

    async fn quota(&self) -> anyhow::Result<Option<Quota>> {
        // S3 does not expose quota information via a standard API call.
        Ok(None)
    }

    /// Return all changes since `cursor`.
    ///
    /// S3 does not provide a native change-stream, so this implementation uses
    /// the cursor as a timestamp and performs a full list, returning all objects
    /// whose `LastModified` time is after the cursor timestamp. When `cursor` is
    /// `None` a full snapshot is returned as `Change::Created` events.
    async fn changes(&self, cursor: Option<&Cursor>) -> anyhow::Result<(Vec<Change>, Cursor)> {
        let since: Option<DateTime<Utc>> = cursor
            .and_then(|c| DateTime::parse_from_rfc3339(&c.0).ok())
            .map(|dt| dt.with_timezone(&Utc));

        let root_parent = ItemId::new(&self.backend_id, "/");
        let all_entries = self.list_all_objects(&root_parent).await?;

        let changes: Vec<Change> = all_entries
            .into_iter()
            .filter(|e| since.is_none_or(|since_dt| e.mod_time.is_none_or(|mt| mt > since_dt)))
            .map(Change::Created)
            .collect();

        let new_cursor = Cursor(Utc::now().to_rfc3339());
        Ok((changes, new_cursor))
    }

    async fn metadata(&self, path: &Path) -> anyhow::Result<FileEntry> {
        let key = self.key_for_path(path);
        let url = self.object_url(&key);

        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_else(|| path.to_str().unwrap_or(""))
            .to_string();

        let parent_path = path.parent().unwrap_or_else(|| Path::new("/"));
        let parent_key = self.key_for_path(parent_path);
        let parent_id = ItemId::new(&self.backend_id, &parent_key);

        let resp = self.signed_request("HEAD", &url, &[], &[], &[]).await?;
        let resp = Self::check_response(resp).await?;

        Ok(parse_head_response(
            resp.headers(),
            &key,
            &name,
            &parent_id,
            &self.backend_id,
        ))
    }

    async fn download(
        &self,
        file: &FileEntry,
        writer: &mut (dyn tokio::io::AsyncWrite + Unpin + Send),
    ) -> anyhow::Result<()> {
        let key = file.id.native_id();
        let url = self.object_url(key);

        let resp = self.signed_request("GET", &url, &[], &[], &[]).await?;
        let resp = Self::check_response(resp).await?;

        let bytes = resp.bytes().await?;
        writer.write_all(&bytes).await?;
        writer.flush().await?;

        tracing::debug!(
            file = %file.id,
            size = bytes.len(),
            "downloaded from S3"
        );
        Ok(())
    }

    async fn upload(
        &self,
        path: &Path,
        reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
        _parent_id: &FileId,
    ) -> anyhow::Result<FileEntry> {
        let key = self.key_for_path(path);
        let url = self.object_url(&key);

        let mut data = Vec::new();
        reader.read_to_end(&mut data).await?;

        // S3 PutObject requires Content-Length upfront. We buffer the full file.
        // For objects > 5 GB, multipart upload (not yet implemented) is required.
        if data.len() > MAX_UPLOAD_BYTES {
            anyhow::bail!("upload exceeds 5 GB limit; multipart upload is not yet implemented");
        }

        let content_length = data.len().to_string();

        let resp = self
            .signed_request(
                "PUT",
                &url,
                &[],
                &data,
                &[("content-length", &content_length)],
            )
            .await?;
        Self::check_response(resp).await?;

        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_else(|| path.to_str().unwrap_or(""))
            .to_string();

        let parent_path = path.parent().unwrap_or_else(|| Path::new("/"));
        let parent_key = self.key_for_path(parent_path);
        let parent_id = ItemId::new(&self.backend_id, &parent_key);

        let size = u64::try_from(data.len()).unwrap_or(u64::MAX);

        tracing::debug!(path = %path.display(), size, "uploaded to S3");

        Ok(self.object_entry(&key, &name, size, Some(Utc::now()), &parent_id))
    }

    async fn update(
        &self,
        file_id: &FileId,
        reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
    ) -> anyhow::Result<FileEntry> {
        let key = file_id.native_id();
        let url = self.object_url(&key);

        let mut data = Vec::new();
        reader.read_to_end(&mut data).await?;

        if data.len() > MAX_UPLOAD_BYTES {
            anyhow::bail!("upload exceeds 5 GB limit; multipart upload is not yet implemented");
        }

        let content_length = data.len().to_string();

        let resp = self
            .signed_request(
                "PUT",
                &url,
                &[],
                &data,
                &[("content-length", &content_length)],
            )
            .await?;
        Self::check_response(resp).await?;

        let size = u64::try_from(data.len()).unwrap_or(u64::MAX);
        let parent_key = key
            .rfind('/')
            .map_or(String::new(), |i| key[..i].to_string());
        let parent_id = ItemId::new(&self.backend_id, &parent_key);
        let name = key.rsplit('/').next().unwrap_or(&key).to_string();

        Ok(self.object_entry(&key, &name, size, Some(Utc::now()), &parent_id))
    }

    async fn create_dir(&self, path: &Path) -> anyhow::Result<FileEntry> {
        // S3 has no real directories; we PUT a zero-byte object with a trailing slash.
        let key = format!("{}/", self.key_for_path(path));
        let url = self.object_url(&key);

        let resp = self
            .signed_request("PUT", &url, &[], &[], &[("content-length", "0")])
            .await?;
        Self::check_response(resp).await?;

        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_else(|| path.to_str().unwrap_or(""))
            .to_string();

        let parent_path = path.parent().unwrap_or_else(|| Path::new("/"));
        let parent_key = self.key_for_path(parent_path);
        let parent_id = ItemId::new(&self.backend_id, &parent_key);

        tracing::debug!(path = %path.display(), "created S3 directory marker");

        Ok(FileEntry::dir(
            ItemId::new(&self.backend_id, &key),
            parent_id,
            name,
        ))
    }

    async fn delete(&self, file: &FileEntry) -> anyhow::Result<()> {
        let key = file.id.native_id();
        let url = self.object_url(key);

        let resp = self.signed_request("DELETE", &url, &[], &[], &[]).await?;
        Self::check_response(resp).await?;

        tracing::debug!(file = %file.id, "deleted from S3");
        Ok(())
    }

    async fn move_entry(&self, src: &Path, dst: &Path) -> anyhow::Result<FileEntry> {
        // S3 has no native move; copy then delete.
        let src_key = self.key_for_path(src);
        let dst_key = self.key_for_path(dst);
        let dst_url = self.object_url(&dst_key);

        let copy_source = format!("{}/{}", self.config.bucket, uri_encode(&src_key, false));

        let resp = self
            .signed_request(
                "PUT",
                &dst_url,
                &[],
                &[],
                &[("x-amz-copy-source", &copy_source)],
            )
            .await?;
        Self::check_response(resp).await?;

        // Delete the source.
        let src_url = self.object_url(&src_key);
        let del_resp = self
            .signed_request("DELETE", &src_url, &[], &[], &[])
            .await?;
        Self::check_response(del_resp).await?;

        let name = dst
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_else(|| dst.to_str().unwrap_or(""))
            .to_string();

        let parent_path = dst.parent().unwrap_or_else(|| Path::new("/"));
        let parent_key = self.key_for_path(parent_path);
        let parent_id = ItemId::new(&self.backend_id, &parent_key);

        // Fetch real metadata for the destination object via HEAD.
        let head_resp = self.signed_request("HEAD", &dst_url, &[], &[], &[]).await?;
        let head_resp = Self::check_response(head_resp).await?;
        let dst_entry = parse_head_response(
            head_resp.headers(),
            &dst_key,
            &name,
            &parent_id,
            &self.backend_id,
        );

        tracing::debug!(
            src = %src.display(),
            dst = %dst.display(),
            "moved in S3"
        );

        Ok(dst_entry)
    }

    async fn poll_interval(&self) -> Option<Duration> {
        // S3 doesn't push changes, so use a fixed 60-second poll.
        #[allow(unknown_lints, clippy::duration_suboptimal_units)]
        Some(Duration::from_secs(60))
    }
}

// ── Private helpers for full-tree listing ────────────────────────────────────

impl S3Backend {
    /// List all objects recursively (no delimiter), used for change detection.
    async fn list_all_objects(&self, parent_id: &ItemId) -> anyhow::Result<Vec<FileEntry>> {
        let base_prefix = self.config.prefix.clone().unwrap_or_default();
        let prefix_with_slash = if base_prefix.is_empty() {
            String::new()
        } else {
            format!("{base_prefix}/")
        };

        let base_url = self.list_base_url();
        let mut entries = Vec::new();
        let mut continuation_token: Option<String> = None;

        loop {
            let mut params: Vec<(&str, &str)> =
                vec![("list-type", "2"), ("prefix", &prefix_with_slash)];
            if let Some(ref token) = continuation_token {
                params.push(("continuation-token", token.as_str()));
            }

            let resp = self
                .signed_request("GET", &base_url, &params, &[], &[])
                .await?;
            let resp = Self::check_response(resp).await?;
            let xml = resp.text().await?;

            let (page_entries, next_token) =
                parse_flat_list_response(&xml, &self.backend_id, &prefix_with_slash, parent_id)?;
            entries.extend(page_entries);

            match next_token {
                Some(token) => continuation_token = Some(token),
                None => break,
            }
        }

        Ok(entries)
    }

    /// List objects under `path` with delimiter `/` (one level deep).
    ///
    /// Returns both file entries and directory entries. Used by `Backend::changes`
    /// and internally by the engine for directory traversal.
    pub async fn list(&self, path: &Path) -> anyhow::Result<Vec<FileEntry>> {
        let list_prefix = self.list_prefix_for_path(path);
        let parent_key = self.key_for_path(path);
        let parent_id = ItemId::new(&self.backend_id, &parent_key);

        let base_url = self.list_base_url();
        let mut entries = Vec::new();
        let mut continuation_token: Option<String> = None;

        loop {
            let mut params: Vec<(&str, &str)> = vec![
                ("list-type", "2"),
                ("delimiter", "/"),
                ("prefix", &list_prefix),
            ];
            if let Some(ref token) = continuation_token {
                params.push(("continuation-token", token.as_str()));
            }

            let resp = self
                .signed_request("GET", &base_url, &params, &[], &[])
                .await?;
            let resp = Self::check_response(resp).await?;
            let xml = resp.text().await?;

            let (page_entries, next_token) =
                parse_list_response(&xml, &self.backend_id, &list_prefix, &parent_id)?;
            entries.extend(page_entries);

            match next_token {
                Some(token) => continuation_token = Some(token),
                None => break,
            }
        }

        Ok(entries)
    }
}

// ── Private helper: strip_key_prefix (used in tests) ─────────────────────────

impl S3Backend {
    /// Strip the list prefix from a full S3 key, returning only the
    /// name component relative to the list prefix.
    #[cfg(test)]
    fn strip_key_prefix<'a>(&self, key: &'a str, list_prefix: &str) -> Option<&'a str> {
        key.strip_prefix(list_prefix)
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config(extra: &[(&str, &str)]) -> toml::Value {
        let mut table = toml::map::Map::new();
        table.insert(
            "endpoint".to_string(),
            toml::Value::String("https://s3.amazonaws.com".to_string()),
        );
        table.insert(
            "bucket".to_string(),
            toml::Value::String("my-bucket".to_string()),
        );
        table.insert(
            "region".to_string(),
            toml::Value::String("us-east-1".to_string()),
        );
        table.insert(
            "access_key_id".to_string(),
            toml::Value::String("AKIAIOSFODNN7EXAMPLE".to_string()),
        );
        table.insert(
            "secret_access_key".to_string(),
            toml::Value::String("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string()),
        );
        for (k, v) in extra {
            table.insert(k.to_string(), toml::Value::String(v.to_string()));
        }
        toml::Value::Table(table)
    }

    #[test]
    fn create_backend_from_config() {
        let config = make_config(&[("id", "s3-test"), ("prefix", "backups")]);
        let backend = create_backend(&config).unwrap();
        assert_eq!(backend.id(), "s3-test");
    }

    #[test]
    fn create_backend_default_id() {
        let config = make_config(&[]);
        let backend = create_backend(&config).unwrap();
        assert_eq!(backend.id(), "s3");
    }

    #[test]
    fn create_backend_missing_required_field() {
        let mut table = toml::map::Map::new();
        table.insert("bucket".to_string(), toml::Value::String("b".to_string()));
        let config = toml::Value::Table(table);
        let err = create_backend(&config).err().unwrap();
        assert!(err.to_string().contains("endpoint"));
    }

    #[test]
    fn s3_key_for_path_without_prefix() {
        // Test key construction directly via the struct.
        let s3 = S3Backend {
            config: S3Config {
                endpoint: "https://s3.amazonaws.com".to_string(),
                bucket: "my-bucket".to_string(),
                region: "us-east-1".to_string(),
                access_key_id: "KEY".to_string(),
                secret_access_key: "SECRET".to_string(),
                prefix: None,
            },
            http: reqwest::Client::new(),
            backend_id: "s3".to_string(),
        };

        assert_eq!(
            s3.key_for_path(Path::new("folder/file.txt")),
            "folder/file.txt"
        );
        assert_eq!(
            s3.key_for_path(Path::new("/folder/file.txt")),
            "folder/file.txt"
        );
        assert_eq!(s3.key_for_path(Path::new("file.txt")), "file.txt");
    }

    #[test]
    fn s3_key_for_path_with_prefix() {
        let s3 = S3Backend {
            config: S3Config {
                endpoint: "https://s3.amazonaws.com".to_string(),
                bucket: "my-bucket".to_string(),
                region: "us-east-1".to_string(),
                access_key_id: "KEY".to_string(),
                secret_access_key: "SECRET".to_string(),
                prefix: Some("backups/2026".to_string()),
            },
            http: reqwest::Client::new(),
            backend_id: "s3".to_string(),
        };

        assert_eq!(
            s3.key_for_path(Path::new("folder/file.txt")),
            "backups/2026/folder/file.txt"
        );
        assert_eq!(
            s3.key_for_path(Path::new("/folder/file.txt")),
            "backups/2026/folder/file.txt"
        );
    }

    #[test]
    fn list_prefix_for_path_root_no_prefix() {
        let s3 = S3Backend {
            config: S3Config {
                endpoint: "https://s3.amazonaws.com".to_string(),
                bucket: "my-bucket".to_string(),
                region: "us-east-1".to_string(),
                access_key_id: "KEY".to_string(),
                secret_access_key: "SECRET".to_string(),
                prefix: None,
            },
            http: reqwest::Client::new(),
            backend_id: "s3".to_string(),
        };

        assert_eq!(s3.list_prefix_for_path(Path::new("/")), "");
        assert_eq!(s3.list_prefix_for_path(Path::new("")), "");
        assert_eq!(s3.list_prefix_for_path(Path::new("folder")), "folder/");
    }

    #[test]
    fn list_prefix_for_path_with_prefix() {
        let s3 = S3Backend {
            config: S3Config {
                endpoint: "https://s3.amazonaws.com".to_string(),
                bucket: "my-bucket".to_string(),
                region: "us-east-1".to_string(),
                access_key_id: "KEY".to_string(),
                secret_access_key: "SECRET".to_string(),
                prefix: Some("backups".to_string()),
            },
            http: reqwest::Client::new(),
            backend_id: "s3".to_string(),
        };

        assert_eq!(s3.list_prefix_for_path(Path::new("/")), "backups/");
        assert_eq!(
            s3.list_prefix_for_path(Path::new("folder")),
            "backups/folder/"
        );
    }

    #[test]
    fn strip_key_prefix_works() {
        let s3 = S3Backend {
            config: S3Config {
                endpoint: "https://s3.amazonaws.com".to_string(),
                bucket: "my-bucket".to_string(),
                region: "us-east-1".to_string(),
                access_key_id: "KEY".to_string(),
                secret_access_key: "SECRET".to_string(),
                prefix: None,
            },
            http: reqwest::Client::new(),
            backend_id: "s3".to_string(),
        };

        assert_eq!(
            s3.strip_key_prefix("folder/file.txt", "folder/"),
            Some("file.txt")
        );
        assert_eq!(s3.strip_key_prefix("other/file.txt", "folder/"), None);
        assert_eq!(s3.strip_key_prefix("file.txt", ""), Some("file.txt"));
    }

    #[test]
    fn parse_list_response_objects() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult>
    <Name>my-bucket</Name>
    <Prefix>backups/</Prefix>
    <KeyCount>2</KeyCount>
    <MaxKeys>1000</MaxKeys>
    <Delimiter>/</Delimiter>
    <IsTruncated>false</IsTruncated>
    <Contents>
        <Key>backups/file1.txt</Key>
        <Size>1234</Size>
        <LastModified>2026-01-01T00:00:00.000Z</LastModified>
    </Contents>
    <CommonPrefixes>
        <Prefix>backups/subdir/</Prefix>
    </CommonPrefixes>
</ListBucketResult>"#;

        let parent_id = ItemId::new("s3", "backups/");
        let (entries, next_token) = parse_list_response(xml, "s3", "backups/", &parent_id).unwrap();

        assert!(next_token.is_none());
        assert_eq!(entries.len(), 2);

        let file = entries.iter().find(|e| !e.is_dir).unwrap();
        assert_eq!(file.name, "file1.txt");
        assert_eq!(file.size, Some(1234));

        let dir = entries.iter().find(|e| e.is_dir).unwrap();
        assert_eq!(dir.name, "subdir");
    }

    #[test]
    fn parse_list_response_pagination() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult>
    <IsTruncated>true</IsTruncated>
    <NextContinuationToken>token-abc</NextContinuationToken>
    <Contents>
        <Key>file.txt</Key>
        <Size>42</Size>
        <LastModified>2026-01-01T00:00:00.000Z</LastModified>
    </Contents>
</ListBucketResult>"#;

        let parent_id = ItemId::new("s3", "/");
        let (entries, next_token) = parse_list_response(xml, "s3", "", &parent_id).unwrap();

        assert_eq!(next_token, Some("token-abc".to_string()));
        assert_eq!(entries.len(), 1);
    }
}
