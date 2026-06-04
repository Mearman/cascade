//! CLI implementations for the managed-side management plane: local
//! administration of the grants a node confers on remote managers.
//!
//! These commands run against the local node's own grant and audit store — the
//! same [`StateDb`] the daemon reads when authorising an incoming
//! [`ManageRequest`](cascade_p2p::protocol::BepMessage::ManageRequest). A grant
//! created here takes effect for the next management request the node receives;
//! the audit log records every authorisation decision the dispatcher made.
//!
//! The node owner is the principal that issues a grant: a grant's `granted_by`
//! is stamped with this device's own identity, resolved from the first
//! configured P2P backend's identity. Granting a *dangerous* capability
//! ([`Capability::is_dangerous`]) over a node-wide / wildcard scope is refused —
//! a dangerous capability must name the exact folder subtree it applies to, so
//! it can never be smuggled in behind `--scope *`.

use anyhow::{Context as _, Result};
use cascade_engine::db::StateDb;
use cascade_engine::manage::{Capability, DeviceId, Grant, Scope};
use chrono::{DateTime, Utc};

use super::CliContext;
use super::init::CascadeConfig;

/// Resolve a P2P backend's user-facing name to its canonical BEP folder id.
///
/// The BEP folder id the runtime data-plane gate consults is always
/// `p2p-<name>`, where `<name>` is the user-facing name the operator passed
/// to `cascade backend add p2p --name <name>`. The data-plane gate and the
/// `Scope::folder(...)` value a grant stores must live in the same namespace —
/// otherwise `Scope::covers` returns `false` and the grant is a silent no-op.
/// This function is the single path that authors a data-verb grant: it maps
/// the operator-facing name to the canonical id at write time, so the stored
/// scope matches the value the runtime gate checks.
///
/// Unknown names are refused loudly with a list of every registered P2P
/// backend, so the operator can correct a typo without running
/// `cascade backend list` first. A registered backend of a different type
/// (for example `gdrive` or `s3`) is not eligible for directional sharing:
/// directional sharing applies to P2P folders only, so its name does not
/// appear in the registered-P2P list and the same loud error is returned.
pub(super) fn resolve_p2p_folder_id(ctx: &CliContext, name: &str) -> Result<String> {
    if name.trim().is_empty() {
        anyhow::bail!("a P2P backend name is required");
    }
    let db = StateDb::open(&ctx.db_path).context("opening the state database")?;
    let known: Vec<String> = db
        .list_backends()?
        .into_iter()
        .filter(|record| record.backend_type == "p2p")
        .map(|record| record.id)
        .collect();
    let resolved = known
        .iter()
        .find(|id| id.as_str() == name)
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no P2P backend named `{name}` is registered; directional sharing only \
                 applies to P2P folders. Registered P2P backends: [{}]",
                known.join(", "),
            )
        })?;
    Ok(format!("p2p-{resolved}"))
}

/// The wildcard scope token a user passes on the command line to mean
/// "node-wide" — every path on the node.
const SCOPE_WILDCARD: &str = "*";

/// Open the state database.
fn open_db(ctx: &CliContext) -> Result<StateDb> {
    StateDb::open(&ctx.db_path)
}

/// Parse a comma-separated capability list (`status:read,pin:write`) into the
/// typed vocabulary, failing loudly on an unrecognised or empty token rather
/// than silently dropping it.
fn parse_capabilities(raw: &str) -> Result<Vec<Capability>> {
    let mut capabilities = Vec::new();
    for token in raw.split(',') {
        let token = token.trim();
        if token.is_empty() {
            anyhow::bail!("empty capability in --cap list `{raw}`");
        }
        let capability = Capability::from_wire(token)
            .with_context(|| format!("unknown capability `{token}` in --cap list"))?;
        capabilities.push(capability);
    }
    if capabilities.is_empty() {
        anyhow::bail!("--cap requires at least one capability");
    }
    Ok(capabilities)
}

/// Parse the `--scope` argument into a [`Scope`].
///
/// The wildcard token [`SCOPE_WILDCARD`] maps to [`Scope::Node`]; any other
/// value is a folder path prefix.
fn parse_scope(raw: &str) -> Scope {
    if raw == SCOPE_WILDCARD {
        Scope::Node
    } else {
        Scope::folder(raw)
    }
}

/// Parse an optional RFC 3339 expiry timestamp.
fn parse_expiry(raw: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    raw.map(|value| {
        DateTime::parse_from_rfc3339(value)
            .with_context(|| format!("parsing --expires timestamp `{value}` (expected RFC 3339)"))
            .map(|dt| dt.with_timezone(&Utc))
    })
    .transpose()
}

