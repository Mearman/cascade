//! `cascade backend auth` — authenticate a named backend.
//!
//! For Google Drive, uses the localhost redirect `OAuth2` flow by default
//! (full Drive scope). Pass `--device-code` to use the device code flow
//! directly (headless environments, limited to `drive.file` scope).

use anyhow::Context as _;
use cascade_backend_gdrive::auth::{
    poll_for_token, resolve_credentials, save_tokens, start_device_code, start_local_redirect,
};

use super::CliContext;

/// Authenticate a named backend via `OAuth2`.
///
/// By default uses the localhost redirect flow (full `drive` scope).
/// Pass `device_code_only = true` to use the device code flow directly
/// (`drive.file` scope only).
pub async fn authenticate(
    ctx: &CliContext,
    name: &str,
    cli_client_id: Option<&str>,
    cli_client_secret: Option<&str>,
    device_code_only: bool,
) -> anyhow::Result<()> {
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

    // CLI flags > config file > compile-time defaults.
    let effective_client_id = cli_client_id.or(config_client_id);
    let effective_client_secret = cli_client_secret.or(config_client_secret);

    let oauth = resolve_credentials(effective_client_id, effective_client_secret)?;

    let http = reqwest::Client::new();

    if device_code_only {
        let (verification_url, user_code, device_code, interval_secs) =
            start_device_code(&http, &oauth).await?;
        println!("Visit {verification_url} and enter code: {user_code}");
        println!("Note: device code flow only grants per-file access (drive.file scope).");
        println!("Waiting for authorisation...");
        let tokens = poll_for_token(&http, &oauth, &device_code, interval_secs).await?;
        save_tokens(name, &tokens)?;
        println!("Authenticated successfully (per-file access only).");
        return Ok(());
    }

    // Localhost redirect flow — full Drive scope.
    match start_local_redirect(&http, &oauth).await {
        Ok(tokens) => {
            save_tokens(name, &tokens)?;
            println!("Authenticated successfully (full Drive access).");
            Ok(())
        }
        Err(e) => {
            eprintln!("Localhost redirect failed: {e}");
            eprintln!(
                "To authenticate from a headless environment, run:\n  cascade backend-auth {name} --device-code"
            );
            eprintln!("Note: device code flow only grants per-file access (drive.file scope).");
            Err(anyhow::anyhow!("authentication failed"))
        }
    }
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

        let result = authenticate(&ctx, "missing", None, None, false).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn authenticate_errors_for_non_gdrive_backend() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);

        let config_path = ctx.config_dir.join("mybackup.toml");
        std::fs::write(&config_path, "type = \"s3\"\n").unwrap();

        let result = authenticate(&ctx, "mybackup", None, None, false).await;
        assert!(result.is_err());
    }

    #[test]
    fn resolve_credentials_from_config() {
        let oauth =
            resolve_credentials(Some("test-client-id"), Some("test-client-secret")).unwrap();
        // Config values take priority over compile-time defaults.
        assert_eq!(oauth.client_id, "test-client-id");
        assert_eq!(oauth.client_secret, "test-client-secret");
    }
}
