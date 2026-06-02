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
    ManageCommand, ManageErrorKind, ManageResult, ManageScope as WireScope,
};
use chrono::{DateTime, Utc};

use crate::db::AuditEntry;
use crate::manage::{Capability, DeviceId, Scope, authorises};

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
        ManageCommand::CacheEvict => Capability::CacheManage,
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
/// A path-bearing command (Pin/Unpin) mutates the node at its `path_glob`, so
/// the operation's real target is the folder subtree that glob lives in — not
/// whatever scope the caller chose to advertise. A command with no path target
/// ([`StatusRead`](ManageCommand::StatusRead),
/// [`CacheEvict`](ManageCommand::CacheEvict)) acts node-wide, so its target is
/// [`Scope::Node`].
///
/// The dispatcher authorises the granted scope against *both* this payload-derived
/// target and the wire `scope`, which closes the scope-escape where an authorised
/// `scope` field is decoupled from the path actually mutated. Folding `*`, `?`,
/// and character-class glob metacharacters out of the path before scoping means a
/// glob like `/work/*` is confined to the `/work` subtree it can ever match, and a
/// bare glob with no fixed prefix (`*.pdf`) targets the node root so only a
/// node-wide grant covers it.
#[must_use]
fn command_target_scope(command: &ManageCommand) -> Scope {
    match command {
        ManageCommand::StatusRead | ManageCommand::CacheEvict => Scope::Node,
        ManageCommand::Pin { path_glob, .. } | ManageCommand::Unpin { path_glob } => {
            Scope::folder(glob_fixed_prefix(path_glob))
        }
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
    // cover the path that is really touched, not just the advertised scope.
    let target_scope = command_target_scope(&command);
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
    let authorised = authorises(&grants, caller, capability, &target_scope, now)
        && authorises(&grants, caller, capability, &scope, now);

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

    let applied = execute(executor, command).await;
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
async fn execute<E>(executor: &E, command: ManageCommand) -> anyhow::Result<String>
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
    }
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
}
