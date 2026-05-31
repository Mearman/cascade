//! `OAuth2` authorisation flows for Google Drive.
//!
//! Supports two flows:
//! - **Localhost redirect** (preferred) — spins up a one-shot HTTP server on
//!   a random port, opens the user's browser to Google's consent screen, and
//!   exchanges the resulting auth code for tokens. Supports full Drive scope.
//! - **Device code** (fallback) — for headless environments. Limited to
//!   `drive.file` and `drive.appdata` scopes per Google's restrictions.
//!
//! Credentials can be baked in at compile time via `CASCADE_GDRIVE_CLIENT_ID`
//! and `CASCADE_GDRIVE_CLIENT_SECRET` environment variables. If not set, the
//! credentials are read from the backend config file at runtime.
//!
//! Token persistence (`save_tokens` / `load_tokens`) uses the macOS Keychain on
//! macOS via the `security` command. On other platforms tokens are persisted to
//! a JSON file under the user's config directory
//! (`${CONFIG_DIR}/cascade/gdrive-tokens/${account}.json`), with `0o600`
//! permissions on Unix systems.

#[cfg(target_os = "macos")]
use std::process::Command;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Drive scope: full read-write access to all files the user can see.
const DRIVE_SCOPE: &str = "https://www.googleapis.com/auth/drive";

/// Device code flow scope: limited to per-file access (Google restriction).
const DEVICE_CODE_SCOPE: &str = "https://www.googleapis.com/auth/drive.file";

/// Compile-time credentials. `None` if the env vars weren't set during build.
pub const DEFAULT_CLIENT_ID: Option<&str> = option_env!("CASCADE_GDRIVE_CLIENT_ID");
pub const DEFAULT_CLIENT_SECRET: Option<&str> = option_env!("CASCADE_GDRIVE_CLIENT_SECRET");

/// `OAuth2` token response from Google.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    #[allow(dead_code)] // Deserialisation target
    expires_in: u64,
    #[allow(dead_code)] // Deserialisation target
    token_type: String,
}

/// Device code response from Google.
#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_url: String,
    #[allow(dead_code)] // Deserialisation target
    expires_in: u64,
    interval: Option<u64>,
}

/// Result of an authorisation flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

impl AuthTokens {
    /// Check if the access token is expired (with 60s buffer).
    #[must_use]
    pub fn is_expired(&self) -> bool {
        chrono::Utc::now() + chrono::Duration::seconds(60) >= self.expires_at
    }
}

/// `OAuth2` configuration for Google Drive.
pub struct OAuthConfig {
    pub client_id: String,
    pub client_secret: String,
}

impl std::fmt::Debug for OAuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthConfig")
            .field("client_id", &self.client_id)
            .field("client_secret", &"[REDACTED]")
            .finish()
    }
}

/// Resolve credentials: explicit values first, then config file, then
/// compile-time defaults.
///
/// This precedence order lets users bring their own OAuth client (highest
/// priority) and falls back to the built-in credentials only when nothing
/// else is provided.
pub fn resolve_credentials(
    config_client_id: Option<&str>,
    config_client_secret: Option<&str>,
) -> anyhow::Result<OAuthConfig> {
    let client_id = config_client_id
        .map(str::to_string)
        .or_else(|| DEFAULT_CLIENT_ID.map(str::to_string))
        .ok_or_else(|| anyhow::anyhow!("no Google client_id: set client_id in config or CASCADE_GDRIVE_CLIENT_ID at build time"))?;

    let client_secret = config_client_secret
        .map(str::to_string)
        .or_else(|| DEFAULT_CLIENT_SECRET.map(str::to_string))
        .ok_or_else(|| anyhow::anyhow!("no Google client_secret: set client_secret in config or CASCADE_GDRIVE_CLIENT_SECRET at build time"))?;

    Ok(OAuthConfig {
        client_id,
        client_secret,
    })
}

/// Keychain service name for storing tokens.
#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "com.cascade.gdrive";

// ---------------------------------------------------------------------------
// Localhost redirect flow (preferred)
// ---------------------------------------------------------------------------

