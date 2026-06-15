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
use crate::manage::token::{CapabilityToken, MAX_TOKEN_JSON_BYTES};
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

    /// Restart the daemon's cache-manager worker. Backs the dangerous
    /// [`Capability::LifecycleControl`]. The sync runner is owned by the daemon
    /// and is not revived in-process; reviving the full worker set requires a
    /// daemon-level process restart. The returned summary states this.
    async fn manage_restart(&self) -> anyhow::Result<String>;

    /// Stop the daemon's background workers — both the cache-manager task and
    /// the backend sync runner. Backs the dangerous
    /// [`Capability::LifecycleControl`].
    async fn manage_stop(&self) -> anyhow::Result<String>;

    /// Add a delegated grant. The grant has already been validated as a subset
    /// of the caller's authority and stamped with the caller as `granted_by`
    /// before this is called. Backs the dangerous [`Capability::GrantAdmin`].
    async fn manage_grant_add(&self, grant: &Grant) -> anyhow::Result<String>;

    /// Revoke a grant by its row id. Backs the dangerous
    /// [`Capability::GrantAdmin`].
    async fn manage_grant_revoke(&self, grant_id: i64) -> anyhow::Result<String>;

    /// Spawn an interactive PTY session, returning its new session id. Backs the
    /// dangerous [`Capability::ExecPty`]. `owner` is the authenticated caller
    /// recorded as the session owner; `scope` is the folder the session is
    /// confined to, both used to record the durable session row the dispatcher
    /// later resolves session-id-only verbs against.
    ///
    /// The exec methods are present only with the `exec` feature; a build without
    /// it carries no exec provider and the wire verbs decode but never reach an
    /// executor (see `run_dispatch`).
    #[cfg(feature = "exec")]
    async fn manage_pty_spawn(
        &self,
        owner: &DeviceId,
        scope: &Scope,
        shell: Option<&str>,
        argv: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
        cols: u16,
        rows: u16,
    ) -> anyhow::Result<u64>;

    /// Write bytes to a PTY session's stdin. Backs [`Capability::ExecPty`].
    #[cfg(feature = "exec")]
    async fn manage_pty_write(&self, session: u64, bytes: &[u8]) -> anyhow::Result<String>;

    /// Resize a PTY session. Backs [`Capability::ExecPty`].
    #[cfg(feature = "exec")]
    async fn manage_pty_resize(&self, session: u64, cols: u16, rows: u16)
    -> anyhow::Result<String>;

    /// Send a signal to a PTY session's child process. Backs
    /// [`Capability::ExecPty`].
    #[cfg(feature = "exec")]
    async fn manage_pty_kill(&self, session: u64, signal: i32) -> anyhow::Result<String>;

    /// Spawn a headless process session, returning its new session id. Backs the
    /// dangerous [`Capability::ExecProc`]. `owner` and `scope` are recorded the
    /// same way as for [`manage_pty_spawn`](Self::manage_pty_spawn).
    #[cfg(feature = "exec")]
    async fn manage_proc_spawn(
        &self,
        owner: &DeviceId,
        scope: &Scope,
        argv: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
    ) -> anyhow::Result<u64>;

    /// Send a signal to a headless process session. Backs
    /// [`Capability::ExecProc`].
    #[cfg(feature = "exec")]
    async fn manage_proc_signal(&self, session: u64, signal: i32) -> anyhow::Result<String>;

    /// Kill a headless process session. Backs [`Capability::ExecProc`].
    #[cfg(feature = "exec")]
    async fn manage_proc_kill(&self, session: u64) -> anyhow::Result<String>;
}

/// The grant store and audit sink the dispatcher reads and writes.
///
/// Implemented by the engine over its [`StateDb`](crate::db::StateDb). Kept as a
/// contract so the dispatch flow can be exercised against an in-memory double in
/// tests without standing up a real database.
pub trait ManageGrantStore: Send + Sync {
    /// Every grant currently held on this node.
    fn manage_grants(&self) -> anyhow::Result<Vec<crate::manage::Grant>>;

