#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::string_slice
    )
)]
//! S3-compatible backend for Cascade.
//!
//! Supports AWS S3, `MinIO`, Backblaze B2, Cloudflare R2, and any other
//! S3-compatible API. Uses AWS Signature Version 4 signing.
//! No heavy AWS SDK dependency — signing is implemented directly.
//!
//! # Feature flags
//!
//! - `native` (default): uses `reqwest` for HTTP transport.
//! - `portable`: uses the `cascade_engine::portable::HttpClient` trait instead,
//!   allowing the backend to run in environments without `reqwest` (e.g. WASM).
//!   When this feature is active, use `create_backend_with_http_client` instead
//!   of `create_backend`.

pub mod signing;

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use cascade_engine::backend::Backend;
use cascade_engine::types::{Change, Cursor, FileEntry, FileId, ItemId, Quota};
use chrono::{DateTime, Utc};
use signing::{SigningParams, build_canonical_query_string, sha256_hex, sign, uri_encode};

// ── Constants ────────────────────────────────────────────────────────────────

/// Objects at or below this size are uploaded via a single `PutObject` request.
///
/// S3 `PutObject` accepts bodies up to 5 GiB. We use exactly that limit so that
/// any object smaller than 5 GiB takes the fast single-request path.
pub const MULTIPART_THRESHOLD: usize = 5 * 1024 * 1024 * 1024;

/// S3 imposes a maximum of 10,000 parts per multipart upload.
pub const MAX_PARTS: usize = 10_000;

/// Minimum size of any part except the last, as mandated by S3.
///
/// S3 rejects `UploadPart` requests smaller than 5 MiB (except for the final
/// part). Using this as the floor ensures compliance for any object larger than
/// `MULTIPART_THRESHOLD`.
pub const MIN_PART_SIZE: usize = 5 * 1024 * 1024;

/// Compute the part size to use for a multipart upload of `total_bytes`.
///
/// Returns the larger of `MIN_PART_SIZE` and the value needed to keep the part
/// count within `MAX_PARTS` — i.e. `max(ceil(total_bytes / MAX_PARTS),
/// MIN_PART_SIZE)`. For objects that fit into `MAX_PARTS` parts of
/// `MIN_PART_SIZE` each, this returns `MIN_PART_SIZE` directly.
fn compute_part_size(total_bytes: usize) -> usize {
    let min_for_limit = total_bytes.div_ceil(MAX_PARTS);
    min_for_limit.max(MIN_PART_SIZE)
}

// ── Internal response type ───────────────────────────────────────────────────

/// Normalised HTTP response shared between the native (reqwest) and portable
/// (`HttpClient` trait) paths. All other backend code is written against this
/// type, keeping the feature-gated surface as small as possible.
struct S3Response {
    status: u16,
    /// Response headers keyed in lower-case for case-insensitive lookup.
    headers: std::collections::HashMap<String, String>,
    body: Vec<u8>,
}

impl S3Response {
    /// Whether the status is in the 2xx success range.
    const fn is_success(&self) -> bool {
        self.status >= 200 && self.status < 300
    }

    /// Decode the body as a UTF-8 string, substituting the replacement
    /// character for any invalid sequences.
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Look up a header value by name, compared case-insensitively.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

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
    /// HTTP client. Under the `native` feature this is a `reqwest::Client`;
    /// under the `portable` feature it is an `Arc<dyn HttpClient>`.
    #[cfg(not(feature = "portable"))]
    http: reqwest::Client,
    #[cfg(feature = "portable")]
    http: std::sync::Arc<dyn cascade_engine::portable::HttpClient>,
    backend_id: String,
}

// ── Factory functions ────────────────────────────────────────────────────────

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
#[cfg(not(feature = "portable"))]
pub fn create_backend(config: &toml::Value) -> anyhow::Result<Box<dyn Backend>> {
    let s3_config = parse_s3_config(config)?;
    let backend_id = config
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("s3")
        .to_string();
    Ok(Box::new(S3Backend {
        config: s3_config,
        http: reqwest::Client::new(),
        backend_id,
    }))
}

