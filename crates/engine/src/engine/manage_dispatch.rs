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

#[cfg(feature = "p2p")]
/// The engine is the grant store and audit sink for the management plane,
/// reading and writing the two `state.db` tables the prior phase added.
impl crate::manage::ManageGrantStore for Engine {
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
}

#[cfg(feature = "p2p")]
/// The engine is the command executor for the management plane. Each method is
/// the *same* operation the local CLI drives — `pin`, `unpin`, `status`, and a
/// cache eviction sweep — so a manager can never do more than the daemon could
/// do to itself, and no command logic is duplicated. The remote path reaches
/// these only after authorisation and auditing in [`crate::manage::run_dispatch`].
#[async_trait]
impl crate::manage::ManageCommandExecutor for Engine {
    async fn manage_status(&self) -> Result<String> {
        let status = self.status();
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
        self.config_push(folder, &config)
    }

    async fn manage_policy_set(
        &self,
        path_glob: &str,
        max_age_secs: Option<i64>,
        max_file_size: Option<i64>,
        priority: i32,
    ) -> Result<String> {
        self.policy_set(path_glob, max_age_secs, max_file_size, priority)
    }

    async fn manage_backend_add(
        &self,
        name: &str,
        backend_type: &str,
        mount_path: &str,
        config_toml: &str,
    ) -> Result<String> {
        self.backend_add(name, backend_type, mount_path, config_toml)
    }

    async fn manage_backend_remove(&self, name: &str, mount_path: &str) -> Result<String> {
        self.backend_remove(name, mount_path)
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
        self.grant_add(grant)
    }

    async fn manage_grant_revoke(&self, grant_id: i64) -> Result<String> {
        self.grant_revoke(grant_id)
    }
}

#[cfg(feature = "p2p")]
#[async_trait]
impl crate::manage::ManageDispatch for Engine {
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
