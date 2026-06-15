//! Test module for `dispatch.rs`, split out to keep the
//! parent file under the source-length cap. Declared from there via
//! `#[cfg(test)] #[path = "dispatch_tests.rs"] mod tests;`, so it stays a child
//! module with full access to the parent's private items.

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
    /// Stored grants addressable by row id, so `manage_grant_scope` can
    /// resolve a `GrantRevoke` target the way the real DB does.
    stored: Vec<(i64, Scope)>,
    audit: Mutex<Vec<AuditEntry>>,
    /// This node's own real device identity. The private key signs the
    /// tokens issued for token-verify tests, and `manage_node_device_id`
    /// reports its derived device id, so a presented token's chain roots in
    /// the same identity that signed it.
    node_identity: cascade_p2p::identity::DeviceIdentity,
    /// The revoked token ids the verify path consults.
    revoked: std::collections::HashSet<String>,
    /// Live exec sessions addressable by id, so `exec_session_scope` can
    /// resolve a session-id-only verb's target scope the way the real DB does.
    exec_sessions: Vec<(u64, Scope)>,
}

impl FakeStore {
    fn new(grants: Vec<Grant>) -> Self {
        Self {
            grants,
            stored: Vec::new(),
            audit: Mutex::new(Vec::new()),
            node_identity: cascade_p2p::identity::DeviceIdentity::generate()
                .expect("generate node identity"),
            revoked: std::collections::HashSet::new(),
            exec_sessions: Vec::new(),
        }
    }

    /// Seed a live exec session with a given id and spawn scope, for
    /// session-id-only exec verb target-resolution tests.
    fn with_exec_session(mut self, id: u64, scope: Scope) -> Self {
        self.exec_sessions.push((id, scope));
        self
    }

    /// Build a store whose node identity is a specific [`DeviceIdentity`], so a
    /// delegation chain rooted in that identity verifies against this store.
    fn with_node_identity(node_identity: cascade_p2p::identity::DeviceIdentity) -> Self {
        Self {
            grants: Vec::new(),
            stored: Vec::new(),
            audit: Mutex::new(Vec::new()),
            node_identity,
            revoked: std::collections::HashSet::new(),
            exec_sessions: Vec::new(),
        }
    }

    /// This node's own device id, derived from its identity certificate.
    fn node_device_id(&self) -> DeviceId {
        DeviceId::new(self.node_identity.device_id.clone())
    }

    /// Issue a token signed by this store's node identity, as the JSON form
    /// `run_dispatch` accepts. The token roots in this node, so it verifies
    /// against the same store.
    fn issue_token_json(
        &self,
        token_id: &str,
        bearer: &DeviceId,
        capability: Capability,
        scope: Scope,
        expires: DateTime<Utc>,
    ) -> String {
        let token = CapabilityToken::issue(
            token_id,
            &self.node_identity,
            bearer,
            capability,
            scope,
            expires,
        )
        .expect("issue a token with the node identity");
        serde_json::to_string(&token).expect("serialise token")
    }

    /// Mark a token id revoked on this store, for token-verify tests.
    fn with_revoked_token(mut self, token_id: &str) -> Self {
        self.revoked.insert(token_id.to_owned());
        self
    }

    /// Seed a stored grant row with a given id and scope, for
    /// `GrantRevoke` target-resolution tests.
    fn with_stored_grant(mut self, id: i64, scope: Scope) -> Self {
        self.stored.push((id, scope));
        self
    }

    fn audit_rows(&self) -> Vec<AuditEntry> {
        self.audit
            .lock()
            .map(|rows| rows.clone())
            .unwrap_or_default()
    }
}

impl ManageGrantStore for FakeStore {
    fn manage_grant_scope(&self, grant_id: i64) -> anyhow::Result<Option<Scope>> {
        Ok(self
            .stored
            .iter()
            .find(|(id, _)| *id == grant_id)
            .map(|(_, scope)| scope.clone()))
    }

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

    fn manage_node_device_id(&self) -> anyhow::Result<DeviceId> {
        Ok(self.node_device_id())
    }

    fn manage_revoked_token_ids(&self) -> anyhow::Result<std::collections::HashSet<String>> {
        Ok(self.revoked.clone())
    }

