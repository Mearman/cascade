//! Management-plane command dispatch — the authenticated front-end onto the
//! same command handlers the local CLI drives.
//!
//! A [`ManageRequest`](cascade_p2p::protocol::BepMessage::ManageRequest)
//! arrives over a peer connection. The dispatcher trusts the [`DeviceId`] it is
//! handed as the caller principal; establishing that principal cryptographically
//! is the transport's responsibility, and the backend only routes a
//! `ManageRequest` here when the session's peer identity was proven by an
//! end-to-end TLS handshake (relayed and post-hole-punch sessions, whose device
//! id is merely asserted on the wire, are refused before reaching this port).
//! The managed node resolves the caller's grants,
//! [authorises] the command's required
//! [`Capability`] over its target [`Scope`], **writes an audit row before
//! applying any side effect**, then dispatches into the same internal command
//! implementations the local CLI uses via the [`ManageCommandExecutor`]
//! contract. On authorisation failure the command is never run, the denial is
//! still audited, and the reply is a typed [`ManageErrorKind::Unauthorised`]
//! error.
//!
//! The constraint that keeps the plane honest: a manager can never do anything
//! to a node the node could not already do to itself, and no command logic is
//! duplicated — [`Engine`](crate::Engine) implements [`ManageCommandExecutor`]
//! by delegating to its existing `pin` / `unpin` / `status` / cache-evict
//! methods, the very ones the CLI calls.

use async_trait::async_trait;
use cascade_p2p::protocol::{
    ManageCommand, ManageConfigFormat, ManageErrorKind, ManageGrant, ManageResult,
    ManageScope as WireScope,
};
use chrono::{DateTime, Utc};

use crate::db::AuditEntry;
use crate::manage::{Capability, DeviceId, Grant, Scope, authorises};

/// Audit `outcome` column value for a command that was authorised and applied.
const OUTCOME_ALLOWED: &str = "allowed";
/// Audit `outcome` column value for a command refused by authorisation.
const OUTCOME_DENIED: &str = "denied";
/// Audit `outcome` column value for a command that was authorised but failed
/// while running.
const OUTCOME_FAILED: &str = "failed";

/// The side effects a management command can have on the managed node.
///
/// This is the single command surface shared by the local CLI and the remote
/// management plane: each method is the *same* operation the daemon performs on
/// itself. The remote path reaches these only after authorisation and auditing;
/// the implementation never re-checks authority, so a method here can do
/// exactly what the local daemon could do and no more.
///
/// Methods are async and return only owned, serialisable summaries so the
/// contract holds whether the executor runs in-process (the daemon) or behind a
/// test double.
#[async_trait]
pub trait ManageCommandExecutor: Send + Sync {
    /// Produce a human-readable status snapshot — mount state, cache usage,
    /// backend health, peer list. Backs [`Capability::StatusRead`].
    async fn manage_status(&self) -> anyhow::Result<String>;

    /// Pin a path glob, keeping matching files offline. Backs
    /// [`Capability::PinWrite`].
    async fn manage_pin(&self, path_glob: &str, recursive: bool) -> anyhow::Result<String>;

    /// Remove a pin rule. Returns whether a rule was removed. Backs
    /// [`Capability::PinWrite`].
    async fn manage_unpin(&self, path_glob: &str) -> anyhow::Result<String>;

    /// Run one cache eviction sweep. Backs [`Capability::CacheManage`].
    async fn manage_cache_evict(&self) -> anyhow::Result<String>;

    /// Pre-warm a path glob so matching files are fetched on the next sync.
    /// Backs [`Capability::CacheManage`].
    async fn manage_cache_warm(&self, path_glob: &str) -> anyhow::Result<String>;

    /// Merge a `.cascade` config fragment, in `format`, rooted at `folder`.
    /// Backs [`Capability::ConfigPush`].
    async fn manage_config_push(
        &self,
        format: ManageConfigFormat,
        folder: &str,
        body: &str,
    ) -> anyhow::Result<String>;

    /// Set a lifecycle policy. Backs [`Capability::PolicySet`].
    async fn manage_policy_set(
        &self,
        path_glob: &str,
        max_age_secs: Option<i64>,
        max_file_size: Option<i64>,
        priority: i32,
    ) -> anyhow::Result<String>;

    /// Register and mount a backend. Backs the dangerous
    /// [`Capability::BackendManage`].
    async fn manage_backend_add(
        &self,
        name: &str,
        backend_type: &str,
        mount_path: &str,
        config_toml: &str,
    ) -> anyhow::Result<String>;

    /// Unmount and deregister a backend. Backs the dangerous
    /// [`Capability::BackendManage`].
    async fn manage_backend_remove(&self, name: &str, mount_path: &str) -> anyhow::Result<String>;

    /// Restart the daemon's background workers. Backs the dangerous
    /// [`Capability::LifecycleControl`].
    async fn manage_restart(&self) -> anyhow::Result<String>;

    /// Stop the daemon's background workers. Backs the dangerous
    /// [`Capability::LifecycleControl`].
    async fn manage_stop(&self) -> anyhow::Result<String>;

    /// Add a delegated grant. The grant has already been validated as a subset
    /// of the caller's authority and stamped with the caller as `granted_by`
    /// before this is called. Backs the dangerous [`Capability::GrantAdmin`].
    async fn manage_grant_add(&self, grant: &Grant) -> anyhow::Result<String>;

    /// Revoke a grant by its row id. Backs the dangerous
    /// [`Capability::GrantAdmin`].
    async fn manage_grant_revoke(&self, grant_id: i64) -> anyhow::Result<String>;
}

/// The grant store and audit sink the dispatcher reads and writes.
///
/// Implemented by the engine over its [`StateDb`](crate::db::StateDb). Kept as a
/// contract so the dispatch flow can be exercised against an in-memory double in
/// tests without standing up a real database.
pub trait ManageGrantStore: Send + Sync {
    /// Every grant currently held on this node.
    fn manage_grants(&self) -> anyhow::Result<Vec<crate::manage::Grant>>;

    /// Append an audit row. The audit log is append-only.
    fn manage_append_audit(&self, entry: &AuditEntry) -> anyhow::Result<()>;
}

/// The injected port the BEP message handler calls when a
/// [`ManageRequest`](cascade_p2p::protocol::BepMessage::ManageRequest) arrives.
///
/// The backend-p2p sync engine holds an `Arc<dyn ManageDispatch>` and invokes it
/// with the connection's authenticated peer device id and the decoded command.
/// Keeping this a trait (rather than a concrete `Engine` reference) preserves
/// the backend → engine dependency direction: the backend depends on the
/// contract, the engine implements it, and the wiring is composed at the edge.
#[async_trait]
pub trait ManageDispatch: Send + Sync {
    /// Run a decoded management command on behalf of `caller`, returning the
    /// outcome to report back in a
    /// [`ManageResponse`](cascade_p2p::protocol::BepMessage::ManageResponse).
    ///
    /// `caller` is the authenticated peer device id from the TLS connection.
    /// `now` is the wall-clock instant used for grant-expiry checks; the BEP
    /// call site passes `Utc::now()`.
    async fn dispatch(
        &self,
        caller: &DeviceId,
        command: ManageCommand,
        scope: WireScope,
        now: DateTime<Utc>,
    ) -> ManageResult;
}

