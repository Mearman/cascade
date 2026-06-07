//! One-shot cleanup of macOS metadata files in Google Drive.
//!
//! Trashes every `._*` `AppleDouble` sidecar, `.DS_Store`, and related
//! Finder/Spotlight files in the named accounts. Items go to Drive Bin
//! and remain recoverable for 30 days.
//!
//! Usage:
//! `cargo run -p cascade-backend-gdrive --example cleanup_macos_junk -- personal work`

use std::time::Duration;

use cascade_backend_gdrive::auth;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let accounts: Vec<String> = std::env::args().skip(1).collect();
    if accounts.is_empty() {
        anyhow::bail!("usage: cleanup_macos_junk <account>...");
    }

    for account in accounts {
        if let Err(e) = run_for_account(&account).await {
            eprintln!("[{account}] failed: {e:#}");
        }
    }
    Ok(())
}

async fn run_for_account(account: &str) -> anyhow::Result<()> {
    let token = load_token(account).await?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    // Queries: each pair is (Drive `q` expression, requires post-filter
    // for prefix). Drive's `name contains '._'` is a substring match, so
    // we filter client-side to keep only true AppleDouble files.
    let queries: &[(&str, bool)] = &[
        ("name contains '._' and trashed=false", true),
        ("name='.DS_Store' and trashed=false", false),
        ("name='.Spotlight-V100' and trashed=false", false),
        ("name='.Trashes' and trashed=false", false),
        ("name='.fseventsd' and trashed=false", false),
        ("name='.TemporaryItems' and trashed=false", false),
        ("name='.DocumentRevisions-V100' and trashed=false", false),
        ("name='.VolumeIcon.icns' and trashed=false", false),
    ];

    let mut total = 0_usize;
    for (q, needs_prefix_filter) in queries {
        let trashed = run_query(&http, &token, account, q, *needs_prefix_filter).await?;
        println!("[{account}] q={q:?} -> trashed {trashed}");
        total += trashed;
    }
    println!("[{account}] DONE: trashed {total} junk file(s)");
    Ok(())
}

async fn run_query(
    http: &reqwest::Client,
    token: &str,
    account: &str,
    q: &str,
    needs_prefix_filter: bool,
) -> anyhow::Result<usize> {
    let mut page: Option<String> = None;
    let mut count = 0_usize;
    loop {
        let mut params: Vec<(&str, &str)> = vec![
            ("q", q),
            ("fields", "nextPageToken,files(id,name)"),
            ("pageSize", "1000"),
            ("corpora", "user"),
            ("supportsAllDrives", "true"),
            ("includeItemsFromAllDrives", "true"),
        ];
        if let Some(ref pt) = page {
            params.push(("pageToken", pt));
        }

        let resp = http
            .get("https://www.googleapis.com/drive/v3/files")
            .bearer_auth(token)
            .query(&params)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("list failed {status}: {body}");
        }

        let body: serde_json::Value = resp.json().await?;
        let files = body
            .get("files")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        for f in files {
            let id = f.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let name = f.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if id.is_empty() {
                continue;
            }
            if needs_prefix_filter && !name.starts_with("._") {
                continue;
            }

            let url = format!(
                "https://www.googleapis.com/drive/v3/files/{id}?supportsAllDrives=true&fields=id"
            );
            let r = http
                .patch(&url)
                .bearer_auth(token)
                .json(&serde_json::json!({"trashed": true}))
                .send()
                .await?;
            if r.status().is_success() {
                count += 1;
                if count.is_multiple_of(50) {
                    eprintln!("[{account}] trashed {count} so far ({q})");
                }
            } else {
                let s = r.status();
                let b = r.text().await.unwrap_or_default();
                eprintln!("[{account}] failed to trash {id} ({name}): {s} {b}");
            }
        }

        page = body
            .get("nextPageToken")
            .and_then(|v| v.as_str())
            .map(String::from);
        if page.is_none() {
            break;
        }
    }
    Ok(count)
}

/// Load OAuth credentials for `account` from its config file and resolve
/// (refreshing if needed) the access token via the auth helpers used by
/// the live backend.
async fn load_token(account: &str) -> anyhow::Result<String> {
    let home = std::env::var("HOME")?;
    let cfg_path = format!("{home}/.config/cascade/{account}.toml");
    let cfg_text = std::fs::read_to_string(&cfg_path)?;
    let cfg: toml::Value = cfg_text.parse()?;
    let client_id = cfg
        .get("client_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing client_id in {cfg_path}"))?
        .to_string();
    let client_secret = cfg
        .get("client_secret")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing client_secret in {cfg_path}"))?
        .to_string();
    let oauth = auth::OAuthConfig {
        client_id,
        client_secret,
        token_url: auth::GOOGLE_TOKEN_URL.to_string(),
    };

    let tokens = auth::load_tokens(account)?
        .ok_or_else(|| anyhow::anyhow!("no tokens stored for account {account}"))?;
    if !tokens.is_expired() {
        return Ok(tokens.access_token);
    }
    let http = cascade_engine::portable::native::ReqwestClient::from_client(
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?,
    );
    let refreshed = auth::refresh_access_token(&http, &oauth, &tokens.refresh_token).await?;
    auth::save_tokens(account, &refreshed)?;
    Ok(refreshed.access_token)
}
