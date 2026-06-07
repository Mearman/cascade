//! HTTP client, rate limiting, retry for Google Drive API.

// `OnceLock` backs the native-only diagnostic-mode global; under the `portable`
// feature there is no per-request client selector, so the import is gated to
// match its users and avoid an unused-import warning.
#[cfg(not(feature = "portable"))]
use std::sync::OnceLock;
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

// ── Native-only helpers ───────────────────────────────────────────────────────

/// Build the per-request `reqwest` client that the Drive TLS deadlock
/// workaround mandates: connection pooling disabled and HTTP/1.1 forced.
///
/// Both the Drive API request path and the `OAuth2` token-refresh path build
/// their clients here so the workaround configuration lives in exactly one
/// place. See the `DriveClient::http` rationale below for the full background.
/// The builder error is propagated rather than swallowed — a fallback to the
/// default client would silently re-enable connection pooling.
///
/// `pool_max_idle_per_host(0)` is the load-bearing mitigation: it stops a stale
/// idle connection ever being reused. `http1_only()` is currently redundant —
/// the workspace `reqwest` is built with `default-features = false` and no
/// `http2` feature (since the first commit), so the client cannot negotiate
/// HTTP/2 regardless. It is kept as a guard should that feature ever be enabled.
/// The deadlock itself is HTTP/1.1; a faithful local reproduction never
/// triggered it (see `crates/presenter-webdav/tests/tls_topology_repro.rs`), so
/// the trigger lives at the real Drive endpoint and the workaround stays.
#[cfg(not(feature = "portable"))]
pub fn build_unpooled_http1_client(timeout: Duration) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(timeout)
        .pool_max_idle_per_host(0)
        .http1_only()
        .build()
}

/// HTTP-client mode selector, controlled at runtime by `CASCADE_GDRIVE_HTTP_DIAG`.
///
/// The env var is read once (at first use) and determines which `reqwest::Client`
/// is built for every Drive API call and `OAuth2` refresh.
///
/// Recognised values:
/// - absent / `"pooled-shared"` (**default**): the daemon injects a single
///   long-lived, pooled `reqwest::Client` shared across all Drive requests and
///   `OAuth2` refreshes, and the `WebDAV` presenter's `run_isolated_blocking` is
///   disabled so the shared connection driver stays polled on the daemon's
///   stable main runtime. This was the original TLS-deadlock workaround's target
///   architecture; it became the default after the authenticated-Drive capture
///   in `docs/tls-deadlock-capture.md` passed.
/// - `"unpooled-legacy"` / `"unpooled-http1"`: the **escape hatch** — the
///   previous workaround. A fresh, unpooled, HTTP/1.1 client is built per
///   request and `run_isolated_blocking` stays on. Set this if the shared
///   pooled client ever wedges in production; it reverts the behaviour instantly
///   without a rebuild.
/// - `"pooled"`: connection pooling re-enabled on a per-request client, HTTP/1.1
///   forced. Diagnostic only — the suspected-bad pre-workaround shape.
/// - `"pooled-http2"`: like `"pooled"` but `http1_only()` dropped. Identical in
///   practice — the workspace `reqwest` is built without the `http2` feature, so
///   HTTP/2 cannot be negotiated. Kept for if/when that feature is enabled.
///
/// Any unrecognised value is treated as the default (`PooledShared`).
#[cfg(not(feature = "portable"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagHttpMode {
    /// Escape hatch (former production workaround): no pooling, HTTP/1.1 only,
    /// fresh client per request, `run_isolated_blocking` on. Selected by
    /// `CASCADE_GDRIVE_HTTP_DIAG=unpooled-legacy`.
    UnpooledHttp1,
    /// Diagnostic only: pooling re-enabled on a per-request client, HTTP/1.1 forced.
    Pooled,
    /// Diagnostic only: pooling re-enabled and `http1_only()` dropped. Cannot
    /// actually negotiate HTTP/2 (reqwest is built without the `http2`
    /// feature), so it behaves like [`Pooled`](Self::Pooled) today.
    PooledHttp2,
    /// **Default.** The daemon injects a single long-lived pooled
    /// `reqwest::Client` shared across all Drive requests and `OAuth2` refreshes,
    /// and the `WebDAV` presenter's `run_isolated_blocking` is disabled so the
    /// shared connection driver stays polled on the daemon's main runtime.
    PooledShared,
}

