//! CLI for signed capability tokens — portable, offline-issuable grants.
//!
//! A token is the portable form of an on-node grant: the node's own device key
//! signs a statement conferring a capability over a scope, with an expiry, onto
//! a bearer device. The bearer carries the token and presents it when it issues
//! a remote command (`cascade remote <node> --token <file> …`); the node
//! verifies it and authorises the command against the token-carried grant
//! exactly as it would an on-node grant row. These commands run against the
//! local node's own store — the same [`StateDb`] the daemon reads when verifying
//! a presented token — so a token issued here takes effect for the next request
//! the node receives, and a revocation here is consulted at every verify.
//!
//! Issuing a *dangerous* capability ([`Capability::is_dangerous`]) over a
//! node-wide / wildcard scope is refused, the same bar `cascade grant add`
//! applies: a dangerous capability must name the exact folder subtree it covers.

use anyhow::{Context as _, Result};
use cascade_engine::db::{AuditEntry, StateDb};
use cascade_engine::manage::token::{CapabilityToken, derive_token_id};
use cascade_engine::manage::{Capability, DeviceId, Scope};
use chrono::{DateTime, Utc};

use super::CliContext;
use super::grant::{resolve_owner_device, resolve_owner_identity};

/// The wildcard scope token a user passes on the command line to mean
/// "node-wide".
const SCOPE_WILDCARD: &str = "*";

/// The `cascade token <subcommand>` surface.
pub enum TokenCommand {
    /// Mint a token for a bearer, print its JSON.
    Issue {
        /// Device id of the bearer the token authorises.
        bearer: String,
        /// The capability conferred, in colon-delimited wire form.
        capability: String,
        /// Scope: a folder path prefix, or `*` for node-wide.
        scope: String,
        /// RFC 3339 expiry timestamp. A token always expires.
        expires: String,
    },
    /// Revoke a token by its id.
    Revoke {
        /// The token id (as printed by `token issue` / `token list`).
        token_id: String,
    },
    /// List every token this node has issued.
    List,
}

/// Open the state database.
fn open_db(ctx: &CliContext) -> Result<StateDb> {
    StateDb::open(&ctx.db_path)
}

/// Parse a single capability from its wire form, failing loudly on an unknown
/// token.
fn parse_capability(raw: &str) -> Result<Capability> {
    let token = raw.trim();
    Capability::from_wire(token).with_context(|| format!("unknown capability `{token}`"))
}

/// Parse the `--scope` argument into a [`Scope`]; `*` maps to node-wide.
fn parse_scope(raw: &str) -> Scope {
    if raw == SCOPE_WILDCARD {
        Scope::Node
    } else {
        Scope::folder(raw)
    }
}

/// Parse a required RFC 3339 expiry timestamp.
fn parse_expiry(raw: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .with_context(|| format!("parsing expiry timestamp `{raw}` (expected RFC 3339)"))
        .map(|dt| dt.with_timezone(&Utc))
}

/// Run a `cascade token` subcommand.
pub fn run(ctx: &CliContext, command: TokenCommand) -> Result<()> {
    match command {
        TokenCommand::Issue {
            bearer,
            capability,
            scope,
            expires,
        } => issue(ctx, &bearer, &capability, &scope, &expires),
        TokenCommand::Revoke { token_id } => revoke(ctx, &token_id),
        TokenCommand::List => list(ctx),
    }
}

/// `cascade token issue <bearer> --cap <c> --scope <path|*> --expires <rfc3339>`.
///
/// Mints a token signed by this node's device key, persists it for listing and
/// reprint, records the issuance in the audit log, and prints the token JSON for
/// the bearer to carry. A dangerous capability over a wildcard scope is refused
/// before anything is written.
fn issue(
    ctx: &CliContext,
    bearer: &str,
    capability: &str,
    scope: &str,
    expires: &str,
) -> Result<()> {
    if bearer.trim().is_empty() {
        anyhow::bail!("token issue requires a non-empty bearer device id");
    }
    let capability = parse_capability(capability)?;
    let scope = parse_scope(scope);
    let expiry = parse_expiry(expires)?;

    if capability.is_dangerous() && scope.is_node_wide() {
        anyhow::bail!(
            "capability `{}` is dangerous and cannot be issued over a wildcard scope; name an \
             explicit folder with --scope <path>",
            capability.as_wire()
        );
    }

    // The issuer is this node — the device whose real private key signs the
    // token, the same identity a verifier roots the token's chain in. The full
    // identity (certificate and private key) is loaded so the signature is a
    // genuine proof of node-key possession, not anything derivable from the
    // public device id.
    let issuer_identity = resolve_owner_identity(ctx)?;
    let issuer = DeviceId::new(issuer_identity.device_id.clone());
    let bearer_id = DeviceId::new(bearer.to_owned());
    let issued_at = Utc::now();
    let token_id = derive_token_id(&issuer, &bearer_id, capability, &scope, expiry, issued_at);

    let token = CapabilityToken::issue(
        token_id.clone(),
        &issuer_identity,
        &bearer_id,
        capability,
        scope.clone(),
        expiry,
    )
    .context("signing the capability token with this node's device key")?;
    let token_json =
        serde_json::to_string(&token).context("serialising the issued capability token")?;

    let db = open_db(ctx)?;
    db.insert_token(&token, issued_at)
        .context("recording the issued token")?;
    db.append_audit(&AuditEntry {
        timestamp: issued_at,
        actor_device: issuer,
        capability,
        scope,
        command: format!("token issue {token_id} to {bearer}"),
        outcome: "allowed".to_owned(),
    })
    .context("recording the token issuance in the audit log")?;

    println!("{token_json}");
    eprintln!(
        "Issued token {token_id}: {} to {bearer} (expires {})",
        capability.as_wire(),
        expiry.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    );
    Ok(())
}