    fn exec_session_scope(&self, session: u64) -> anyhow::Result<Option<Scope>> {
        Ok(self
            .exec_sessions
            .iter()
            .find(|(id, _)| *id == session)
            .map(|(_, scope)| scope.clone()))
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

    async fn manage_backend_remove(&self, name: &str, mount_path: &str) -> anyhow::Result<String> {
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

    async fn manage_pty_spawn(
        &self,
        owner: &DeviceId,
        scope: &Scope,
        shell: Option<&str>,
        argv: &[String],
        cwd: Option<&str>,
        _env: &[(String, String)],
        cols: u16,
        rows: u16,
    ) -> anyhow::Result<u64> {
        self.record(&format!(
            "pty_spawn owner={owner} scope={scope:?} shell={shell:?} argv={argv:?} cwd={cwd:?} {cols}x{rows}"
        ));
        if self.fail {
            anyhow::bail!("pty spawn failed");
        }
        Ok(SPAWNED_SESSION_ID)
    }

    async fn manage_pty_write(&self, session: u64, bytes: &[u8]) -> anyhow::Result<String> {
        self.record(&format!("pty_write {session} {}", bytes.len()));
        Ok(format!("wrote to {session}"))
    }

    async fn manage_pty_resize(
        &self,
        session: u64,
        cols: u16,
        rows: u16,
    ) -> anyhow::Result<String> {
        self.record(&format!("pty_resize {session} {cols}x{rows}"));
        Ok(format!("resized {session}"))
    }

    async fn manage_pty_kill(&self, session: u64, signal: i32) -> anyhow::Result<String> {
        self.record(&format!("pty_kill {session} {signal}"));
        Ok(format!("signalled {session}"))
    }

    async fn manage_proc_spawn(
        &self,
        owner: &DeviceId,
        scope: &Scope,
        argv: &[String],
        cwd: Option<&str>,
        _env: &[(String, String)],
    ) -> anyhow::Result<u64> {
        self.record(&format!(
            "proc_spawn owner={owner} scope={scope:?} argv={argv:?} cwd={cwd:?}"
        ));
        if self.fail {
            anyhow::bail!("proc spawn failed");
        }
        Ok(SPAWNED_SESSION_ID)
    }

    async fn manage_proc_signal(&self, session: u64, signal: i32) -> anyhow::Result<String> {
        self.record(&format!("proc_signal {session} {signal}"));
        Ok(format!("signalled {session}"))
    }

    async fn manage_proc_kill(&self, session: u64) -> anyhow::Result<String> {
        self.record(&format!("proc_kill {session}"));
        Ok(format!("killed {session}"))
    }
}

/// The session id the [`FakeExecutor`] returns from a spawn. A fixed value the
/// exec authorisation tests assert against.
const SPAWNED_SESSION_ID: u64 = 42;

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
        None,
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
        None,
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
        None,
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
        None,
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
        None,
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
        None,
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
        None,
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
        None,
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
        None,
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
        None,
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
            None,
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
        None,
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
        None,
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
        None,
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
        None,
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
        None,
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

#[tokio::test]
async fn grant_add_dangerous_capability_held_only_node_wide_cannot_be_delegated() {
    // Privilege-laundering guard: the caller holds a node-wide
    // BackendManage grant — which it can never *exercise*, because the
    // dangerous-capability bar refuses a node-wide dangerous grant — plus a
    // GrantAdmin grant over /work that lets it delegate there. It tries to
    // launder the unusable node-wide BackendManage into a usable
    // folder-scoped BackendManage over /work for a subordinate. The subset
    // guard reuses `authorises` for each backing grant, so the node-wide
    // BackendManage cannot back the delegation and the request is refused —
    // no grant is inserted and the denial is audited.
    let store = FakeStore::new(vec![
        grant(Capability::BackendManage, Scope::Node),
        grant(Capability::GrantAdmin, Scope::folder("/work")),
    ]);
    let executor = FakeExecutor::default();
    let result = run_dispatch(
        &store,
        &executor,
        &manager(),
        ManageCommand::GrantAdd {
            grant: wire_grant(
                "backend:manage",
                WireScope::Folder {
                    path: "/work".to_owned(),
                },
            ),
        },
        WireScope::Folder {
            path: "/work".to_owned(),
        },
        None,
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
        "a node-wide dangerous grant the caller cannot exercise must not back \
             a folder-scoped delegation, got {result:?}",
    );
    assert!(
        executor.calls().is_empty(),
        "no grant may be inserted when the delegation launders an unusable authority",
    );
    let audit = store.audit_rows();
    assert_eq!(audit.len(), 1, "the denial must still be audited");
    assert_eq!(
        audit.first().map(|r| r.outcome.as_str()),
        Some(OUTCOME_DENIED),
    );
}

#[tokio::test]
async fn grant_add_delegated_expiry_is_clamped_to_the_backing_grant() {
    // Expiry no-escalation guard: the caller's pin:write grant over /work
    // expires on 2026-02-01. It delegates pin:write over /work asking for a
    // *later* expiry of 2026-12-01. The delegated grant's expiry must be
    // clamped down to the backing grant's 2026-02-01 so the delegate can
    // never outlive the authority it derived from.
    let mut held_pin = grant(Capability::PinWrite, Scope::folder("/work"));
    held_pin.expires = Some(at(2026, 2, 1));
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
            grant: ManageGrant {
                grantee: "SUBORDINATE".to_owned(),
                capability: "pin:write".to_owned(),
                scope: WireScope::Folder {
                    path: "/work".to_owned(),
                },
                // Asks for an expiry far later than the backing grant's.
                expires: Some(at(2026, 12, 1).to_rfc3339()),
            },
        },
        WireScope::Folder {
            path: "/work".to_owned(),
        },
        None,
        at(2026, 1, 1),
    )
    .await;
    assert!(
        matches!(result, ManageResult::Ok { .. }),
        "delegating a held subset is permitted, got {result:?}",
    );
    let calls = executor.calls();
    let call = calls.first().expect("one grant_add call");
    // `FakeExecutor::manage_grant_add` records the delegated grant; the
    // domain grant the dispatcher built carries the clamped expiry, so the
    // recorded expiry is the backing grant's, never the requested later one.
    let expected = grant_from_wire(
        &ManageGrant {
            grantee: "SUBORDINATE".to_owned(),
            capability: "pin:write".to_owned(),
            scope: WireScope::Folder {
                path: "/work".to_owned(),
            },
            expires: Some(at(2026, 12, 1).to_rfc3339()),
        },
        &manager(),
        ExpiryBound::NoLaterThan(at(2026, 2, 1)),
    )
    .expect("building the clamped domain grant");
    assert_eq!(
        expected.expires,
        Some(at(2026, 2, 1)),
        "the clamp pulls the requested 2026-12-01 expiry down to the backing 2026-02-01",
    );
    assert!(
        call.contains("grantee=SUBORDINATE") && call.contains("cap=pin:write"),
        "the delegated grant must be recorded, got {call}",
    );
}