/// Portable stub for `create_backend` — always returns an error.
///
/// When the `portable` feature is active, reqwest is not available and the
/// backend must be constructed with an explicit `HttpClient`. Use
/// [`create_backend_with_http_client`] instead.
#[cfg(feature = "portable")]
pub fn create_backend(_config: &toml::Value) -> anyhow::Result<Box<dyn Backend>> {
    Err(anyhow::anyhow!(
        "the S3 backend's `portable` feature requires an explicit HttpClient — \
         use `create_backend_with_http_client` instead of `create_backend`"
    ))
}

/// Create an S3 backend with an injected HTTP client (portable build).
///
/// Use this function when the `portable` feature is enabled and `reqwest` is not
/// available. The caller supplies a [`cascade_engine::portable::HttpClient`]
/// implementation — for example the `ReqwestClient` adapter under a native
/// integration, or a `fetch`-based adapter in a WASM context.
#[cfg(feature = "portable")]
pub fn create_backend_with_http_client(
    config: &toml::Value,
    http: std::sync::Arc<dyn cascade_engine::portable::HttpClient>,
) -> anyhow::Result<Box<dyn Backend>> {
    let s3_config = parse_s3_config(config)?;
    let backend_id = config
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("s3")
        .to_string();
    Ok(Box::new(S3Backend {
        config: s3_config,
        http,
        backend_id,
    }))
}

