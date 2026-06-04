//! Node management plane — capability grants and authorisation.
//!
//! Cascade has no built-in notion of one node administering another beyond the
//! data-plane trust expressed by `trusted_device_ids`. The management plane
//! adds delegated administrative authority: a node grants specific
//! capabilities, over specific scopes, to specific peer devices. Authority is
//! modelled as **capabilities, not roles** — each capability is a verb over a
//! scope — and grants live *on the managed node*, mirroring the consent
//! direction of the introducer relationship: you grant authority to a manager;
//! a manager cannot assert it.
//!
//! This module is the foundation only — the in-memory and on-disk
//! representation of grants plus the pure authorisation decision. Wire frames
//! (`ManageRequest` / `ManageResponse`) and command dispatch land in later
//! phases.

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use cascade_config::{GrantConfig, ScopeConfig};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub mod dispatch;
pub mod token;

pub use dispatch::{
    ManageCommandExecutor, ManageDispatch, ManageGrantStore, required_capability, run_dispatch,
    scope_from_wire, verify_data_token,
};
pub use token::{
    CapabilityToken, DelegateError, MAX_DELEGATION_DEPTH, TokenClaims, TokenVerifyError,
    derive_token_id,
};

/// A device identity — the principal of the management plane.
///
/// This is the base32-encoded SHA-256 of a device's TLS certificate, the same
/// identity used by the P2P data plane (see `cascade_p2p::identity`). It is
/// kept as a distinct newtype so a device ID can never be confused with an
/// arbitrary string at a call site.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DeviceId(pub String);

impl DeviceId {
    /// Construct a device ID from its string form.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The string form of the device ID.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// An administrative capability — a verb the holder may exercise over a
/// [`Scope`].
///
/// The serialised form matches the colon-delimited identifiers in the design
/// document (`status:read`, `pin:write`, …) so a capability survives the wire
/// and the config file unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Capability {
    /// Read mount status, cache usage, backend health, peer list.
    #[serde(rename = "status:read")]
    StatusRead,
    /// Pin / unpin paths.
    #[serde(rename = "pin:write")]
    PinWrite,
    /// Evict / warm cache entries.
    #[serde(rename = "cache:manage")]
    CacheManage,
    /// Merge `.cascade` / device config.
    #[serde(rename = "config:push")]
    ConfigPush,
    /// Set lifecycle and pin policies.
    #[serde(rename = "policy:set")]
    PolicySet,
    /// Add / remove backends. Dangerous — never satisfied by a wildcard scope.
    #[serde(rename = "backend:manage")]
    BackendManage,
    /// Start / stop / restart the daemon. Dangerous — never satisfied by a
    /// wildcard scope.
    #[serde(rename = "lifecycle:control")]
    LifecycleControl,
    /// Delegate a subset of held grants. Dangerous — never satisfied by a
    /// wildcard scope.
    #[serde(rename = "grant:admin")]
    GrantAdmin,
    /// Data-plane read: the bearer (peer device) may read this node's data for
    /// the scoped folder — we serve our index and blocks to them. Gates the
    /// outbound/serve direction. Folder-scoped; not dangerous.
    #[serde(rename = "data:read")]
    DataRead,
    /// Data-plane write: the bearer may write its data into this node for the
    /// scoped folder — we accept and merge its index and blocks. Gates the
    /// inbound/accept direction. Folder-scoped; not dangerous.
    #[serde(rename = "data:write")]
    DataWrite,
}

impl Capability {
    /// The stable wire/storage identifier for this capability — the
    /// colon-delimited form from the design document (`status:read`, …). This
    /// is the same string the serde representation uses; it is exposed
    /// directly so storage code can persist a capability without routing
    /// through JSON.
    #[must_use]
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::StatusRead => "status:read",
            Self::PinWrite => "pin:write",
            Self::CacheManage => "cache:manage",
            Self::ConfigPush => "config:push",
            Self::PolicySet => "policy:set",
            Self::BackendManage => "backend:manage",
            Self::LifecycleControl => "lifecycle:control",
            Self::GrantAdmin => "grant:admin",
            Self::DataRead => "data:read",
            Self::DataWrite => "data:write",
        }
    }

    /// Parse a capability from its [wire form](Self::as_wire). Returns `None`
    /// for an unrecognised identifier.
    #[must_use]
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "status:read" => Some(Self::StatusRead),
            "pin:write" => Some(Self::PinWrite),
            "cache:manage" => Some(Self::CacheManage),
            "config:push" => Some(Self::ConfigPush),
            "policy:set" => Some(Self::PolicySet),
            "backend:manage" => Some(Self::BackendManage),
            "lifecycle:control" => Some(Self::LifecycleControl),
            "grant:admin" => Some(Self::GrantAdmin),
            "data:read" => Some(Self::DataRead),
            "data:write" => Some(Self::DataWrite),
            _ => None,
        }
    }

    /// Whether this capability is *dangerous* — meaning it can never be
    /// implicitly satisfied by a node-wide / wildcard grant. A dangerous
    /// capability must be granted explicitly for the exact scope it is
    /// exercised over.
    #[must_use]
    pub const fn is_dangerous(self) -> bool {
        matches!(
            self,
            Self::BackendManage | Self::LifecycleControl | Self::GrantAdmin
        )
    }

    /// Whether this capability is a data-plane verb (`data:read` or
    /// `data:write`). Data verbs are folder-scoped and not dangerous — they
    /// gate the BEP sync serve/accept path rather than the management command
    /// surface.
    #[must_use]
    pub const fn is_data_verb(self) -> bool {
        matches!(self, Self::DataRead | Self::DataWrite)
    }
}

/// The extent over which a [`Capability`] applies.
///
/// A grant is either node-wide (the wildcard `*` in the design document) or
/// confined to a folder subtree identified by a path prefix.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Scope {
    /// The whole node — every path. Written `*` in the design document.
    Node,
    /// A folder subtree, identified by its path prefix. Matching is by path
    /// *component* containment, never raw substring: `/work` covers
    /// `/work/reports` but not `/workspace`.
    Folder {
        /// The folder path prefix this scope covers.
        path: String,
    },
}