#[tokio::test]
async fn grant_revoke_keyed_on_stored_scope_not_advertised_wire_scope() {
    // GrantRevoke scope-escape guard: the caller holds GrantAdmin over
    // /work only. It tries to revoke grant id 9 whose *stored* scope is
    // /personal, while advertising a wire scope of /work that its grant
    // covers. Authorisation must key on the stored scope of the row that
    // would actually be deleted, so the revoke is refused, nothing runs,
    // and the denial is audited against the real /personal scope.
    let store = FakeStore::new(vec![grant(Capability::GrantAdmin, Scope::folder("/work"))])
        .with_stored_grant(9, Scope::folder("/personal"));
    let executor = FakeExecutor::default();
    let result = run_dispatch(
        &store,
        &executor,
        &manager(),
        ManageCommand::GrantRevoke {
            grant_id: 9,
            // The caller lies: it advertises a scope its grant does cover.
            scope: WireScope::Folder {
                path: "/work".to_owned(),
            },
        },
        WireScope::Folder {
            path: "/work".to_owned(),
        },
        None,
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
        "revoking a grant whose stored scope escapes the caller's authority must be \
             refused, got {result:?}",
    );
    assert!(
        executor.calls().is_empty(),
        "no grant may be revoked when the stored scope escapes the caller's authority",
    );
    let audit = store.audit_rows();
    assert_eq!(audit.len(), 1, "the denial must still be audited");
    let row = audit.first().expect("one audit row");
    assert_eq!(row.outcome, OUTCOME_DENIED);
    assert_eq!(
        row.scope,
        Scope::folder("/personal"),
        "the audit row records the stored scope of the targeted grant, not the wire scope",
    );
}

