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

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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
}

impl Capability {
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

impl Scope {
    /// Construct a folder scope from a path.
    #[must_use]
    pub fn folder(path: impl Into<String>) -> Self {
        Self::Folder { path: path.into() }
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

/// Whether `grant` is a path-component prefix of (or equal to) `target`.
///
/// Comparison is on normalised path components, never raw bytes, so a grant on
/// `/work` covers `/work/reports` but not `/workspace`. Trailing separators are
/// ignored. An empty grant path (the filesystem root) covers every folder.
fn path_prefix_covers(grant: &str, target: &str) -> bool {
    let grant_components: Vec<&str> = grant.split('/').filter(|s| !s.is_empty()).collect();
    let target_components: Vec<&str> = target.split('/').filter(|s| !s.is_empty()).collect();

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
///   *not* node-wide — a dangerous capability is never implicitly satisfied by
///   a wildcard grant, only by an explicit folder grant covering `target`.
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
            // they must be granted explicitly for the exact scope.
            && !(needed.is_dangerous() && matches!(grant.scope, Scope::Node))
    })
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

    #[test]
    fn scope_round_trips_through_json() {
        let node = serde_json::to_string(&Scope::Node).unwrap();
        assert_eq!(serde_json::from_str::<Scope>(&node).unwrap(), Scope::Node);

        let folder = Scope::folder("/work");
        let json = serde_json::to_string(&folder).unwrap();
        assert_eq!(serde_json::from_str::<Scope>(&json).unwrap(), folder);
    }
}