/// The storage discriminant for a node-wide [`Scope`].
pub const SCOPE_KIND_NODE: &str = "node";
/// The storage discriminant for a folder [`Scope`].
pub const SCOPE_KIND_FOLDER: &str = "folder";

impl Scope {
    /// Construct a folder scope from a path.
    #[must_use]
    pub fn folder(path: impl Into<String>) -> Self {
        Self::Folder { path: path.into() }
    }

    /// Whether this scope is *node-wide* — covering every path on the node.
    ///
    /// [`Scope::Node`] is node-wide by construction. A [`Scope::Folder`] is
    /// *also* node-wide when its path normalises to no components — the
    /// filesystem root (`"/"`, `""`, `"//"`, `"/."` …) is a wildcard over the
    /// whole tree in everything but name. The dangerous-capability bar relies
    /// on this so a root folder grant cannot smuggle a dangerous capability
    /// past the wildcard check.
    #[must_use]
    pub fn is_node_wide(&self) -> bool {
        match self {
            Self::Node => true,
            Self::Folder { path } => {
                normalise_components(path).is_some_and(|components| components.is_empty())
            }
        }
    }

    /// Decompose a scope into its two storage columns: a kind discriminant and
    /// an optional path. A [`Scope::Node`] has no path; a [`Scope::Folder`]
    /// always carries one.
    #[must_use]
    pub const fn to_columns(&self) -> (&'static str, Option<&str>) {
        match self {
            Self::Node => (SCOPE_KIND_NODE, None),
            Self::Folder { path } => (SCOPE_KIND_FOLDER, Some(path.as_str())),
        }
    }

    /// Reconstruct a scope from its two storage columns. Returns `None` for an
    /// unrecognised kind, or for a folder kind missing its path.
    #[must_use]
    pub fn from_columns(kind: &str, path: Option<String>) -> Option<Self> {
        match kind {
            SCOPE_KIND_NODE => Some(Self::Node),
            SCOPE_KIND_FOLDER => path.map(|path| Self::Folder { path }),
            _ => None,
        }
    }

    /// Whether `self` (a held grant's scope) covers `target` (the scope an
    /// operation is requested over).
    ///
    /// - [`Scope::Node`] covers every target.
    /// - A [`Scope::Folder`] covers a target [`Scope::Folder`] when the
    ///   target's path lies within the grant's subtree, matched by path
    ///   component (so `/work` covers `/work` and `/work/reports`, but not
    ///   `/workspace`).
    /// - A [`Scope::Folder`] never covers [`Scope::Node`].
    #[must_use]
    pub fn covers(&self, target: &Self) -> bool {
        match (self, target) {
            (Self::Node, _) => true,
            (Self::Folder { .. }, Self::Node) => false,
            (Self::Folder { path: grant }, Self::Folder { path: target }) => {
                path_prefix_covers(grant, target)
            }
        }
    }
}

/// Normalise a path into its resolved component list, folding `.` and `..`
/// against a component stack.
///
/// Empty segments (from leading, trailing, or repeated separators) and `.`
/// segments are dropped; a `..` segment pops the most recent component. A `..`
/// that would pop above the root is a traversal escape: the path cannot be
/// represented relative to the root, so the function returns `None` rather than
/// silently clamping at the root. Callers treat `None` as "does not match",
/// which fails closed.
///
/// Returning the normalised components (rather than comparing raw bytes) is what
/// lets `/work` cover `/work/reports` but never `/workspace`, and what stops a
/// crafted `/work/../personal` target from being treated as living under
/// `/work`.
fn normalise_components(path: &str) -> Option<Vec<&str>> {
    let mut components: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                // A `..` with nothing to pop escapes above the root; the path
                // cannot be normalised relative to the root.
                components.pop()?;
            }
            other => components.push(other),
        }
    }
    Some(components)
}

/// Whether `grant` is a path-component prefix of (or equal to) `target`.
///
/// Both paths are first normalised by [`normalise_components`] — empty, `.`, and
/// `..` segments are folded — so comparison is on resolved path components,
/// never raw bytes. A grant on `/work` covers `/work/reports` but not
/// `/workspace`, and never `/work/../personal` (which normalises to
/// `/personal`). A path that traverses above the root (an unbalanced `..`)
/// fails to normalise and therefore never matches. A grant that normalises to
/// no components (the filesystem root) covers every folder.
fn path_prefix_covers(grant: &str, target: &str) -> bool {
    let (Some(grant_components), Some(target_components)) =
        (normalise_components(grant), normalise_components(target))
    else {
        return false;
    };

    if grant_components.len() > target_components.len() {
        return false;
    }
    grant_components
        .iter()
        .zip(target_components.iter())
        .all(|(g, t)| g == t)
}

/// A single capability grant held on the managed node.
///
/// The grantee is the peer device authorised to exercise `capability` over
/// `scope`. `granted_by` records which device issued the grant (the node
/// owner, or — once `grant:admin` delegation lands — a delegating manager).
/// `expires` bounds the grant in time; `None` means it never expires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    /// The device authorised by this grant.
    pub grantee: DeviceId,
    /// The capability conferred.
    pub capability: Capability,
    /// The scope over which the capability applies.
    pub scope: Scope,
    /// The device that issued this grant.
    pub granted_by: DeviceId,
    /// When the grant expires, if ever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<DateTime<Utc>>,
}