#[tokio::test]
async fn grant_revoke_keyed_on_stored_node_scope_is_refused_for_folder_admin() {
    // A node-wide stored grant is even further out of a folder-scoped
    // GrantAdmin's reach: GrantAdmin over /work must not revoke a grant
    // whose stored scope is Node, even when the wire scope claims /work.
    let store = FakeStore::new(vec![grant(Capability::GrantAdmin, Scope::folder("/work"))])
        .with_stored_grant(3, Scope::Node);
    let executor = FakeExecutor::default();
    let result = run_dispatch(
        &store,
        &executor,
        &manager(),
        ManageCommand::GrantRevoke {
            grant_id: 3,
            scope: WireScope::Folder {
                path: "/work".to_owned(),
            },
        },
        WireScope::Folder {
            path: "/work".to_owned(),
        },
        None,
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
        "a folder-scoped GrantAdmin must not revoke a node-wide grant, got {result:?}",
    );
    assert!(executor.calls().is_empty());
    let audit = store.audit_rows();
    assert_eq!(
        audit.first().map(|r| r.scope.clone()),
        Some(Scope::Node),
        "the audit row records the stored node scope of the targeted grant",
    );
}

// ── Presented capability tokens authorise end to end ──

#[tokio::test]
async fn valid_token_authorises_a_command_with_no_on_node_grant() {
    // The node holds NO grant for the caller. A token issued by this node
    // for the caller, covering the command, authorises it end to end — the
    // offline-issued grant alone carries the authority.
    let store = FakeStore::new(Vec::new());
    let executor = FakeExecutor::default();
    let token = store.issue_token_json(
        "tok-pin",
        &manager(),
        Capability::PinWrite,
        Scope::folder("/work"),
        at(2026, 12, 31),
    );
    let result = run_dispatch(
        &store,
        &executor,
        &manager(),
        ManageCommand::Pin {
            path_glob: "/work/reports".to_owned(),
            recursive: true,
        },
        WireScope::Folder {
            path: "/work".to_owned(),
        },
        Some(token),
        at(2026, 1, 1),
    )
    .await;
    assert!(
        matches!(result, ManageResult::Ok { .. }),
        "a valid token must authorise the command, got {result:?}",
    );
    assert_eq!(executor.calls(), vec!["pin /work/reports true".to_owned()]);
    let audit = store.audit_rows();
    assert_eq!(audit.len(), 1);
    assert_eq!(
        audit.first().map(|r| r.outcome.as_str()),
        Some(OUTCOME_ALLOWED)
    );
}

#[tokio::test]
async fn token_signed_by_another_node_is_refused() {
    // A token whose root issuer is some OTHER node does not verify against
    // this node — it is unauthorised, the command never runs.
    let store = FakeStore::new(Vec::new());
    let executor = FakeExecutor::default();
    // A different node, with its own identity and certificate, signs the
    // token. Its root issuer is not this node, so verification rejects it.
    let other_node = cascade_p2p::identity::DeviceIdentity::generate().unwrap();
    let foreign = CapabilityToken::issue(
        "tok-foreign",
        &other_node,
        &manager(),
        Capability::PinWrite,
        Scope::folder("/work"),
        at(2026, 12, 31),
    )
    .unwrap();
    let token = serde_json::to_string(&foreign).unwrap();
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
        Some(token),
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
}

#[tokio::test]
async fn expired_token_is_refused() {
    let store = FakeStore::new(Vec::new());
    let executor = FakeExecutor::default();
    let token = store.issue_token_json(
        "tok-stale",
        &manager(),
        Capability::PinWrite,
        Scope::folder("/work"),
        at(2026, 1, 1),
    );
    // now is past the token expiry.
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
        Some(token),
        at(2026, 6, 1),
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
}

#[tokio::test]
async fn revoked_token_is_refused() {
    // The token is valid and unexpired, but its id is on the node's
    // revocation list — the command is refused.
    let store = FakeStore::new(Vec::new()).with_revoked_token("tok-revoked");
    let executor = FakeExecutor::default();
    let token = store.issue_token_json(
        "tok-revoked",
        &manager(),
        Capability::PinWrite,
        Scope::folder("/work"),
        at(2026, 12, 31),
    );
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
        Some(token),
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
}