    /// The stored [`Scope`] of the grant with row id `grant_id`, or `None` when
    /// no such grant exists.
    ///
    /// `GrantRevoke` authorisation derives its target scope from the row that
    /// will actually be mutated, never the caller-advertised wire scope, so the
    /// dispatcher needs to resolve the real scope before authorising. This is
    /// kept separate from [`manage_grants`](Self::manage_grants) because the
    /// latter returns grants without their row ids.
    fn manage_grant_scope(&self, grant_id: i64) -> anyhow::Result<Option<Scope>>;

    /// Append an audit row. The audit log is append-only.
    fn manage_append_audit(&self, entry: &AuditEntry) -> anyhow::Result<()>;

    /// This node's own device id — the identity a presented capability token's
    /// delegation chain must root in to authorise against this node.
    fn manage_node_device_id(&self) -> anyhow::Result<DeviceId>;

    /// The set of revoked token ids on this node, for building the revocation
    /// predicate the token verify path consults. Returning the whole set lets a
    /// chain be checked without a database round-trip per token.
    fn manage_revoked_token_ids(&self) -> anyhow::Result<std::collections::HashSet<String>>;

    /// The [`Scope`] a live exec session was spawned under, or `None` when no
    /// live session with that id exists.
    ///
    /// A session-id-only exec verb (pty.write/resize/kill, proc.signal/kill)
    /// derives its authorisation target from the scope the session actually runs
    /// in, never the caller-advertised wire scope. The dispatcher resolves the
    /// real scope from here before authorising, exactly as `GrantRevoke`
    /// resolves the stored grant's scope via [`manage_grant_scope`]. This closes
    /// the scope-escape class where a caller holding `exec:pty` over `/work`
    /// drives a session spawned under `/personal` by lying in the wire scope.
    ///
    /// A node built without the exec capability has no exec sessions; the
    /// default implementation returns `None`, so a session-id-only verb finds no
    /// session and is refused.
    fn exec_session_scope(&self, _session: u64) -> anyhow::Result<Option<Scope>> {
        Ok(None)
    }
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
    /// `token` is an optional signed capability token (in its JSON form)
    /// presented to authorise the command; when present and valid, the
    /// token-carried grant authorises the command in addition to any on-node
    /// grant the caller holds. `now` is the wall-clock instant used for grant-
    /// and token-expiry checks; the BEP call site passes `Utc::now()`.
    async fn dispatch(
        &self,
        caller: &DeviceId,
        command: ManageCommand,
        scope: WireScope,
        token: Option<String>,
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
        ManageCommand::PtySpawn { .. }
        | ManageCommand::PtyWrite { .. }
        | ManageCommand::PtyResize { .. }
        | ManageCommand::PtyKill { .. } => Capability::ExecPty,
        ManageCommand::ProcSpawn { .. }
        | ManageCommand::ProcSignal { .. }
        | ManageCommand::ProcKill { .. } => Capability::ExecProc,
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
        // An exec spawn's real target is the working directory it runs in: a
        // caller holding `exec:pty`/`exec:proc` over `/work` may only spawn a
        // session rooted there. An absent `cwd` has no folder to confine the
        // session, so it targets the node root `/` — which is node-wide, and the
        // dangerous-capability bar therefore refuses it. This forces an explicit
        // cwd: a remote shell can never be opened without naming the folder it is
        // confined to.
        ManageCommand::PtySpawn { cwd, .. } | ManageCommand::ProcSpawn { cwd, .. } => Some(
            cwd.as_deref()
                .map_or_else(|| Scope::folder("/"), Scope::folder),
        ),
        // These derive no target from their payload and so return `None`:
        //
        // - `GrantRevoke`'s real target is the scope of the *stored* grant being
        //   revoked, resolved from the store in `run_dispatch` (the payload only
        //   carries a row id and a caller-advertised scope that must not be
        //   trusted), so it is never routed through this function.
        // - `Restart`/`Stop` carry no payload path; only the advertised wire
        //   scope confines them, and the dangerous-capability bar requires that
        //   to be an explicit folder anyway.
        // - The session-id-only exec verbs (write/resize/kill/signal) target the
        //   scope the *session* was spawned under, resolved from `exec_sessions`
        //   in `run_dispatch` — never the caller-advertised wire scope. The
        //   payload carries only a session id, so they are routed specially, the
        //   same way `GrantRevoke` resolves the stored grant's scope.
        ManageCommand::GrantRevoke { .. }
        | ManageCommand::Restart
        | ManageCommand::Stop
        | ManageCommand::PtyWrite { .. }
        | ManageCommand::PtyResize { .. }
        | ManageCommand::PtyKill { .. }
        | ManageCommand::ProcSignal { .. }
        | ManageCommand::ProcKill { .. } => None,
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
        ManageCommand::PtySpawn { shell, cwd, .. } => {
            let shell = shell.as_deref().unwrap_or("<default shell>");
            let cwd = cwd.as_deref().unwrap_or("<default cwd>");
            format!("pty spawn {shell} in {cwd}")
        }
        ManageCommand::PtyWrite { session, bytes } => {
            format!("pty write {} bytes to {session}", bytes.len())
        }
        ManageCommand::PtyResize {
            session,
            cols,
            rows,
        } => format!("pty resize {session} to {cols}x{rows}"),
        ManageCommand::PtyKill { session, signal } => format!("pty signal {signal} to {session}"),
        ManageCommand::ProcSpawn { argv, cwd, .. } => {
            let program = argv.first().map_or("<empty argv>", String::as_str);
            let cwd = cwd.as_deref().unwrap_or("<default cwd>");
            format!("proc spawn {program} in {cwd}")
        }
        ManageCommand::ProcSignal { session, signal } => {
            format!("proc signal {signal} to {session}")
        }
        ManageCommand::ProcKill { session } => format!("proc kill {session}"),
    }
}

