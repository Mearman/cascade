//! OAuth2 device code + refresh flow for Google Drive.
//!
//! Tokens are stored in the macOS Keychain via the `security` command.
//! The device code flow is used for initial authorisation — no local
//! web server required.

use std::process::Command;

use serde::{Deserialize, Serialize};

/// OAuth2 token response from Google.
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

/// Result of a device code authorisation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

impl AuthTokens {
    /// Check if the access token is expired (with 60s buffer).
    pub fn is_expired(&self) -> bool {
        chrono::Utc::now() + chrono::Duration::seconds(60) >= self.expires_at
    }
}

/// OAuth2 configuration for Google Drive.
pub struct OAuthConfig {
    pub client_id: String,
    pub client_secret: String,
}

/// Keychain service name for storing tokens.
const KEYCHAIN_SERVICE: &str = "com.cascade.gdrive";

/// Initiate the device code flow. Returns the URL and user code.
pub async fn start_device_code(
    http: &reqwest::Client,
    config: &OAuthConfig,
) -> anyhow::Result<(String, String, String, u64)> {
    let resp = http
        .post("https://oauth2.googleapis.com/device/code")
        .form(&[
            ("client_id", config.client_id.as_str()),
            ("scope", "https://www.googleapis.com/auth/drive.readonly"),
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
            let expires_at =
                chrono::Utc::now() + chrono::Duration::seconds(token_resp.expires_in as i64);
            return Ok(AuthTokens {
                access_token: token_resp.access_token,
                refresh_token: token_resp.refresh_token.unwrap_or_default(),
                expires_at,
            });
        }

        // Check for pending vs error.
        #[derive(Deserialize)]
        struct ErrorBody {
            error: String,
        }
        let err: ErrorBody = serde_json::from_str(&body).unwrap_or(ErrorBody {
            error: "unknown".to_string(),
        });

        match err.error.as_str() {
            "authorization_pending" => continue,
            "slow_down" => {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
            "expired_token" => anyhow::bail!("Device code expired. Please try again."),
            other => anyhow::bail!("OAuth2 error: {other}"),
        }
    }
}

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
    let expires_at = chrono::Utc::now() + chrono::Duration::seconds(token_resp.expires_in as i64);

    Ok(AuthTokens {
        access_token: token_resp.access_token,
        refresh_token: token_resp
            .refresh_token
            .unwrap_or_else(|| refresh_token.to_string()),
        expires_at,
    })
}

/// Save tokens to macOS Keychain.
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

/// Load tokens from macOS Keychain.
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
}