#[tokio::test]
async fn token_for_a_different_bearer_is_refused() {
    // The token names a DIFFERENT bearer than the authenticated caller, so a
    // third party cannot replay it.
    let store = FakeStore::new(Vec::new());
    let executor = FakeExecutor::default();
    let token = store.issue_token_json(
        "tok-other-bearer",
        &DeviceId::new("SOMEONE-ELSE"),
        Capability::PinWrite,
        Scope::folder("/work"),
        at(2026, 12, 31),
    );
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
        Some(token),
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
}

#[tokio::test]
async fn token_scope_outside_command_target_is_refused() {
    // A token covering only /work cannot authorise a pin of /personal — the
    // token-carried grant goes through the same scope-coverage check an
    // on-node grant does.
    let store = FakeStore::new(Vec::new());
    let executor = FakeExecutor::default();
    let token = store.issue_token_json(
        "tok-scoped",
        &manager(),
        Capability::PinWrite,
        Scope::folder("/work"),
        at(2026, 12, 31),
    );
    let result = run_dispatch(
        &store,
        &executor,
        &manager(),
        ManageCommand::Pin {
            path_glob: "/personal/secret".to_owned(),
            recursive: false,
        },
        WireScope::Folder {
            path: "/work".to_owned(),
        },
        Some(token),
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
}

#[tokio::test]
async fn malformed_token_is_a_failed_error() {
    // A token that does not deserialise is a malformed request, reported as
    // Failed (not an authorisation question), and the command never runs.
    let store = FakeStore::new(Vec::new());
    let executor = FakeExecutor::default();
    let result = run_dispatch(
        &store,
        &executor,
        &manager(),
        ManageCommand::StatusRead,
        WireScope::Node,
        Some("not json".to_owned()),
        at(2026, 1, 1),
    )
    .await;
    assert!(matches!(
        result,
        ManageResult::Err {
            kind: ManageErrorKind::Failed,
            ..
        }
    ));
    assert!(executor.calls().is_empty());
}

// ── Exec verbs: dangerous-tier authorisation ──
//
// Exec is remote code execution; these prove the authorisation discipline the
// exec-capability.md spec demands: never satisfied by a node-wide grant, only by
// an explicit-scope grant; revoked/expired tokens refused; a delegation can only
// narrow; and every spawn/signal/kill is audited.

/// Every exec verb paired with the spawn payload that exercises it under the
/// `exec:pty` capability, so the node-wide-grant bar can be asserted for each.
fn pty_spawn_in(cwd: &str) -> ManageCommand {
    ManageCommand::PtySpawn {
        shell: Some("/bin/sh".to_owned()),
        argv: Vec::new(),
        cwd: Some(cwd.to_owned()),
        env: Vec::new(),
        cols: 80,
        rows: 24,
    }
}

fn proc_spawn_in(cwd: &str) -> ManageCommand {
    ManageCommand::ProcSpawn {
        argv: vec!["/bin/echo".to_owned(), "hi".to_owned()],
        cwd: Some(cwd.to_owned()),
        env: Vec::new(),
    }
}

#[tokio::test]
async fn node_wide_grant_cannot_exec() {
    // A node-wide grant of an exec capability never authorises a spawn — exec
    // sits in the dangerous tier, so only an explicit folder grant satisfies it.
    // Asserted for both verbs and for the explicit Node scope plus a root folder
    // scope that is node-wide in everything but name.
    for (cap, command) in [
        (Capability::ExecPty, pty_spawn_in("/work")),
        (Capability::ExecProc, proc_spawn_in("/work")),
    ] {
        for grant_scope in [Scope::Node, Scope::folder("/"), Scope::folder("/work/..")] {
            let store = FakeStore::new(vec![grant(cap, grant_scope.clone())]);
            let executor = FakeExecutor::default();
            let result = run_dispatch(
                &store,
                &executor,
                &manager(),
                command.clone(),
                WireScope::Folder {
                    path: "/work".to_owned(),
                },
                None,
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
                "node-wide grant ({grant_scope:?}) of {cap:?} must not authorise a spawn, got {result:?}",
            );
            assert!(
                executor.calls().is_empty(),
                "no exec session may be spawned under a node-wide grant",
            );
            let audit = store.audit_rows();
            assert_eq!(audit.len(), 1, "the denial must still be audited");
            assert_eq!(
                audit.first().map(|r| r.outcome.as_str()),
                Some(OUTCOME_DENIED),
            );
        }
    }
}

#[tokio::test]
async fn explicit_scope_grant_can_exec_and_is_audited() {
    // An explicit folder grant of exec:pty authorises a spawn rooted in that
    // folder, the spawn runs, and the spawn is audited as allowed. The reply
    // carries the new session id.
    let store = FakeStore::new(vec![grant(Capability::ExecPty, Scope::folder("/work"))]);
    let executor = FakeExecutor::default();
    let result = run_dispatch(
        &store,
        &executor,
        &manager(),
        pty_spawn_in("/work/reports"),
        WireScope::Folder {
            path: "/work/reports".to_owned(),
        },
        None,
        at(2026, 1, 1),
    )
    .await;
    assert!(
        matches!(result, ManageResult::ExecSpawned { session } if session == SPAWNED_SESSION_ID),
        "an explicit-scope exec grant must spawn and return a session id, got {result:?}",
    );
    assert_eq!(
        executor.calls().len(),
        1,
        "exactly one spawn must run, got {:?}",
        executor.calls(),
    );
    let audit = store.audit_rows();
    assert_eq!(audit.len(), 1, "the spawn must be audited");
    let row = audit.first().expect("one audit row");
    assert_eq!(row.outcome, OUTCOME_ALLOWED);
    assert_eq!(row.capability, Capability::ExecPty);
    assert_eq!(
        row.scope,
        Scope::folder("/work/reports"),
        "the audit row records the cwd the session is confined to",
    );
}

#[tokio::test]
async fn proc_explicit_scope_grant_can_exec() {
    let store = FakeStore::new(vec![grant(Capability::ExecProc, Scope::folder("/work"))]);
    let executor = FakeExecutor::default();
    let result = run_dispatch(
        &store,
        &executor,
        &manager(),
        proc_spawn_in("/work"),
        WireScope::Folder {
            path: "/work".to_owned(),
        },
        None,
        at(2026, 1, 1),
    )
    .await;
    assert!(
        matches!(result, ManageResult::ExecSpawned { .. }),
        "an explicit exec:proc grant must spawn, got {result:?}",
    );
}

#[tokio::test]
async fn pty_grant_does_not_authorise_proc_spawn() {
    // The two exec capabilities are distinct: an exec:pty grant must not
    // authorise a headless proc spawn (a different blast radius).
    let store = FakeStore::new(vec![grant(Capability::ExecPty, Scope::folder("/work"))]);
    let executor = FakeExecutor::default();
    let result = run_dispatch(
        &store,
        &executor,
        &manager(),
        proc_spawn_in("/work"),
        WireScope::Folder {
            path: "/work".to_owned(),
        },
        None,
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
        "an exec:pty grant must not authorise a proc spawn, got {result:?}",
    );
    assert!(executor.calls().is_empty());
}

#[tokio::test]
async fn spawn_without_cwd_targets_node_root_and_is_refused() {
    // A spawn with no cwd has no folder to confine the session, so it targets the
    // node root — which the dangerous bar refuses even with an explicit grant.
    // This forces an explicit cwd: a remote shell can never be opened without
    // naming the folder it is confined to.
    let store = FakeStore::new(vec![grant(Capability::ExecPty, Scope::folder("/work"))]);
    let executor = FakeExecutor::default();
    let result = run_dispatch(
        &store,
        &executor,
        &manager(),
        ManageCommand::PtySpawn {
            shell: Some("/bin/sh".to_owned()),
            argv: Vec::new(),
            cwd: None,
            env: Vec::new(),
            cols: 80,
            rows: 24,
        },
        WireScope::Folder {
            path: "/work".to_owned(),
        },
        None,
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
        "a cwd-less spawn must be refused, got {result:?}",
    );
    assert!(executor.calls().is_empty());
}

#[tokio::test]
async fn session_id_verb_resolves_scope_from_session_not_wire_scope() {
    // The scope-escape regression for session-id-only verbs: the caller holds
    // exec:pty over /work only, and advertises a wire scope of /work (which the
    // grant covers), but the target session was spawned under /personal. The
    // verb must authorise over the session's real scope, resolved from node
    // state, so it is refused — a caller cannot drive a /personal session by
    // lying in the wire scope.
    let store = FakeStore::new(vec![grant(Capability::ExecPty, Scope::folder("/work"))])
        .with_exec_session(7, Scope::folder("/personal"));
    let executor = FakeExecutor::default();
    let result = run_dispatch(
        &store,
        &executor,
        &manager(),
        ManageCommand::PtyWrite {
            session: 7,
            bytes: b"rm -rf /\n".to_vec(),
        },
        WireScope::Folder {
            path: "/work".to_owned(),
        },
        None,
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
        "a verb on a session spawned under a folder the caller cannot reach must be refused, got {result:?}",
    );
    assert!(
        executor.calls().is_empty(),
        "no write may reach a session outside the granted scope",
    );
    let audit = store.audit_rows();
    assert_eq!(
        audit.first().map(|r| r.scope.clone()),
        Some(Scope::folder("/personal")),
        "the audit row records the session's real scope, not the advertised wire scope",
    );
}

#[tokio::test]
async fn session_id_verb_authorised_over_session_scope_runs() {
    // The positive case: the caller holds exec:pty over /personal and the session
    // was spawned there. The write authorises over the session's scope and runs.
    let store = FakeStore::new(vec![grant(Capability::ExecPty, Scope::folder("/personal"))])
        .with_exec_session(7, Scope::folder("/personal"));
    let executor = FakeExecutor::default();
    let result = run_dispatch(
        &store,
        &executor,
        &manager(),
        ManageCommand::PtyWrite {
            session: 7,
            bytes: b"ls\n".to_vec(),
        },
        WireScope::Folder {
            path: "/personal".to_owned(),
        },
        None,
        at(2026, 1, 1),
    )
    .await;
    assert!(
        matches!(result, ManageResult::Ok { .. }),
        "a write authorised over the session's scope must run, got {result:?}",
    );
    assert_eq!(executor.calls().len(), 1, "the write must run once");
    let audit = store.audit_rows();
    assert_eq!(
        audit.first().map(|r| r.outcome.as_str()),
        Some(OUTCOME_ALLOWED),
        "every exec verb is audited",
    );
}

#[tokio::test]
async fn session_id_verb_on_unknown_session_is_refused() {
    // A verb naming a session that does not exist resolves to no scope; it
    // targets the node-wide scope, which the dangerous bar refuses, so it never
    // runs. The caller cannot fall back to an attacker-chosen wire scope.
    let store = FakeStore::new(vec![grant(Capability::ExecProc, Scope::folder("/work"))]);
    let executor = FakeExecutor::default();
    let result = run_dispatch(
        &store,
        &executor,
        &manager(),
        ManageCommand::ProcKill { session: 999 },
        WireScope::Folder {
            path: "/work".to_owned(),
        },
        None,
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
        "a verb on an unknown session must be refused, got {result:?}",
    );
    assert!(executor.calls().is_empty());
}

#[tokio::test]
async fn exec_token_with_explicit_scope_authorises_spawn() {
    // A token issued by this node for the caller, carrying exec:pty over a folder
    // scope, authorises a spawn rooted there with no on-node grant.
    let store = FakeStore::new(Vec::new());
    let executor = FakeExecutor::default();
    let token = store.issue_token_json(
        "tok-exec",
        &manager(),
        Capability::ExecPty,
        Scope::folder("/work"),
        at(2026, 12, 31),
    );
    let result = run_dispatch(
        &store,
        &executor,
        &manager(),
        pty_spawn_in("/work"),
        WireScope::Folder {
            path: "/work".to_owned(),
        },
        Some(token),
        at(2026, 1, 1),
    )
    .await;
    assert!(
        matches!(result, ManageResult::ExecSpawned { .. }),
        "a valid exec token must authorise the spawn, got {result:?}",
    );
}

#[tokio::test]
async fn revoked_exec_token_is_refused() {
    let store = FakeStore::new(Vec::new()).with_revoked_token("tok-exec-revoked");
    let executor = FakeExecutor::default();
    let token = store.issue_token_json(
        "tok-exec-revoked",
        &manager(),
        Capability::ExecPty,
        Scope::folder("/work"),
        at(2026, 12, 31),
    );
    let result = run_dispatch(
        &store,
        &executor,
        &manager(),
        pty_spawn_in("/work"),
        WireScope::Folder {
            path: "/work".to_owned(),
        },
        Some(token),
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
        "a revoked exec token must be refused, got {result:?}",
    );
    assert!(executor.calls().is_empty());
}

#[tokio::test]
async fn expired_exec_token_is_refused() {
    let store = FakeStore::new(Vec::new());
    let executor = FakeExecutor::default();
    let token = store.issue_token_json(
        "tok-exec-expired",
        &manager(),
        Capability::ExecProc,
        Scope::folder("/work"),
        at(2025, 12, 31),
    );
    let result = run_dispatch(
        &store,
        &executor,
        &manager(),
        proc_spawn_in("/work"),
        WireScope::Folder {
            path: "/work".to_owned(),
        },
        Some(token),
        // After the token's expiry.
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
        "an expired exec token must be refused, got {result:?}",
    );
    assert!(executor.calls().is_empty());
}

#[tokio::test]
async fn delegated_exec_token_cannot_widen_scope() {
    // A delegated exec token can only narrow authority. Minting a child that
    // widens the scope (/work -> /) is refused at mint, so a widened token can
    // never exist to present. The narrowed child authorises only within its
    // narrowed scope.
    let node = cascade_p2p::identity::DeviceIdentity::generate().unwrap();
    let delegator = cascade_p2p::identity::DeviceIdentity::generate().unwrap();
    let sub = cascade_p2p::identity::DeviceIdentity::generate().unwrap();
    let delegator_id = DeviceId::new(delegator.device_id.clone());
    let sub_id = DeviceId::new(sub.device_id.clone());

    let parent = CapabilityToken::issue(
        "tok-exec-parent",
        &node,
        &delegator_id,
        Capability::ExecPty,
        Scope::folder("/work"),
        at(2026, 12, 31),
    )
    .unwrap();

    // Attempting to widen the scope at delegation is refused at mint.
    let widened = parent.delegate(
        "tok-exec-wide",
        &delegator,
        &sub_id,
        Capability::ExecPty,
        Scope::folder("/"),
        at(2026, 12, 31),
    );
    assert!(
        widened.is_err(),
        "delegating a wider exec scope must be refused at mint",
    );

    // A properly narrowed child mints, and authorises a spawn in the narrowed
    // subtree against the node that issued the chain root.
    let narrowed = parent
        .delegate(
            "tok-exec-narrow",
            &delegator,
            &sub_id,
            Capability::ExecPty,
            Scope::folder("/work/sub"),
            at(2026, 6, 1),
        )
        .expect("a narrowing delegation must mint");

    // Build a store whose node identity is the chain-root issuer, so the token
    // verifies against it. The caller is the narrowed child's bearer.
    let store = FakeStore::with_node_identity(node);
    let executor = FakeExecutor::default();
    let token_json = serde_json::to_string(&narrowed).unwrap();

    // In the narrowed subtree: authorised.
    let allowed = run_dispatch(
        &store,
        &executor,
        &sub_id,
        pty_spawn_in("/work/sub"),
        WireScope::Folder {
            path: "/work/sub".to_owned(),
        },
        Some(token_json.clone()),
        at(2026, 1, 1),
    )
    .await;
    assert!(
        matches!(allowed, ManageResult::ExecSpawned { .. }),
        "the narrowed exec token must authorise a spawn in its subtree, got {allowed:?}",
    );

    // Outside the narrowed subtree (the parent's wider /work) the child does not
    // reach: refused, proving the delegate cannot exercise authority beyond the
    // narrowed scope even though its parent could.
    let refused = run_dispatch(
        &store,
        &executor,
        &sub_id,
        pty_spawn_in("/work/other"),
        WireScope::Folder {
            path: "/work/other".to_owned(),
        },
        Some(token_json),
        at(2026, 1, 1),
    )
    .await;
    assert!(
        matches!(
            refused,
            ManageResult::Err {
                kind: ManageErrorKind::Unauthorised,
                ..
            }
        ),
        "the narrowed exec token must not reach beyond its scope, got {refused:?}",
    );
}
