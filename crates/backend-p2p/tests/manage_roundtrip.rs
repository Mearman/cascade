#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::string_slice
)]
//! Management-plane round-trip integration test.
//!
//! A manager-side [`SyncEngine`] drives a target node's `StatusRead` and
//! `PinWrite` commands end to end over loopback TCP + mutual TLS — the same
//! wire path the daemon uses, with no real network and no Docker. This proves
//! the full manager → wire → target-node-dispatch → wire → manager round-trip:
//! the manager-side [`SyncEngine::send_manage_request`] sends a
//! `BepMessage::ManageRequest`, the target node authorises + audits + runs it
//! through its injected [`ManageDispatch`], and the
//! `BepMessage::ManageResponse` is correlated back to the waiting caller.
//!
//! The wire commands are built exactly as the manager CLI's `RemoteCommand`
//! maps them, so the test mirrors what `cascade remote <device-id> status` and
//! `cascade remote <device-id> pin <path>` put on the wire.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use cascade_backend_p2p::index::FolderIndex;
use cascade_backend_p2p::sync::{Peer, SyncEngine};
use cascade_engine::db::AuditEntry;
use cascade_engine::manage::{
    Capability, DeviceId, Grant, ManageCommandExecutor, ManageDispatch, ManageGrantStore, Scope,
    run_dispatch,
};
use cascade_p2p::identity::DeviceIdentity;
use cascade_p2p::protocol::{ManageCommand, ManageErrorKind, ManageResult, ManageScope};
use cascade_p2p::store::BlockStore;
use chrono::{DateTime, Utc};
use tempfile::TempDir;

/// In-memory grant store + audit sink, plus a recording executor — the
/// in-process double standing in for the daemon's `Engine` so the test
/// exercises the real authorise → audit → execute dispatch core without a
/// database or live filesystem.
struct TestNode {
    grants: Vec<Grant>,
    audit: Mutex<Vec<AuditEntry>>,
    calls: Mutex<Vec<String>>,
}

impl TestNode {
    const fn new(grants: Vec<Grant>) -> Self {
        Self {
            grants,
            audit: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().map(|c| c.clone()).unwrap_or_default()
    }

    fn audit_outcomes(&self) -> Vec<String> {
        self.audit
            .lock()
            .map(|rows| rows.iter().map(|r| r.outcome.clone()).collect())
            .unwrap_or_default()
    }

    fn record(&self, call: &str) {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push(call.to_owned());
        }
    }
}

impl ManageGrantStore for TestNode {
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

#[async_trait]
impl ManageCommandExecutor for TestNode {
    async fn manage_status(&self) -> anyhow::Result<String> {
        self.record("status");
        Ok("mount: ok; cache: 0 files".to_owned())
    }

    async fn manage_pin(&self, path_glob: &str, recursive: bool) -> anyhow::Result<String> {
        self.record(&format!("pin {path_glob} {recursive}"));
        Ok(format!("pinned {path_glob}"))
    }

    async fn manage_unpin(&self, path_glob: &str) -> anyhow::Result<String> {
        self.record(&format!("unpin {path_glob}"));
        Ok(format!("unpinned {path_glob}"))
    }