impl Grant {
    /// Whether this grant has expired as of `now`.
    #[must_use]
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires.is_some_and(|expiry| now >= expiry)
    }

    /// Build a domain grant from a declarative [`GrantConfig`] in the root
    /// device config, supplying the local node owner as `granted_by`.
    ///
    /// Validates the capability against the known vocabulary, the scope, and
    /// any expiry timestamp (RFC 3339), failing loudly rather than silently
    /// dropping a malformed declaration.
    pub fn from_config(config: &GrantConfig, granted_by: &DeviceId) -> Result<Self> {
        let capability = Capability::from_wire(&config.capability)
            .ok_or_else(|| anyhow!("unknown capability in grant config: {}", config.capability))?;
        let scope = match &config.scope {
            ScopeConfig::Node => Scope::Node,
            ScopeConfig::Folder { path } => Scope::folder(path.clone()),
        };
        let expires = config
            .expires
            .as_deref()
            .map(|raw| {
                DateTime::parse_from_rfc3339(raw)
                    .with_context(|| format!("parsing grant expiry timestamp: {raw}"))
                    .map(|dt| dt.with_timezone(&Utc))
            })
            .transpose()?;
        Ok(Self {
            grantee: DeviceId::new(config.grantee.clone()),
            capability,
            scope,
            granted_by: granted_by.clone(),
            expires,
        })
    }
}

/// Decide whether `caller` is authorised to exercise `needed` over `target`,
/// given the grants held on this node and the current time `now`.
///
/// Authorisation holds when at least one grant satisfies all of:
///
/// - the grantee is `caller`;
/// - the capability equals `needed`;
/// - the grant has not expired at `now`;
/// - the grant's scope [covers](Scope::covers) `target`; and
/// - if `needed` is [dangerous](Capability::is_dangerous), the grant's scope is
///   *not* [node-wide](Scope::is_node_wide) — a dangerous capability is never
///   implicitly satisfied by a wildcard grant, only by an explicit folder grant
///   covering `target`. A root/empty folder scope counts as node-wide here, so
///   it cannot smuggle a dangerous capability past the bar.
#[must_use]
pub fn authorises(
    grants: &[Grant],
    caller: &DeviceId,
    needed: Capability,
    target: &Scope,
    now: DateTime<Utc>,
) -> bool {
    grants.iter().any(|grant| {
        grant.grantee == *caller
            && grant.capability == needed
            && !grant.is_expired(now)
            && grant.scope.covers(target)
            // Dangerous capabilities are never satisfied by a node-wide grant;
            // they must be granted explicitly for the exact scope. A root/empty
            // folder scope is node-wide in everything but name, so it is barred
            // here too — not just the `Scope::Node` variant.
            && !(needed.is_dangerous() && grant.scope.is_node_wide())
    })
}

/// The data-plane access decision for a single (peer, folder) pair.
///
/// Both fields are independent: a peer with `read = true, write = false` is
/// read-only (we serve them data, they cannot push to us); `read = false,
/// write = true` is write-only / drop-sink; `read = true, write = true` is
/// full bidirectional sharing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataAccess {
    /// Whether we serve our index and blocks to this peer for this folder.
    pub read: bool,
    /// Whether we accept this peer's index and blocks for this folder.
    pub write: bool,
}

/// Decide the data-plane access a `peer` has for `folder` given the grants
/// held on this node and the revoked token ids, at `now`.
///
/// This is a **default-open** decision, unlike [`authorises`] (which is
/// default-closed):
///
/// 1. An unexpired, matching-verb grant covering the folder for this peer
///    allows that direction.
/// 2. If the peer has **any** data grant for this folder (either direction,
///    even if the other is absent or expired), the absent / lapsed direction
///    is **denied** — the presence of any data grant opts the peer into
///    explicit directional control.
/// 3. If the peer has **no** data grant at all for this folder, **both**
///    directions are allowed — the trusted-peer default, preserving today's
///    full bidirectional behaviour.
///
/// Revocation is applied per-grant-token via `revoked_token_ids`; a grant
/// row in the grants table has no token, so revocation is only relevant when
/// a data grant was synthesised from a presented capability token (the
/// `DataAuthority` implementation folds those in before calling here).
/// Grant-row expiry is honoured directly via [`Grant::is_expired`].
///
/// The function is pure so it can be used in tests and in the hot BEP path
/// without I/O. The `DataAuthority` trait wraps the I/O boundary.
#[must_use]
pub fn data_access(
    grants: &[Grant],
    peer: &DeviceId,
    folder: &str,
    now: DateTime<Utc>,
) -> DataAccess {
    data_access_with_explicit_control(grants, peer, folder, now, &[])
}

/// Decide the data-plane access a `peer` has for `folder` given the grants
/// held on this node, the revoked token ids, and the F2 explicit-control
/// bit, at `now`.
///
/// The F2 invariant: a peer who has *ever* presented a verified data-verb
/// token for a folder is in explicit-control mode for that folder, even
/// after the token has been revoked or has expired. The absent direction
/// stays denied; a token-only restriction cannot be widened back to the
/// trusted-peer default by revoking or letting the token lapse.
///
/// `explicit_control` is the slice of `(peer, folder, data_read, data_write)`
/// rows the engine's in-memory mirror holds. When the slice contains a row
/// for `(peer, folder)`, that row's per-direction state is honoured in
/// addition to the grant-driven decision, so a token-only restriction
/// survives the token revocation: the bit is set when the token first
/// verifies and never cleared by revocation or expiry.
///
/// The function is pure so it can be used in tests and in the hot BEP path
/// without I/O. The `DataAuthority` trait wraps the I/O boundary.
#[must_use]
pub fn data_access_with_explicit_control(
    grants: &[Grant],
    peer: &DeviceId,
    folder: &str,
    now: DateTime<Utc>,
    explicit_control: &[ExplicitControlState],
) -> DataAccess {
    let folder_scope = Scope::folder(folder);

    // Collect data grants that cover this peer and folder, partitioned by
    // whether they are currently active (unexpired) and which verb they carry.
    let mut has_any_data_grant = false;
    let mut read_allowed = false;
    let mut write_allowed = false;

    for grant in grants {
        if grant.grantee != *peer {
            continue;
        }
        if !grant.capability.is_data_verb() {
            continue;
        }
        // The grant's scope must cover the folder.
        if !grant.scope.covers(&folder_scope) {
            continue;
        }
        // F4 (defence in depth): ignore data grants whose scope is
        // node-wide. The runtime gate keys on the BEP folder id
        // (`p2p-<name>`) and there is no such id at the node root, so
        // a node-wide data grant cannot satisfy any folder check. The
        // local CLI and the wire-side `grant_from_wire` both refuse to
        // author such a grant; this filter catches a row that slipped
        // through a future code path so a node-wide data grant cannot
        // accidentally contribute to "has_any_data_grant" and narrow
        // the access for every folder the peer might touch.
        if grant.scope.is_node_wide() {
            continue;
        }
        // At least one data grant exists for this peer+folder — this opts the
        // peer into explicit directional control (rule 2).
        has_any_data_grant = true;

        if grant.is_expired(now) {
            // An expired grant contributes to "has_any_data_grant" (so the
            // other direction is narrowed if it has no active grant), but does
            // not itself allow the verb.
            continue;
        }

        match grant.capability {
            Capability::DataRead => read_allowed = true,
            Capability::DataWrite => write_allowed = true,
            _ => {}
        }
    }

    if has_any_data_grant {
        // Rule 2: at least one data grant exists; allow only what was
        // explicitly granted and unexpired.
        DataAccess {
            read: read_allowed,
            write: write_allowed,
        }
    } else {
        // Rule 3: no data grant of any kind — trusted-peer default (full sharing).
        // But the F2 bit may still pin the peer into explicit-control mode.
        // If a bit exists for this (peer, folder), apply it: the bit carries
        // the per-direction state observed on the first successful verify,
        // and it survives any token revocation or expiry.
        explicit_control
            .iter()
            .find(|s| s.peer == peer.as_str() && s.folder == folder)
            .map_or(
                DataAccess {
                    read: true,
                    write: true,
                },
                |state| DataAccess {
                    read: state.data_read,
                    write: state.data_write,
                },
            )
    }
}