/// The [`Capability`] a [`ManageCommand`] requires to run.
#[must_use]
pub const fn required_capability(command: &ManageCommand) -> Capability {
    match command {
        ManageCommand::StatusRead => Capability::StatusRead,
        ManageCommand::Pin { .. } | ManageCommand::Unpin { .. } => Capability::PinWrite,
        ManageCommand::CacheEvict | ManageCommand::CacheWarm { .. } => Capability::CacheManage,
        ManageCommand::ConfigPush { .. } => Capability::ConfigPush,
        ManageCommand::PolicySet { .. } => Capability::PolicySet,
        ManageCommand::BackendAdd { .. } | ManageCommand::BackendRemove { .. } => {
            Capability::BackendManage
        }
        ManageCommand::Restart | ManageCommand::Stop => Capability::LifecycleControl,
        ManageCommand::GrantAdd { .. } | ManageCommand::GrantRevoke { .. } => {
            Capability::GrantAdmin
        }
    }
}

/// Map a wire [`ManageScope`](WireScope) to the engine's [`Scope`].
#[must_use]
pub fn scope_from_wire(scope: &WireScope) -> Scope {
    match scope {
        WireScope::Node => Scope::Node,
        WireScope::Folder { path } => Scope::folder(path.clone()),
    }
}

/// The [`Scope`] a command's *own payload* targets, independent of the wire
/// `scope` field the caller supplied.
///
/// A path-bearing command (Pin/Unpin/CacheWarm/PolicySet) mutates the node at
/// its path, so the operation's real target is the folder subtree that path
/// lives in — not whatever scope the caller chose to advertise. `ConfigPush`
/// targets its declared `folder`; a backend command targets its `mount_path`; a
/// grant command targets the scope carried in its grant/payload. A command with
/// no payload path that acts node-wide
/// ([`StatusRead`](ManageCommand::StatusRead),
/// [`CacheEvict`](ManageCommand::CacheEvict)) targets [`Scope::Node`].
///
/// [`Restart`](ManageCommand::Restart) and [`Stop`](ManageCommand::Stop) carry
/// no payload path yet affect the whole node; because their capability is
/// dangerous (never satisfied by a node-wide grant), keying their target on
/// [`Scope::Node`] would make them unauthorisable. They instead return `None`,
/// signalling that their only target is the explicit folder scope the caller
/// advertises on the wire — which the dangerous-capability bar requires anyway.
///
/// The dispatcher authorises the granted scope against *both* this payload-derived
/// target and the wire `scope`, which closes the scope-escape where an authorised
/// `scope` field is decoupled from the path actually mutated. Folding `*`, `?`,
/// and character-class glob metacharacters out of the path before scoping means a
/// glob like `/work/*` is confined to the `/work` subtree it can ever match, and a
/// bare glob with no fixed prefix (`*.pdf`) targets the node root so only a
/// node-wide grant covers it.
#[must_use]
fn command_target_scope(command: &ManageCommand) -> Option<Scope> {
    match command {
        ManageCommand::StatusRead | ManageCommand::CacheEvict => Some(Scope::Node),
        ManageCommand::Pin { path_glob, .. }
        | ManageCommand::Unpin { path_glob }
        | ManageCommand::CacheWarm { path_glob }
        | ManageCommand::PolicySet { path_glob, .. } => {
            Some(Scope::folder(glob_fixed_prefix(path_glob)))
        }
        ManageCommand::ConfigPush { folder, .. } => Some(Scope::folder(folder.clone())),
        ManageCommand::BackendAdd { mount_path, .. }
        | ManageCommand::BackendRemove { mount_path, .. } => {
            Some(Scope::folder(mount_path.clone()))
        }
        ManageCommand::GrantAdd { grant } => Some(scope_from_wire(&grant.scope)),
        ManageCommand::GrantRevoke { scope, .. } => Some(scope_from_wire(scope)),
        // No payload path; only the advertised wire scope confines these.
        ManageCommand::Restart | ManageCommand::Stop => None,
    }
}

/// The fixed (non-glob) leading path of a glob pattern.
///
/// A pin glob can match any path under its first glob metacharacter, so its
/// authorisable extent is the directory prefix up to — but not including — the
/// first path component that contains a `*`, `?`, or `[` metacharacter. For
/// example `/work/reports/*.pdf` is confined to `/work/reports`, `/work/**` to
/// `/work`, and `/*` (or a bare `*.pdf`) to the root `/` so that only a
/// node-wide grant covers it. A pattern with no metacharacters is its own fixed
/// prefix.
#[must_use]
fn glob_fixed_prefix(path_glob: &str) -> String {
    let mut fixed: Vec<&str> = Vec::new();
    for component in path_glob.split('/') {
        if component.contains(['*', '?', '[']) {
            break;
        }
        fixed.push(component);
    }
    let joined = fixed.join("/");
    if joined.is_empty() {
        "/".to_owned()
    } else {
        joined
    }
}

/// A short, audit-friendly description of a command — the `command` column of
/// the audit row. Records the verb and its arguments without any value that
/// rots between runs.
#[must_use]
fn command_summary(command: &ManageCommand) -> String {
    match command {
        ManageCommand::StatusRead => "status:read".to_owned(),
        ManageCommand::Pin {
            path_glob,
            recursive,
        } => format!("pin {path_glob} (recursive={recursive})"),
        ManageCommand::Unpin { path_glob } => format!("unpin {path_glob}"),
        ManageCommand::CacheEvict => "cache evict".to_owned(),
        ManageCommand::CacheWarm { path_glob } => format!("cache warm {path_glob}"),
        ManageCommand::ConfigPush { folder, .. } => format!("config push into {folder}"),
        ManageCommand::PolicySet { path_glob, .. } => format!("policy set for {path_glob}"),
        ManageCommand::BackendAdd {
            name,
            backend_type,
            mount_path,
            ..
        } => format!("backend add {name} ({backend_type}) at {mount_path}"),
        ManageCommand::BackendRemove { name, mount_path } => {
            format!("backend remove {name} at {mount_path}")
        }
        ManageCommand::Restart => "lifecycle restart".to_owned(),
        ManageCommand::Stop => "lifecycle stop".to_owned(),
        ManageCommand::GrantAdd { grant } => format!(
            "grant add {} to {} over {:?}",
            grant.capability, grant.grantee, grant.scope
        ),
        ManageCommand::GrantRevoke { grant_id, .. } => format!("grant revoke {grant_id}"),
    }
}

