//! Management-plane dispatch: the engine as grant store, command executor, and
//! dispatch endpoint.
//!
//! The engine is the production grant store, command executor, and dispatch
//! endpoint for the management plane. It owns the grant store, the audit log,
//! and the command executor. A backend that runs its own peer transport
//! receives inbound `ManageRequest` frames authorised, audited, and executed
//! through these trait impls.
//!
//! With the `p2p` feature enabled, the engine implements
//! [`ManageGrantStore`](crate::manage::ManageGrantStore),
//! [`ManageCommandExecutor`](crate::manage::ManageCommandExecutor), and
//! the `ManageDispatch` trait from the `cascade_p2p` protocol module.

use std::collections::HashSet;

use anyhow::Result;
#[cfg(feature = "p2p")]
use async_trait::async_trait;
#[cfg(feature = "p2p")]
use chrono::{DateTime, Utc};

#[cfg(feature = "p2p")]
use cascade_p2p::protocol::{ManageCommand, ManageResult, ManageScope as WireScope};

use super::Engine;
use crate::db::AuditEntry;
use crate::manage::{DeviceId, Grant, Scope};
use crate::portable::{Clock, RuntimeHandle};

#[cfg(feature = "p2p")]
/// The engine is the grant store and audit sink for the management plane,
/// reading and writing the two `state.db` tables the prior phase added.
impl<R: RuntimeHandle, C: Clock> crate::manage::ManageGrantStore for Engine<R, C> {
    fn manage_grants(&self) -> Result<Vec<Grant>> {
        Ok(self
            .db
            .list_grants()?
            .into_iter()
            .map(|record| record.grant)
            .collect())
    }

    fn manage_grant_scope(&self, grant_id: i64) -> Result<Option<Scope>> {
        self.db.grant_scope(grant_id)
    }

    fn manage_append_audit(&self, entry: &AuditEntry) -> Result<()> {
        self.db.append_audit(entry).map(|_id| ())
    }

    fn manage_node_device_id(&self) -> Result<DeviceId> {
        // The management plane's identity is the P2P data-plane device identity —
        // the same key a capability token's chain roots in. Without a configured
        // P2P backend the node has no device identity, so token verification
        // fails loudly rather than rooting a chain in a placeholder.
        let id = self
            .p2p
            .as_ref()
            .map(|b| b.device_id().to_owned())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no P2P backend configured — the node has no device identity to verify a \
                     capability token against"
                )
            })?;
        Ok(DeviceId::new(id))
    }

    fn manage_revoked_token_ids(&self) -> Result<HashSet<String>> {
        self.db.revoked_token_ids()
    }

    #[cfg(feature = "exec")]
    fn exec_session_scope(&self, session: u64) -> Result<Option<Scope>> {
        // Resolve the scope the live session was spawned under, dropping the
        // owner the row also records: authorisation keys on the scope, and the
        // owner is enforced separately at the live-stdio frame path.
        Ok(self
            .db
            .exec_session_scope(session)?
            .map(|(_owner, scope)| scope))
    }
}

#[cfg(feature = "p2p")]
/// The engine is the command executor for the management plane. Each method is
/// the *same* operation the local CLI drives — `pin`, `unpin`, `status`, and a
/// cache eviction sweep — so a manager can never do more than the daemon could
/// do to itself, and no command logic is duplicated. The remote path reaches
/// these only after authorisation and auditing in [`crate::manage::run_dispatch`].
#[async_trait]
impl<R: RuntimeHandle, C: Clock> crate::manage::ManageCommandExecutor for Engine<R, C> {
    async fn manage_status(&self) -> Result<String> {
        let status = self.status().await;
        Ok(format!(
            "running={} backends={} online={} cached={} pinned={} p2p_enabled={}",
            status.running,
            status.backends.len(),
            status.cache_stats.online_count,
            status.cache_stats.cached_count,
            status.cache_stats.pinned_count,
            status.p2p_enabled,
        ))
    }

    async fn manage_pin(&self, path_glob: &str, recursive: bool) -> Result<String> {
        self.pin(path_glob, recursive).await?;
        Ok(format!("pinned {path_glob} (recursive={recursive})"))
    }