/// The in-memory representation of one F2 explicit-control row. Mirrored
/// from the `data_explicit_control` table into the engine on startup and
/// refreshed on `clear_data_explicit_control`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplicitControlState {
    /// The peer device in explicit-control mode.
    pub peer: String,
    /// The BEP folder id the control applies to.
    pub folder: String,
    /// Whether the verified token granted `data:read` for this folder.
    pub data_read: bool,
    /// Whether the verified token granted `data:write` for this folder.
    pub data_write: bool,
}

/// The data-plane authority port: consults the on-node data grants and
/// revocation list to decide whether a peer may read or write a folder.
///
/// Implemented by the engine over `StateDb`. Injected into `SyncEngine` so
/// the BEP sync path can check access at the frame level without taking a
/// direct dependency on the engine crate.
///
/// When the port is **unset** (bare `SyncEngine` not yet wired, or a unit
/// test that never injects it), the BEP path applies the default-open
/// behaviour — every trusted peer gets full bidirectional access — matching
/// the pre-feature behaviour exactly.
#[async_trait]
pub trait DataAuthority: Send + Sync {
    /// Return the read/write access for `peer` over `folder` as of `now`.
    ///
    /// The implementation reads the on-node data grants and the revocation
    /// list from the state database, folds any presented-token grants in, and
    /// delegates to [`data_access`].
    ///
    /// `presented_token` is the optional signed capability token the peer
    /// carried in the `data_token` field of its `BepMessage::ClusterConfig` sync
    /// frame, in its JSON form. When present, the implementation verifies it
    /// against this node — signed by this node or a chain rooting in it,
    /// unexpired, not revoked, with the bearer matching the authenticated
    /// `peer` — and folds the carried data-verb grant into the grant set before
    /// deciding. A token that does not verify, or that carries a non-data verb,
    /// is ignored for the data-plane decision (it cannot widen access); it is
    /// never an error, because the data path is default-open and a bad token
    /// simply confers nothing.
    async fn data_access(
        &self,
        peer: &DeviceId,
        folder: &str,
        presented_token: Option<&str>,
        now: DateTime<Utc>,
    ) -> anyhow::Result<DataAccess>;

