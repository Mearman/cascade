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
//! Tokens are stored in the macOS Keychain via the `security` command.
//! Token persistence (`save_tokens` / `load_tokens`) is only available on
//! macOS. On other platforms both functions return an error immediately.

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

/// Resolve credentials: baked-in first, then config file values.
pub fn resolve_credentials(
    config_client_id: Option<&str>,
    config_client_secret: Option<&str>,
) -> anyhow::Result<OAuthConfig> {
    let client_id = DEFAULT_CLIENT_ID
        .map(str::to_string)
        .or_else(|| config_client_id.map(str::to_string))
        .ok_or_else(|| anyhow::anyhow!("no Google client_id: set CASCADE_GDRIVE_CLIENT_ID at build time or client_id in config"))?;

    let client_secret = DEFAULT_CLIENT_SECRET
        .map(str::to_string)
        .or_else(|| config_client_secret.map(str::to_string))
        .ok_or_else(|| anyhow::anyhow!("no Google client_secret: set CASCADE_GDRIVE_CLIENT_SECRET at build time or client_secret in config"))?;

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
// Token persistence (macOS Keychain)
// ---------------------------------------------------------------------------

/// Save tokens to the macOS Keychain.
///
/// Only available on macOS. Returns an error on other platforms.
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

#[cfg(not(target_os = "macos"))]
pub fn save_tokens(_account: &str, _tokens: &AuthTokens) -> anyhow::Result<()> {
    anyhow::bail!("token storage via Keychain is only supported on macOS")
}

/// Load tokens from the macOS Keychain.
///
/// Only available on macOS. Returns an error on other platforms.
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

#[cfg(not(target_os = "macos"))]
pub fn load_tokens(_account: &str) -> anyhow::Result<Option<AuthTokens>> {
    anyhow::bail!("token storage via Keychain is only supported on macOS")
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
    fn resolve_credentials_prefers_built_in() {
        // If compiled without env vars, falls back to config values.
        let config = resolve_credentials(Some("cfg-id"), Some("cfg-secret"));
        assert!(config.is_ok());
        let c = config.unwrap();
        // With no baked-in creds, config values are used.
        if DEFAULT_CLIENT_ID.is_none() {
            assert_eq!(c.client_id, "cfg-id");
            assert_eq!(c.client_secret, "cfg-secret");
        }
    }

    #[test]
    fn resolve_credentials_errors_when_both_missing() {
        // If no baked-in creds and no config values, should error.
        let result = resolve_credentials(None, None);
        assert!(result.is_err());
    }
}