/// Global diagnostic mode, read once from `CASCADE_GDRIVE_HTTP_DIAG` on first call.
#[cfg(not(feature = "portable"))]
static DIAG_HTTP_MODE: OnceLock<DiagHttpMode> = OnceLock::new();

/// Map a `CASCADE_GDRIVE_HTTP_DIAG` value to a mode. Pure — no logging, no
/// global state — so the env→mode policy (including the default) is unit-tested.
/// Any unrecognised value resolves to the default, [`DiagHttpMode::PooledShared`].
#[cfg(not(feature = "portable"))]
fn parse_diag_mode(value: &str) -> DiagHttpMode {
    match value {
        "unpooled-legacy" | "unpooled-http1" => DiagHttpMode::UnpooledHttp1,
        "pooled" => DiagHttpMode::Pooled,
        "pooled-http2" => DiagHttpMode::PooledHttp2,
        // Absent, "pooled-shared", or any unrecognised value: the default.
        _ => DiagHttpMode::PooledShared,
    }
}

/// Read the mode from the environment, initialising the global on first call.
///
/// This is the single source of truth for the mode. Both the gdrive injection
/// decision (in `crates/cascade/src/cli/mount.rs`) and the `WebDAV` isolation
/// decision (`WebDavServer::start`'s `skip_isolation` parameter) must derive
/// their mode from this accessor — never by re-parsing the env var — so the two
/// halves can never disagree.
#[cfg(not(feature = "portable"))]
pub fn diag_http_mode() -> DiagHttpMode {
    *DIAG_HTTP_MODE.get_or_init(|| {
        let mode = parse_diag_mode(
            std::env::var("CASCADE_GDRIVE_HTTP_DIAG")
                .as_deref()
                .unwrap_or(""),
        );
        match mode {
            DiagHttpMode::UnpooledHttp1 => tracing::warn!(
                "CASCADE_GDRIVE_HTTP_DIAG=unpooled-legacy: reverting to the legacy per-request \
                 unpooled HTTP/1.1 client and the WebDAV run_isolated_blocking workaround. This \
                 is the escape hatch from the default shared pooled client; use it only if the \
                 shared client wedges."
            ),
            DiagHttpMode::Pooled => tracing::warn!(
                "CASCADE_GDRIVE_HTTP_DIAG=pooled: per-request pooled connections. This is the \
                 suspected-bad pre-workaround shape. Diagnostic use only."
            ),
            DiagHttpMode::PooledHttp2 => tracing::warn!(
                "CASCADE_GDRIVE_HTTP_DIAG=pooled-http2: per-request pooled connections with \
                 http1_only dropped. HTTP/2 cannot actually be negotiated (no http2 feature), so \
                 this behaves like 'pooled'. Diagnostic use only."
            ),
            // The default — a single shared pooled client on the daemon runtime.
            DiagHttpMode::PooledShared => {}
        }
        mode
    })
}

