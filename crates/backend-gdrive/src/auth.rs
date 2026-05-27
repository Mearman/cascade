//! OAuth2 device code + refresh flow for Google Drive.

/// Result of a device code authorisation.
#[derive(Debug)]
pub struct AuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

/// Initiate the device code flow. Returns the URL and code for the user to visit.
pub fn device_code_url(client_id: &str) -> (String, String) {
    // TODO: Call https://oauth2.googleapis.com/device/code
    let code = "PLACEHOLDER";
    let url = format!("https://www.google.com/device");
    (url, code.to_string())
}

/// Poll for the device code token.
pub async fn poll_for_token(
    _client_id: &str,
    _client_secret: &str,
    _device_code: &str,
) -> anyhow::Result<AuthTokens> {
    // TODO: Poll https://oauth2.googleapis.com/token
    anyhow::bail!("OAuth2 device code flow not yet implemented")
}

/// Refresh an access token using a refresh token.
pub async fn refresh_access_token(
    _client_id: &str,
    _client_secret: &str,
    _refresh_token: &str,
) -> anyhow::Result<AuthTokens> {
    // TODO: Call https://oauth2.googleapis.com/token with grant_type=refresh_token
    anyhow::bail!("token refresh not yet implemented")
}