    /// Record a `peer`'s proposed file row as a flagged local addition in the
    /// receive quarantine for `folder`, because the peer may not write
    /// (`data:write` denied) but its edit must not be silently discarded.
    ///
    /// `path` is the file path the row would occupy and `file_json` is the
    /// serialised `FileInfo` exactly as the peer sent it. A newer proposal for
    /// the same `(folder, peer, path)` replaces the older one, so the quarantine
    /// stays bounded by the number of distinct paths. `observed_at` stamps when
    /// the proposal arrived. Quarantined rows are surfaced to the operator
    /// ("N rejected local additions from `<peer>`"), never merged, block-fetched,
    /// or re-advertised; if the operator later grants `data:write`, the peer
    /// re-sends and the rows become eligible to merge on the next exchange.
    async fn quarantine_received(
        &self,
        peer: &DeviceId,
        folder: &str,
        path: &str,
        file_json: &str,
        observed_at: DateTime<Utc>,
    ) -> anyhow::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 0, 0, 0)
            .single()
            .expect("valid date")
    }

    fn manager() -> DeviceId {
        DeviceId::new("MANAGER")
    }

    fn owner() -> DeviceId {
        DeviceId::new("OWNER")
    }

    fn grant(capability: Capability, scope: Scope) -> Grant {
        Grant {
            grantee: manager(),
            capability,
            scope,
            granted_by: owner(),
            expires: None,
        }
    }

    // ── Capability classification ──

    #[test]
    fn dangerous_capabilities_are_classified() {
        assert!(Capability::BackendManage.is_dangerous());
        assert!(Capability::LifecycleControl.is_dangerous());
        assert!(Capability::GrantAdmin.is_dangerous());
    }

    #[test]
    fn safe_capabilities_are_not_dangerous() {
        assert!(!Capability::StatusRead.is_dangerous());
        assert!(!Capability::PinWrite.is_dangerous());
        assert!(!Capability::CacheManage.is_dangerous());
        assert!(!Capability::ConfigPush.is_dangerous());
        assert!(!Capability::PolicySet.is_dangerous());
    }

    // ── Scope coverage ──

    #[test]
    fn node_scope_covers_everything() {
        assert!(Scope::Node.covers(&Scope::Node));
        assert!(Scope::Node.covers(&Scope::folder("/work")));
    }

    #[test]
    fn folder_scope_never_covers_node() {
        assert!(!Scope::folder("/work").covers(&Scope::Node));
    }

    #[test]
    fn folder_scope_covers_itself_and_descendants() {
        let work = Scope::folder("/work");
        assert!(work.covers(&Scope::folder("/work")));
        assert!(work.covers(&Scope::folder("/work/reports")));
        assert!(work.covers(&Scope::folder("/work/reports/q1")));
    }

    #[test]
    fn folder_scope_matches_by_component_not_substring() {
        // The escalation guard for path matching: `/work` must not cover
        // `/workspace`, even though "work" is a substring of "workspace".
        let work = Scope::folder("/work");
        assert!(!work.covers(&Scope::folder("/workspace")));
        assert!(!work.covers(&Scope::folder("/workspace/reports")));
    }

    #[test]
    fn folder_scope_does_not_cover_parent_or_sibling() {
        let reports = Scope::folder("/work/reports");
        assert!(!reports.covers(&Scope::folder("/work")));
        assert!(!reports.covers(&Scope::folder("/work/budgets")));
    }

    #[test]
    fn root_folder_scope_covers_all_folders() {
        let root = Scope::folder("/");
        assert!(root.covers(&Scope::folder("/work")));
        assert!(root.covers(&Scope::folder("/anything/deep")));
        // Root folder still does not cover the node-wide scope.
        assert!(!root.covers(&Scope::Node));
    }

    #[test]
    fn folder_coverage_ignores_trailing_separators() {
        assert!(Scope::folder("/work/").covers(&Scope::folder("/work")));
        assert!(Scope::folder("/work").covers(&Scope::folder("/work/")));
    }

    #[test]
    fn folder_coverage_normalises_dot_segments() {
        // A `.` segment is inert and must not alter coverage.
        let work = Scope::folder("/work");
        assert!(work.covers(&Scope::folder("/work/./reports")));
        assert!(Scope::folder("/work/.").covers(&Scope::folder("/work")));
    }

    #[test]
    fn folder_coverage_rejects_parent_traversal() {
        // `/work/../personal` normalises to `/personal`, outside the granted
        // subtree — a grant on `/work` must not cover it. This is the scope
        // escape the "normalised path components" contract promises to prevent.
        let work = Scope::folder("/work");
        assert!(!work.covers(&Scope::folder("/work/../personal")));
        // `/work/..` normalises to the root, which is not under `/work`.
        assert!(!work.covers(&Scope::folder("/work/..")));
    }

    #[test]
    fn folder_coverage_rejects_traversal_above_root() {
        // An unbalanced `..` escapes above the root: the path cannot be
        // normalised, so neither a grant nor a target carrying it ever matches.
        assert!(!Scope::folder("/work").covers(&Scope::folder("/../etc")));
        assert!(!Scope::folder("/../work").covers(&Scope::folder("/work")));
        assert!(!Scope::folder("/work/../../etc").covers(&Scope::folder("/etc")));
    }

    #[test]
    fn is_node_wide_truth_table() {
        // The explicit node scope is node-wide.
        assert!(Scope::Node.is_node_wide());
        // Root/empty folder paths normalise to zero components — node-wide.
        for path in ["/", "", "//", "/.", "/./", "/work/.."] {
            assert!(
                Scope::folder(path).is_node_wide(),
                "folder scope {path:?} should be node-wide",
            );
        }
        // A real subtree is not node-wide.
        assert!(!Scope::folder("/work").is_node_wide());
        assert!(!Scope::folder("/work/reports").is_node_wide());
        // A path that escapes above the root does not normalise, and is not
        // treated as node-wide (it fails closed).
        assert!(!Scope::folder("/..").is_node_wide());
    }

    // ── authorises() truth table ──

    #[test]
    fn unmatched_caller_is_denied() {
        let grants = vec![grant(Capability::StatusRead, Scope::Node)];
        let stranger = DeviceId::new("STRANGER");
        assert!(!authorises(
            &grants,
            &stranger,
            Capability::StatusRead,
            &Scope::Node,
            at(2026, 1, 1),
        ));
    }

    #[test]
    fn empty_grant_list_denies() {
        assert!(!authorises(
            &[],
            &manager(),
            Capability::StatusRead,
            &Scope::Node,
            at(2026, 1, 1),
        ));
    }

    #[test]
    fn wrong_capability_is_denied() {
        let grants = vec![grant(Capability::StatusRead, Scope::Node)];
        assert!(!authorises(
            &grants,
            &manager(),
            Capability::PinWrite,
            &Scope::Node,
            at(2026, 1, 1),
        ));
    }

    #[test]
    fn safe_capability_with_node_scope_is_allowed() {
        let grants = vec![grant(Capability::StatusRead, Scope::Node)];
        assert!(authorises(
            &grants,
            &manager(),
            Capability::StatusRead,
            &Scope::folder("/work"),
            at(2026, 1, 1),
        ));
    }

    #[test]
    fn safe_capability_with_folder_scope_is_scoped() {
        let grants = vec![grant(Capability::PinWrite, Scope::folder("/work"))];
        // Within scope: allowed.
        assert!(authorises(
            &grants,
            &manager(),
            Capability::PinWrite,
            &Scope::folder("/work/reports"),
            at(2026, 1, 1),
        ));
        // Outside scope: denied.
        assert!(!authorises(
            &grants,
            &manager(),
            Capability::PinWrite,
            &Scope::folder("/personal"),
            at(2026, 1, 1),
        ));
        // Substring escalation: denied.
        assert!(!authorises(
            &grants,
            &manager(),
            Capability::PinWrite,
            &Scope::folder("/workspace"),
            at(2026, 1, 1),
        ));
    }

    // ── Expiry ──

    #[test]
    fn expired_grant_is_denied() {
        let mut g = grant(Capability::StatusRead, Scope::Node);
        g.expires = Some(at(2026, 1, 1));
        let grants = vec![g];
        // Exactly at expiry: denied (expiry is inclusive of the boundary).
        assert!(!authorises(
            &grants,
            &manager(),
            Capability::StatusRead,
            &Scope::Node,
            at(2026, 1, 1),
        ));
        // After expiry: denied.
        assert!(!authorises(
            &grants,
            &manager(),
            Capability::StatusRead,
            &Scope::Node,
            at(2026, 6, 1),
        ));
    }

    #[test]
    fn unexpired_grant_is_allowed() {
        let mut g = grant(Capability::StatusRead, Scope::Node);
        g.expires = Some(at(2026, 6, 1));
        let grants = vec![g];
        assert!(authorises(
            &grants,
            &manager(),
            Capability::StatusRead,
            &Scope::Node,
            at(2026, 1, 1),
        ));
    }

    #[test]
    fn never_expiring_grant_is_allowed_far_in_future() {
        let grants = vec![grant(Capability::StatusRead, Scope::Node)];
        assert!(authorises(
            &grants,
            &manager(),
            Capability::StatusRead,
            &Scope::Node,
            at(2999, 12, 31),
        ));
    }

    // ── Dangerous capability wildcard bar ──

    #[test]
    fn dangerous_capability_is_barred_by_node_scope() {
        // A node-wide grant of a dangerous capability never authorises it —
        // even for the node-wide target and an unexpired grant.
        for cap in [
            Capability::BackendManage,
            Capability::LifecycleControl,
            Capability::GrantAdmin,
        ] {
            let grants = vec![grant(cap, Scope::Node)];
            assert!(
                !authorises(&grants, &manager(), cap, &Scope::Node, at(2026, 1, 1)),
                "node-wide grant must not satisfy dangerous capability {cap:?}",
            );
            assert!(
                !authorises(
                    &grants,
                    &manager(),
                    cap,
                    &Scope::folder("/work"),
                    at(2026, 1, 1),
                ),
                "node-wide grant must not satisfy dangerous capability {cap:?} over a folder",
            );
        }
    }

    #[test]
    fn dangerous_capability_is_barred_by_root_folder_scope() {
        // A folder scope that normalises to the root is node-wide in
        // everything but name. It must not slip a dangerous capability past the
        // wildcard bar — the same denial as an explicit `Scope::Node` grant.
        for cap in [
            Capability::BackendManage,
            Capability::LifecycleControl,
            Capability::GrantAdmin,
        ] {
            for scope_path in ["/", "", "//", "/.", "/work/.."] {
                let grants = vec![grant(cap, Scope::folder(scope_path))];
                // Against a node-wide target: denied.
                assert!(
                    !authorises(&grants, &manager(), cap, &Scope::Node, at(2026, 1, 1)),
                    "root folder scope {scope_path:?} must not satisfy {cap:?} over the node",
                );
                // Against any folder target the root scope would otherwise
                // cover: still denied.
                assert!(
                    !authorises(
                        &grants,
                        &manager(),
                        cap,
                        &Scope::folder("/work/reports"),
                        at(2026, 1, 1),
                    ),
                    "root folder scope {scope_path:?} must not satisfy {cap:?} over a folder",
                );
            }
        }
    }

    #[test]
    fn dangerous_capability_is_satisfied_by_explicit_folder_scope() {
        for cap in [
            Capability::BackendManage,
            Capability::LifecycleControl,
            Capability::GrantAdmin,
        ] {
            let grants = vec![grant(cap, Scope::folder("/work"))];
            // Exact scope: allowed.
            assert!(
                authorises(
                    &grants,
                    &manager(),
                    cap,
                    &Scope::folder("/work"),
                    at(2026, 1, 1),
                ),
                "explicit folder grant must satisfy dangerous capability {cap:?}",
            );
            // Descendant scope: allowed (folder coverage applies).
            assert!(
                authorises(
                    &grants,
                    &manager(),
                    cap,
                    &Scope::folder("/work/reports"),
                    at(2026, 1, 1),
                ),
                "explicit folder grant must cover descendants for {cap:?}",
            );
            // Out-of-scope folder: denied.
            assert!(
                !authorises(
                    &grants,
                    &manager(),
                    cap,
                    &Scope::folder("/personal"),
                    at(2026, 1, 1),
                ),
                "explicit folder grant must not leak to siblings for {cap:?}",
            );
        }
    }

    #[test]
    fn dangerous_folder_grant_does_not_authorise_node_target() {
        // Even an explicit folder grant of a dangerous capability cannot reach
        // the node-wide target — a folder scope never covers Node.
        let grants = vec![grant(Capability::GrantAdmin, Scope::folder("/work"))];
        assert!(!authorises(
            &grants,
            &manager(),
            Capability::GrantAdmin,
            &Scope::Node,
            at(2026, 1, 1),
        ));
    }

    #[test]
    fn first_satisfying_grant_authorises_among_many() {
        // A node-wide status grant plus a folder-scoped pin grant: each
        // authorises only its own capability/scope combination.
        let grants = vec![
            grant(Capability::StatusRead, Scope::Node),
            grant(Capability::PinWrite, Scope::folder("/work")),
        ];
        assert!(authorises(
            &grants,
            &manager(),
            Capability::StatusRead,
            &Scope::folder("/personal"),
            at(2026, 1, 1),
        ));
        assert!(authorises(
            &grants,
            &manager(),
            Capability::PinWrite,
            &Scope::folder("/work/q1"),
            at(2026, 1, 1),
        ));
        assert!(!authorises(
            &grants,
            &manager(),
            Capability::PinWrite,
            &Scope::folder("/personal"),
            at(2026, 1, 1),
        ));
    }

    // ── Serde round-trips ──

    #[test]
    fn capability_serialises_to_colon_form() {
        let json = serde_json::to_string(&Capability::StatusRead).unwrap();
        assert_eq!(json, "\"status:read\"");
        let back: Capability = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Capability::StatusRead);
    }

    #[test]
    fn grant_round_trips_through_json() {
        let g = Grant {
            grantee: manager(),
            capability: Capability::PinWrite,
            scope: Scope::folder("/work"),
            granted_by: owner(),
            expires: Some(at(2026, 12, 31)),
        };
        let json = serde_json::to_string(&g).unwrap();
        let back: Grant = serde_json::from_str(&json).unwrap();
        assert_eq!(g, back);
    }

    // ── Config conversion ──

    #[test]
    fn grant_from_config_folder_scope() {
        let config = GrantConfig {
            grantee: "MANAGER".to_string(),
            capability: "pin:write".to_string(),
            scope: ScopeConfig::Folder {
                path: "/work".to_string(),
            },
            expires: None,
        };
        let grant = Grant::from_config(&config, &owner()).unwrap();
        assert_eq!(grant.grantee, DeviceId::new("MANAGER"));
        assert_eq!(grant.capability, Capability::PinWrite);
        assert_eq!(grant.scope, Scope::folder("/work"));
        assert_eq!(grant.granted_by, owner());
        assert!(grant.expires.is_none());
    }

    #[test]
    fn grant_from_config_node_scope_with_expiry() {
        let config = GrantConfig {
            grantee: "MANAGER".to_string(),
            capability: "status:read".to_string(),
            scope: ScopeConfig::Node,
            expires: Some("2026-12-31T00:00:00Z".to_string()),
        };
        let grant = Grant::from_config(&config, &owner()).unwrap();
        assert_eq!(grant.scope, Scope::Node);
        assert_eq!(grant.expires, Some(at(2026, 12, 31)));
    }

    #[test]
    fn grant_from_config_rejects_unknown_capability() {
        let config = GrantConfig {
            grantee: "MANAGER".to_string(),
            capability: "totally:bogus".to_string(),
            scope: ScopeConfig::Node,
            expires: None,
        };
        assert!(Grant::from_config(&config, &owner()).is_err());
    }

    #[test]
    fn grant_from_config_rejects_bad_expiry() {
        let config = GrantConfig {
            grantee: "MANAGER".to_string(),
            capability: "status:read".to_string(),
            scope: ScopeConfig::Node,
            expires: Some("not-a-timestamp".to_string()),
        };
        assert!(Grant::from_config(&config, &owner()).is_err());
    }

    #[test]
    fn scope_round_trips_through_json() {
        let node = serde_json::to_string(&Scope::Node).unwrap();
        assert_eq!(serde_json::from_str::<Scope>(&node).unwrap(), Scope::Node);

        let folder = Scope::folder("/work");
        let json = serde_json::to_string(&folder).unwrap();
        assert_eq!(serde_json::from_str::<Scope>(&json).unwrap(), folder);
    }

    // ── Capability classification: data verbs ──

    #[test]
    fn data_verbs_are_not_dangerous() {
        assert!(!Capability::DataRead.is_dangerous());
        assert!(!Capability::DataWrite.is_dangerous());
    }

    #[test]
    fn data_verbs_are_classified_as_data_verbs() {
        assert!(Capability::DataRead.is_data_verb());
        assert!(Capability::DataWrite.is_data_verb());
    }

    #[test]
    fn non_data_verbs_are_not_data_verbs() {
        assert!(!Capability::StatusRead.is_data_verb());
        assert!(!Capability::PinWrite.is_data_verb());
        assert!(!Capability::GrantAdmin.is_data_verb());
    }

    #[test]
    fn data_verbs_round_trip_through_wire_form() {
        assert_eq!(Capability::DataRead.as_wire(), "data:read");
        assert_eq!(Capability::DataWrite.as_wire(), "data:write");
        assert_eq!(
            Capability::from_wire("data:read"),
            Some(Capability::DataRead)
        );
        assert_eq!(
            Capability::from_wire("data:write"),
            Some(Capability::DataWrite)
        );
    }

    #[test]
    fn data_verbs_round_trip_through_serde() {
        let json = serde_json::to_string(&Capability::DataRead).unwrap();
        assert_eq!(json, "\"data:read\"");
        assert_eq!(
            serde_json::from_str::<Capability>(&json).unwrap(),
            Capability::DataRead
        );

        let json = serde_json::to_string(&Capability::DataWrite).unwrap();
        assert_eq!(json, "\"data:write\"");
        assert_eq!(
            serde_json::from_str::<Capability>(&json).unwrap(),
            Capability::DataWrite
        );
    }

    // ── data_access truth table ──

    fn peer() -> DeviceId {
        DeviceId::new("PEER")
    }

    fn data_grant(capability: Capability, folder: &str) -> Grant {
        Grant {
            grantee: peer(),
            capability,
            scope: Scope::folder(folder),
            granted_by: owner(),
            expires: None,
        }
    }

    fn data_grant_expiring(capability: Capability, folder: &str, expires: DateTime<Utc>) -> Grant {
        Grant {
            grantee: peer(),
            capability,
            scope: Scope::folder(folder),
            granted_by: owner(),
            expires: Some(expires),
        }
    }

    #[test]
    fn data_access_no_grant_returns_full_access_default() {
        // Rule 3: no data grant of any kind — trusted-peer default is
        // full bidirectional sharing.
        let access = data_access(&[], &peer(), "/work", at(2026, 1, 1));
        assert!(access.read, "default: read must be allowed");
        assert!(access.write, "default: write must be allowed");
    }

    #[test]
    fn data_access_read_only_grant_allows_read_denies_write() {
        let grants = vec![data_grant(Capability::DataRead, "/work")];
        let access = data_access(&grants, &peer(), "/work", at(2026, 1, 1));
        assert!(access.read, "read:only grant must allow read");
        assert!(!access.write, "read-only grant must deny write");
    }

    #[test]
    fn data_access_write_only_grant_allows_write_denies_read() {
        let grants = vec![data_grant(Capability::DataWrite, "/work")];
        let access = data_access(&grants, &peer(), "/work", at(2026, 1, 1));
        assert!(!access.read, "write-only grant must deny read");
        assert!(access.write, "write-only grant must allow write");
    }

    #[test]
    fn data_access_read_write_grants_allow_both() {
        let grants = vec![
            data_grant(Capability::DataRead, "/work"),
            data_grant(Capability::DataWrite, "/work"),
        ];
        let access = data_access(&grants, &peer(), "/work", at(2026, 1, 1));
        assert!(access.read, "read-write grants must allow read");
        assert!(access.write, "read-write grants must allow write");
    }

    #[test]
    fn data_access_expired_read_grant_narrows_but_does_not_default_open() {
        // An expired read grant still opts the peer into explicit control
        // (rule 2): the absent/lapsed direction is denied, not defaulted.
        let grants = vec![data_grant_expiring(
            Capability::DataRead,
            "/work",
            at(2025, 12, 31),
        )];
        let access = data_access(&grants, &peer(), "/work", at(2026, 1, 1));
        assert!(
            !access.read,
            "expired read grant must not allow read (rule 2: explicit control mode, expired)"
        );
        assert!(
            !access.write,
            "expired read grant must not allow write (no write grant exists)"
        );
    }

    #[test]
    fn data_access_expired_read_grant_with_active_write_allows_only_write() {
        // An expired read grant + an active write grant: write is allowed,
        // read is denied (the lapsed direction narrows under explicit control).
        let grants = vec![
            data_grant_expiring(Capability::DataRead, "/work", at(2025, 12, 31)),
            data_grant(Capability::DataWrite, "/work"),
        ];
        let access = data_access(&grants, &peer(), "/work", at(2026, 1, 1));
        assert!(!access.read, "expired read grant must not allow read");
        assert!(access.write, "active write grant must allow write");
    }

    #[test]
    fn data_access_grant_for_different_peer_is_ignored() {
        let mut g = data_grant(Capability::DataRead, "/work");
        g.grantee = DeviceId::new("OTHER-PEER");
        let grants = vec![g];
        // The grant is for a different peer; our peer has no data grant,
        // so it defaults to full access.
        let access = data_access(&grants, &peer(), "/work", at(2026, 1, 1));
        assert!(
            access.read,
            "different peer's grant must not affect our peer"
        );
        assert!(
            access.write,
            "different peer's grant must not affect our peer"
        );
    }

    #[test]
    fn data_access_grant_for_different_folder_does_not_affect_this_folder() {
        // A data grant on /personal does not opt /work into explicit control.
        let grants = vec![data_grant(Capability::DataRead, "/personal")];
        let access = data_access(&grants, &peer(), "/work", at(2026, 1, 1));
        assert!(access.read, "grant on /personal must not restrict /work");
        assert!(access.write, "grant on /personal must not restrict /work");
    }

    #[test]
    fn data_access_node_wide_data_grant_is_ignored_as_defence_in_depth() {
        // F4: a data-verb grant whose scope is node-wide cannot
        // contribute to the per-direction decision. The local CLI and
        // the wire-side parse both refuse such a grant outright; this
        // pure-function filter is defence in depth for any row that
        // somehow slipped through (a future code path, a corrupt
        // state, a bug we have not yet found). Without the filter, a
        // node-wide `data:read` would set `has_any_data_grant = true`
        // and silently narrow every folder the peer might touch — the
        // F1 silent-no-op failure mode in a different shape.
        let grants = vec![
            Grant {
                grantee: peer(),
                capability: Capability::DataRead,
                scope: Scope::Node,
                granted_by: owner(),
                expires: None,
            },
            Grant {
                grantee: peer(),
                capability: Capability::DataWrite,
                scope: Scope::Node,
                granted_by: owner(),
                expires: None,
            },
        ];
        let access = data_access(&grants, &peer(), "/work", at(2026, 1, 1));
        assert!(
            access.read,
            "node-wide data:read must not restrict the folder (F4 defence in depth)"
        );
        assert!(
            access.write,
            "node-wide data:write must not restrict the folder (F4 defence in depth)"
        );
        // A root folder grant (`/`) is also node-wide in everything
        // but name, so the same filter catches it.
        let grants = vec![Grant {
            grantee: peer(),
            capability: Capability::DataRead,
            scope: Scope::folder("/"),
            granted_by: owner(),
            expires: None,
        }];
        let access = data_access(&grants, &peer(), "/work", at(2026, 1, 1));
        assert!(
            access.read,
            "root-folder data:read must not restrict the folder (F4 defence in depth)"
        );
        assert!(
            access.write,
            "root-folder data:read must not restrict the folder (F4 defence in depth)"
        );
    }

    #[test]
    fn data_access_parent_folder_grant_covers_child() {
        // A data:read grant on /work covers /work/reports (folder prefix coverage).
        let grants = vec![data_grant(Capability::DataRead, "/work")];
        let access = data_access(&grants, &peer(), "/work/reports", at(2026, 1, 1));
        assert!(access.read, "parent folder grant must cover child folder");
        assert!(!access.write, "parent folder read grant must deny write");
    }

    #[test]
    fn data_access_delegation_cannot_escalate_scope() {
        // A read grant on /work/sub does NOT cover /work (the parent). The
        // grant system's subset rule means a delegate can only narrow scope,
        // never widen it: a grant on /work/sub cannot make the peer read /work.
        let grants = vec![data_grant(Capability::DataRead, "/work/sub")];
        let access = data_access(&grants, &peer(), "/work", at(2026, 1, 1));
        // The grant for /work/sub does not cover /work, so no data grant
        // applies to this folder. Default-open applies.
        assert!(
            access.read,
            "grant on /work/sub must not affect /work (default-open applies)"
        );
        assert!(
            access.write,
            "grant on /work/sub must not affect /work (default-open applies)"
        );
    }

    #[test]
    fn data_access_revoked_token_grant_is_excluded() {
        // A grant derived from a revoked token must not authorise access.
        // The data_access function operates on a grants slice that callers
        // populate; a revoked-token grant must simply not be included in the
        // slice the caller passes. Here we verify that a slice with no grants
        // for this peer yields the default-open result, and that excluding the
        // grant (as the DataAuthority impl must for revoked tokens) works.
        let access = data_access(&[], &peer(), "/work", at(2026, 1, 1));
        assert!(access.read, "empty grants yields default-open");
        assert!(access.write, "empty grants yields default-open");
    }
}