    async fn manage_unpin(&self, path_glob: &str) -> Result<String> {
        let removed = self.unpin(path_glob).await?;
        Ok(if removed {
            format!("unpinned {path_glob}")
        } else {
            format!("no pin rule matched {path_glob}")
        })
    }

    async fn manage_cache_evict(&self) -> Result<String> {
        let report = self.cache.evict().await?;
        Ok(format!(
            "evicted {} files ({} lifecycle, {} size), freed {} bytes",
            report.total_evicted(),
            report.lifecycle_evicted,
            report.size_evicted,
            report.bytes_freed,
        ))
    }

    async fn manage_cache_warm(&self, path_glob: &str) -> Result<String> {
        self.warm(path_glob).await?;
        Ok(format!("warmed {path_glob}"))
    }

    async fn manage_config_push(
        &self,
        format: cascade_p2p::protocol::ManageConfigFormat,
        folder: &str,
        body: &str,
    ) -> Result<String> {
        let config = super::operations::parse_config_fragment(format, body)?;
        self.config_push(folder, &config).await
    }

    async fn manage_policy_set(
        &self,
        path_glob: &str,
        max_age_secs: Option<i64>,
        max_file_size: Option<i64>,
        priority: i32,
    ) -> Result<String> {
        self.policy_set(path_glob, max_age_secs, max_file_size, priority)
            .await
    }

    async fn manage_backend_add(
        &self,
        name: &str,
        backend_type: &str,
        mount_path: &str,
        config_toml: &str,
    ) -> Result<String> {
        self.backend_add(name, backend_type, mount_path, config_toml)
            .await
    }

    async fn manage_backend_remove(&self, name: &str, mount_path: &str) -> Result<String> {
        self.backend_remove(name, mount_path).await
    }

    async fn manage_restart(&self) -> Result<String> {
        // The returned handle owns the freshly spawned cache-manager task. The
        // daemon keeps the engine alive past this call, so detaching the handle
        // lets the new worker run for the daemon's lifetime exactly as the
        // initial `start()` handle does. The sync runner is owned by the daemon
        // and not revived here — see `Engine::restart`.
        let _handle = self.restart()?;
        Ok("daemon cache-manager worker restarted (sync runner unaffected — full restart requires restarting the daemon)".to_owned())
    }

    async fn manage_stop(&self) -> Result<String> {
        Ok(self.stop())
    }

    async fn manage_grant_add(&self, grant: &Grant) -> Result<String> {
        self.grant_add(grant).await
    }

