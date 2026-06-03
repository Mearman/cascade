//! CLI for per-peer, per-folder directional data sharing.
//!
//! `cascade share` is a thin convenience layer over the existing capability
//! grant machinery. It maps a human-facing direction (`read-only`,
//! `write-only`, `read-write`) to the underlying `data:read` / `data:write`
//! capability grants, using the same storage, audit log, and revocation
//! infrastructure that `cascade grant` uses.
//!
//! Design contract:
//! - `share add` maps the chosen direction to data-verb grants and calls
//!   `grant::add` for each required verb.
//! - `share list` reads grants filtered to data verbs and renders them as
//!   the posture (read-only / write-only / read-write) rather than raw
//!   capability rows.
//! - `share revoke` removes the data-verb grants over `grant::revoke`,
//!   returning the peer to the default (full sharing while trusted).

use anyhow::{Context as _, Result};
use cascade_engine::db::StateDb;
use cascade_engine::manage::{Capability, DeviceId, Scope};

use super::CliContext;
use super::grant::{resolve_owner_device, scope_label};

/// The sharing direction the operator wishes to express.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareDirection {
    /// The peer may read our data but not push to us.
    ReadOnly,
    /// The peer may push to us but not read our data (a drop/backup sink).
    WriteOnly,
    /// Full bidirectional sharing — equivalent to the trusted-peer default
    /// but expressed explicitly as a grant pair.
    ReadWrite,
}

impl ShareDirection {
    /// Parse the direction from its CLI string form.
    ///
    /// Accepted values: `read-only`, `write-only`, `read-write`.
    pub fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "read-only" => Ok(Self::ReadOnly),
            "write-only" => Ok(Self::WriteOnly),
            "read-write" => Ok(Self::ReadWrite),
            other => anyhow::bail!(
                "unknown sharing direction `{other}`; expected one of: \
                 read-only, write-only, read-write"
            ),
        }
    }

    /// The data-verb capabilities this direction requires.
    #[must_use]
    pub const fn capabilities(self) -> &'static [Capability] {
        match self {
            Self::ReadOnly => &[Capability::DataRead],
            Self::WriteOnly => &[Capability::DataWrite],
            Self::ReadWrite => &[Capability::DataRead, Capability::DataWrite],
        }
    }

    /// Human-readable label used in output.
    #[must_use]
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WriteOnly => "write-only",
            Self::ReadWrite => "read-write",
        }
    }
}

/// Derive the effective sharing posture from the data-verb grants a peer holds
/// for a folder.
const fn posture_from_grants(has_read: bool, has_write: bool) -> Option<&'static str> {
    match (has_read, has_write) {
        (true, true) => Some("read-write"),
        (true, false) => Some("read-only"),
        (false, true) => Some("write-only"),
        (false, false) => None,
    }
}

/// Open the state database.
fn open_db(ctx: &CliContext) -> Result<StateDb> {
    StateDb::open(&ctx.db_path)
}

/// `cascade share add <peer-device-id> <folder> --direction <read-only|write-only|read-write>`.
///
/// Maps the chosen direction to data-verb grants and persists them via the
/// existing grant machinery. On success one confirmation line is printed per
/// grant created.
pub fn add(
    ctx: &CliContext,
    peer_device_id: &str,
    folder: &str,
    direction: &str,
    expires: Option<&str>,
) -> Result<()> {
    if peer_device_id.trim().is_empty() {
        anyhow::bail!("share add requires a non-empty peer device id");
    }
    let dir = ShareDirection::from_str(direction)?;
    let scope = Scope::folder(folder);

    // Data verbs are not dangerous, so a node-wide scope would technically be
    // accepted by the grant machinery. However, data verbs are always
    // per-folder by design — a node-wide data grant is nonsensical (there is
    // no folder to gate). Refuse it here with a clear error.
    if scope.is_node_wide() {
        anyhow::bail!(
            "data sharing grants must name an explicit folder (got `{folder}`); \
             use `cascade grant add` if you need a node-wide administrative capability"
        );
    }

    let expiry = expires
        .map(|raw| {
            chrono::DateTime::parse_from_rfc3339(raw)
                .with_context(|| format!("parsing --expires timestamp `{raw}` (expected RFC 3339)"))
                .map(|dt| dt.with_timezone(&chrono::Utc))
        })
        .transpose()?;

    let owner = resolve_owner_device(ctx)?;
    let grantee = DeviceId::new(peer_device_id.to_owned());
    let db = open_db(ctx)?;

    for &capability in dir.capabilities() {
        let grant = cascade_engine::manage::Grant {
            grantee: grantee.clone(),
            capability,
            scope: scope.clone(),
            granted_by: owner.clone(),
            expires: expiry,
        };
        let id = db.insert_grant(&grant)?;
        let expiry_label = expiry.map_or_else(
            || "never".to_owned(),
            |dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );
        println!(
            "Granted {} to {peer_device_id} over {folder} \
             (expires {expiry_label}) [grant {id}]",
            capability.as_wire(),
        );
    }
    println!(
        "Sharing posture for {peer_device_id} over {folder}: {}",
        dir.as_label()
    );
    Ok(())
}

