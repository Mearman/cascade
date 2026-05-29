//! `cascade backend auth` — authenticate a named backend.
//!
//! For Google Drive, uses the localhost redirect `OAuth2` flow (full Drive
//! scope). Falls back to the device code flow if the local redirect fails
//! (e.g. headless environment).

use anyhow::Context as _;
use cascade_backend_gdrive::auth::{
    poll_for_token, resolve_credentials, save_tokens, start_device_code, start_local_redirect,
};

use super::CliContext;

/// Authenticate a named backend via `OAuth2`.
///
/// Tries the localhost redirect flow first (full `drive` scope). Falls back
/// to the device code flow (`drive.file` scope only) if the redirect fails.
pub async fn authenticate(ctx: &CliContext, name: &str) -> anyhow::Result<()> {
    let config_path = ctx.config_dir.join(format!("{name}.toml"));

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

    let config_client_id = config.get("client_id").and_then(|v| v.as_str());
    let config_client_secret = config.get("client_secret").and_then(|v| v.as_str());

    let oauth = resolve_credentials(config_client_id, config_client_secret)?;

    let http = reqwest::Client::new();

    // Try localhost redirect flow first (full Drive scope).
    match start_local_redirect(&http, &oauth).await {
        Ok(tokens) => {
            save_tokens(name, &tokens)?;
            println!("Authenticated successfully (full Drive access).");
            return Ok(());
        }
        Err(e) => {
            eprintln!("Localhost redirect failed ({e}), falling back to device code flow...");
            eprintln!("Note: device code flow only grants per-file access (drive.file scope).");
        }
    }

    // Fallback: device code flow.
    let (verification_url, user_code, device_code, interval_secs) =
        start_device_code(&http, &oauth).await?;

    println!("Visit {verification_url} and enter code: {user_code}");
    println!("Waiting for authorisation...");

    let tokens = poll_for_token(&http, &oauth, &device_code, interval_secs).await?;

    save_tokens(name, &tokens)?;

    println!("Authenticated successfully (per-file access only).");

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_ctx(dir: &TempDir) -> CliContext {
        let config_dir = dir.path().to_path_buf();
        CliContext {
            db_path: config_dir.join("state.db"),
            pid_path: config_dir.join("cascade.pid"),
            config_dir,
        }
    }

    #[tokio::test]
    async fn authenticate_errors_when_config_missing() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);

        let result = authenticate(&ctx, "missing").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn authenticate_errors_for_non_gdrive_backend() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);

        let config_path = ctx.config_dir.join("mybackup.toml");
        std::fs::write(&config_path, "type = \"s3\"\n").unwrap();

        let result = authenticate(&ctx, "mybackup").await;
        assert!(result.is_err());
    }

    #[test]
    fn resolve_credentials_from_config() {
        let oauth =
            resolve_credentials(Some("test-client-id"), Some("test-client-secret")).unwrap();
        // If baked-in creds exist, they take priority; otherwise config values.
        if cascade_backend_gdrive::auth::DEFAULT_CLIENT_ID.is_none() {
            assert_eq!(oauth.client_id, "test-client-id");
            assert_eq!(oauth.client_secret, "test-client-secret");
        }
    }
}
