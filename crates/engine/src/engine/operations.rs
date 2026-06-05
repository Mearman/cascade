//! Engine-level operations that mutate the state database or VFS tree.
//!
//! These functions are the implementation behind [`super::Engine`] public
//! methods that deal with config pushes, lifecycle policies, and backend
//! lifecycle. Extracted from the monolithic engine so the core struct stays
//! focused on wiring and lifecycle.

use std::path::PathBuf;

use anyhow::Result;

use super::Engine;
use crate::manage::Scope;

/// Merge a parsed `.cascade` fragment rooted at `folder` into the node's
/// rule set.
///
/// See [`Engine::config_push`] for the full contract.
pub(super) fn config_push(
    engine: &Engine,
    folder: &str,
    config: &cascade_config::CascadeConfig,
) -> Result<String> {
    let folder_scope = Scope::folder(folder.to_owned());

    // Resolve and confine every rule path before applying any, so a single
    // escaping rule rejects the entire push without partial application.
    let pin_paths = config
        .pin
        .iter()
        .map(|pin| confine_rule_path(folder, &pin.path, &folder_scope))
        .collect::<Result<Vec<_>>>()?;

    let policy_inputs = config
        .lifecycle
        .iter()
        .map(|policy| {
            let path = confine_rule_path(folder, &policy.path, &folder_scope)?;
            let max_age = policy
                .max_age
                .as_deref()
                .map(parse_duration_secs)
                .transpose()?;
            let max_file_size = policy
                .max_file_size
                .as_deref()
                .map(parse_size_bytes)
                .transpose()?;
            Ok::<_, anyhow::Error>((path, max_age, max_file_size, policy.priority))
        })
        .collect::<Result<Vec<_>>>()?;

    let pins_applied = pin_paths.len();
    for path in pin_paths {
        engine.db.add_pin_rule(&path, true, None)?;
    }

    let policies_applied = policy_inputs.len();
    for (path, max_age, max_file_size, priority) in policy_inputs {
        engine
            .db
            .add_lifecycle_policy(&path, max_age, max_file_size, priority, None)?;
    }

    Ok(format!(
        "config push into {folder}: {pins_applied} pin rule(s), {policies_applied} lifecycle policy/policies applied",
    ))
}

/// Set a single lifecycle policy on the node.
pub(super) fn policy_set(
    engine: &Engine,
    path_glob: &str,
    max_age_secs: Option<i64>,
    max_file_size: Option<i64>,
    priority: i32,
) -> Result<String> {
    engine
        .db
        .add_lifecycle_policy(path_glob, max_age_secs, max_file_size, priority, None)?;
    Ok(format!("lifecycle policy set for {path_glob}"))
}

/// Register and mount a backend at runtime.
pub(super) fn backend_add(
    engine: &Engine,
    name: &str,
    backend_type: &str,
    mount_path: &str,
    config_toml: &str,
) -> Result<String> {
    let factory = engine.backend_factory.as_ref().ok_or_else(|| {
        anyhow::anyhow!("this node cannot add backends: no backend factory is configured")
    })?;
    let backend = factory.create(name, backend_type, config_toml)?;
    engine.db.register_backend(
        name,
        backend_type,
        backend.display_name(),
        Some(mount_path),
        Some(config_toml),
    )?;
    engine.mount_backend(PathBuf::from(mount_path), backend);
    Ok(format!(
        "backend {name} ({backend_type}) added at {mount_path}",
    ))
}

/// Unmount and deregister a backend by name.
pub(super) fn backend_remove(
    engine: &Engine,
    name: &str,
    mount_path: &str,
) -> Result<String> {
    engine.unmount_backend(std::path::Path::new(mount_path));
    let removed = engine.db.remove_backend(name)?;
    Ok(if removed {
        format!("backend {name} removed from {mount_path}")
    } else {
        format!("no backend named {name} was registered")
    })
}

// ── Max file length rule operations ──

/// Add a max file length rule.
///
/// Files matching `path_glob` that exceed `max_bytes` will be skipped during
/// sync. Rules are ordered by `priority` (higher wins). An optional
/// `conditions` expression is evaluated against the engine's `EvalContext`.
pub(super) fn add_max_file_length_rule(
    engine: &Engine,
    path_glob: &str,
    max_bytes: u64,
    priority: i32,
    conditions: Option<&str>,
) -> Result<()> {
    engine
        .db
        .add_max_file_length_rule(path_glob, max_bytes, priority, conditions)
}

/// List all max file length rules, ordered by priority descending.
pub(super) fn list_max_file_length_rules(
    engine: &Engine,
) -> Result<Vec<crate::db::MaxFileLengthRecord>> {
    engine.db.list_max_file_length_rules()
}

/// Remove a max file length rule by id. Returns `true` if a row was removed.
pub(super) fn remove_max_file_length_rule(engine: &Engine, id: i64) -> Result<bool> {
    engine.db.remove_max_file_length_rule(id)
}

/// Root a config rule's path under the fragment's target folder, joining
/// unconditionally.
///
/// A rule path is always interpreted relative to `folder`: a leading `/` is
/// stripped so an absolute-looking rule path (`/personal`) is rooted *under*
/// the pushed folder (`/work/personal`) rather than escaping to node-absolute
/// `/personal`. This is the documented intent — a pushed fragment's rules live
/// in the subtree the push is authorised over. A `..` segment is left in the
/// joined string for the caller's containment check to fold and reject; this
/// function only performs the join.
pub(crate) fn root_under(folder: &str, rule_path: &str) -> String {
    let folder = folder.trim_end_matches('/');
    let rule = rule_path.trim_start_matches('/');
    if folder.is_empty() {
        format!("/{rule}")
    } else {
        format!("{folder}/{rule}")
    }
}