    async fn manage_cache_evict(&self) -> anyhow::Result<String> {
        self.record("evict");
        Ok("evicted 0 files".to_owned())
    }
}

/// Bridge the test node into the [`ManageDispatch`] port the target-side
/// [`SyncEngine`] calls when a `ManageRequest` arrives. Delegates straight to
/// the real [`run_dispatch`] core, so the wire round-trip runs the production
/// authorisation, audit, and execution path against the in-memory double.
#[async_trait]
impl ManageDispatch for TestNode {
    async fn dispatch(
        &self,
        caller: &DeviceId,
        command: ManageCommand,
        scope: ManageScope,
        now: DateTime<Utc>,
    ) -> ManageResult {
        run_dispatch(self, self, caller, command, scope, now).await
    }
}

/// Build a bare [`SyncEngine`] backed by a fresh tempdir index + block store
/// and a freshly generated device identity. The tempdir is returned so it
/// outlives the engine for the test's duration.
fn make_engine(folder_id: &str) -> (TempDir, SyncEngine) {
    let dir = tempfile::tempdir().unwrap();
    let index = Arc::new(FolderIndex::open(&dir.path().join("index.db")).unwrap());
    let blocks = Arc::new(BlockStore::new(&dir.path().join("blocks")).unwrap());
    let identity = DeviceIdentity::load_or_generate(&dir.path().join("identity")).unwrap();
    let engine = SyncEngine::new(folder_id.to_owned(), index, blocks, identity);
    (dir, engine)
}

/// A grant of `capability` over `scope` for `grantee`, issued by `owner`, with
/// no expiry.
fn grant(grantee: &str, capability: Capability, scope: Scope) -> Grant {
    Grant {
        grantee: DeviceId::new(grantee.to_owned()),
        capability,
        scope,
        granted_by: DeviceId::new("OWNER"),
        expires: None,
    }
}

#[tokio::test]
async fn manager_drives_status_and_pin_over_loopback() {
    // The target node grants the manager status:read node-wide and pin:write
    // over /work — exactly what `cascade grant add` would persist.
    let (_manager_dir, manager) = make_engine("shared");
    let manager_id = manager.device_id().to_owned();

    let node = Arc::new(TestNode::new(vec![
        grant(&manager_id, Capability::StatusRead, Scope::Node),
        grant(&manager_id, Capability::PinWrite, Scope::folder("/work")),
    ]));

    let (_target_dir, target) = make_engine("shared");
    let dispatch: Arc<dyn ManageDispatch> = node.clone();
    let target = target.with_manage_dispatch(dispatch);
    let target_id = target.device_id().to_owned();

    // Mutual trust — the target node only accepts a TLS handshake from a
    // trusted device, and the manager only dials a trusted device.
    target.trust(manager_id.clone()).await;
    manager.trust(target_id.clone()).await;

    // Target node listens; manager dials in over loopback TCP + mutual TLS.
    // The held cancel sender keeps the listener task alive for the test.
    let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let (addr, _task) = target
        .start_listener("127.0.0.1:0".parse().unwrap(), cancel_rx)
        .await
        .unwrap();
    manager
        .connect_to(Peer {
            device_id: target_id.clone(),
            address: addr,
        })
        .await
        .unwrap();

    // Wait for the session to register before sending — `connect_to` spawns
    // the session loop, which registers the peer handle a moment later.
    for _ in 0..100 {
        if manager.has_peer(&target_id).await {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        manager.has_peer(&target_id).await,
        "manager never established a session with the target node",
    );

    // ── StatusRead — the wire command `cascade remote <id> status` sends ──
    let status = manager
        .send_manage_request(&target_id, ManageCommand::StatusRead, ManageScope::Node)
        .await
        .expect("status round-trip should not fail at the transport");
    assert!(
        matches!(status, ManageResult::Ok { .. }),
        "authorised status:read should succeed, got {status:?}",
    );

    // ── PinWrite within the granted scope — `cascade remote <id> pin /work/x` ──
    let pin = manager
        .send_manage_request(
            &target_id,
            ManageCommand::Pin {
                path_glob: "/work/reports".to_owned(),
                recursive: true,
            },
            ManageScope::Folder {
                path: "/work/reports".to_owned(),
            },
        )
        .await
        .expect("pin round-trip should not fail at the transport");
    assert!(
        matches!(pin, ManageResult::Ok { .. }),
        "authorised pin should succeed, got {pin:?}",
    );

    // The target node ran both side effects exactly once, in order.
    assert_eq!(
        node.calls(),
        vec!["status".to_owned(), "pin /work/reports true".to_owned()],
        "the target node must have run status then pin",
    );
    // Both attempts were audited as allowed.
    assert_eq!(
        node.audit_outcomes(),
        vec!["allowed".to_owned(), "allowed".to_owned()],
    );

    // ── PinWrite OUTSIDE the granted scope — must be refused by the node ──
    let denied = manager
        .send_manage_request(
            &target_id,
            ManageCommand::Pin {
                path_glob: "/personal/secret".to_owned(),
                recursive: true,
            },
            ManageScope::Folder {
                path: "/personal/secret".to_owned(),
            },
        )
        .await
        .expect("an unauthorised pin still returns a typed reply, not a transport error");
    assert!(
        matches!(
            denied,
            ManageResult::Err {
                kind: ManageErrorKind::Unauthorised,
                ..
            }
        ),
        "a pin outside the granted scope must be refused, got {denied:?}",
    );
    // No new side effect ran for the denied pin; the denial was audited.
    assert_eq!(
        node.calls().len(),
        2,
        "the denied pin must not have run a side effect",
    );
    assert_eq!(
        node.audit_outcomes(),
        vec![
            "allowed".to_owned(),
            "allowed".to_owned(),
            "denied".to_owned(),
        ],
    );
}