/// `cascade token revoke <token-id>` — add a token id to the revocation list.
fn revoke(ctx: &CliContext, token_id: &str) -> Result<()> {
    if token_id.trim().is_empty() {
        anyhow::bail!("token revoke requires a non-empty token id");
    }
    let issuer = resolve_owner_device(ctx)?;
    let now = Utc::now();
    let db = open_db(ctx)?;
    let newly = db
        .revoke_token(token_id, now)
        .context("recording the token revocation")?;
    db.append_audit(&AuditEntry {
        timestamp: now,
        actor_device: issuer,
        capability: Capability::GrantAdmin,
        scope: Scope::Node,
        command: format!("token revoke {token_id}"),
        outcome: if newly { "allowed" } else { "noop" }.to_owned(),
    })
    .context("recording the revocation in the audit log")?;

    if newly {
        println!("Revoked token {token_id}.");
    } else {
        println!("Token {token_id} was already revoked.");
    }
    Ok(())
}

/// `cascade token list` — list every token this node has issued.
fn list(ctx: &CliContext) -> Result<()> {
    let db = open_db(ctx)?;
    let records = db.list_tokens()?;
    if records.is_empty() {
        println!("No tokens issued.");
        return Ok(());
    }
    let revoked = db.revoked_token_ids()?;
    println!("Issued tokens:");
    for record in &records {
        let claims = &record.token.claims;
        let scope_label = match &claims.scope {
            Scope::Node => SCOPE_WILDCARD.to_owned(),
            Scope::Folder { path } => path.clone(),
        };
        let status = if revoked.contains(&claims.token_id) {
            " [revoked]"
        } else {
            ""
        };
        println!(
            "  {} {} -> {} over {scope_label} (expires {}){status}",
            claims.token_id,
            claims.capability.as_wire(),
            claims.bearer,
            claims
                .expires
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );
    }
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

    /// Write a minimal config.toml plus a p2p backend config so
    /// `resolve_owner_device` finds a device identity to sign with.
    fn seed_p2p_owner(ctx: &CliContext) {
        std::fs::create_dir_all(&ctx.config_dir).unwrap();
        std::fs::write(
            ctx.config_dir.join("config.toml"),
            "[backends.shared]\ntype = \"p2p\"\n",
        )
        .unwrap();
        let data_dir = ctx.config_dir.join("p2p-data");
        let backend = format!(
            "type = \"p2p\"\nname = \"shared\"\ndata_dir = '{}'\n",
            data_dir.display()
        );
        std::fs::write(ctx.config_dir.join("shared.toml"), backend).unwrap();
    }

    #[test]
    fn issue_writes_a_verifiable_token_and_audits() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_p2p_owner(&ctx);

        issue(&ctx, "BEARER", "pin:write", "/work", "2027-01-01T00:00:00Z").unwrap();

        let db = StateDb::open(&ctx.db_path).unwrap();
        let tokens = db.list_tokens().unwrap();
        assert_eq!(tokens.len(), 1);
        let claims = &tokens[0].token.claims;
        assert_eq!(claims.bearer, DeviceId::new("BEARER"));
        assert_eq!(claims.capability, Capability::PinWrite);
        assert_eq!(claims.scope, Scope::folder("/work"));

        // The token verifies against this node, presented by its bearer.
        let issuer = resolve_owner_device(&ctx).unwrap();
        let verified = tokens[0].token.verify(
            &issuer,
            &DeviceId::new("BEARER"),
            chrono::Utc::now(),
            &|_id| false,
        );
        assert!(
            verified.is_ok(),
            "issued token must verify against its node"
        );

        // The issuance was audited.
        let audit = db.list_audit().unwrap();
        assert_eq!(audit.len(), 1);
        assert!(audit[0].entry.command.starts_with("token issue"));
    }

    #[test]
    fn issue_dangerous_capability_over_wildcard_is_refused() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_p2p_owner(&ctx);

        let result = issue(
            &ctx,
            "BEARER",
            "backend:manage",
            "*",
            "2027-01-01T00:00:00Z",
        );
        assert!(result.is_err());
        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains("dangerous") && message.contains("wildcard"));

        // Nothing was written.
        let db = StateDb::open(&ctx.db_path).unwrap();
        assert!(db.list_tokens().unwrap().is_empty());
    }

    #[test]
    fn issue_rejects_bad_expiry() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_p2p_owner(&ctx);
        assert!(issue(&ctx, "BEARER", "pin:write", "/work", "not-a-time").is_err());
    }

    #[test]
    fn revoke_then_list_marks_revoked() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_p2p_owner(&ctx);
        issue(&ctx, "BEARER", "pin:write", "/work", "2027-01-01T00:00:00Z").unwrap();

        let db = StateDb::open(&ctx.db_path).unwrap();
        let token_id = db.list_tokens().unwrap()[0].token.claims.token_id.clone();
        drop(db);

        revoke(&ctx, &token_id).unwrap();

        let db = StateDb::open(&ctx.db_path).unwrap();
        assert!(db.is_token_revoked(&token_id).unwrap());
        // A second revoke is a no-op, not an error.
        drop(db);
        revoke(&ctx, &token_id).unwrap();
    }

    #[test]
    fn list_empty_is_fine() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_p2p_owner(&ctx);
        let _db = StateDb::open(&ctx.db_path).unwrap();
        list(&ctx).unwrap();
    }

    #[test]
    fn issue_rejects_empty_bearer() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_p2p_owner(&ctx);
        assert!(issue(&ctx, "  ", "pin:write", "/work", "2027-01-01T00:00:00Z").is_err());
    }
}