/// Verify a presented capability token against this node and project it to the
/// [`Grant`] it confers, or return the typed [`ManageResult::Err`] that should
/// be reported when the token is unusable.
///
/// Every failure is a hard rejection reported to the caller, never a silent
/// fall-through: a token that does not deserialise is a `Failed` error (it is a
/// malformed request, not an authorisation question), while a token that
/// deserialises but does not verify — wrong issuer, expired, revoked, bearer
/// mismatch, or an over-reaching delegation — is an `Unauthorised` error, the
/// same code an insufficient on-node grant earns. The returned grant carries the
/// token's `(bearer, capability, scope, expiry)` and is authorised by the same
/// [`authorises`] path an on-node grant takes.
fn verify_presented_token<S>(
    store: &S,
    caller: &DeviceId,
    token_json: &str,
    now: DateTime<Utc>,
) -> Result<Grant, ManageResult>
where
    S: ManageGrantStore + ?Sized,
{
    if token_json.len() > MAX_TOKEN_JSON_BYTES {
        return Err(ManageResult::Err {
            kind: ManageErrorKind::Failed,
            message: format!(
                "presented capability token exceeds maximum length ({} > {MAX_TOKEN_JSON_BYTES} bytes)",
                token_json.len(),
            ),
        });
    }

    let token: CapabilityToken =
        serde_json::from_str(token_json).map_err(|e| ManageResult::Err {
            kind: ManageErrorKind::Failed,
            message: format!("could not parse presented capability token: {e}"),
        })?;

    let node_device_id = store
        .manage_node_device_id()
        .map_err(|e| ManageResult::Err {
            kind: ManageErrorKind::Failed,
            message: format!("could not resolve this node's device identity: {e}"),
        })?;

    let revoked = store
        .manage_revoked_token_ids()
        .map_err(|e| ManageResult::Err {
            kind: ManageErrorKind::Failed,
            message: format!("could not read token revocation list: {e}"),
        })?;

    let is_revoked = |id: &str| revoked.contains(id);
    match token.verify(&node_device_id, caller, now, &is_revoked) {
        Ok(claims) => Ok(claims.to_grant()),
        Err(e) => Err(ManageResult::Err {
            kind: ManageErrorKind::Unauthorised,
            message: format!("presented capability token rejected: {e}"),
        }),
    }
}