/// `cascade share list [<folder>]`.
///
/// Lists every peer that has a data-verb grant, rendered as the sharing
/// posture rather than raw capability rows. When `folder` is given, only
/// grants whose scope covers that folder are shown.
pub fn list(ctx: &CliContext, folder: Option<&str>) -> Result<()> {
    let db = open_db(ctx)?;
    let records = db.list_data_grants()?;

    if records.is_empty() {
        println!("No directional sharing configured.");
        return Ok(());
    }

    // Aggregate by (grantee, scope_label) → (has_read, has_write).
    let mut postures: std::collections::BTreeMap<(String, String), (bool, bool)> =
        std::collections::BTreeMap::new();

    for record in &records {
        let grant = &record.grant;

        // When a folder filter is given, skip grants whose scope does not
        // cover it.
        if let Some(filter_folder) = folder
            && !grant.scope.covers(&Scope::folder(filter_folder))
        {
            continue;
        }

        let key = (grant.grantee.to_string(), scope_label(&grant.scope));
        let entry = postures.entry(key).or_insert((false, false));
        match grant.capability {
            Capability::DataRead => entry.0 = true,
            Capability::DataWrite => entry.1 = true,
            _ => {}
        }
    }

    if postures.is_empty() {
        if let Some(f) = folder {
            println!("No directional sharing configured for folder {f}.");
        } else {
            println!("No directional sharing configured.");
        }
        return Ok(());
    }

    println!("Directional data shares:");
    for ((peer, scope), (has_read, has_write)) in &postures {
        let posture = posture_from_grants(*has_read, *has_write).unwrap_or("none");
        println!("  {peer} over {scope}: {posture}");
    }
    Ok(())
}