/// Extract the common S3 config fields from a TOML value.
fn parse_s3_config(config: &toml::Value) -> anyhow::Result<S3Config> {
    let get_str = |key: &str| -> anyhow::Result<String> {
        config
            .get(key)
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("S3 backend requires '{key}' config"))
    };

    Ok(S3Config {
        endpoint: get_str("endpoint")?,
        bucket: get_str("bucket")?,
        region: get_str("region")?,
        access_key_id: get_str("access_key_id")?,
        secret_access_key: get_str("secret_access_key")?,
        prefix: config
            .get("prefix")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from),
    })
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

    /// Build the common `SigV4` signing parameters for a request, independent
    /// of the HTTP transport layer.
    fn signing_params<'a>(
        &'a self,
        method: &'a str,
        uri_path: &'a str,
        query_params: &'a [(&'a str, &'a str)],
        host: &'a str,
        payload_hash: &'a str,
        now: DateTime<Utc>,
    ) -> SigningParams<'a> {
        SigningParams {
            method,
            uri_path,
            query_params,
            host,
            payload_hash,
            access_key_id: &self.config.access_key_id,
            secret_access_key: &self.config.secret_access_key,
            region: &self.config.region,
            service: "s3",
            now,
        }
    }

    /// Sign and execute an HTTP request using the native `reqwest` transport,
    /// returning a normalised [`S3Response`].
    #[cfg(not(feature = "portable"))]
    async fn signed_request(
        &self,
        method: &str,
        base_url: &str,
        query_params: &[(&str, &str)],
        body: &[u8],
        extra_headers: &[(&str, &str)],
    ) -> anyhow::Result<S3Response> {
        let parsed = url::Url::parse(base_url)
            .map_err(|e| anyhow::anyhow!("invalid URL {base_url}: {e}"))?;

        let uri_path = parsed.path().to_string();
        let payload_hash = sha256_hex(body);
        let host = self.endpoint_host();
        let now = Utc::now();

        let params =
            self.signing_params(method, &uri_path, query_params, &host, &payload_hash, now);
        let signed = sign(&params);

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

        let status = resp.status().as_u16();
        let headers: std::collections::HashMap<String, String> = resp
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|v| (name.as_str().to_lowercase(), v.to_string()))
            })
            .collect();
        let body_bytes = resp.bytes().await?.to_vec();

        Ok(S3Response {
            status,
            headers,
            body: body_bytes,
        })
    }

    /// Sign and execute an HTTP request using the portable `HttpClient` trait,
    /// returning a normalised [`S3Response`].
    #[cfg(feature = "portable")]
    async fn signed_request(
        &self,
        method: &str,
        base_url: &str,
        query_params: &[(&str, &str)],
        body: &[u8],
        extra_headers: &[(&str, &str)],
    ) -> anyhow::Result<S3Response> {
        use cascade_engine::portable::HeaderMap;

        let parsed = url::Url::parse(base_url)
            .map_err(|e| anyhow::anyhow!("invalid URL {base_url}: {e}"))?;

        let uri_path = parsed.path().to_string();
        let payload_hash = sha256_hex(body);
        let host = self.endpoint_host();
        let now = Utc::now();

        let params =
            self.signing_params(method, &uri_path, query_params, &host, &payload_hash, now);
        let signed = sign(&params);

        let request_url = if query_params.is_empty() {
            base_url.to_string()
        } else {
            let qs = build_canonical_query_string(query_params);
            format!("{base_url}?{qs}")
        };

        let mut headers = HeaderMap::new();
        headers.insert("x-amz-date", signed.x_amz_date.as_str());
        headers.insert("x-amz-content-sha256", signed.x_amz_content_sha256.as_str());
        headers.insert("authorization", signed.authorization.as_str());
        headers.insert("host", host.as_str());
        for (name, value) in extra_headers {
            headers.insert(*name, *value);
        }

        let body_vec = body.to_vec();
        let http_resp = match method {
            "GET" => self
                .http
                .get(&request_url, headers)
                .await
                .map_err(|e| anyhow::anyhow!("S3 GET failed: {e}"))?,
            "PUT" => self
                .http
                .put(&request_url, headers, body_vec)
                .await
                .map_err(|e| anyhow::anyhow!("S3 PUT failed: {e}"))?,
            "POST" => self
                .http
                .post(&request_url, headers, body_vec)
                .await
                .map_err(|e| anyhow::anyhow!("S3 POST failed: {e}"))?,
            "DELETE" => self
                .http
                .delete(&request_url, headers)
                .await
                .map_err(|e| anyhow::anyhow!("S3 DELETE failed: {e}"))?,
            "HEAD" => self
                .http
                .head(&request_url, headers)
                .await
                .map_err(|e| anyhow::anyhow!("S3 HEAD failed: {e}"))?,
            other => anyhow::bail!("unsupported HTTP method for S3 portable backend: {other}"),
        };

        let headers_map: std::collections::HashMap<String, String> = http_resp
            .headers
            .as_pairs()
            .iter()
            .map(|(k, v)| (k.to_lowercase(), v.clone()))
            .collect();

        Ok(S3Response {
            status: http_resp.status,
            headers: headers_map,
            body: http_resp.body,
        })
    }

    /// Return an error if `resp` does not have a 2xx status code. The error
    /// message includes the status code and the response body.
    fn check_response(resp: S3Response) -> anyhow::Result<S3Response> {
        if resp.is_success() {
            return Ok(resp);
        }
        let body = resp.text();
        anyhow::bail!("S3 error {}: {body}", resp.status);
    }

    /// Upload `data` to S3 key `key` using the multipart upload API.
    ///
    /// Splits `data` into parts sized by [`compute_part_size`], issues
    /// `CreateMultipartUpload`, then one `UploadPart` per chunk, then
    /// `CompleteMultipartUpload`. Calls `AbortMultipartUpload` if any step
    /// fails so the partial upload does not leak storage.
    async fn multipart_upload(&self, key: &str, data: &[u8]) -> anyhow::Result<()> {
        let url = self.object_url(key);

        // ── 1. CreateMultipartUpload ──────────────────────────────────────────
        let create_resp = self
            .signed_request("POST", &url, &[("uploads", "")], &[], &[])
            .await?;
        let create_resp = Self::check_response(create_resp)?;
        let create_xml = create_resp.text();
        let upload_id = parse_upload_id(&create_xml)?;

        tracing::debug!(key, upload_id = %upload_id, "created multipart upload");

        // Abort helper — called on any failure after the upload is created.
        let abort = |upload_id: String| {
            let this = self;
            let key = key.to_owned();
            async move {
                let abort_url = this.object_url(&key);
                match this
                    .signed_request(
                        "DELETE",
                        &abort_url,
                        &[("uploadId", upload_id.as_str())],
                        &[],
                        &[],
                    )
                    .await
                {
                    Ok(r) => {
                        if !r.is_success() {
                            tracing::warn!(
                                key,
                                upload_id = %upload_id,
                                "AbortMultipartUpload returned non-2xx"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(key, upload_id = %upload_id, error = %e, "AbortMultipartUpload failed");
                    }
                }
            }
        };

        // ── 2. UploadPart ─────────────────────────────────────────────────────
        let part_size = compute_part_size(data.len());
        let mut completed_parts: Vec<(usize, String)> = Vec::new();

        for (index, chunk) in data.chunks(part_size).enumerate() {
            let part_number = index + 1;
            let part_number_str = part_number.to_string();

            let part_resp = self
                .signed_request(
                    "PUT",
                    &url,
                    &[
                        ("partNumber", part_number_str.as_str()),
                        ("uploadId", upload_id.as_str()),
                    ],
                    chunk,
                    &[("content-length", &chunk.len().to_string())],
                )
                .await;

            let part_resp = match part_resp {
                Ok(r) => r,
                Err(e) => {
                    abort(upload_id).await;
                    return Err(e);
                }
            };

            let part_resp = match Self::check_response(part_resp) {
                Ok(r) => r,
                Err(e) => {
                    abort(upload_id).await;
                    return Err(e);
                }
            };

            let Some(etag) = part_resp.header("etag").map(String::from) else {
                abort(upload_id).await;
                return Err(anyhow::anyhow!("UploadPart response missing ETag header"));
            };

            tracing::debug!(key, part_number, etag = %etag, "uploaded part");

            completed_parts.push((part_number, etag));
        }

        // ── 3. CompleteMultipartUpload ────────────────────────────────────────
        let complete_body = build_complete_xml(&completed_parts);
        let complete_bytes = complete_body.as_bytes();
        let content_length = complete_bytes.len().to_string();

        let complete_resp = self
            .signed_request(
                "POST",
                &url,
                &[("uploadId", upload_id.as_str())],
                complete_bytes,
                &[
                    ("content-length", &content_length),
                    ("content-type", "application/xml"),
                ],
            )
            .await;

        let complete_resp = match complete_resp {
            Ok(r) => r,
            Err(e) => {
                abort(upload_id).await;
                return Err(e);
            }
        };

        let complete_resp = match Self::check_response(complete_resp) {
            Ok(r) => r,
            Err(e) => {
                abort(upload_id).await;
                return Err(e);
            }
        };

        // AWS S3 may return HTTP 200 with an `<Error>` element in the body when
        // it encounters a server-side error after it has begun streaming the
        // response. `check_response` only inspects the status code, so we must
        // inspect the body too.
        let complete_text = complete_resp.text();
        if let Some(code) = xml_text(&complete_text, "Code") {
            abort(upload_id).await;
            return Err(anyhow::anyhow!(
                "CompleteMultipartUpload returned 200 with error: {code}"
            ));
        }

        tracing::debug!(
            key,
            parts = completed_parts.len(),
            "completed multipart upload"
        );
        Ok(())
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
/// entries. Returns `(entries, next_continuation_token)`.
fn parse_list_response(
    xml: &str,
    backend_id: &str,
    list_prefix: &str,
    parent_id: &ItemId,
) -> anyhow::Result<(Vec<FileEntry>, Option<String>)> {
    let mut entries = Vec::new();

    for_each_xml_block(xml, "Contents", |block| {
        let key = xml_text(block, "Key")
            .ok_or_else(|| anyhow::anyhow!("ListObjectsV2 <Contents> missing <Key>"))?;

        if !key.ends_with('/') {
            let size: u64 = xml_text(block, "Size")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);

            let last_modified = xml_text(block, "LastModified")
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Utc));

            let name = key
                .strip_prefix(list_prefix)
                .unwrap_or(key)
                .trim_end_matches('/');

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

    for_each_xml_block(xml, "CommonPrefixes", |block| {
        let prefix = xml_text(block, "Prefix")
            .ok_or_else(|| anyhow::anyhow!("ListObjectsV2 <CommonPrefixes> missing <Prefix>"))?;

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

    let next_token = xml_text(xml, "NextContinuationToken").map(String::from);

    Ok((entries, next_token))
}

/// Parse the XML body of a flat (no-delimiter) `ListObjectsV2` response.
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

        if !key.ends_with('/') {
            let size: u64 = xml_text(block, "Size")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);

            let last_modified = xml_text(block, "LastModified")
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Utc));

            let relative = key.strip_prefix(prefix).unwrap_or(key);

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

/// Parse the `<UploadId>` from a `CreateMultipartUpload` XML response body.
fn parse_upload_id(xml: &str) -> anyhow::Result<String> {
    xml_text(xml, "UploadId")
        .map(String::from)
        .ok_or_else(|| anyhow::anyhow!("CreateMultipartUpload response missing <UploadId>"))
}

/// Build the XML body for a `CompleteMultipartUpload` request.
fn build_complete_xml(parts: &[(usize, String)]) -> String {
    let mut xml = String::from("<CompleteMultipartUpload>");
    for (number, etag) in parts {
        xml.push_str("<Part><PartNumber>");
        xml.push_str(&number.to_string());
        xml.push_str("</PartNumber><ETag>");
        xml.push_str(etag);
        xml.push_str("</ETag></Part>");
    }
    xml.push_str("</CompleteMultipartUpload>");
    xml
}

/// Parse a `HeadObject` (or any response carrying metadata headers) into a
/// `FileEntry`. Accepts the normalised [`S3Response`] so the same parser works
/// for both native and portable paths.
fn parse_head_response(
    response: &S3Response,
    key: &str,
    name: &str,
    parent_id: &ItemId,
    backend_id: &str,
) -> FileEntry {
    let size: u64 = response
        .header("content-length")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let last_modified = response
        .header("last-modified")
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
        Ok(None)
    }

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
        let resp = Self::check_response(resp)?;

        Ok(parse_head_response(
            &resp,
            &key,
            &name,
            &parent_id,
            &self.backend_id,
        ))
    }

    async fn download(&self, file: &FileEntry) -> anyhow::Result<Vec<u8>> {
        let key = file.id.native_id();
        let url = self.object_url(key);

        let resp = self.signed_request("GET", &url, &[], &[], &[]).await?;
        let resp = Self::check_response(resp)?;

        tracing::debug!(
            file = %file.id,
            size = resp.body.len(),
            "downloaded from S3"
        );
        Ok(resp.body)
    }

    async fn read_range(
        &self,
        file: &FileEntry,
        offset: u64,
        length: u32,
    ) -> anyhow::Result<Vec<u8>> {
        if length == 0 {
            return Ok(Vec::new());
        }

        let key = file.id.native_id();
        let url = self.object_url(key);

        let end = offset.saturating_add(u64::from(length)).saturating_sub(1);
        let range_value = format!("bytes={offset}-{end}");

        let resp = self
            .signed_request("GET", &url, &[], &[], &[("range", &range_value)])
            .await?;

        // 416: the requested range starts at or past the end of the object.
        if resp.status == 416 {
            return Ok(Vec::new());
        }

        let server_honoured_range = resp.status == 206;
        let resp = Self::check_response(resp)?;
        let bytes = resp.body;

        let out = if server_honoured_range {
            bytes
        } else {
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(bytes.len());
            let len = usize::try_from(length).unwrap_or(usize::MAX);
            let slice_end = start.saturating_add(len).min(bytes.len());
            bytes.get(start..slice_end).unwrap_or_default().to_vec()
        };

        tracing::debug!(
            file = %file.id,
            offset,
            length,
            returned = out.len(),
            honoured_range = server_honoured_range,
            "read range from S3"
        );

        Ok(out)
    }

    async fn upload(
        &self,
        path: &Path,
        data: &[u8],
        _parent_id: &FileId,
    ) -> anyhow::Result<FileEntry> {
        let key = self.key_for_path(path);
        let url = self.object_url(&key);

        if data.len() > MULTIPART_THRESHOLD {
            self.multipart_upload(&key, data).await?;
        } else {
            let content_length = data.len().to_string();
            let resp = self
                .signed_request(
                    "PUT",
                    &url,
                    &[],
                    data,
                    &[("content-length", &content_length)],
                )
                .await?;
            Self::check_response(resp)?;
        }

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

    async fn update(&self, file_id: &FileId, data: &[u8]) -> anyhow::Result<FileEntry> {
        let key = file_id.native_id();
        let url = self.object_url(key);

        if data.len() > MULTIPART_THRESHOLD {
            self.multipart_upload(key, data).await?;
        } else {
            let content_length = data.len().to_string();
            let resp = self
                .signed_request(
                    "PUT",
                    &url,
                    &[],
                    data,
                    &[("content-length", &content_length)],
                )
                .await?;
            Self::check_response(resp)?;
        }

        let size = u64::try_from(data.len()).unwrap_or(u64::MAX);
        let parent_key = key
            .rsplit_once('/')
            .map_or_else(String::new, |(prefix, _)| prefix.to_owned());
        let parent_id = ItemId::new(&self.backend_id, &parent_key);
        let name = key.rsplit('/').next().unwrap_or(key).to_string();

        Ok(self.object_entry(key, &name, size, Some(Utc::now()), &parent_id))
    }

    async fn create_dir(&self, path: &Path) -> anyhow::Result<FileEntry> {
        let key = format!("{}/", self.key_for_path(path));
        let url = self.object_url(&key);

        let resp = self
            .signed_request("PUT", &url, &[], &[], &[("content-length", "0")])
            .await?;
        Self::check_response(resp)?;

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
        Self::check_response(resp)?;

        tracing::debug!(file = %file.id, "deleted from S3");
        Ok(())
    }

    async fn move_entry(&self, src: &Path, dst: &Path) -> anyhow::Result<FileEntry> {
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
        Self::check_response(resp)?;

        let src_url = self.object_url(&src_key);
        let del_resp = self
            .signed_request("DELETE", &src_url, &[], &[], &[])
            .await?;
        Self::check_response(del_resp)?;

        let name = dst
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_else(|| dst.to_str().unwrap_or(""))
            .to_string();

        let parent_path = dst.parent().unwrap_or_else(|| Path::new("/"));
        let parent_key = self.key_for_path(parent_path);
        let parent_id = ItemId::new(&self.backend_id, &parent_key);

        let head_resp = self.signed_request("HEAD", &dst_url, &[], &[], &[]).await?;
        let head_resp = Self::check_response(head_resp)?;
        let dst_entry =
            parse_head_response(&head_resp, &dst_key, &name, &parent_id, &self.backend_id);

        tracing::debug!(
            src = %src.display(),
            dst = %dst.display(),
            "moved in S3"
        );

        Ok(dst_entry)
    }

    async fn poll_interval(&self) -> Option<Duration> {
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
            let resp = Self::check_response(resp)?;
            let xml = resp.text();

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
            let resp = Self::check_response(resp)?;
            let xml = resp.text();

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
    #[cfg(test)]
    fn strip_key_prefix<'a>(key: &'a str, list_prefix: &str) -> Option<&'a str> {
        key.strip_prefix(list_prefix)
    }
}

// ── Test helpers ──────────────────────────────────────────────────────────────

#[cfg(all(feature = "testing", not(feature = "portable")))]
impl S3Backend {
    /// Construct an `S3Backend` directly from parts (native path).
    #[must_use]
    pub fn new_for_test(
        endpoint: String,
        bucket: &str,
        region: &str,
        access_key_id: &str,
        secret_access_key: &str,
    ) -> Self {
        Self {
            config: S3Config {
                endpoint,
                bucket: bucket.to_string(),
                region: region.to_string(),
                access_key_id: access_key_id.to_string(),
                secret_access_key: secret_access_key.to_string(),
                prefix: None,
            },
            http: reqwest::Client::new(),
            backend_id: "s3".to_string(),
        }
    }

    /// Drive the multipart upload path directly.
    pub async fn multipart_upload_pub(&self, key: &str, data: &[u8]) -> anyhow::Result<()> {
        self.multipart_upload(key, data).await
    }
}

#[cfg(all(feature = "testing", feature = "portable"))]
impl S3Backend {
    /// Construct an `S3Backend` directly from parts (portable path).
    #[must_use]
    pub fn new_for_test(
        endpoint: String,
        bucket: &str,
        region: &str,
        access_key_id: &str,
        secret_access_key: &str,
        http: std::sync::Arc<dyn cascade_engine::portable::HttpClient>,
    ) -> Self {
        Self {
            config: S3Config {
                endpoint,
                bucket: bucket.to_string(),
                region: region.to_string(),
                access_key_id: access_key_id.to_string(),
                secret_access_key: secret_access_key.to_string(),
                prefix: None,
            },
            http,
            backend_id: "s3".to_string(),
        }
    }

    /// Drive the multipart upload path directly.
    pub async fn multipart_upload_pub(&self, key: &str, data: &[u8]) -> anyhow::Result<()> {
        self.multipart_upload(key, data).await
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

    /// Construct a minimal `S3Backend` for unit tests that exercise pure logic
    /// (key construction, prefix handling, XML parsing) without making network
    /// calls. Gated to the `native` feature because the struct requires a
    /// concrete HTTP client to be initialised.
    #[cfg(not(feature = "portable"))]
    fn make_backend(prefix: Option<&str>) -> S3Backend {
        S3Backend {
            config: S3Config {
                endpoint: "https://s3.amazonaws.com".to_string(),
                bucket: "my-bucket".to_string(),
                region: "us-east-1".to_string(),
                access_key_id: "KEY".to_string(),
                secret_access_key: "SECRET".to_string(),
                prefix: prefix.map(String::from),
            },
            http: reqwest::Client::new(),
            backend_id: "s3".to_string(),
        }
    }

    #[test]
    #[cfg(not(feature = "portable"))]
    fn create_backend_from_config() {
        let config = make_config(&[("id", "s3-test"), ("prefix", "backups")]);
        let backend = create_backend(&config).unwrap();
        assert_eq!(backend.id(), "s3-test");
    }

    #[test]
    #[cfg(not(feature = "portable"))]
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
        // Under portable the function returns a different error, but it still
        // errors — which is what this test verifies.
        let err = create_backend(&config).err().unwrap();
        // portable: "requires an explicit HttpClient" / native: "requires 'endpoint'"
        assert!(!err.to_string().is_empty());
    }

    #[test]
    #[cfg(not(feature = "portable"))]
    fn s3_key_for_path_without_prefix() {
        let s3 = make_backend(None);
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
    #[cfg(not(feature = "portable"))]
    fn s3_key_for_path_with_prefix() {
        let s3 = make_backend(Some("backups/2026"));
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
    #[cfg(not(feature = "portable"))]
    fn list_prefix_for_path_root_no_prefix() {
        let s3 = make_backend(None);
        assert_eq!(s3.list_prefix_for_path(Path::new("/")), "");
        assert_eq!(s3.list_prefix_for_path(Path::new("")), "");
        assert_eq!(s3.list_prefix_for_path(Path::new("folder")), "folder/");
    }

    #[test]
    #[cfg(not(feature = "portable"))]
    fn list_prefix_for_path_with_prefix() {
        let s3 = make_backend(Some("backups"));
        assert_eq!(s3.list_prefix_for_path(Path::new("/")), "backups/");
        assert_eq!(
            s3.list_prefix_for_path(Path::new("folder")),
            "backups/folder/"
        );
    }

    #[test]
    fn strip_key_prefix_works() {
        assert_eq!(
            S3Backend::strip_key_prefix("folder/file.txt", "folder/"),
            Some("file.txt")
        );
        assert_eq!(
            S3Backend::strip_key_prefix("other/file.txt", "folder/"),
            None
        );
        assert_eq!(
            S3Backend::strip_key_prefix("file.txt", ""),
            Some("file.txt")
        );
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