/// Run the localhost redirect `OAuth2` flow.
///
/// 1. Bind a random port on localhost
/// 2. Print the authorisation URL and attempt to open the browser
/// 3. Wait for the callback with the auth code
/// 4. Exchange the code for tokens
pub async fn start_local_redirect(
    http: &reqwest::Client,
    config: &OAuthConfig,
) -> anyhow::Result<AuthTokens> {
    let listener = tokio::net::TcpListener::bind("localhost:0").await?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://localhost:{port}");
    let auth_url = format!(
        "https://accounts.google.com/o/oauth2/v2/auth?client_id={}&redirect_uri={}&response_type=code&scope={}&access_type=offline&prompt=consent",
        config.client_id,
        urlencoding::encode(&redirect_uri),
        urlencoding::encode(DRIVE_SCOPE),
    );

    println!("Opening browser for Google Drive authorisation...");
    println!("If the browser doesn't open, visit:\n  {auth_url}");

    // Try to open the browser (best-effort).
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(&auth_url).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open")
            .arg(&auth_url)
            .spawn();
    }

    println!("Waiting for authorisation callback on port {port}...");

    // Accept one connection, parse the callback.
    let code = wait_for_callback(&listener).await?;

    // Exchange the auth code for tokens.
    exchange_code(http, config, &code, &redirect_uri).await
}

/// Wait for the `OAuth2` callback on the localhost listener.
///
/// Parses the query string from the GET request, extracts the `code` parameter,
/// sends a simple HTML response to the browser, and returns the auth code.
async fn wait_for_callback(listener: &tokio::net::TcpListener) -> anyhow::Result<String> {
    let (stream, _addr) = listener.accept().await?;
    let mut buf = vec![0u8; 4096];
    let (mut reader, mut writer) = tokio::io::split(stream);

    let n = reader.read(&mut buf).await?;
    let request = String::from_utf8_lossy(buf.get(..n).unwrap_or_default());

    // Parse the request line: GET /callback?code=...&scope=... HTTP/1.1
    let path = request
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("malformed callback request"))?;

    let query = path.split('?').nth(1).unwrap_or_default();

    // Extract the code parameter.
    let mut code: Option<String> = None;
    let mut error: Option<String> = None;
    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            match key {
                "code" => {
                    code = Some(
                        urlencoding::decode(value)
                            .map_err(|e| anyhow::anyhow!("invalid URL encoding: {e}"))?
                            .into_owned(),
                    );
                }
                "error" => {
                    error = Some(
                        urlencoding::decode(value)
                            .map_err(|e| anyhow::anyhow!("invalid URL encoding: {e}"))?
                            .into_owned(),
                    );
                }
                _ => {}
            }
        }
    }

    // Send a response to the browser.
    let body = if code.is_some() {
        "<html><body><h1>Authorised!</h1><p>You can close this tab.</p></body></html>"
    } else {
        "<html><body><h1>Authorisation failed</h1><p>Please try again.</p></body></html>"
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = writer.write_all(response.as_bytes()).await;
    let _ = writer.shutdown().await;

    if let Some(err) = error {
        anyhow::bail!("OAuth2 authorisation error: {err}");
    }

    code.ok_or_else(|| anyhow::anyhow!("no auth code in callback"))
}

/// Exchange an authorisation code for tokens.
async fn exchange_code(
    http: &reqwest::Client,
    config: &OAuthConfig,
    code: &str,
    redirect_uri: &str,
) -> anyhow::Result<AuthTokens> {
    let resp = http
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("client_id", config.client_id.as_str()),
            ("client_secret", config.client_secret.as_str()),
            ("code", code),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect_uri),
        ])
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Token exchange failed ({status}): {body}");
    }

    let token_resp: TokenResponse = resp.json().await?;
    let secs = i64::try_from(token_resp.expires_in).unwrap_or(i64::MAX);
    let expires_at = chrono::Utc::now() + chrono::Duration::seconds(secs);

    Ok(AuthTokens {
        access_token: token_resp.access_token,
        refresh_token: token_resp.refresh_token.unwrap_or_default(),
        expires_at,
    })
}