    async fn manage_grant_revoke(&self, grant_id: i64) -> Result<String> {
        self.grant_revoke(grant_id).await
    }

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
    ) -> Result<u64> {
        let exec = self.exec_provider()?;
        let spec = cascade_exec::PtySpec {
            shell: shell.map(ToOwned::to_owned),
            argv: argv.to_vec(),
            cwd: cwd.map(ToOwned::to_owned),
            env: env.to_vec(),
            cols,
            rows,
        };
        let id = exec.pty_spawn(spec).await?;
        let command = pty_command_summary(shell, argv);
        self.record_exec_session(id, cascade_exec::ExecKind::Pty, owner, scope, command)?;
        Ok(id.0)
    }

    #[cfg(feature = "exec")]
    async fn manage_pty_write(&self, session: u64, bytes: &[u8]) -> Result<String> {
        let exec = self.exec_provider()?;
        exec.pty_write(cascade_exec::ExecSessionId(session), bytes)
            .await?;
        Ok(format!("wrote {} bytes to session {session}", bytes.len()))
    }

    #[cfg(feature = "exec")]
    async fn manage_pty_resize(&self, session: u64, cols: u16, rows: u16) -> Result<String> {
        let exec = self.exec_provider()?;
        exec.pty_resize(cascade_exec::ExecSessionId(session), cols, rows)
            .await?;
        Ok(format!("resized session {session} to {cols}x{rows}"))
    }

    #[cfg(feature = "exec")]
    async fn manage_pty_kill(&self, session: u64, signal: i32) -> Result<String> {
        let exec = self.exec_provider()?;
        exec.pty_kill(cascade_exec::ExecSessionId(session), signal)
            .await?;
        self.db
            .mark_exec_session_ended(session, self.clock.now(), None, Some(signal))?;
        Ok(format!("sent signal {signal} to session {session}"))
    }

    #[cfg(feature = "exec")]
    async fn manage_proc_spawn(
        &self,
        owner: &DeviceId,
        scope: &Scope,
        argv: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
    ) -> Result<u64> {
        let exec = self.exec_provider()?;
        let spec = cascade_exec::ProcSpec {
            argv: argv.to_vec(),
            cwd: cwd.map(ToOwned::to_owned),
            env: env.to_vec(),
        };
        let id = exec.proc_spawn(spec).await?;
        let command = proc_command_summary(argv);
        self.record_exec_session(id, cascade_exec::ExecKind::Proc, owner, scope, command)?;
        Ok(id.0)
    }

    #[cfg(feature = "exec")]
    async fn manage_proc_signal(&self, session: u64, signal: i32) -> Result<String> {
        let exec = self.exec_provider()?;
        exec.proc_signal(cascade_exec::ExecSessionId(session), signal)
            .await?;
        Ok(format!("sent signal {signal} to session {session}"))
    }

    #[cfg(feature = "exec")]
    async fn manage_proc_kill(&self, session: u64) -> Result<String> {
        let exec = self.exec_provider()?;
        exec.proc_kill(cascade_exec::ExecSessionId(session)).await?;
        self.db
            .mark_exec_session_ended(session, self.clock.now(), None, None)?;
        Ok(format!("killed session {session}"))
    }
}

#[cfg(feature = "exec")]
impl<R: RuntimeHandle, C: Clock> Engine<R, C> {
    /// The injected exec provider, or a loud error when none is wired.
    ///
    /// A node without the exec provider refuses an exec verb rather than
    /// silently doing nothing, mirroring `manage_node_device_id`'s "no P2P
    /// backend" failure.
    fn exec_provider(&self) -> Result<&std::sync::Arc<dyn cascade_exec::ExecProvider>> {
        self.exec.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "no exec provider configured — this node cannot broker terminals or processes"
            )
        })
    }

    /// Record a freshly spawned exec session in the durable `exec_sessions`
    /// table so a later session-id-only verb resolves its authorisation scope
    /// and owner from it.
    fn record_exec_session(
        &self,
        id: cascade_exec::ExecSessionId,
        kind: cascade_exec::ExecKind,
        owner: &DeviceId,
        scope: &Scope,
        command: String,
    ) -> Result<()> {
        self.db.insert_exec_session(&crate::db::ExecSessionRow {
            id: id.0,
            kind,
            owner_device: owner.clone(),
            scope: scope.clone(),
            command,
            started_at: self.clock.now(),
        })
    }
}

/// A short, audit-friendly summary of a PTY spawn's command.
#[cfg(feature = "exec")]
fn pty_command_summary(shell: Option<&str>, argv: &[String]) -> String {
    let shell = shell.unwrap_or("<default shell>");
    if argv.is_empty() {
        shell.to_owned()
    } else {
        format!("{shell} {}", argv.join(" "))
    }
}

/// A short, audit-friendly summary of a process spawn's command.
#[cfg(feature = "exec")]
fn proc_command_summary(argv: &[String]) -> String {
    if argv.is_empty() {
        "<empty argv>".to_owned()
    } else {
        argv.join(" ")
    }
}

#[cfg(feature = "p2p")]
#[async_trait]
impl<R: RuntimeHandle, C: Clock> crate::manage::ManageDispatch for Engine<R, C> {
    async fn dispatch(
        &self,
        caller: &DeviceId,
        command: ManageCommand,
        scope: WireScope,
        token: Option<String>,
        now: DateTime<Utc>,
    ) -> ManageResult {
        crate::manage::run_dispatch(self, self, caller, command, scope, token, now).await
    }
}
