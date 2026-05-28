//! `cascade backend auth` — run the `OAuth2` device-code flow for a named backend.

use std::path::PathBuf;

use anyhow::Context as _;
use cascade_backend_gdrive::auth::{OAuthConfig, poll_for_token, save_tokens, start_device_code};

/// Compute the cascade config directory.
fn config_dir() -> anyhow::Result<PathBuf> {
    dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("could not determine config directory"))
        .map(|p| p.join("cascade"))
}

/// Authenticate a named backend via the `OAuth2` device-code flow.
pub async fn authenticate(name: &str) -> anyhow::Result<()> {
    let config_dir = config_dir()?;
    let config_path = config_dir.join(format!("{name}.toml"));

    let raw = std::fs::read_to_string(&config_path)
        .with_context(|| format!("backend '{}' not found ({})", name, config_path.display()))?;

    let config: toml::Value = toml::from_str(&raw)
        .with_context(|| format!("failed to parse {}", config_path.display()))?;

    let backend_type = config
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("backend config is missing the 'type' field"))?;

    if backend_type != "gdrive" {
        anyhow::bail!("backend auth is only supported for gdrive backends");
    }

    let client_id = config
        .get("client_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("gdrive backend config is missing 'client_id'"))?
        .to_string();

    let client_secret = config
        .get("client_secret")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("gdrive backend config is missing 'client_secret'"))?
        .to_string();

    let oauth = OAuthConfig {
        client_id,
        client_secret,
    };

    let http = reqwest::Client::new();

    let (verification_url, user_code, device_code, interval_secs) =
        start_device_code(&http, &oauth).await?;

    println!("Visit {verification_url} and enter code: {user_code}");
    println!("Waiting for authorisation...");

    let tokens = poll_for_token(&http, &oauth, &device_code, interval_secs).await?;

    save_tokens(name, &tokens)?;

    println!("Authenticated successfully.");

    Ok(())
}