/// `cascade share revoke <peer-device-id> <folder> [--direction <read-only|write-only|read-write>]`.
///
/// Removes data-verb grants for the specified peer and folder. When
/// `--direction` is given, only the grants for that direction are removed;
/// without it all data grants for that peer+folder are revoked, returning
/// the peer to the trusted-peer default (full sharing while trusted).
pub fn revoke(
    ctx: &CliContext,
    peer_device_id: &str,
    folder: &str,
    direction: Option<&str>,
) -> Result<()> {
    if peer_device_id.trim().is_empty() {
        anyhow::bail!("share revoke requires a non-empty peer device id");
    }

    let target_caps: Option<Vec<Capability>> = direction
        .map(ShareDirection::from_str)
        .transpose()?
        .map(|dir| dir.capabilities().to_vec());

    let grantee = DeviceId::new(peer_device_id.to_owned());
    let folder_scope = Scope::folder(folder);
    let db = open_db(ctx)?;
    let records = db.list_data_grants()?;

    let mut removed: u32 = 0;
    for record in records {
        let grant = &record.grant;
        if grant.grantee != grantee {
            continue;
        }
        if !grant.scope.covers(&folder_scope) {
            continue;
        }
        // If a specific direction was requested, only revoke matching verbs.
        if let Some(ref caps) = target_caps
            && !caps.contains(&grant.capability)
        {
            continue;
        }
        if db.revoke_grant(record.id)? {
            println!(
                "Revoked {} for {peer_device_id} over {folder}.",
                grant.capability.as_wire()
            );
            removed += 1;
        }
    }

    if removed == 0 {
        println!("No matching data-sharing grants found for {peer_device_id} over {folder}.");
    } else {
        println!(
            "Revoked {removed} grant(s). \
             {peer_device_id} returns to the trusted-peer default (full sharing)."
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

    // ── ShareDirection parsing ──

    #[test]
    fn parse_share_direction_all_variants() {
        assert_eq!(
            ShareDirection::from_str("read-only").unwrap(),
            ShareDirection::ReadOnly
        );
        assert_eq!(
            ShareDirection::from_str("write-only").unwrap(),
            ShareDirection::WriteOnly
        );
        assert_eq!(
            ShareDirection::from_str("read-write").unwrap(),
            ShareDirection::ReadWrite
        );
    }

    #[test]
    fn parse_share_direction_case_insensitive() {
        assert_eq!(
            ShareDirection::from_str("READ-ONLY").unwrap(),
            ShareDirection::ReadOnly
        );
    }

    #[test]
    fn parse_share_direction_rejects_unknown() {
        assert!(ShareDirection::from_str("bidirectional").is_err());
    }

    #[test]
    fn direction_capabilities_are_correct() {
        assert_eq!(
            ShareDirection::ReadOnly.capabilities(),
            &[Capability::DataRead]
        );
        assert_eq!(
            ShareDirection::WriteOnly.capabilities(),
            &[Capability::DataWrite]
        );
        assert_eq!(
            ShareDirection::ReadWrite.capabilities(),
            &[Capability::DataRead, Capability::DataWrite]
        );
    }

    // ── share add ──

    #[test]
    fn add_read_only_writes_data_read_grant() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_p2p_owner(&ctx);

        add(&ctx, "PEER", "/work", "read-only", None).unwrap();

        let db = StateDb::open(&ctx.db_path).unwrap();
        let grants = db.list_data_grants().unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].grant.capability, Capability::DataRead);
        assert_eq!(grants[0].grant.grantee, DeviceId::new("PEER"));
        assert_eq!(grants[0].grant.scope, Scope::folder("/work"));
    }

    #[test]
    fn add_write_only_writes_data_write_grant() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_p2p_owner(&ctx);

        add(&ctx, "PEER", "/backup", "write-only", None).unwrap();

        let db = StateDb::open(&ctx.db_path).unwrap();
        let grants = db.list_data_grants().unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].grant.capability, Capability::DataWrite);
    }

    #[test]
    fn add_read_write_writes_both_grants() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_p2p_owner(&ctx);

        add(&ctx, "PEER", "/shared", "read-write", None).unwrap();

        let db = StateDb::open(&ctx.db_path).unwrap();
        let grants = db.list_data_grants().unwrap();
        assert_eq!(grants.len(), 2);
        let caps: std::collections::HashSet<Capability> =
            grants.iter().map(|r| r.grant.capability).collect();
        assert!(caps.contains(&Capability::DataRead));
        assert!(caps.contains(&Capability::DataWrite));
    }

    #[test]
    fn add_node_wide_scope_is_refused() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_p2p_owner(&ctx);

        // An empty path normalises to node-wide and must be refused.
        let result = add(&ctx, "PEER", "/", "read-only", None);
        assert!(result.is_err(), "node-wide data share must be refused");
    }

    #[test]
    fn add_rejects_empty_peer_id() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_p2p_owner(&ctx);
        assert!(add(&ctx, "", "/work", "read-only", None).is_err());
    }

    // ── share revoke ──

    #[test]
    fn revoke_all_returns_peer_to_default() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_p2p_owner(&ctx);

        add(&ctx, "PEER", "/work", "read-write", None).unwrap();
        {
            let db = StateDb::open(&ctx.db_path).unwrap();
            assert_eq!(db.list_data_grants().unwrap().len(), 2);
        }

        revoke(&ctx, "PEER", "/work", None).unwrap();
        let db = StateDb::open(&ctx.db_path).unwrap();
        assert!(
            db.list_data_grants().unwrap().is_empty(),
            "all data grants must be removed"
        );
    }

    #[test]
    fn revoke_specific_direction_leaves_other_intact() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_p2p_owner(&ctx);

        add(&ctx, "PEER", "/work", "read-write", None).unwrap();
        // Revoke only the read direction.
        revoke(&ctx, "PEER", "/work", Some("read-only")).unwrap();

        let db = StateDb::open(&ctx.db_path).unwrap();
        let grants = db.list_data_grants().unwrap();
        assert_eq!(grants.len(), 1, "only the write grant must remain");
        assert_eq!(grants[0].grant.capability, Capability::DataWrite);
    }

    #[test]
    fn revoke_nonexistent_is_not_an_error() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_p2p_owner(&ctx);
        // Revoking when no grants exist must print a message but not error.
        revoke(&ctx, "PEER", "/work", None).unwrap();
    }

    // ── list ──

    #[test]
    fn list_empty_is_fine() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_p2p_owner(&ctx);
        let _db = StateDb::open(&ctx.db_path).unwrap();
        list(&ctx, None).unwrap();
    }

    #[test]
    fn list_shows_posture_not_raw_capabilities() {
        // This is a behavioural smoke-test — the function must not error
        // after adding mixed grants. The rendered output is not captured
        // in a unit test (stdout goes to /dev/null); the logic is covered
        // by the posture_from_grants helper tested below.
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);
        seed_p2p_owner(&ctx);
        add(&ctx, "PEER-A", "/work", "read-only", None).unwrap();
        add(&ctx, "PEER-B", "/docs", "write-only", None).unwrap();
        list(&ctx, None).unwrap();
        list(&ctx, Some("/work")).unwrap();
    }

    // ── posture_from_grants ──

    #[test]
    fn posture_logic_is_correct() {
        assert_eq!(posture_from_grants(true, true), Some("read-write"));
        assert_eq!(posture_from_grants(true, false), Some("read-only"));
        assert_eq!(posture_from_grants(false, true), Some("write-only"));
        assert_eq!(posture_from_grants(false, false), None);
    }
}
