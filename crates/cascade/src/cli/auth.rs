//! `cascade backend auth` — authenticate a named backend.
//!
//! For Google Drive, uses the localhost redirect `OAuth2` flow by default
//! (full Drive scope). Pass `--device-code` to use the device code flow
//! directly (headless environments, limited to `drive.file` scope).
//!
//! Also contains `cascade auth pair/authorize/secret` — PWA authentication
//! commands that generate pairing codes, authorise device codes, and manage
//! the daemon's shared secret.

use anyhow::{Context as _, Result};
use cascade_backend_gdrive::auth::{
    poll_for_token, resolve_credentials, save_tokens, start_device_code, start_local_redirect,
};
use cascade_engine::db::StateDb;
use chrono::{Duration, Utc};
use data_encoding::{BASE32_NOPAD, HEXLOWER};
use sha2::{Digest, Sha256};

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

// ── PWA authentication commands ──

use super::AuthCommands;

/// Generate a short code derived from SHA-256 of timestamp + counter.
fn generate_pairing_code() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = Utc::now();
    let counter = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut hasher = Sha256::new();
    hasher.update(b"cascade-pair-code-v1");
    hasher.update(now.timestamp_nanos_opt().map_or([0u8; 8], i64::to_be_bytes));
    hasher.update(counter.to_be_bytes());
    let digest = hasher.finalize();
    let prefix: [u8; 5] = digest
        .get(..5)
        .and_then(|s| s.try_into().ok())
        .unwrap_or([0u8; 5]);
    BASE32_NOPAD.encode(&prefix)
}

/// Generate 16 random bytes from SHA-256 and return them as hex (32 chars).
fn generate_secret() -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"cascade-daemon-secret-v1");
    hasher.update(
        Utc::now()
            .timestamp_nanos_opt()
            .map_or([0u8; 8], i64::to_be_bytes),
    );
    let digest = hasher.finalize();
    HEXLOWER.encode(digest.get(..16).unwrap_or(&[0u8; 16]))
}

/// Handle `cascade auth pair/authorize/secret`.
pub async fn pwa_auth(ctx: &super::CliContext, command: AuthCommands) -> Result<()> {
    let db = StateDb::open(&ctx.db_path).context("could not open state database")?;

    match command {
        AuthCommands::Pair => {
            let code = generate_pairing_code();
            let expires_at = Utc::now() + Duration::minutes(5);
            db.insert_auth_code(&code, "pairing", expires_at)
                .context("could not store pairing code")?;
            println!("Pairing code: {code}");
            println!("Enter this code in the Cascade web UI within 5 minutes.");
            Ok(())
        }
        AuthCommands::Authorize { code } => {
            // The daemon must be running for this to work — the PWA is polling
            // it. Call the daemon's authorize endpoint via HTTP.
            let client = reqwest::Client::new();
            let url = format!("http://127.0.0.1:7842/v1/auth/device/{code}/authorize");
            let resp = client
                .post(&url)
                .send()
                .await
                .context("could not reach the daemon — is it running?")?;

            if resp.status().is_success() {
                println!("Authorised. The web UI will connect automatically.");
                Ok(())
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("daemon returned {status}: {body}");
            }
        }
        AuthCommands::Secret => {
            let existing = db
                .get_daemon_secret()
                .context("could not read daemon secret")?;
            let secret = if let Some(s) = existing {
                s
            } else {
                let generated = generate_secret();
                db.set_daemon_secret(&generated)
                    .context("could not store daemon secret")?;
                generated
            };
            println!("Daemon secret: {secret}");
            println!("Use this secret in the Cascade web UI to authenticate.");
            Ok(())
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