// ---------------------------------------------------------------------------
// Device code flow (fallback)
// ---------------------------------------------------------------------------

/// Initiate the device code flow. Returns the URL and user code.
pub async fn start_device_code(
    http: &reqwest::Client,
    config: &OAuthConfig,
) -> anyhow::Result<(String, String, String, u64)> {
    let resp = http
        .post("https://oauth2.googleapis.com/device/code")
        .form(&[
            ("client_id", config.client_id.as_str()),
            ("scope", DEVICE_CODE_SCOPE),
        ])
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Device code request failed ({status}): {body}");
    }

    let dcr = resp.json::<DeviceCodeResponse>().await?;
    Ok((
        dcr.verification_url,
        dcr.user_code,
        dcr.device_code,
        dcr.interval.unwrap_or(5),
    ))
}

/// Poll for the device code token. Blocks until the user authorises.
pub async fn poll_for_token(
    http: &reqwest::Client,
    config: &OAuthConfig,
    device_code: &str,
    interval_secs: u64,
) -> anyhow::Result<AuthTokens> {
    #[derive(Deserialize)]
    struct ErrorBody {
        error: String,
    }

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;

        let resp = http
            .post("https://oauth2.googleapis.com/token")
            .form(&[
                ("client_id", config.client_id.as_str()),
                ("client_secret", config.client_secret.as_str()),
                ("code", device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();

        if status.is_success() {
            let token_resp: TokenResponse = serde_json::from_str(&body)?;
            let secs = i64::try_from(token_resp.expires_in).unwrap_or(i64::MAX);
            let expires_at = chrono::Utc::now() + chrono::Duration::seconds(secs);
            return Ok(AuthTokens {
                access_token: token_resp.access_token,
                refresh_token: token_resp.refresh_token.unwrap_or_default(),
                expires_at,
            });
        }

        // Check for pending vs error.
        let err: ErrorBody = serde_json::from_str(&body).unwrap_or_else(|_| ErrorBody {
            error: "unknown".to_string(),
        });

        match err.error.as_str() {
            "authorization_pending" => {}
            "slow_down" => {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
            "expired_token" => anyhow::bail!("Device code expired. Please try again."),
            other => anyhow::bail!("OAuth2 error: {other}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Token refresh
// ---------------------------------------------------------------------------

/// Refresh an access token using a refresh token.
pub async fn refresh_access_token(
    http: &reqwest::Client,
    config: &OAuthConfig,
    refresh_token: &str,
) -> anyhow::Result<AuthTokens> {
    let resp = http
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("client_id", config.client_id.as_str()),
            ("client_secret", config.client_secret.as_str()),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Token refresh failed ({status}): {body}");
    }

    let token_resp: TokenResponse = resp.json().await?;
    let secs = i64::try_from(token_resp.expires_in).unwrap_or(i64::MAX);
    let expires_at = chrono::Utc::now() + chrono::Duration::seconds(secs);

    Ok(AuthTokens {
        access_token: token_resp.access_token,
        refresh_token: token_resp
            .refresh_token
            .unwrap_or_else(|| refresh_token.to_string()),
        expires_at,
    })
}

// ---------------------------------------------------------------------------
// Token persistence
// ---------------------------------------------------------------------------
//
// macOS uses the system Keychain via the `security` command. Other platforms
// fall back to a JSON file under the user's config directory.

/// Save tokens to the macOS Keychain.
#[cfg(target_os = "macos")]
pub fn save_tokens(account: &str, tokens: &AuthTokens) -> anyhow::Result<()> {
    let json = serde_json::to_string(tokens)?;

    // Delete existing entry first (add-only will fail if it exists).
    let _ = Command::new("security")
        .args([
            "delete-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            account,
        ])
        .output();

    let output = Command::new("security")
        .args([
            "add-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            account,
            "-w",
            &json,
        ])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to save tokens to Keychain: {stderr}");
    }
    Ok(())
}

/// Load tokens from the macOS Keychain.
#[cfg(target_os = "macos")]
pub fn load_tokens(account: &str) -> anyhow::Result<Option<AuthTokens>> {
    let output = Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            account,
            "-w",
        ])
        .output();

    match output {
        Ok(output) if output.status.success() => {
            let json = String::from_utf8_lossy(&output.stdout);
            let tokens: AuthTokens = serde_json::from_str(json.trim())?;
            Ok(Some(tokens))
        }
        _ => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// File-based persistence
// ---------------------------------------------------------------------------
//
// Used by the public `save_tokens`/`load_tokens` on non-macOS targets. The
// module is also compiled in `cfg(test)` so the round trip can be exercised
// from the test suite on every host, while macOS continues to use the
// Keychain implementation above for the public API.

#[cfg(any(test, not(target_os = "macos")))]
mod file_store {
    use super::AuthTokens;
    use std::path::{Path, PathBuf};

    /// Sub-path under the config directory where token files live.
    #[cfg(not(target_os = "macos"))]
    const TOKEN_SUBDIR: &str = "cascade/gdrive-tokens";

    /// Resolve the default token storage directory under the user's config dir.
    #[cfg(not(target_os = "macos"))]
    pub(super) fn default_tokens_dir() -> anyhow::Result<PathBuf> {
        Ok(dirs::config_dir()
            .ok_or_else(|| anyhow::anyhow!("could not determine user config directory"))?
            .join(TOKEN_SUBDIR))
    }

    /// Replace path-unsafe characters in an account name with underscores.
    pub(super) fn sanitise_account(account: &str) -> String {
        account.replace(['/', '\\', ':', '\0'], "_")
    }

    /// Compose the on-disk path for the given account's tokens under `base`.
    pub(super) fn tokens_path_in(base: &Path, account: &str) -> PathBuf {
        base.join(format!("{}.json", sanitise_account(account)))
    }

    /// Save tokens as JSON under the given directory.
    ///
    /// On Unix the file is created atomically with `0o600` permissions
    /// (owner read+write only) — the file never exists on disk with
    /// permissive bits. Any pre-existing file is removed first so that
    /// `create_new` always creates a fresh inode at the restrictive mode,
    /// avoiding the TOCTOU window that an `open(write|truncate)` followed
    /// by a separate `set_permissions` call would leave behind.
    /// On Windows the file inherits NTFS ACLs from its parent dir.
    pub(super) fn save_tokens_in(
        base: &Path,
        account: &str,
        tokens: &AuthTokens,
    ) -> anyhow::Result<()> {
        std::fs::create_dir_all(base)?;

        let path = tokens_path_in(base, account);
        let json = serde_json::to_string(tokens)?;

        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;

            // Remove any existing file so `create_new` always allocates a
            // fresh inode with the requested mode. Without this, a pre-existing
            // file with permissive bits would keep those bits after the write
            // (the `mode` argument to `OpenOptions` only applies on creation).
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }

            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)?;
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&path, json.as_bytes())?;
        }

        Ok(())
    }

    /// Load tokens from the per-account JSON file under `base`, if it exists.
    pub(super) fn load_tokens_in(base: &Path, account: &str) -> anyhow::Result<Option<AuthTokens>> {
        let path = tokens_path_in(base, account);
        match std::fs::read(&path) {
            Ok(bytes) => {
                let tokens: AuthTokens = serde_json::from_slice(&bytes)?;
                Ok(Some(tokens))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }
}

/// Save tokens to a JSON file under the user's config directory.
#[cfg(not(target_os = "macos"))]
pub fn save_tokens(account: &str, tokens: &AuthTokens) -> anyhow::Result<()> {
    file_store::save_tokens_in(&file_store::default_tokens_dir()?, account, tokens)
}

/// Load tokens from the per-account JSON file under the user's config dir.
#[cfg(not(target_os = "macos"))]
pub fn load_tokens(account: &str) -> anyhow::Result<Option<AuthTokens>> {
    file_store::load_tokens_in(&file_store::default_tokens_dir()?, account)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_tokens_expiry_check() {
        let future = AuthTokens {
            access_token: "test".to_string(),
            refresh_token: "refresh".to_string(),
            expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
        };
        assert!(!future.is_expired());

        let past = AuthTokens {
            access_token: "test".to_string(),
            refresh_token: "refresh".to_string(),
            expires_at: chrono::Utc::now() - chrono::Duration::hours(1),
        };
        assert!(past.is_expired());
    }

    #[test]
    fn resolve_credentials_prefers_config_over_built_in() {
        // Config values take priority over compile-time defaults.
        let config = resolve_credentials(Some("cfg-id"), Some("cfg-secret"));
        assert!(config.is_ok());
        let c = config.unwrap();
        assert_eq!(c.client_id, "cfg-id");
        assert_eq!(c.client_secret, "cfg-secret");
    }

    #[test]
    fn resolve_credentials_errors_when_both_missing() {
        // If no baked-in creds and no config values, should error.
        let result = resolve_credentials(None, None);
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // File-fallback tests
    //
    // These exercise the JSON-file persistence path used by the public
    // `save_tokens`/`load_tokens` on Linux and Windows. The helpers are
    // compiled under `cfg(any(test, not(target_os = "macos")))`, so the same
    // round trip also runs on macOS hosts — useful for catching regressions
    // in CI without waiting for a Linux runner.
    // -----------------------------------------------------------------------

    use super::file_store::{load_tokens_in, sanitise_account, save_tokens_in, tokens_path_in};

    fn sample_tokens() -> AuthTokens {
        AuthTokens {
            access_token: "access-123".to_string(),
            refresh_token: "refresh-456".to_string(),
            expires_at: chrono::DateTime::from_timestamp(1_700_000_000, 0)
                .unwrap_or_else(chrono::Utc::now),
        }
    }

    #[test]
    fn file_fallback_round_trips_tokens() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let tokens = sample_tokens();

        save_tokens_in(dir.path(), "primary", &tokens)?;
        let loaded = load_tokens_in(dir.path(), "primary")?
            .ok_or_else(|| anyhow::anyhow!("expected tokens to be present after save"))?;

        assert_eq!(loaded.access_token, tokens.access_token);
        assert_eq!(loaded.refresh_token, tokens.refresh_token);
        assert_eq!(loaded.expires_at, tokens.expires_at);
        Ok(())
    }

    #[test]
    fn file_fallback_returns_none_when_missing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let loaded = load_tokens_in(dir.path(), "never-saved")?;
        assert!(loaded.is_none());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn file_fallback_sets_owner_only_permissions() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir()?;
        save_tokens_in(dir.path(), "perms", &sample_tokens())?;

        let path = tokens_path_in(dir.path(), "perms");
        let mode = std::fs::metadata(&path)?.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected file mode 0o600, found {mode:o}");
        Ok(())
    }

    /// Regression test for the TOCTOU permission window: when a pre-existing
    /// token file has permissive bits (e.g. left behind by an older build that
    /// wrote at the umask before tightening), the next save must end with
    /// `0o600` and no intermediate state visible to other local users.
    /// Implementation-wise this means the existing file is unlinked and
    /// replaced rather than overwritten in place.
    #[cfg(unix)]
    #[test]
    fn file_fallback_tightens_permissive_existing_file() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir()?;
        let path = tokens_path_in(dir.path(), "stale");

        // Seed a pre-existing token file with world-readable permissions, as
        // would happen on a host where an older binary wrote at the umask.
        std::fs::write(&path, b"stale")?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
        let stale_mode = std::fs::metadata(&path)?.permissions().mode() & 0o777;
        assert_eq!(stale_mode, 0o644, "seed file should start at 0o644");

        save_tokens_in(dir.path(), "stale", &sample_tokens())?;

        let mode = std::fs::metadata(&path)?.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected file mode 0o600, found {mode:o}");
        Ok(())
    }

    #[test]
    fn file_fallback_sanitises_account_names() {
        // Slashes and colons must not escape the storage directory.
        let dirty = "dir/with:bad\\chars";
        let cleaned = sanitise_account(dirty);
        assert!(!cleaned.contains('/'));
        assert!(!cleaned.contains('\\'));
        assert!(!cleaned.contains(':'));
    }
}