/// Resolve the local node owner's device id — the principal that issues a
/// grant created on this node.
///
/// The owner is this device's own identity. It is read from the first
/// configured P2P backend's identity (the device-wide identity the data plane
/// also uses); without a configured P2P backend a node has no device identity
/// to stamp, so the command fails loudly rather than inventing a placeholder.
pub(super) fn resolve_owner_device(ctx: &CliContext) -> Result<DeviceId> {
    let identity = resolve_owner_identity(ctx)?;
    Ok(DeviceId::new(identity.device_id))
}

/// Resolve the local node owner's full device identity — certificate and private
/// key — for signing a capability token.
///
/// Reads the same first-configured P2P backend identity `resolve_owner_device`
/// resolves, but returns the whole [`cascade_p2p::identity::DeviceIdentity`] so the caller can sign with
/// the node's real private key. Without a configured P2P backend a node has no
/// device identity, so the command fails loudly rather than inventing one.
pub(super) fn resolve_owner_identity(
    ctx: &CliContext,
) -> Result<cascade_p2p::identity::DeviceIdentity> {
    let main_config_path = ctx.config_dir.join("config.toml");
    let main_config: CascadeConfig = if main_config_path.exists() {
        let raw = std::fs::read_to_string(&main_config_path)
            .with_context(|| format!("reading {}", main_config_path.display()))?;
        toml::from_str(&raw).with_context(|| format!("parsing {}", main_config_path.display()))?
    } else {
        anyhow::bail!(
            "no config.toml at {} — run `cascade init` before issuing grants",
            main_config_path.display()
        );
    };

    let p2p_name = main_config
        .backends
        .iter()
        .find_map(|(name, value)| {
            value
                .get("type")
                .and_then(toml::Value::as_str)
                .filter(|t| *t == "p2p")
                .map(|_| name.clone())
        })
        .context(
            "no P2P backend configured — the node has no device identity to stamp a grant's \
             issuer. Add one with `cascade backend-add p2p`.",
        )?;

    let backend_config_path = ctx.config_dir.join(format!("{p2p_name}.toml"));
    let raw = std::fs::read_to_string(&backend_config_path)
        .with_context(|| format!("reading {}", backend_config_path.display()))?;
    let backend_config: toml::Value = toml::from_str(&raw)
        .with_context(|| format!("parsing {}", backend_config_path.display()))?;
    cascade_backend_p2p::identity_from_config(&backend_config)
        .context("resolving local device identity")
}

/// `cascade grant add <device-id> --cap <c>[,<c>] --scope <path|*> [--expires <rfc3339>]`.
///
/// Confers each named capability on `grantee` over `scope`. A dangerous
/// capability over a node-wide / wildcard scope is refused before any grant is
/// written, so the rejection is all-or-nothing. On success one confirmation
/// line is printed per grant created.
pub fn add(
    ctx: &CliContext,
    grantee: &str,
    caps: &str,
    scope: &str,
    expires: Option<&str>,
) -> Result<()> {
    if grantee.trim().is_empty() {
        anyhow::bail!("grant add requires a non-empty device id");
    }
    let capabilities = parse_capabilities(caps)?;
    let parsed_scope = parse_scope(scope);
    let expiry = parse_expiry(expires)?;

    // A dangerous capability is never satisfied by a node-wide / wildcard grant,
    // so refuse to write one rather than create a grant that can never
    // authorise the command it names. Check every capability up front so the
    // command is all-or-nothing.
    //
    // A data verb (`data:read` / `data:write`) is also never satisfied by a
    // node-wide grant: the runtime data-plane gate keys on the BEP folder id
    // (`p2p-<name>`) and there is no such id at the node root. A node-wide data
    // grant is a silent no-op that would only confuse the operator, so it is
    // refused the same way as a dangerous capability. `cascade share` applies
    // the same bar by resolving the operator-facing name to the canonical id
    // before storing the grant, but `cascade grant` and the wire-side
    // `ManageCommand::GrantAdd` path are open-coded — they have to apply the
    // bar themselves.
    for capability in &capabilities {
        if parsed_scope.is_node_wide() && (capability.is_dangerous() || capability.is_data_verb()) {
            anyhow::bail!(
                "capability `{}` cannot be granted over a wildcard scope; \
                 name an explicit folder with --scope <path> (data verbs are \
                 folder-scoped, not node-wide)",
                capability.as_wire()
            );
        }
    }

    let owner = resolve_owner_device(ctx)?;
    let grantee_id = DeviceId::new(grantee.to_owned());
    let db = open_db(ctx)?;

    for capability in capabilities {
        let grant = Grant {
            grantee: grantee_id.clone(),
            capability,
            scope: parsed_scope.clone(),
            granted_by: owner.clone(),
            expires: expiry,
        };
        let id = db.insert_grant(&grant)?;
        let scope_label = scope_label(&parsed_scope);
        let expiry_label = expiry.map_or_else(
            || "never".to_owned(),
            |dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );
        println!(
            "Granted {} to {grantee} over {scope_label} (expires {expiry_label}) [grant {id}]",
            capability.as_wire(),
        );
    }
    Ok(())
}