/// Root a config rule's path under `folder` and confine it to that subtree.
///
/// Returns the rooted path when it normalises to a location covered by
/// `folder_scope`. Fails loudly when the rule escapes the authorised subtree
/// (for example a `..` traversal that climbs above `folder`), so a `ConfigPush`
/// authorised only over `folder` can never plant a rule outside it. The
/// containment test reuses [`Scope::covers`], which normalises `.`/`..`/empty
/// segments and matches on path components, so the same defence the authz layer
/// applies to scopes is applied to every rule path.
pub(crate) fn confine_rule_path(folder: &str, rule_path: &str, folder_scope: &Scope) -> Result<String> {
    let rooted = root_under(folder, rule_path);
    if folder_scope.covers(&Scope::folder(rooted.clone())) {
        Ok(rooted)
    } else {
        anyhow::bail!(
            "config push rule path {rule_path:?} escapes the authorised folder {folder:?} \
             (resolved to {rooted:?}); the whole push is refused",
        )
    }
}

/// Seconds in one minute.
const SECS_PER_MINUTE: i64 = 60;
/// Seconds in one hour.
const SECS_PER_HOUR: i64 = SECS_PER_MINUTE * 60;
/// Seconds in one day.
const SECS_PER_DAY: i64 = SECS_PER_HOUR * 24;
/// Seconds in one week.
const SECS_PER_WEEK: i64 = SECS_PER_DAY * 7;

/// Parse a human duration in the `.cascade` lifecycle form into whole seconds.
///
/// Accepts an integer count followed by a single unit suffix: `s` (seconds),
/// `m` (minutes), `h` (hours), `d` (days), or `w` (weeks). A bare integer is
/// taken as seconds. Fails loudly on an empty, non-numeric, or unknown-unit
/// value rather than guessing.
pub(crate) fn parse_duration_secs(raw: &str) -> Result<i64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("empty duration");
    }
    let (digits, unit_secs) = match trimmed.strip_suffix(['s', 'm', 'h', 'd', 'w']) {
        Some(stripped) => {
            let unit = trimmed
                .as_bytes()
                .last()
                .copied()
                .ok_or_else(|| anyhow::anyhow!("duration unit missing: {raw}"))?;
            let multiplier = match unit {
                b's' => 1,
                b'm' => SECS_PER_MINUTE,
                b'h' => SECS_PER_HOUR,
                b'd' => SECS_PER_DAY,
                b'w' => SECS_PER_WEEK,
                _ => anyhow::bail!("unknown duration unit in {raw}"),
            };
            (stripped, multiplier)
        }
        None => (trimmed, 1),
    };
    let count: i64 = digits
        .trim()
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid duration count in {raw}: {e}"))?;
    count
        .checked_mul(unit_secs)
        .ok_or_else(|| anyhow::anyhow!("duration overflow in {raw}"))
}

/// Bytes in one kibibyte.
const BYTES_PER_KIB: i64 = 1024;
/// Bytes in one mebibyte.
const BYTES_PER_MIB: i64 = BYTES_PER_KIB * 1024;
/// Bytes in one gibibyte.
const BYTES_PER_GIB: i64 = BYTES_PER_MIB * 1024;
/// Bytes in one tebibyte.
const BYTES_PER_TIB: i64 = BYTES_PER_GIB * 1024;

/// Parse a human byte size in the `.cascade` cache/lifecycle form into bytes.
///
/// Accepts an integer count followed by an optional binary unit suffix: `KB`,
/// `MB`, `GB`, or `TB` (interpreted as binary multiples, matching the rest of
/// the cache sizing in this codebase). A bare integer is taken as bytes.
/// Case-insensitive. Fails loudly on an empty, non-numeric, or unknown-unit
/// value.
pub(crate) fn parse_size_bytes(raw: &str) -> Result<i64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("empty size");
    }
    let upper = trimmed.to_ascii_uppercase();
    // Two-letter binary units first (longest suffix wins), then the bare-byte
    // suffix, then a plain integer.
    let binary_units: [(&str, i64); 4] = [
        ("TB", BYTES_PER_TIB),
        ("GB", BYTES_PER_GIB),
        ("MB", BYTES_PER_MIB),
        ("KB", BYTES_PER_KIB),
    ];
    let (digits, multiplier) = binary_units
        .iter()
        .find_map(|(suffix, mult)| upper.strip_suffix(suffix).map(|d| (d, *mult)))
        .or_else(|| upper.strip_suffix('B').map(|d| (d, 1)))
        .unwrap_or((upper.as_str(), 1));
    let count: i64 = digits
        .trim()
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid size count in {raw}: {e}"))?;
    count
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow::anyhow!("size overflow in {raw}"))
}

/// Parse a pushed `.cascade` config fragment in `format` into a
/// [`CascadeConfig`](cascade_config::CascadeConfig).
///
/// Routes to the matching `cascade-config` parser for the wire format. The
/// gitignore parser is infallible; the structured parsers surface a parse error
/// loudly rather than yielding an empty config.
#[cfg(feature = "p2p")]
pub(crate) fn parse_config_fragment(
    format: cascade_p2p::protocol::ManageConfigFormat,
    body: &str,
) -> Result<cascade_config::CascadeConfig> {
    use cascade_p2p::protocol::ManageConfigFormat;
    match format {
        ManageConfigFormat::Gitignore => Ok(cascade_config::parse::gitignore::parse(body)),
        ManageConfigFormat::Toml => cascade_config::parse::toml::parse(body),
        ManageConfigFormat::Yaml => cascade_config::parse::yaml::parse(body),
        ManageConfigFormat::Json => cascade_config::parse::json::parse(body),
    }
}