/// Verify a presented capability token for the **data plane** and project it to
/// the data-verb [`Grant`] it confers, if any.
///
/// Unlike the management-plane token verification, this never returns an error:
/// the BEP data path is default-open, so a token that does not parse, does not
/// verify, or carries a non-data verb simply confers nothing and the decision
/// falls back to the on-node data grants. The reason a token is unusable is
/// logged (so an
/// operator can diagnose a peer presenting a stale or wrong token), but it can
/// never *widen* access, only narrow-or-grant a direction the bearer was issued.
///
/// A token carrying a data verb that verifies cleanly yields its
/// [`to_grant`](crate::manage::TokenClaims::to_grant) projection, ready to be
/// folded into the grant slice passed to [`data_access`](crate::manage::data_access).
#[must_use]
pub fn verify_data_token<S>(
    store: &S,
    peer: &DeviceId,
    token_json: &str,
    now: DateTime<Utc>,
) -> Option<Grant>
where
    S: ManageGrantStore + ?Sized,
{
    if token_json.len() > MAX_TOKEN_JSON_BYTES {
        tracing::debug!(
            target: "cascade::manage::data",
            len = token_json.len(),
            max = MAX_TOKEN_JSON_BYTES,
            "ignoring oversized data token presented on sync frame",
        );
        return None;
    }

    let token: CapabilityToken = match serde_json::from_str(token_json) {
        Ok(token) => token,
        Err(e) => {
            tracing::debug!(
                target: "cascade::manage::data",
                error = %e,
                "ignoring unparseable data token presented on sync frame",
            );
            return None;
        }
    };

    let node_device_id = match store.manage_node_device_id() {
        Ok(id) => id,
        Err(e) => {
            tracing::debug!(
                target: "cascade::manage::data",
                error = %e,
                "cannot resolve node device id to verify data token — ignoring token",
            );
            return None;
        }
    };

    let revoked = match store.manage_revoked_token_ids() {
        Ok(set) => set,
        Err(e) => {
            tracing::debug!(
                target: "cascade::manage::data",
                error = %e,
                "cannot read token revocation list to verify data token — ignoring token",
            );
            return None;
        }
    };

    let is_revoked = |id: &str| revoked.contains(id);
    match token.verify(&node_device_id, peer, now, &is_revoked) {
        Ok(claims) if claims.capability.is_data_verb() => {
            // F4 (defence in depth): a verified data-verb token whose
            // scope is node-wide cannot satisfy any folder check at
            // the runtime gate (the gate keys on the BEP folder id
            // `p2p-<name>`). A token minted by a buggy issuer that
            // somehow made it past the local `token issue` guard
            // would still be a silent no-op; the data_access filter
            // catches it. Refuse explicitly here too so a node-wide
            // data token is logged and never folded into the grant
            // set the gate consults.
            if claims.scope.is_node_wide() {
                tracing::debug!(
                    target: "cascade::manage::data",
                    "data token carries a node-wide scope — not folded into decision",
                );
                return None;
            }
            Some(claims.to_grant())
        }
        Ok(claims) => {
            tracing::debug!(
                target: "cascade::manage::data",
                capability = claims.capability.as_wire(),
                "data token carries a non-data verb — not folded into data-plane decision",
            );
            None
        }
        Err(e) => {
            tracing::debug!(
                target: "cascade::manage::data",
                error = %e,
                "data token presented on sync frame rejected — not folded into decision",
            );
            None
        }
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
    token: Option<String>,
    now: DateTime<Utc>,
) -> ManageResult
where
    S: ManageGrantStore + ?Sized,
    E: ManageCommandExecutor + ?Sized,
{
    let capability = required_capability(&command);
    let scope = scope_from_wire(&wire_scope);
    let command_text = command_summary(&command);

    let mut grants = match store.manage_grants() {
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

    // A presented capability token is a portable, offline-issued grant. Verify
    // it against this node (signed by this node or a chain rooting in it,
    // unexpired, not revoked, bearer == the authenticated caller) and, on
    // success, fold the token-carried grant into the grant set so the command is
    // authorised against it by the *same* `authorises` path an on-node grant
    // takes. A presented-but-invalid token is a hard rejection — never a silent
    // fall-through to "no token" — because accepting the command without the
    // authority the caller tried to assert would mask the rejection.
    if let Some(token_json) = token {
        match verify_presented_token(store, caller, &token_json, now) {
            Ok(token_grant) => grants.push(token_grant),
            Err(rejection) => return rejection,
        }
    }

    // The scope the command's own payload actually mutates, derived from the
    // command rather than the caller-supplied wire `scope`. Authorising over
    // both closes the scope-escape where a Pin{path_glob:"/personal"} carries a
    // wire scope of "/work" the caller does hold a grant over: the grant must
    // cover the path that is really touched, not just the advertised scope.
    //
    // `GrantRevoke` is the special case: its real target is the scope of the
    // *stored grant being revoked*, which cannot be derived from the payload —
    // the payload only carries a row id and a caller-advertised scope. We resolve
    // the stored scope from the store and authorise over it, so a manager holding
    // GrantAdmin over `/work` cannot revoke a grant whose real scope is
    // `/personal` (or Node) by advertising `/work`. A row id that resolves to no
    // grant has no real scope to escape over; the revoke is a no-op, and we fall
    // back to the wire scope purely so the attempt is still authorised and
    // audited coherently.
    //
    // A payload-less command (Restart/Stop) has no derived target; the audit row
    // and authorisation then key on the advertised wire scope alone.
    let target_scope = match &command {
        ManageCommand::GrantRevoke { grant_id, .. } => match store.manage_grant_scope(*grant_id) {
            Ok(Some(stored_scope)) => stored_scope,
            Ok(None) => scope.clone(),
            Err(e) => {
                return ManageResult::Err {
                    kind: ManageErrorKind::Failed,
                    message: format!("could not resolve grant {grant_id} for revocation: {e}"),
                };
            }
        },
        // A session-id-only exec verb authorises over the scope the *session*
        // was spawned under, resolved from node state — never the caller-
        // advertised wire scope. A verb naming a session that does not exist has
        // no real scope to authorise over; it targets the node-wide scope, which
        // the dangerous-capability bar always refuses, so it can never run. This
        // closes the scope-escape where a caller holding exec over `/work`
        // drives a `/personal` session by lying in the wire scope, and refuses a
        // verb on an unknown (or already-ended) session rather than falling back
        // to an attacker-chosen scope.
        ManageCommand::PtyWrite { session, .. }
        | ManageCommand::PtyResize { session, .. }
        | ManageCommand::PtyKill { session, .. }
        | ManageCommand::ProcSignal { session, .. }
        | ManageCommand::ProcKill { session } => match store.exec_session_scope(*session) {
            Ok(Some(session_scope)) => session_scope,
            Ok(None) => Scope::Node,
            Err(e) => {
                return ManageResult::Err {
                    kind: ManageErrorKind::Failed,
                    message: format!("could not resolve exec session {session} scope: {e}"),
                };
            }
        },
        _ => command_target_scope(&command).unwrap_or_else(|| scope.clone()),
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
    //
    // `delegation_bound` carries the expiry the delegated grant must not outlive
    // — the latest expiry among the caller's held grants that authorise the
    // delegation (`None` when at least one such grant never expires). It is
    // applied when the grant is built in `execute`, so a delegate can never live
    // longer than the authority it derived from.
    let delegation = match &command {
        ManageCommand::GrantAdd { grant } => caller_can_delegate(&grants, caller, grant, now),
        _ => Delegation::Permitted(ExpiryBound::Unbounded),
    };
    let delegation_permitted = matches!(delegation, Delegation::Permitted(_));

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

    let delegation_bound = match delegation {
        Delegation::Permitted(bound) => bound,
        // Unreachable: `authorised` is false when delegation is refused, so the
        // function has already returned above. Bound is irrelevant here.
        Delegation::Refused => ExpiryBound::Unbounded,
    };
    let applied = execute(executor, caller, &target_scope, command, delegation_bound).await;
    match applied {
        Ok(DispatchOutcome::Summary(summary)) => ManageResult::Ok { summary },
        #[cfg(feature = "exec")]
        Ok(DispatchOutcome::ExecSpawned(session)) => ManageResult::ExecSpawned { session },
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

/// The outcome of executing an authorised command.
///
/// Most commands return a human-readable summary; a `pty.spawn` / `proc.spawn`
/// returns the new session id, which the dispatcher reports as a typed
/// [`ManageResult::ExecSpawned`] so the caller learns the id to drive subsequent
/// verbs against.
enum DispatchOutcome {
    /// A human-readable summary of the command's effect.
    Summary(String),
    /// A new exec session was spawned with this id. Only produced by the exec
    /// spawn arms, which a build without the `exec` feature does not compile.
    #[cfg(feature = "exec")]
    ExecSpawned(u64),
}

/// Dispatch an authorised command into the executor's matching method.
///
/// `caller` is the authenticated delegating principal; it is the `granted_by`
/// stamped onto a delegated grant, never a value taken off the wire. For an exec
/// spawn it is also recorded as the session owner, and `target_scope` is the
/// folder the session is confined to — both resolved before this point and
/// passed through so the session row records the real authorisation extent.
async fn execute<E>(
    executor: &E,
    caller: &DeviceId,
    target_scope: &Scope,
    command: ManageCommand,
    delegation_bound: ExpiryBound,
) -> anyhow::Result<DispatchOutcome>
where
    E: ManageCommandExecutor + ?Sized,
{
    Ok(match command {
        ManageCommand::StatusRead => DispatchOutcome::Summary(executor.manage_status().await?),
        ManageCommand::Pin {
            path_glob,
            recursive,
        } => DispatchOutcome::Summary(executor.manage_pin(&path_glob, recursive).await?),
        ManageCommand::Unpin { path_glob } => {
            DispatchOutcome::Summary(executor.manage_unpin(&path_glob).await?)
        }
        ManageCommand::CacheEvict => DispatchOutcome::Summary(executor.manage_cache_evict().await?),
        ManageCommand::CacheWarm { path_glob } => {
            DispatchOutcome::Summary(executor.manage_cache_warm(&path_glob).await?)
        }
        ManageCommand::ConfigPush {
            format,
            folder,
            body,
        } => DispatchOutcome::Summary(executor.manage_config_push(format, &folder, &body).await?),
        ManageCommand::PolicySet {
            path_glob,
            max_age_secs,
            max_file_size,
            priority,
        } => DispatchOutcome::Summary(
            executor
                .manage_policy_set(&path_glob, max_age_secs, max_file_size, priority)
                .await?,
        ),
        ManageCommand::BackendAdd {
            name,
            backend_type,
            mount_path,
            config_toml,
        } => DispatchOutcome::Summary(
            executor
                .manage_backend_add(&name, &backend_type, &mount_path, &config_toml)
                .await?,
        ),
        ManageCommand::BackendRemove { name, mount_path } => {
            DispatchOutcome::Summary(executor.manage_backend_remove(&name, &mount_path).await?)
        }
        ManageCommand::Restart => DispatchOutcome::Summary(executor.manage_restart().await?),
        ManageCommand::Stop => DispatchOutcome::Summary(executor.manage_stop().await?),
        ManageCommand::GrantAdd { grant } => {
            // Build the domain grant, stamping the authenticated caller as
            // `granted_by`. The grantee/capability/scope/expiry come off the
            // wire and are validated here; a malformed grant is a Failed error,
            // not a silent skip. The expiry is clamped to `delegation_bound` so
            // a delegate can never outlive the authority that backed it.
            let domain = grant_from_wire(&grant, caller, delegation_bound)?;
            DispatchOutcome::Summary(executor.manage_grant_add(&domain).await?)
        }
        ManageCommand::GrantRevoke { grant_id, .. } => {
            DispatchOutcome::Summary(executor.manage_grant_revoke(grant_id).await?)
        }
        #[cfg(feature = "exec")]
        ManageCommand::PtySpawn {
            shell,
            argv,
            cwd,
            env,
            cols,
            rows,
        } => DispatchOutcome::ExecSpawned(
            executor
                .manage_pty_spawn(
                    caller,
                    target_scope,
                    shell.as_deref(),
                    &argv,
                    cwd.as_deref(),
                    &env,
                    cols,
                    rows,
                )
                .await?,
        ),
        #[cfg(feature = "exec")]
        ManageCommand::PtyWrite { session, bytes } => {
            DispatchOutcome::Summary(executor.manage_pty_write(session, &bytes).await?)
        }
        #[cfg(feature = "exec")]
        ManageCommand::PtyResize {
            session,
            cols,
            rows,
        } => DispatchOutcome::Summary(executor.manage_pty_resize(session, cols, rows).await?),
        #[cfg(feature = "exec")]
        ManageCommand::PtyKill { session, signal } => {
            DispatchOutcome::Summary(executor.manage_pty_kill(session, signal).await?)
        }
        #[cfg(feature = "exec")]
        ManageCommand::ProcSpawn { argv, cwd, env } => DispatchOutcome::ExecSpawned(
            executor
                .manage_proc_spawn(caller, target_scope, &argv, cwd.as_deref(), &env)
                .await?,
        ),
        #[cfg(feature = "exec")]
        ManageCommand::ProcSignal { session, signal } => {
            DispatchOutcome::Summary(executor.manage_proc_signal(session, signal).await?)
        }
        #[cfg(feature = "exec")]
        ManageCommand::ProcKill { session } => {
            DispatchOutcome::Summary(executor.manage_proc_kill(session).await?)
        }
        // A build without the exec feature decodes the exec wire verbs but has no
        // provider to run them; refuse loudly rather than silently doing nothing.
        #[cfg(not(feature = "exec"))]
        ManageCommand::PtySpawn { .. }
        | ManageCommand::PtyWrite { .. }
        | ManageCommand::PtyResize { .. }
        | ManageCommand::PtyKill { .. }
        | ManageCommand::ProcSpawn { .. }
        | ManageCommand::ProcSignal { .. }
        | ManageCommand::ProcKill { .. } => {
            // `target_scope` is only read by the exec arms above, which this
            // build does not compile; bind it so it is not an unused parameter.
            let _ = target_scope;
            anyhow::bail!("this node was built without the exec capability");
        }
    })
}

/// The upper bound a delegated grant's expiry must respect, derived from the
/// caller's own authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExpiryBound {
    /// At least one backing grant never expires, so the delegate may carry any
    /// expiry (or none).
    Unbounded,
    /// The delegate must expire no later than this instant — the latest expiry
    /// among the backing grants that authorise the delegation.
    NoLaterThan(DateTime<Utc>),
}

impl ExpiryBound {
    /// Clamp a requested expiry to this bound.
    ///
    /// An [`Unbounded`](Self::Unbounded) bound returns the request unchanged. A
    /// [`NoLaterThan`](Self::NoLaterThan) bound returns the earlier of the
    /// request and the bound; a request of `None` (never expires) is pulled down
    /// to the bound so the delegate cannot outlive its backing authority.
    fn clamp(self, requested: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
        match self {
            Self::Unbounded => requested,
            Self::NoLaterThan(limit) => Some(requested.map_or(limit, |req| req.min(limit))),
        }
    }
}

/// The outcome of the delegation subset/no-escalation gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delegation {
    /// The delegation is permitted; the delegated expiry is bounded as carried.
    Permitted(ExpiryBound),
    /// The delegation is refused — an escalation attempt.
    Refused,
}

/// Build a domain [`Grant`] from a wire [`ManageGrant`], stamping `granted_by`
/// with the authenticated caller and clamping the expiry to `bound`.
///
/// Validates the capability against the known vocabulary and parses the optional
/// RFC 3339 expiry, failing loudly rather than dropping a malformed field. The
/// parsed expiry is then clamped to `bound` so a delegated grant can never
/// outlive the authority that backed it (see [`caller_can_delegate`]).
fn grant_from_wire(
    wire: &ManageGrant,
    granted_by: &DeviceId,
    bound: ExpiryBound,
) -> anyhow::Result<Grant> {
    let capability = Capability::from_wire(&wire.capability).ok_or_else(|| {
        anyhow::anyhow!("unknown capability in delegated grant: {}", wire.capability)
    })?;
    let scope = scope_from_wire(&wire.scope);
    // F4: refuse a data-verb grant over a node-wide / wildcard scope on the
    // wire side too. The runtime data-plane gate keys on the BEP folder id
    // (`p2p-<name>`) and there is no such id at the node root — a node-wide
    // data grant is a silent no-op. The local `cascade grant` and
    // `cascade token issue` CLIs apply the same bar (see
    // `crate::cascade::cli::grant::add` and `crate::cascade::cli::token::issue`),
    // so the wire-side parse is defence in depth, not the only line of
    // defence. The check fires before `bound.clamp` and before the
    // `Grant` is constructed, so a refused grant never reaches the store.
    if scope.is_node_wide() && (capability.is_dangerous() || capability.is_data_verb()) {
        anyhow::bail!(
            "capability `{}` cannot be granted over a wildcard scope; \
             name an explicit folder (data verbs are folder-scoped, not \
             node-wide)",
            capability.as_wire()
        );
    }
    let requested_expiry = wire
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
        expires: bound.clamp(requested_expiry),
    })
}

/// Whether `caller` may delegate `delegated` — the subset/no-escalation guard
/// for [`ManageCommand::GrantAdd`].
///
/// The delegated grant must be wholly contained in authority the caller can
/// *itself exercise*. For each held grant the caller owns, the delegation is
/// backed by that grant only when [`authorises`] holds for it — the same
/// decision the caller would face when using the capability directly. Reusing
/// `authorises` is load-bearing: it folds in the dangerous-capability +
/// node-wide bar, so a node-wide dangerous grant (which the caller can never
/// exercise) can never back a delegation, closing the privilege-laundering path
/// where a narrow folder-scoped dangerous grant is manufactured from an
/// unusable node-wide one.
///
/// Expiry is bounded, not merely ignored: the returned [`ExpiryBound`] is the
/// latest expiry among the backing grants (`Unbounded` when at least one never
/// expires). [`grant_from_wire`] clamps the delegated expiry to it, so a
/// delegate can never outlive the authority it derived from — correcting the
/// previous, false claim that a longer delegated expiry was "bounded in effect
/// by the caller's own grant lapsing".
fn caller_can_delegate(
    grants: &[Grant],
    caller: &DeviceId,
    delegated: &ManageGrant,
    now: DateTime<Utc>,
) -> Delegation {
    let Some(capability) = Capability::from_wire(&delegated.capability) else {
        // An unrecognised capability cannot be a subset of anything the caller
        // holds — refuse rather than letting it through to a later Failed.
        return Delegation::Refused;
    };
    let delegated_scope = scope_from_wire(&delegated.scope);

    // The grants that back the delegation: the caller's own grants of the same
    // capability whose scope covers the delegated scope AND which the caller
    // could actually exercise (the `authorises` check applies the dangerous +
    // node-wide bar and the expiry check).
    let backing: Vec<&Grant> = grants
        .iter()
        .filter(|held| {
            held.grantee == *caller
                && held.capability == capability
                && held.scope.covers(&delegated_scope)
                && authorises(grants, caller, capability, &held.scope, now)
        })
        .collect();

    if backing.is_empty() {
        return Delegation::Refused;
    }

    // The delegate may live as long as the longest-living backing grant: if any
    // backing grant never expires the bound is unbounded, otherwise it is the
    // latest backing expiry.
    let mut bound = ExpiryBound::NoLaterThan(DateTime::<Utc>::MIN_UTC);
    for held in backing {
        match held.expires {
            None => {
                bound = ExpiryBound::Unbounded;
                break;
            }
            Some(expiry) => {
                if let ExpiryBound::NoLaterThan(current) = bound
                    && expiry > current
                {
                    bound = ExpiryBound::NoLaterThan(expiry);
                }
            }
        }
    }
    Delegation::Permitted(bound)
}

#[cfg(test)]
#[path = "dispatch_tests.rs"]
mod tests;