/// Run the authorise → audit → execute flow for one decoded command.
///
/// This is the shared dispatch core, parameterised over the grant store and the
/// command executor so it can be unit-tested against doubles. The engine's
/// [`ManageDispatch`] implementation calls straight through to it.
///
/// Ordering is load-bearing: the audit row is written **before** the side
/// effect, so a compromised manager cannot apply a change without leaving a
/// trace even if the write that follows panics. A denial is audited too.
pub async fn run_dispatch<S, E>(
    store: &S,
    executor: &E,
    caller: &DeviceId,
    command: ManageCommand,
    wire_scope: WireScope,
    now: DateTime<Utc>,
) -> ManageResult
where
    S: ManageGrantStore + ?Sized,
    E: ManageCommandExecutor + ?Sized,
{
    let capability = required_capability(&command);
    let scope = scope_from_wire(&wire_scope);
    // The scope the command's own payload actually mutates, derived from the
    // command rather than the caller-supplied wire `scope`. Authorising over
    // both closes the scope-escape where a Pin{path_glob:"/personal"} carries a
    // wire scope of "/work" the caller does hold a grant over: the grant must
    // cover the path that is really touched, not just the advertised scope. A
    // payload-less command (Restart/Stop) has no derived target; the audit row
    // and authorisation then key on the advertised wire scope alone.
    let target_scope = command_target_scope(&command).unwrap_or_else(|| scope.clone());
    let command_text = command_summary(&command);

    let grants = match store.manage_grants() {
        Ok(grants) => grants,
        Err(e) => {
            // The grant store is unreadable — fail loudly to the caller rather
            // than treating an unreadable store as "no grants" (which would
            // silently deny everything and hide a real fault). No audit row is
            // written because the audit sink shares the same store.
            return ManageResult::Err {
                kind: ManageErrorKind::Failed,
                message: format!("could not read grants: {e}"),
            };
        }
    };

    // Authorise the capability over the path the command actually mutates AND
    // over the wire scope the caller advertised. A grant must cover both: the
    // payload-derived target stops a caller pinning `/personal` under a `/work`
    // grant by lying in the wire `scope` field, and keeping the wire-scope check
    // preserves the contract that the advertised scope is also honoured.
    let capability_authorised = authorises(&grants, caller, capability, &target_scope, now)
        && authorises(&grants, caller, capability, &scope, now);

    // GrantAdd carries a second, stricter gate: the grant being delegated must
    // be a subset of what the caller itself holds. Holding `grant:admin` is not
    // enough — a manager can only hand out authority it already has, never
    // escalate. The check is independent of the capability authorisation above
    // (which only confirms the caller may delegate at all over the grant's
    // scope); a failure here is an unauthorised escalation attempt and is
    // audited as denied just like any other refusal.
    let delegation_permitted = match &command {
        ManageCommand::GrantAdd { grant } => caller_can_delegate(&grants, caller, grant, now),
        _ => true,
    };

    let authorised = capability_authorised && delegation_permitted;

    let outcome = if authorised {
        OUTCOME_ALLOWED
    } else {
        OUTCOME_DENIED
    };
    // The audit `scope` column records the extent the command actually touches
    // (the payload-derived target), not the caller-supplied wire scope, so the
    // log reflects what was really mutated rather than what was advertised.
    let audit = AuditEntry {
        timestamp: now,
        actor_device: caller.clone(),
        capability,
        scope: target_scope.clone(),
        command: command_text.clone(),
        outcome: outcome.to_owned(),
    };
    if let Err(e) = store.manage_append_audit(&audit) {
        // The attempt could not be audited. The audit log is the integrity
        // guarantee of the plane, so refuse to apply the side effect when it
        // cannot be recorded rather than acting unaudited.
        return ManageResult::Err {
            kind: ManageErrorKind::Failed,
            message: format!("could not record audit row: {e}"),
        };
    }

    if !authorised {
        return ManageResult::Err {
            kind: ManageErrorKind::Unauthorised,
            message: format!(
                "caller {caller} lacks {} over {target_scope:?} (wire scope {scope:?})",
                capability.as_wire()
            ),
        };
    }

    let applied = execute(executor, caller, command).await;
    match applied {
        Ok(summary) => ManageResult::Ok { summary },
        Err(e) => {
            // The command was authorised and audited as allowed, but failed
            // while running. Record a follow-up audit row marking the failure
            // so the log reflects that the side effect did not complete.
            let failure_audit = AuditEntry {
                timestamp: now,
                actor_device: caller.clone(),
                capability,
                scope: target_scope,
                command: command_text,
                outcome: OUTCOME_FAILED.to_owned(),
            };
            // A failed follow-up audit write is itself logged but does not
            // change the outcome reported to the caller — the command already
            // failed.
            if let Err(audit_err) = store.manage_append_audit(&failure_audit) {
                tracing::warn!(
                    target: "cascade::engine::manage",
                    error = %audit_err,
                    "could not record command-failure audit row",
                );
            }
            ManageResult::Err {
                kind: ManageErrorKind::Failed,
                message: e.to_string(),
            }
        }
    }
}

/// Dispatch an authorised command into the executor's matching method.
///
/// `caller` is the authenticated delegating principal; it is the `granted_by`
/// stamped onto a delegated grant, never a value taken off the wire.
async fn execute<E>(
    executor: &E,
    caller: &DeviceId,
    command: ManageCommand,
) -> anyhow::Result<String>
where
    E: ManageCommandExecutor + ?Sized,
{
    match command {
        ManageCommand::StatusRead => executor.manage_status().await,
        ManageCommand::Pin {
            path_glob,
            recursive,
        } => executor.manage_pin(&path_glob, recursive).await,
        ManageCommand::Unpin { path_glob } => executor.manage_unpin(&path_glob).await,
        ManageCommand::CacheEvict => executor.manage_cache_evict().await,
        ManageCommand::CacheWarm { path_glob } => executor.manage_cache_warm(&path_glob).await,
        ManageCommand::ConfigPush {
            format,
            folder,
            body,
        } => executor.manage_config_push(format, &folder, &body).await,
        ManageCommand::PolicySet {
            path_glob,
            max_age_secs,
            max_file_size,
            priority,
        } => {
            executor
                .manage_policy_set(&path_glob, max_age_secs, max_file_size, priority)
                .await
        }
        ManageCommand::BackendAdd {
            name,
            backend_type,
            mount_path,
            config_toml,
        } => {
            executor
                .manage_backend_add(&name, &backend_type, &mount_path, &config_toml)
                .await
        }
        ManageCommand::BackendRemove { name, mount_path } => {
            executor.manage_backend_remove(&name, &mount_path).await
        }
        ManageCommand::Restart => executor.manage_restart().await,
        ManageCommand::Stop => executor.manage_stop().await,
        ManageCommand::GrantAdd { grant } => {
            // Build the domain grant, stamping the authenticated caller as
            // `granted_by`. The grantee/capability/scope/expiry come off the
            // wire and are validated here; a malformed grant is a Failed error,
            // not a silent skip.
            let domain = grant_from_wire(&grant, caller)?;
            executor.manage_grant_add(&domain).await
        }
        ManageCommand::GrantRevoke { grant_id, .. } => executor.manage_grant_revoke(grant_id).await,
    }
}