/// `cascade grant list` — list every grant held on this node.
pub fn list(ctx: &CliContext) -> Result<()> {
    let db = open_db(ctx)?;
    let records = db.list_grants()?;
    if records.is_empty() {
        println!("No grants.");
        return Ok(());
    }
    println!("Grants:");
    for record in &records {
        let grant = &record.grant;
        let scope_label = scope_label(&grant.scope);
        let expiry_label = grant.expires.map_or_else(
            || "never".to_owned(),
            |dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );
        println!(
            "  [{}] {} -> {} over {scope_label} (granted by {}, expires {expiry_label})",
            record.id,
            grant.capability.as_wire(),
            grant.grantee,
            grant.granted_by,
        );
    }
    Ok(())
}

/// `cascade grant revoke <grant-id>` — remove a grant by its row id.
pub fn revoke(ctx: &CliContext, grant_id: i64) -> Result<()> {
    let db = open_db(ctx)?;
    if db.revoke_grant(grant_id)? {
        println!("Revoked grant {grant_id}.");
    } else {
        println!("No grant with id {grant_id}.");
    }
    Ok(())
}

/// `cascade grant audit` — print the append-only management audit log.
pub fn audit(ctx: &CliContext) -> Result<()> {
    let db = open_db(ctx)?;
    let records = db.list_audit()?;
    if records.is_empty() {
        println!("No audit entries.");
        return Ok(());
    }
    println!("Management audit log:");
    for record in &records {
        let entry = &record.entry;
        let scope_label = scope_label(&entry.scope);
        let timestamp = entry
            .timestamp
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        println!(
            "  [{}] {timestamp} {} {} over {scope_label} -> {} ({})",
            record.id,
            entry.actor_device,
            entry.capability.as_wire(),
            entry.outcome,
            entry.command,
        );
    }
    Ok(())
}