/// Build a per-request `reqwest` client for a `DriveClient` that has no injected
/// shared client (`http_client == None`).
///
/// The daemon, in the default `PooledShared` mode, injects a shared client into
/// every `DriveClient` it builds, so its Drive calls never reach here. This
/// function serves the non-injecting callers (standalone/CLI Drive calls, tests):
/// - `UnpooledHttp1` (the escape hatch) — the per-request unpooled HTTP/1.1
///   workaround client.
/// - `PooledShared` (the default) — falls back to the same safe unpooled
///   per-request client. A non-injecting caller has no shared driver to strand,
///   so the per-request workaround is correct here; a debug line records that
///   the shared client was not injected for this `DriveClient`.
/// - `Pooled` / `PooledHttp2` — diagnostic per-request pooled clients.
///
/// This is the single call site that must replace every direct call to
/// `reqwest::Client::builder()` inside this crate's native path.
#[cfg(not(feature = "portable"))]
pub fn build_diag_http_client(timeout: Duration) -> anyhow::Result<reqwest::Client> {
    match diag_http_mode() {
        DiagHttpMode::UnpooledHttp1 => Ok(build_unpooled_http1_client(timeout)?),
        DiagHttpMode::Pooled => Ok(reqwest::Client::builder()
            .timeout(timeout)
            .http1_only()
            .build()?),
        DiagHttpMode::PooledHttp2 => Ok(reqwest::Client::builder().timeout(timeout).build()?),
        DiagHttpMode::PooledShared => {
            // A DriveClient with no injected shared client reached the per-request
            // path while pooled-shared is the default. Fall back to the safe
            // unpooled workaround client (one fresh connection, driver polled on
            // the calling runtime for the single request — no stranding). The
            // daemon's injected DriveClients never hit this branch.
            tracing::debug!(
                "pooled-shared: a DriveClient without an injected shared client built a \
                 per-request unpooled client (expected for standalone/CLI Drive calls)"
            );
            Ok(build_unpooled_http1_client(timeout)?)
        }
    }
}

// ── DriveClient struct ────────────────────────────────────────────────────────

/// Google Drive API HTTP client.
pub struct DriveClient {
    rate_limiter: RateLimiter,
    pub(crate) base_url: String,
    pub(crate) upload_url: String,
    /// Injected HTTP client.
    ///
    /// Under the `portable` feature this is always `Some` — the backend requires
    /// an explicit client because `reqwest` is unavailable.
    ///
    /// Under the `native` feature this is `Some` only in `pooled-shared` mode,
    /// where a single daemon-owned pooled `reqwest::Client` is injected at
    /// startup. When `None`, the existing per-request `build_diag_http_client`
    /// path runs unchanged (the TLS deadlock workaround).
    #[cfg(feature = "portable")]
    http: std::sync::Arc<dyn cascade_engine::portable::HttpClient>,
    /// Optional injected HTTP client for native builds (pooled-shared mode only).
    ///
    /// `None` → default per-request `build_diag_http_client` path (unchanged).
    /// `Some` → injected shared client; per-request builder is bypassed entirely.
    #[cfg(not(feature = "portable"))]
    http_client: Option<std::sync::Arc<dyn cascade_engine::portable::HttpClient>>,
    /// Monotonically increasing request sequence number used in tracing spans
    /// to correlate a wedged request (open span, `before-send` event, no
    /// `after-headers` event) with its URL and method.
    #[cfg(not(feature = "portable"))]
    request_seq: std::sync::atomic::AtomicU64,
}

impl std::fmt::Debug for DriveClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriveClient")
            .field("base_url", &self.base_url)
            .field("upload_url", &self.upload_url)
            .finish_non_exhaustive()
    }
}

// ── Native constructors and HTTP helpers ──────────────────────────────────────

#[cfg(not(feature = "portable"))]
impl DriveClient {
    #[must_use]
    pub fn new() -> Self {
        Self::with_urls(
            "https://www.googleapis.com/drive/v3".to_string(),
            "https://www.googleapis.com/upload/drive/v3".to_string(),
        )
    }

