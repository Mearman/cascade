//! `cascade backend auth` — run the `OAuth2` device-code flow for a named backend.

use anyhow::Context as _;
use cascade_backend_gdrive::auth::{OAuthConfig, poll_for_token, save_tokens, start_device_code};

use super::CliContext;

/// Authenticate a named backend via the `OAuth2` device-code flow.
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

        // No .toml file for "missing" backend — should fail with a context
        // error rather than panic.
        let result = authenticate(&ctx, "missing").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn authenticate_errors_for_non_gdrive_backend() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);

        // Write an S3 backend config — auth only supports gdrive.
        let config_path = ctx.config_dir.join("mybackup.toml");
        std::fs::write(&config_path, "type = \"s3\"\n").unwrap();

        let result = authenticate(&ctx, "mybackup").await;
        assert!(result.is_err());
    }
}