/// Human-readable label for a scope: the wildcard token for a node-wide scope,
/// the path for a folder scope.
pub fn scope_label(scope: &Scope) -> String {
    match scope {
        Scope::Node => SCOPE_WILDCARD.to_owned(),
        Scope::Folder { path } => path.clone(),
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

    /// Write a minimal config.toml plus a p2p backend config so
    /// `resolve_owner_device` finds a device identity to stamp.
    fn seed_p2p_owner(ctx: &CliContext) {
        std::fs::create_dir_all(&ctx.config_dir).unwrap();
        let main = "[backends.shared]\ntype = \"p2p\"\n";
        std::fs::write(ctx.config_dir.join("config.toml"), main).unwrap();
        let data_dir = ctx.config_dir.join("p2p-data");
        // A TOML literal string (single quotes) so Windows paths — whose
        // backslashes are escape sequences in a basic string — round-trip
        // verbatim. This is exactly the case literal strings exist for.
        let backend = format!(
            "type = \"p2p\"\nname = \"shared\"\ndata_dir = '{}'\n",
            data_dir.display()
        );
        std::fs::write(ctx.config_dir.join("shared.toml"), backend).unwrap();
    }

    #[test]
    fn parse_capabilities_accepts_known_list() {
        let caps = parse_capabilities("status:read,pin:write").unwrap();
        assert_eq!(caps, vec![Capability::StatusRead, Capability::PinWrite]);
    }

    #[test]
    fn parse_capabilities_rejects_unknown() {
        assert!(parse_capabilities("status:read,totally:bogus").is_err());
    }

    #[test]
    fn parse_capabilities_rejects_empty_token() {
        assert!(parse_capabilities("status:read,").is_err());
        assert!(parse_capabilities("").is_err());
    }

    #[test]
    fn parse_scope_maps_wildcard_to_node() {
        assert_eq!(parse_scope("*"), Scope::Node);
        assert_eq!(parse_scope("/work"), Scope::folder("/work"));
    }

    #[test]
    fn parse_expiry_round_trips_rfc3339() {
        let parsed = parse_expiry(Some("2026-12-31T00:00:00Z")).unwrap();
        assert!(parsed.is_some());
        assert!(parse_expiry(None).unwrap().is_none());
        assert!(parse_expiry(Some("not-a-timestamp")).is_err());
    }

    #[test]
    fn add_safe_capability_writes_grant() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_p2p_owner(&ctx);

        add(&ctx, "MANAGER", "status:read,pin:write", "/work", None).unwrap();

        let db = StateDb::open(&ctx.db_path).unwrap();
        let grants = db.list_grants().unwrap();
        assert_eq!(grants.len(), 2);
        assert!(grants.iter().all(|g| g.grant.grantee.as_str() == "MANAGER"));
        assert!(
            grants
                .iter()
                .all(|g| g.grant.scope == Scope::folder("/work"))
        );
        // granted_by must be this device's own identity, not a placeholder.
        let owner = resolve_owner_device(&ctx).unwrap();
        assert!(grants.iter().all(|g| g.grant.granted_by == owner));
    }

    #[test]
    fn add_dangerous_capability_over_wildcard_is_rejected() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_p2p_owner(&ctx);

        // A dangerous capability over `*` must be refused with a clear error,
        // and must NOT write any grant.
        let result = add(&ctx, "MANAGER", "backend:manage", "*", None);
        assert!(
            result.is_err(),
            "dangerous capability over * must be refused"
        );
        let message = format!("{:#}", result.unwrap_err());
        assert!(
            message.contains("wildcard") && message.contains("--scope"),
            "error must explain the wildcard rejection and the fix, got: {message}",
        );

        let db = StateDb::open(&ctx.db_path).unwrap();
        assert!(
            db.list_grants().unwrap().is_empty(),
            "no grant may be written when a dangerous capability is refused",
        );
    }

    #[test]
    fn add_dangerous_capability_is_all_or_nothing_in_mixed_list() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_p2p_owner(&ctx);

        // A list mixing a safe and a dangerous capability over `*` must refuse
        // the WHOLE command — the safe grant must not be written either.
        let result = add(&ctx, "MANAGER", "status:read,grant:admin", "*", None);
        assert!(result.is_err());

        let db = StateDb::open(&ctx.db_path).unwrap();
        assert!(
            db.list_grants().unwrap().is_empty(),
            "a refused dangerous capability must not leave a partial grant behind",
        );
    }

    #[test]
    fn add_data_verb_over_wildcard_is_refused() {
        // F4: a data-verb grant over a wildcard scope is a silent
        // no-op at the runtime gate (the gate keys on `p2p-<name>`, not
        // the node root), so refuse it here so the operator cannot
        // accidentally write a row that can never authorise a frame.
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_p2p_owner(&ctx);

        for cap in ["data:read", "data:write"] {
            let result = add(&ctx, "PEER", cap, "*", None);
            assert!(
                result.is_err(),
                "data-verb {cap} over wildcard must be refused (F4)"
            );
            let message = format!("{:#}", result.unwrap_err());
            assert!(
                message.contains("wildcard") && message.contains("--scope"),
                "error must explain the wildcard rejection, got: {message}",
            );
        }

        // Nothing was written.
        let db = StateDb::open(&ctx.db_path).unwrap();
        assert!(db.list_grants().unwrap().is_empty());
    }

    #[test]
    fn add_dangerous_capability_over_folder_is_allowed() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_p2p_owner(&ctx);

        add(&ctx, "MANAGER", "backend:manage", "/work", None).unwrap();
        let db = StateDb::open(&ctx.db_path).unwrap();
        let grants = db.list_grants().unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].grant.capability, Capability::BackendManage);
    }

    #[test]
    fn revoke_removes_grant() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_p2p_owner(&ctx);

        add(&ctx, "MANAGER", "status:read", "*", None).unwrap();
        let db = StateDb::open(&ctx.db_path).unwrap();
        let id = db.list_grants().unwrap()[0].id;
        drop(db);

        revoke(&ctx, id).unwrap();
        let db = StateDb::open(&ctx.db_path).unwrap();
        assert!(db.list_grants().unwrap().is_empty());
    }

    #[test]
    fn revoke_unknown_id_succeeds() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_p2p_owner(&ctx);
        // Revoking a non-existent id is not an error — it prints "no grant".
        revoke(&ctx, 999).unwrap();
    }

    #[test]
    fn list_and_audit_empty() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_p2p_owner(&ctx);
        let _db = StateDb::open(&ctx.db_path).unwrap();
        list(&ctx).unwrap();
        audit(&ctx).unwrap();
    }

    #[test]
    fn add_rejects_empty_device_id() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_p2p_owner(&ctx);
        assert!(add(&ctx, "  ", "status:read", "*", None).is_err());
    }
}