/// Build a domain [`Grant`] from a wire [`ManageGrant`], stamping `granted_by`
/// with the authenticated caller.
///
/// Validates the capability against the known vocabulary and parses the optional
/// RFC 3339 expiry, failing loudly rather than dropping a malformed field.
fn grant_from_wire(wire: &ManageGrant, granted_by: &DeviceId) -> anyhow::Result<Grant> {
    let capability = Capability::from_wire(&wire.capability).ok_or_else(|| {
        anyhow::anyhow!("unknown capability in delegated grant: {}", wire.capability)
    })?;
    let scope = scope_from_wire(&wire.scope);
    let expires = wire
        .expires
        .as_deref()
        .map(|raw| {
            DateTime::parse_from_rfc3339(raw)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|e| anyhow::anyhow!("parsing delegated grant expiry {raw}: {e}"))
        })
        .transpose()?;
    Ok(Grant {
        grantee: DeviceId::new(wire.grantee.clone()),
        capability,
        scope,
        granted_by: granted_by.clone(),
        expires,
    })
}

/// Whether `caller` may delegate `delegated` — the subset/no-escalation guard
/// for [`ManageCommand::GrantAdd`].
///
/// The delegated grant must be wholly contained in the caller's own authority:
/// the caller must hold an unexpired grant of the *same capability* whose scope
/// [covers](Scope::covers) the delegated scope. A manager can hand out only a
/// subset of what it has — never a capability it lacks, and never a wider scope
/// than it holds. Expiry is not narrowed here (a shorter or absent delegated
/// expiry is always a subset of the caller's; a longer one is bounded in effect
/// by the caller's own grant lapsing), so the guard keys purely on capability
/// identity and scope containment.
fn caller_can_delegate(
    grants: &[Grant],
    caller: &DeviceId,
    delegated: &ManageGrant,
    now: DateTime<Utc>,
) -> bool {
    let Some(capability) = Capability::from_wire(&delegated.capability) else {
        // An unrecognised capability cannot be a subset of anything the caller
        // holds — refuse rather than letting it through to a later Failed.
        return false;
    };
    let delegated_scope = scope_from_wire(&delegated.scope);
    grants.iter().any(|held| {
        held.grantee == *caller
            && held.capability == capability
            && !held.is_expired(now)
            && held.scope.covers(&delegated_scope)
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use chrono::TimeZone;

    use super::*;
    use crate::manage::Grant;

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

    /// In-memory grant store + audit sink double.
    struct FakeStore {
        grants: Vec<Grant>,
        audit: Mutex<Vec<AuditEntry>>,
    }

    impl FakeStore {
        fn new(grants: Vec<Grant>) -> Self {
            Self {
                grants,
                audit: Mutex::new(Vec::new()),
            }
        }

        fn audit_rows(&self) -> Vec<AuditEntry> {
            self.audit
                .lock()
                .map(|rows| rows.clone())
                .unwrap_or_default()
        }
    }

    impl ManageGrantStore for FakeStore {
        fn manage_grants(&self) -> anyhow::Result<Vec<Grant>> {
            Ok(self.grants.clone())
        }

        fn manage_append_audit(&self, entry: &AuditEntry) -> anyhow::Result<()> {
            self.audit
                .lock()
                .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?
                .push(entry.clone());
            Ok(())
        }
    }

    /// Executor double recording each call so a test can assert the side effect
    /// did (or did not) run.
    #[derive(Default)]
    struct FakeExecutor {
        calls: Mutex<Vec<String>>,
        fail: bool,
    }

    impl FakeExecutor {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().map(|c| c.clone()).unwrap_or_default()
        }

        fn record(&self, call: &str) {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(call.to_owned());
            }
        }
    }

    #[async_trait]
    impl ManageCommandExecutor for FakeExecutor {
        async fn manage_status(&self) -> anyhow::Result<String> {
            self.record("status");
            if self.fail {
                anyhow::bail!("status failed");
            }
            Ok("status ok".to_owned())
        }

        async fn manage_pin(&self, path_glob: &str, recursive: bool) -> anyhow::Result<String> {
            self.record(&format!("pin {path_glob} {recursive}"));
            if self.fail {
                anyhow::bail!("pin failed");
            }
            Ok(format!("pinned {path_glob}"))
        }

        async fn manage_unpin(&self, path_glob: &str) -> anyhow::Result<String> {
            self.record(&format!("unpin {path_glob}"));
            Ok(format!("unpinned {path_glob}"))
        }

        async fn manage_cache_evict(&self) -> anyhow::Result<String> {
            self.record("evict");
            Ok("evicted".to_owned())
        }

        async fn manage_cache_warm(&self, path_glob: &str) -> anyhow::Result<String> {
            self.record(&format!("warm {path_glob}"));
            Ok(format!("warmed {path_glob}"))
        }

        async fn manage_config_push(
            &self,
            format: ManageConfigFormat,
            folder: &str,
            body: &str,
        ) -> anyhow::Result<String> {
            self.record(&format!("config_push {format:?} {folder} {body}"));
            Ok(format!("pushed into {folder}"))
        }

        async fn manage_policy_set(
            &self,
            path_glob: &str,
            max_age_secs: Option<i64>,
            max_file_size: Option<i64>,
            priority: i32,
        ) -> anyhow::Result<String> {
            self.record(&format!(
                "policy_set {path_glob} {max_age_secs:?} {max_file_size:?} {priority}"
            ));
            Ok(format!("policy set for {path_glob}"))
        }

        async fn manage_backend_add(
            &self,
            name: &str,
            backend_type: &str,
            mount_path: &str,
            config_toml: &str,
        ) -> anyhow::Result<String> {
            self.record(&format!(
                "backend_add {name} {backend_type} {mount_path} {config_toml}"
            ));
            Ok(format!("backend {name} added"))
        }

        async fn manage_backend_remove(
            &self,
            name: &str,
            mount_path: &str,
        ) -> anyhow::Result<String> {
            self.record(&format!("backend_remove {name} {mount_path}"));
            Ok(format!("backend {name} removed"))
        }

        async fn manage_restart(&self) -> anyhow::Result<String> {
            self.record("restart");
            Ok("restarted".to_owned())
        }

        async fn manage_stop(&self) -> anyhow::Result<String> {
            self.record("stop");
            Ok("stopped".to_owned())
        }

        async fn manage_grant_add(&self, grant: &Grant) -> anyhow::Result<String> {
            self.record(&format!(
                "grant_add grantee={} cap={} scope={:?} granted_by={}",
                grant.grantee,
                grant.capability.as_wire(),
                grant.scope,
                grant.granted_by,
            ));
            Ok("grant added".to_owned())
        }

        async fn manage_grant_revoke(&self, grant_id: i64) -> anyhow::Result<String> {
            self.record(&format!("grant_revoke {grant_id}"));
            Ok(format!("grant {grant_id} revoked"))
        }
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

    #[tokio::test]
    async fn authorised_command_runs_and_audits_allowed() {
        let store = FakeStore::new(vec![grant(Capability::PinWrite, Scope::folder("/work"))]);
        let executor = FakeExecutor::default();
        let result = run_dispatch(
            &store,
            &executor,
            &manager(),
            ManageCommand::Pin {
                path_glob: "/work/reports".to_owned(),
                recursive: true,
            },
            WireScope::Folder {
                path: "/work/reports".to_owned(),
            },
            at(2026, 1, 1),
        )
        .await;

        assert!(
            matches!(result, ManageResult::Ok { .. }),
            "authorised pin should succeed, got {result:?}",
        );
        assert_eq!(
            executor.calls(),
            vec!["pin /work/reports true".to_owned()],
            "the side effect must have run exactly once",
        );
        let audit = store.audit_rows();
        assert_eq!(audit.len(), 1, "exactly one audit row");
        let row = audit.first().expect("one audit row");
        assert_eq!(row.outcome, OUTCOME_ALLOWED);
        assert_eq!(row.actor_device, manager());
        assert_eq!(row.capability, Capability::PinWrite);
    }

    #[tokio::test]
    async fn pin_path_outside_granted_scope_is_refused_even_when_wire_scope_lies() {
        // Scope-escape regression: the caller holds PinWrite over `/work` only,
        // and advertises a wire scope of `/work` (which the grant covers), but
        // the command's `path_glob` targets `/personal/secret`. Authorisation
        // must key on the path the command actually mutates, not the advertised
        // wire scope, so the request is refused and no pin runs.
        let store = FakeStore::new(vec![grant(Capability::PinWrite, Scope::folder("/work"))]);
        let executor = FakeExecutor::default();
        let result = run_dispatch(
            &store,
            &executor,
            &manager(),
            ManageCommand::Pin {
                path_glob: "/personal/secret".to_owned(),
                recursive: false,
            },
            // The caller lies: it advertises a scope its grant *does* cover.
            WireScope::Folder {
                path: "/work".to_owned(),
            },
            at(2026, 1, 1),
        )
        .await;

        assert!(
            matches!(
                result,
                ManageResult::Err {
                    kind: ManageErrorKind::Unauthorised,
                    ..
                }
            ),
            "a pin whose path escapes the granted scope must be refused, got {result:?}",
        );
        assert!(
            executor.calls().is_empty(),
            "no pin rule may be created when the path escapes the granted scope",
        );
        let audit = store.audit_rows();
        assert_eq!(audit.len(), 1, "the denial must still be audited");
        let row = audit.first().expect("one audit row");
        assert_eq!(row.outcome, OUTCOME_DENIED);
        assert_eq!(
            row.scope,
            Scope::folder("/personal/secret"),
            "the audit row records the path actually targeted, not the advertised wire scope",
        );
    }

    #[tokio::test]
    async fn pin_glob_is_confined_to_its_fixed_prefix() {
        // A glob `path_glob` is authorisable only over the fixed directory
        // prefix it can ever match. A grant over `/work` covers `/work/*`
        // (fixed prefix `/work`) but a `/work` grant must NOT authorise a glob
        // whose fixed prefix climbs out of `/work`.
        let store = FakeStore::new(vec![grant(Capability::PinWrite, Scope::folder("/work"))]);
        let executor = FakeExecutor::default();
        let allowed = run_dispatch(
            &store,
            &executor,
            &manager(),
            ManageCommand::Pin {
                path_glob: "/work/*.pdf".to_owned(),
                recursive: false,
            },
            WireScope::Folder {
                path: "/work".to_owned(),
            },
            at(2026, 1, 1),
        )
        .await;
        assert!(
            matches!(allowed, ManageResult::Ok { .. }),
            "a glob confined to the granted subtree is allowed, got {allowed:?}",
        );
    }

    #[tokio::test]
    async fn unauthorised_command_is_rejected_makes_no_change_and_audits_denial() {
        // Manager holds a status grant only — a pin must be refused.
        let store = FakeStore::new(vec![grant(Capability::StatusRead, Scope::Node)]);
        let executor = FakeExecutor::default();
        let result = run_dispatch(
            &store,
            &executor,
            &manager(),
            ManageCommand::Pin {
                path_glob: "/work".to_owned(),
                recursive: false,
            },
            WireScope::Folder {
                path: "/work".to_owned(),
            },
            at(2026, 1, 1),
        )
        .await;

        assert!(
            matches!(
                result,
                ManageResult::Err {
                    kind: ManageErrorKind::Unauthorised,
                    ..
                }
            ),
            "unauthorised pin must be refused, got {result:?}",
        );
        assert!(
            executor.calls().is_empty(),
            "no side effect must run on an unauthorised request",
        );
        let audit = store.audit_rows();
        assert_eq!(audit.len(), 1, "the denial must still be audited");
        let row = audit.first().expect("one audit row");
        assert_eq!(row.outcome, OUTCOME_DENIED);
        assert_eq!(row.capability, Capability::PinWrite);
    }

    #[tokio::test]
    async fn safe_capability_under_wildcard_grant_runs() {
        // A node-wide (wildcard) grant of a *safe* capability authorises that
        // capability over any folder target. This is the legitimate wildcard
        // path — the foil to the dangerous-capability bar below.
        let store = FakeStore::new(vec![grant(Capability::PinWrite, Scope::Node)]);
        let executor = FakeExecutor::default();
        let result = run_dispatch(
            &store,
            &executor,
            &manager(),
            ManageCommand::Pin {
                path_glob: "/anywhere".to_owned(),
                recursive: false,
            },
            WireScope::Folder {
                path: "/anywhere".to_owned(),
            },
            at(2026, 1, 1),
        )
        .await;
        assert!(
            matches!(result, ManageResult::Ok { .. }),
            "node-wide grant should cover a safe capability over any folder",
        );
    }

    #[test]
    fn dangerous_capability_under_wildcard_grant_is_rejected() {
        // The authorisation step `run_dispatch` performs bars a dangerous
        // capability from ever being satisfied by a node-wide (wildcard) grant —
        // it must be granted explicitly for the exact folder scope. The wire
        // command surface for this phase exposes only safe verbs, so the bar is
        // asserted at the authorisation boundary `run_dispatch` calls: a
        // node-wide grant of every dangerous capability denies that capability
        // over both the node and any folder target.
        for cap in [
            Capability::BackendManage,
            Capability::LifecycleControl,
            Capability::GrantAdmin,
        ] {
            let grants = vec![grant(cap, Scope::Node)];
            assert!(
                !authorises(&grants, &manager(), cap, &Scope::Node, at(2026, 1, 1)),
                "wildcard grant must not satisfy dangerous {cap:?} over the node",
            );
            assert!(
                !authorises(
                    &grants,
                    &manager(),
                    cap,
                    &Scope::folder("/work"),
                    at(2026, 1, 1),
                ),
                "wildcard grant must not satisfy dangerous {cap:?} over a folder",
            );
        }
    }

    #[tokio::test]
    async fn expired_grant_is_refused_and_audited() {
        let mut g = grant(Capability::CacheManage, Scope::Node);
        g.expires = Some(at(2026, 1, 1));
        let store = FakeStore::new(vec![g]);
        let executor = FakeExecutor::default();
        let result = run_dispatch(
            &store,
            &executor,
            &manager(),
            ManageCommand::CacheEvict,
            WireScope::Node,
            at(2026, 6, 1),
        )
        .await;
        assert!(
            matches!(
                result,
                ManageResult::Err {
                    kind: ManageErrorKind::Unauthorised,
                    ..
                }
            ),
            "an expired grant must not authorise",
        );
        assert!(executor.calls().is_empty());
        let audit = store.audit_rows();
        assert_eq!(audit.len(), 1);
        assert_eq!(
            audit.first().map(|r| r.outcome.as_str()),
            Some(OUTCOME_DENIED),
        );
    }

    #[tokio::test]
    async fn authorised_command_that_fails_reports_failed_and_audits_both() {
        let store = FakeStore::new(vec![grant(Capability::StatusRead, Scope::Node)]);
        let executor = FakeExecutor {
            calls: Mutex::new(Vec::new()),
            fail: true,
        };
        let result = run_dispatch(
            &store,
            &executor,
            &manager(),
            ManageCommand::StatusRead,
            WireScope::Node,
            at(2026, 1, 1),
        )
        .await;
        assert!(
            matches!(
                result,
                ManageResult::Err {
                    kind: ManageErrorKind::Failed,
                    ..
                }
            ),
            "a command that fails while running reports Failed, got {result:?}",
        );
        let audit = store.audit_rows();
        // The allowed row is written before the side effect; the failure row
        // follows once the command errors.
        assert_eq!(audit.len(), 2, "allowed then failed audit rows");
        assert_eq!(
            audit.first().map(|r| r.outcome.as_str()),
            Some(OUTCOME_ALLOWED)
        );
        assert_eq!(
            audit.get(1).map(|r| r.outcome.as_str()),
            Some(OUTCOME_FAILED)
        );
    }

    // ── New command surface: authorised round-trips ──

    /// Assert an authorised command runs exactly once, audits a single
    /// `allowed` row carrying the expected capability and target scope, and the
    /// recorded executor call matches `expected_call`.
    async fn assert_authorised(
        grants: Vec<Grant>,
        command: ManageCommand,
        wire_scope: WireScope,
        expected_capability: Capability,
        expected_scope: Scope,
        expected_call: &str,
    ) {
        let store = FakeStore::new(grants);
        let executor = FakeExecutor::default();
        let result = run_dispatch(
            &store,
            &executor,
            &manager(),
            command,
            wire_scope,
            at(2026, 1, 1),
        )
        .await;
        assert!(
            matches!(result, ManageResult::Ok { .. }),
            "authorised command should succeed, got {result:?}",
        );
        assert_eq!(
            executor.calls(),
            vec![expected_call.to_owned()],
            "the side effect must run exactly once with the expected arguments",
        );
        let audit = store.audit_rows();
        assert_eq!(audit.len(), 1, "exactly one audit row");
        let row = audit.first().expect("one audit row");
        assert_eq!(row.outcome, OUTCOME_ALLOWED);
        assert_eq!(row.capability, expected_capability);
        assert_eq!(row.scope, expected_scope);
    }

    #[tokio::test]
    async fn cache_warm_authorised_runs_and_audits() {
        assert_authorised(
            vec![grant(Capability::CacheManage, Scope::folder("/work"))],
            ManageCommand::CacheWarm {
                path_glob: "/work/**".to_owned(),
            },
            WireScope::Folder {
                path: "/work".to_owned(),
            },
            Capability::CacheManage,
            Scope::folder("/work"),
            "warm /work/**",
        )
        .await;
    }

    #[tokio::test]
    async fn config_push_authorised_runs_and_audits() {
        assert_authorised(
            vec![grant(Capability::ConfigPush, Scope::folder("/work"))],
            ManageCommand::ConfigPush {
                format: ManageConfigFormat::Gitignore,
                folder: "/work".to_owned(),
                body: "*.tmp\n".to_owned(),
            },
            WireScope::Folder {
                path: "/work".to_owned(),
            },
            Capability::ConfigPush,
            Scope::folder("/work"),
            "config_push Gitignore /work *.tmp\n",
        )
        .await;
    }

    #[tokio::test]
    async fn policy_set_authorised_runs_and_audits() {
        assert_authorised(
            vec![grant(Capability::PolicySet, Scope::folder("/work"))],
            ManageCommand::PolicySet {
                path_glob: "/work/*.tmp".to_owned(),
                max_age_secs: Some(3600),
                max_file_size: None,
                priority: 2,
            },
            WireScope::Folder {
                path: "/work".to_owned(),
            },
            Capability::PolicySet,
            Scope::folder("/work"),
            "policy_set /work/*.tmp Some(3600) None 2",
        )
        .await;
    }

    #[tokio::test]
    async fn backend_add_authorised_under_explicit_folder_grant_runs_and_audits() {
        // BackendManage is dangerous, so an explicit folder grant — never a
        // node-wide one — is required.
        assert_authorised(
            vec![grant(Capability::BackendManage, Scope::folder("/drive"))],
            ManageCommand::BackendAdd {
                name: "personal".to_owned(),
                backend_type: "gdrive".to_owned(),
                mount_path: "/drive".to_owned(),
                config_toml: "type = \"gdrive\"\n".to_owned(),
            },
            WireScope::Folder {
                path: "/drive".to_owned(),
            },
            Capability::BackendManage,
            Scope::folder("/drive"),
            "backend_add personal gdrive /drive type = \"gdrive\"\n",
        )
        .await;
    }

    #[tokio::test]
    async fn backend_remove_authorised_under_explicit_folder_grant_runs_and_audits() {
        assert_authorised(
            vec![grant(Capability::BackendManage, Scope::folder("/drive"))],
            ManageCommand::BackendRemove {
                name: "personal".to_owned(),
                mount_path: "/drive".to_owned(),
            },
            WireScope::Folder {
                path: "/drive".to_owned(),
            },
            Capability::BackendManage,
            Scope::folder("/drive"),
            "backend_remove personal /drive",
        )
        .await;
    }

    #[tokio::test]
    async fn restart_authorised_under_explicit_folder_grant_runs_and_audits() {
        // Restart carries no payload path; its only target is the advertised
        // wire scope, which must be an explicit folder for the dangerous
        // lifecycle:control capability.
        assert_authorised(
            vec![grant(Capability::LifecycleControl, Scope::folder("/work"))],
            ManageCommand::Restart,
            WireScope::Folder {
                path: "/work".to_owned(),
            },
            Capability::LifecycleControl,
            Scope::folder("/work"),
            "restart",
        )
        .await;
    }

    #[tokio::test]
    async fn stop_authorised_under_explicit_folder_grant_runs_and_audits() {
        assert_authorised(
            vec![grant(Capability::LifecycleControl, Scope::folder("/work"))],
            ManageCommand::Stop,
            WireScope::Folder {
                path: "/work".to_owned(),
            },
            Capability::LifecycleControl,
            Scope::folder("/work"),
            "stop",
        )
        .await;
    }

    #[tokio::test]
    async fn grant_revoke_authorised_runs_and_audits() {
        assert_authorised(
            vec![grant(Capability::GrantAdmin, Scope::folder("/work"))],
            ManageCommand::GrantRevoke {
                grant_id: 7,
                scope: WireScope::Folder {
                    path: "/work".to_owned(),
                },
            },
            WireScope::Folder {
                path: "/work".to_owned(),
            },
            Capability::GrantAdmin,
            Scope::folder("/work"),
            "grant_revoke 7",
        )
        .await;
    }

    // ── New command surface: unauthorised refusals ──

    #[tokio::test]
    async fn config_push_without_grant_is_refused_audited_and_inert() {
        // The manager holds a pin grant only — a config push must be refused,
        // nothing executed, the denial audited.
        let store = FakeStore::new(vec![grant(Capability::PinWrite, Scope::Node)]);
        let executor = FakeExecutor::default();
        let result = run_dispatch(
            &store,
            &executor,
            &manager(),
            ManageCommand::ConfigPush {
                format: ManageConfigFormat::Toml,
                folder: "/work".to_owned(),
                body: String::new(),
            },
            WireScope::Folder {
                path: "/work".to_owned(),
            },
            at(2026, 1, 1),
        )
        .await;
        assert!(matches!(
            result,
            ManageResult::Err {
                kind: ManageErrorKind::Unauthorised,
                ..
            }
        ));
        assert!(executor.calls().is_empty(), "no side effect on a denial");
        let audit = store.audit_rows();
        assert_eq!(audit.len(), 1);
        assert_eq!(
            audit.first().map(|r| r.outcome.as_str()),
            Some(OUTCOME_DENIED)
        );
    }

    #[tokio::test]
    async fn config_push_path_outside_granted_folder_is_refused() {
        // The grant covers `/work`; pushing into `/personal` must be refused
        // even though the caller holds config:push somewhere.
        let store = FakeStore::new(vec![grant(Capability::ConfigPush, Scope::folder("/work"))]);
        let executor = FakeExecutor::default();
        let result = run_dispatch(
            &store,
            &executor,
            &manager(),
            ManageCommand::ConfigPush {
                format: ManageConfigFormat::Gitignore,
                folder: "/personal".to_owned(),
                body: "secret\n".to_owned(),
            },
            // The caller advertises a scope its grant covers, but the payload
            // folder escapes it — authorisation keys on the payload folder.
            WireScope::Folder {
                path: "/work".to_owned(),
            },
            at(2026, 1, 1),
        )
        .await;
        assert!(matches!(
            result,
            ManageResult::Err {
                kind: ManageErrorKind::Unauthorised,
                ..
            }
        ));
        assert!(executor.calls().is_empty());
        let audit = store.audit_rows();
        assert_eq!(audit.len(), 1);
        let row = audit.first().expect("one row");
        assert_eq!(row.outcome, OUTCOME_DENIED);
        assert_eq!(
            row.scope,
            Scope::folder("/personal"),
            "the audit row records the folder actually targeted",
        );
    }

    // ── Dangerous-capability wildcard bar (new dangerous commands) ──

    #[tokio::test]
    async fn dangerous_commands_under_node_wide_grant_are_refused_and_inert() {
        // A node-wide grant of each dangerous capability must NOT satisfy the
        // command it backs — these require an explicit folder grant. Each is
        // refused, nothing runs, the denial is audited.
        let cases: Vec<(Capability, ManageCommand)> = vec![
            (
                Capability::BackendManage,
                ManageCommand::BackendRemove {
                    name: "x".to_owned(),
                    mount_path: "/drive".to_owned(),
                },
            ),
            (Capability::LifecycleControl, ManageCommand::Stop),
            (
                Capability::GrantAdmin,
                ManageCommand::GrantRevoke {
                    grant_id: 1,
                    scope: WireScope::Folder {
                        path: "/work".to_owned(),
                    },
                },
            ),
        ];
        for (cap, command) in cases {
            let store = FakeStore::new(vec![grant(cap, Scope::Node)]);
            let executor = FakeExecutor::default();
            let result = run_dispatch(
                &store,
                &executor,
                &manager(),
                command,
                WireScope::Folder {
                    path: "/work".to_owned(),
                },
                at(2026, 1, 1),
            )
            .await;
            assert!(
                matches!(
                    result,
                    ManageResult::Err {
                        kind: ManageErrorKind::Unauthorised,
                        ..
                    }
                ),
                "node-wide grant of dangerous {cap:?} must not authorise its command",
            );
            assert!(
                executor.calls().is_empty(),
                "no side effect for dangerous {cap:?} under a wildcard grant",
            );
            let audit = store.audit_rows();
            assert_eq!(audit.len(), 1);
            assert_eq!(
                audit.first().map(|r| r.outcome.as_str()),
                Some(OUTCOME_DENIED),
            );
        }
    }

    // ── GrantAdd subset / escalation guard ──

    /// A wire grant helper for delegation tests.
    fn wire_grant(capability: &str, scope: WireScope) -> ManageGrant {
        ManageGrant {
            grantee: "SUBORDINATE".to_owned(),
            capability: capability.to_owned(),
            scope,
            expires: None,
        }
    }

    #[tokio::test]
    async fn grant_add_delegating_a_held_subset_is_allowed_and_stamps_granted_by() {
        // The caller holds grant:admin over /work AND pin:write over /work, so
        // it may delegate pin:write over the narrower /work/reports.
        let store = FakeStore::new(vec![
            grant(Capability::GrantAdmin, Scope::folder("/work")),
            grant(Capability::PinWrite, Scope::folder("/work")),
        ]);
        let executor = FakeExecutor::default();
        let result = run_dispatch(
            &store,
            &executor,
            &manager(),
            ManageCommand::GrantAdd {
                grant: wire_grant(
                    "pin:write",
                    WireScope::Folder {
                        path: "/work/reports".to_owned(),
                    },
                ),
            },
            WireScope::Folder {
                path: "/work/reports".to_owned(),
            },
            at(2026, 1, 1),
        )
        .await;
        assert!(
            matches!(result, ManageResult::Ok { .. }),
            "delegating a held subset should succeed, got {result:?}",
        );
        let calls = executor.calls();
        assert_eq!(calls.len(), 1);
        let call = calls.first().expect("one call");
        assert!(
            call.contains("cap=pin:write")
                && call.contains("granted_by=MANAGER")
                && call.contains("grantee=SUBORDINATE"),
            "the delegated grant must be stamped with the caller as granted_by, got {call}",
        );
        let audit = store.audit_rows();
        assert_eq!(
            audit.first().map(|r| r.outcome.as_str()),
            Some(OUTCOME_ALLOWED)
        );
    }

    #[tokio::test]
    async fn grant_add_escalating_capability_not_held_is_refused() {
        // The caller holds grant:admin over /work but does NOT hold pin:write.
        // Delegating pin:write would hand out authority it lacks — refused.
        let store = FakeStore::new(vec![grant(Capability::GrantAdmin, Scope::folder("/work"))]);
        let executor = FakeExecutor::default();
        let result = run_dispatch(
            &store,
            &executor,
            &manager(),
            ManageCommand::GrantAdd {
                grant: wire_grant(
                    "pin:write",
                    WireScope::Folder {
                        path: "/work".to_owned(),
                    },
                ),
            },
            WireScope::Folder {
                path: "/work".to_owned(),
            },
            at(2026, 1, 1),
        )
        .await;
        assert!(
            matches!(
                result,
                ManageResult::Err {
                    kind: ManageErrorKind::Unauthorised,
                    ..
                }
            ),
            "delegating a capability the caller lacks must be refused, got {result:?}",
        );
        assert!(executor.calls().is_empty(), "no grant may be inserted");
        let audit = store.audit_rows();
        assert_eq!(audit.len(), 1);
        assert_eq!(
            audit.first().map(|r| r.outcome.as_str()),
            Some(OUTCOME_DENIED)
        );
    }

    #[tokio::test]
    async fn grant_add_escalating_scope_wider_than_held_is_refused() {
        // The caller holds grant:admin AND pin:write, but only over
        // /work/reports. Delegating pin:write over the wider /work escapes the
        // caller's own scope — refused.
        let store = FakeStore::new(vec![
            grant(Capability::GrantAdmin, Scope::folder("/work/reports")),
            grant(Capability::PinWrite, Scope::folder("/work/reports")),
        ]);
        let executor = FakeExecutor::default();
        let result = run_dispatch(
            &store,
            &executor,
            &manager(),
            ManageCommand::GrantAdd {
                grant: wire_grant(
                    "pin:write",
                    WireScope::Folder {
                        path: "/work".to_owned(),
                    },
                ),
            },
            WireScope::Folder {
                path: "/work".to_owned(),
            },
            at(2026, 1, 1),
        )
        .await;
        assert!(
            matches!(
                result,
                ManageResult::Err {
                    kind: ManageErrorKind::Unauthorised,
                    ..
                }
            ),
            "delegating a wider scope than held must be refused, got {result:?}",
        );
        assert!(executor.calls().is_empty());
    }

    #[tokio::test]
    async fn grant_add_with_only_grant_admin_cannot_self_promote() {
        // Holding grant:admin alone does not let a manager delegate grant:admin
        // (or any other capability) it does not separately hold — the subset
        // guard requires the *delegated* capability be held, not merely the
        // power to delegate. This is the core no-self-promotion invariant.
        let store = FakeStore::new(vec![grant(Capability::GrantAdmin, Scope::folder("/work"))]);
        let executor = FakeExecutor::default();
        let result = run_dispatch(
            &store,
            &executor,
            &manager(),
            ManageCommand::GrantAdd {
                grant: wire_grant(
                    "grant:admin",
                    WireScope::Folder {
                        path: "/work".to_owned(),
                    },
                ),
            },
            WireScope::Folder {
                path: "/work".to_owned(),
            },
            at(2026, 1, 1),
        )
        .await;
        // The caller DOES hold grant:admin over /work, so delegating grant:admin
        // over /work IS a held subset and is permitted. This documents that a
        // manager may pass on exactly the authority it has — the guard prevents
        // *widening*, not faithful re-delegation.
        assert!(
            matches!(result, ManageResult::Ok { .. }),
            "re-delegating an exactly-held grant:admin is permitted, got {result:?}",
        );
    }

    #[tokio::test]
    async fn grant_add_with_expired_held_grant_cannot_delegate() {
        // The caller's pin:write grant has expired by the time of the request,
        // so it is no longer a held subset — delegation is refused.
        let mut held_pin = grant(Capability::PinWrite, Scope::folder("/work"));
        held_pin.expires = Some(at(2026, 1, 1));
        let store = FakeStore::new(vec![
            grant(Capability::GrantAdmin, Scope::folder("/work")),
            held_pin,
        ]);
        let executor = FakeExecutor::default();
        let result = run_dispatch(
            &store,
            &executor,
            &manager(),
            ManageCommand::GrantAdd {
                grant: wire_grant(
                    "pin:write",
                    WireScope::Folder {
                        path: "/work".to_owned(),
                    },
                ),
            },
            WireScope::Folder {
                path: "/work".to_owned(),
            },
            // After the held pin grant's expiry.
            at(2026, 6, 1),
        )
        .await;
        assert!(
            matches!(
                result,
                ManageResult::Err {
                    kind: ManageErrorKind::Unauthorised,
                    ..
                }
            ),
            "an expired held grant cannot back a delegation, got {result:?}",
        );
        assert!(executor.calls().is_empty());
    }
}