    /// Construct a client with custom base URLs — used in integration tests.
    #[must_use]
    pub fn with_urls(base_url: String, upload_url: String) -> Self {
        Self {
            rate_limiter: RateLimiter::new(10_000),
            base_url,
            upload_url,
            http_client: None,
            request_seq: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Construct a client pointing at the production Google Drive API, with an
    /// injected shared HTTP client (pooled-shared mode only).
    ///
    /// When the daemon is started with `CASCADE_GDRIVE_HTTP_DIAG=pooled-shared`
    /// it builds a single long-lived pooled `reqwest::Client`, wraps it in
    /// `cascade_engine::portable::native::ReqwestClient::from_client`, and
    /// injects it here via this constructor. All Drive API calls and `OAuth2`
    /// refreshes route through the shared client instead of building a fresh
    /// per-request client.
    #[must_use]
    pub fn new_with_http_client_native(
        http: std::sync::Arc<dyn cascade_engine::portable::HttpClient>,
    ) -> Self {
        Self::with_urls_and_http_client_native(
            "https://www.googleapis.com/drive/v3".to_string(),
            "https://www.googleapis.com/upload/drive/v3".to_string(),
            http,
        )
    }

    /// Construct a client with custom base URLs and an injected shared HTTP
    /// client (pooled-shared mode / integration tests).
    #[must_use]
    pub fn with_urls_and_http_client_native(
        base_url: String,
        upload_url: String,
        http: std::sync::Arc<dyn cascade_engine::portable::HttpClient>,
    ) -> Self {
        Self {
            rate_limiter: RateLimiter::new(10_000),
            base_url,
            upload_url,
            http_client: Some(http),
            request_seq: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Build the per-request `reqwest` client (default path only).
    ///
    /// By default this produces the production-safe workaround client: pooling
    /// disabled, HTTP/1.1 forced (`build_unpooled_http1_client`). Set the
    /// environment variable `CASCADE_GDRIVE_HTTP_DIAG` to `"pooled"` or
    /// `"pooled-http2"` to opt into a diagnostic mode that re-enables
    /// pooling/HTTP2 against the real Drive endpoint.
    ///
    /// A fresh client per request with connection pooling disabled and
    /// HTTP/1.1 only is a deliberate workaround for a hang first seen on the
    /// `WebDAV` write path, not an oversight. **Do not** restore pooling or
    /// HTTP/2 here without a confirmed root cause and a passing reproduction —
    /// the standing rule is recorded in `docs/design.md` ("Google Drive TLS
    /// deadlock workaround").
    ///
    /// The builder error is propagated, never swallowed; falling back to the
    /// default client would silently re-enable pooling and HTTP/2.
    ///
    /// **Never call this in pooled-shared mode.** In that mode the injected
    /// `http_client` field is `Some` and the caller must use it instead.
    fn build_per_request_client() -> anyhow::Result<reqwest::Client> {
        const DRIVE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
        build_diag_http_client(DRIVE_REQUEST_TIMEOUT)
    }

    /// Issue an authenticated GET and return the response, rate-limited.
    /// Returns `Err` for any 4xx/5xx status.
    ///
    /// Dispatches through the injected shared client when `self.http_client` is
    /// `Some` (pooled-shared mode), or builds a fresh per-request client
    /// otherwise (the default TLS deadlock workaround).
    pub(crate) async fn authenticated_get(
        &self,
        path: &str,
        token: &str,
        query: &[(&str, &str)],
    ) -> anyhow::Result<DriveHttpResponse> {
        use cascade_engine::portable::HeaderMap;

        self.rate_limiter.acquire().await;
        let url = build_query_url(&format!("{}/{path}", self.base_url), query);

        if let Some(http) = &self.http_client {
            // Pooled-shared mode: route through the injected shared client.
            let mut headers = HeaderMap::new();
            headers.insert("authorization", format!("Bearer {token}").as_str());
            let resp = http
                .get(&url, headers)
                .await
                .map_err(|e| anyhow::anyhow!("Drive GET failed: {e}"))?;
            if resp.status >= 400 {
                let body_str = String::from_utf8_lossy(&resp.body).into_owned();
                return Err(drive_api_error("Drive API error", resp.status, body_str));
            }
            return Ok(DriveHttpResponse {
                status: resp.status,
                body: resp.body,
            });
        }

        // Default path: per-request client (TLS deadlock workaround).
        let seq = self.request_seq.fetch_add(1, Ordering::Relaxed);
        // Log before-send and drop the span guard before `.await` — `EnteredSpan`
        // is not `Send` and must not be held across async yield points.
        tracing::debug_span!("drive_request", method = "GET", %url, seq)
            .in_scope(|| tracing::debug!(seq, method = "GET", %url, "before-send"));
        let resp = Self::build_per_request_client()?
            .get(&url)
            .bearer_auth(token)
            .send()
            .await?;
        tracing::debug!(seq, method = "GET", %url, "after-headers");

        let status = resp.status().as_u16();
        let body = resp.bytes().await?.to_vec();

        if status >= 400 {
            let body_str = String::from_utf8_lossy(&body).into_owned();
            return Err(drive_api_error("Drive API error", status, body_str));
        }
        Ok(DriveHttpResponse { status, body })
    }

    /// Issue a request with a body (POST, PATCH, PUT) and return the response.
    /// `url` is the full request URL including any embedded query parameters;
    /// `extra_query` is appended via `build_query_url`.
    ///
    /// Dispatches through the injected shared client when `self.http_client` is
    /// `Some` (pooled-shared mode), or builds a fresh per-request client
    /// otherwise (the default TLS deadlock workaround).
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

        if let Some(http) = &self.http_client {
            // Pooled-shared mode: route through the injected shared client.
            let mut headers = HeaderMap::new();
            headers.insert("authorization", format!("Bearer {token}").as_str());
            headers.insert("content-type", content_type);
            let resp = match method {
                "POST" => http
                    .post(&full_url, headers, body)
                    .await
                    .map_err(|e| anyhow::anyhow!("Drive POST failed: {e}"))?,
                "PATCH" => http
                    .patch(&full_url, headers, body)
                    .await
                    .map_err(|e| anyhow::anyhow!("Drive PATCH failed: {e}"))?,
                "PUT" => http
                    .put(&full_url, headers, body)
                    .await
                    .map_err(|e| anyhow::anyhow!("Drive PUT failed: {e}"))?,
                other => anyhow::bail!(
                    "unsupported write method for Drive native (injected) backend: {other}"
                ),
            };
            if resp.status >= 400 {
                let body_str = String::from_utf8_lossy(&resp.body).into_owned();
                return Err(drive_api_error("Drive API error", resp.status, body_str));
            }
            return Ok(DriveHttpResponse {
                status: resp.status,
                body: resp.body,
            });
        }

        // Default path: per-request client (TLS deadlock workaround).
        let seq = self.request_seq.fetch_add(1, Ordering::Relaxed);
        // Log before-send and drop the span guard before `.await` — `EnteredSpan`
        // is not `Send` and must not be held across async yield points.
        tracing::debug_span!("drive_request", method = %method, url = %full_url, seq)
            .in_scope(|| tracing::debug!(seq, method = %method, url = %full_url, "before-send"));
        let m = method.parse::<reqwest::Method>()?;
        let resp = Self::build_per_request_client()?
            .request(m, &full_url)
            .bearer_auth(token)
            .header("content-type", content_type)
            .body(body)
            .send()
            .await?;
        tracing::debug!(seq, method = %method, url = %full_url, "after-headers");

        let status = resp.status().as_u16();
        let resp_body = resp.bytes().await?.to_vec();

        if status >= 400 {
            let body_str = String::from_utf8_lossy(&resp_body).into_owned();
            return Err(drive_api_error("Drive API error", status, body_str));
        }
        Ok(DriveHttpResponse {
            status,
            body: resp_body,
        })
    }

    /// Issue a GET with an HTTP `Range` header, returning the raw response
    /// **without** error-checking the status (callers handle 416 specially).
    ///
    /// Dispatches through the injected shared client when `self.http_client` is
    /// `Some` (pooled-shared mode), or builds a fresh per-request client
    /// otherwise (the default TLS deadlock workaround).
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

        if let Some(http) = &self.http_client {
            // Pooled-shared mode: route through the injected shared client.
            let mut headers = HeaderMap::new();
            headers.insert("authorization", format!("Bearer {token}").as_str());
            if !range.is_empty() {
                headers.insert("range", range);
            }
            let resp = http
                .get(&full_url, headers)
                .await
                .map_err(|e| anyhow::anyhow!("Drive range GET failed: {e}"))?;
            return Ok(DriveHttpResponse {
                status: resp.status,
                body: resp.body,
            });
        }

        // Default path: per-request client (TLS deadlock workaround).
        let seq = self.request_seq.fetch_add(1, Ordering::Relaxed);
        // Log before-send and drop the span guard before `.await` — `EnteredSpan`
        // is not `Send` and must not be held across async yield points.
        tracing::debug_span!("drive_request", method = "GET-range", %full_url, seq).in_scope(
            || tracing::debug!(seq, method = "GET-range", %full_url, range, "before-send"),
        );
        let resp = Self::build_per_request_client()?
            .get(&full_url)
            .bearer_auth(token)
            .header(reqwest::header::RANGE, range)
            .send()
            .await?;
        tracing::debug!(seq, method = "GET-range", %full_url, "after-headers");

        let status = resp.status().as_u16();
        let body = resp.bytes().await?.to_vec();
        Ok(DriveHttpResponse { status, body })
    }
}

#[cfg(not(feature = "portable"))]
impl Default for DriveClient {
    fn default() -> Self {
        Self::new()
    }
}

// ── Portable constructors and HTTP helpers ────────────────────────────────────

#[cfg(feature = "portable")]
impl DriveClient {
    /// Construct a client with custom base URLs and an injected HTTP client.
    /// Used when the `portable` feature is active and `reqwest` is unavailable.
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

    /// Construct a client pointing at the production Google Drive API.
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
            other => anyhow::bail!("unsupported write method for Drive portable backend: {other}"),
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
        headers.insert("range", range);

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

// ── Shared public API (works with either transport) ───────────────────────────

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

    #[test]
    #[cfg(not(feature = "portable"))]
    fn client_construction() {
        let _client = DriveClient::new();
    }

    /// The bare `DriveClient::new()` constructor (used by non-injecting callers
    /// such as the CLI and tests) carries no injected HTTP client, so it falls
    /// back to the per-request `build_diag_http_client` path. The daemon, by
    /// contrast, injects a shared client into the `DriveClient`s it builds.
    #[test]
    #[cfg(not(feature = "portable"))]
    fn bare_drive_client_constructor_has_no_injected_http_client() {
        let client = DriveClient::new();
        assert!(
            client.http_client.is_none(),
            "DriveClient::new() is the no-injection constructor; the daemon injects \
             via create_backend_with_store_and_http instead"
        );
    }

    /// The env→mode policy, including the flipped default. With `pooled-shared`
    /// now the default, an absent or unrecognised value resolves to it; the
    /// former workaround is the `unpooled-legacy` escape hatch.
    #[test]
    #[cfg(not(feature = "portable"))]
    fn diag_mode_default_is_pooled_shared() {
        assert_eq!(
            parse_diag_mode(""),
            DiagHttpMode::PooledShared,
            "absent → default"
        );
        assert_eq!(parse_diag_mode("pooled-shared"), DiagHttpMode::PooledShared);
        assert_eq!(
            parse_diag_mode("anything-unknown"),
            DiagHttpMode::PooledShared
        );
        assert_eq!(
            parse_diag_mode("unpooled-legacy"),
            DiagHttpMode::UnpooledHttp1
        );
        assert_eq!(
            parse_diag_mode("unpooled-http1"),
            DiagHttpMode::UnpooledHttp1
        );
        assert_eq!(parse_diag_mode("pooled"), DiagHttpMode::Pooled);
        assert_eq!(parse_diag_mode("pooled-http2"), DiagHttpMode::PooledHttp2);
    }
}
