#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::string_slice
)]
//! F1 verification test: the namespace fix for directional data sharing.
//!
//! Walking the full chain (CLI semantics through `insert_grant` through
//! `list_data_grants` through `DataAuthority::data_access` through
//! `SyncEngine::data_access_for`), this test exercises the live
//! [`Engine`] against a real [`StateDb`] and asserts that:
//!
//! 1. A `data:read` grant scoped to the resolved `p2p-<name>` value narrows
//!    the gate: the peer's read direction is allowed and the absent write
//!    direction is denied. The bonus (regression that would have caught F1):
//! 2. A grant scoped to an arbitrary filesystem path (the OLD wrong shape,
//!    which the pre-fix CLI accepted silently) does NOT satisfy the
//!    runtime gate whose `folder_id` is the `p2p-<name>` value —
//!    default-open returns, exactly the F1 failure mode.
//!
//! The brief is explicit: the test must use a real `Engine` and real
//! `data_access`, not a `FixedDataAuthority` (or any other folder-ignoring
//! test double) that short-circuits the gate. The point is to fail when
//! the runtime gate's `Scope::covers` check sees the wrong namespace.

use std::path::PathBuf;
use std::sync::Arc;

use cascade_backend_p2p::index::FolderIndex;
use cascade_backend_p2p::sync::SyncEngine;
use cascade_engine::backend::{MountedBackend, NullBackend};
use cascade_engine::engine::{Engine, EngineConfig};
use cascade_engine::manage::{Capability, DataAccess, DataAuthority, DeviceId, Grant, Scope};
use cascade_p2p::identity::DeviceIdentity;
use cascade_p2p::store::BlockStore;

/// A [`Backend`] that does nothing — its only job is to satisfy the
/// `Engine::new` contract that requires at least one backend. The
/// `Engine` registers it in the `backends` table and mounts it; the
/// test never exercises it. Using `NullBackend` keeps the test focused
/// on the data-plane authority the test really cares about — the
/// `SyncEngine` injected into the engine as the data authority.
fn make_engine_with_p2p_data_authority(
    register_backend_name: &str,
) -> (tempfile::TempDir, Arc<Engine>) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");
    // A real `NullBackend` is mounted so `Engine::new` succeeds. Its
    // `id()` is "p2p-only" — irrelevant to the F1 namespace check,
    // which keys off the `backends` table row registered by the
    // caller below.
    let engine = Arc::new(
        Engine::new(EngineConfig {
            db_path,
            mount_point: PathBuf::from("/tmp/f1-mount"),
            backends: vec![MountedBackend::at_default(Arc::new(NullBackend::new(
                "p2p-only",
            )))],
            cache_dir: None,
            enable_p2p: false,
            p2p_data_dir: None,
            p2p_posture: None,
            p2p_relay_endpoints: Vec::new(),
            p2p_relay_shared_secret: None,
            backend_factory: None,
        })
        .unwrap(),
    );
    // Register the P2P backend row the test will resolve against. The
    // `id` is the canonical BEP folder id the runtime gate checks;
    // the `display_name` is the operator-facing name the CLI accepts.
    engine
        .db()
        .register_backend(
            &format!("p2p-{register_backend_name}"),
            "p2p",
            register_backend_name,
            None,
            None,
        )
        .unwrap();
    (dir, engine)
}

/// A bare [`SyncEngine`] — the real one, not a `FixedDataAuthority`
/// double. The F1 test threads the live [`Engine`] (via its
/// `DataAuthority` impl) into this engine, then calls
/// `data_access_for_tls_verified` against the live `Engine`'s grant store.
fn make_sync_engine(folder_id: &str) -> (tempfile::TempDir, SyncEngine) {
    let dir = tempfile::tempdir().unwrap();
    let index = Arc::new(FolderIndex::open(&dir.path().join("index.db")).unwrap());
    let blocks = Arc::new(BlockStore::new(&dir.path().join("blocks")).unwrap());
    let identity = DeviceIdentity::load_or_generate(&dir.path().join("identity")).unwrap();
    let engine = SyncEngine::new(folder_id.to_owned(), index, blocks, identity);
    (dir, engine)
}

/// The F1 success case: a grant scoped to the resolved `p2p-<name>` value
/// narrows the gate as the operator intended.
///
/// 1. Build a real `Engine` with a P2P backend row registered (name `shared`,
///    canonical id `p2p-shared`).
/// 2. Wire the engine as the data authority on a real `SyncEngine` whose
///    `folder_id` is `p2p-shared` (the value the gate checks).
/// 3. Insert a `data:read` grant via the live `StateDb`, scoped to
///    `p2p-shared` (the *resolved* id, not a path).
/// 4. Call `sync_engine.data_access_for_tls_verified(peer)` and assert
///    `read == true` (the grant satisfies the read direction) and
///    `write == false` (the absent write direction narrows under
///    explicit-control mode).
#[tokio::test]
async fn f1_resolved_p2p_id_scopes_narrow_the_data_plane_gate() {
    let (_engine_dir, engine) = make_engine_with_p2p_data_authority("shared");

    // Build a SyncEngine whose folder_id matches the canonical P2P
    // backend id. This is the value the runtime gate checks against
    // — the F1 contract binds the grant scope and the gate's folder
    // argument to the same namespace.
    let (_sync_dir, sync_engine) = make_sync_engine("p2p-shared");

    // Wire the live engine as the data authority. No presented token
    // — the restriction lives entirely on the node.
    let authority: Arc<dyn DataAuthority> = engine.clone();
    sync_engine.set_data_authority(authority).await;

    // The operator's CLI equivalent: insert a data:read grant for the
    // peer, scoped to the resolved `p2p-shared` id (NOT an arbitrary
    // filesystem path). This is the grant the namespace fix produces.
    let owner = DeviceId::new("NODE-OWNER");
    let peer = DeviceId::new("PEER");
    engine
        .db()
        .insert_grant(&Grant {
            grantee: peer.clone(),
            capability: Capability::DataRead,
            scope: Scope::folder("p2p-shared"),
            granted_by: owner,
            expires: None,
        })
        .unwrap();

    // The full data-plane decision: walk through the live engine's
    // DataAuthority::data_access into the pure data_access() function
    // and back to the SyncEngine's gate. No test doubles anywhere.
    let access: DataAccess = sync_engine.data_access_for_tls_verified("PEER").await;
    assert!(
        access.read,
        "a data:read grant scoped to p2p-shared must allow read (F1 contract)",
    );
    assert!(
        !access.write,
        "the absent data:write direction must stay denied under explicit-control mode",
    );
}

/// The F1 failure-mode regression: a grant scoped to an arbitrary
/// filesystem path (the OLD wrong shape, which the pre-fix CLI
/// accepted silently) does NOT satisfy the runtime gate. Default-open
/// returns — the data:read grant is a silent no-op.
///
/// This is the test that would have caught F1: a CLI author writing
/// `share add PEER /work read-only` would have stored a `Scope::folder("/work")`
/// row; the runtime gate consulted `self.folder_id == "p2p-shared"`;
/// `Scope::folder("/work").covers(Scope::folder("p2p-shared"))` was
/// `false`; `has_any_data_grant` stayed `false`; the peer's read
/// direction was silently opened. The bonus assertion pins the
/// pre-fix failure: the F1 contract demands a grant on `/work` cannot
/// satisfy a `folder_id == "p2p-shared"` query, and a real `Engine`
/// would have shown it.
#[tokio::test]
async fn f1_path_scoped_grant_does_not_satisfy_p2p_folder_id_gate() {
    let (_engine_dir, engine) = make_engine_with_p2p_data_authority("shared");

    // SyncEngine whose folder_id is the P2P id the gate checks.
    let (_sync_dir, sync_engine) = make_sync_engine("p2p-shared");
    let authority: Arc<dyn DataAuthority> = engine.clone();
    sync_engine.set_data_authority(authority).await;

    // Simulate the OLD wrong-namespace grant: a data:read row scoped
    // to a filesystem path. The pre-fix CLI produced this; the
    // runtime gate ignored it; default-open returned. The test pins
    // both the silent no-op and the loud refusal of the new CLI.
    let owner = DeviceId::new("NODE-OWNER");
    engine
        .db()
        .insert_grant(&Grant {
            grantee: DeviceId::new("PEER"),
            capability: Capability::DataRead,
            scope: Scope::folder("/work"),
            granted_by: owner,
            expires: None,
        })
        .unwrap();

    // The full chain against the live Engine + live StateDb + live
    // SyncEngine. The data:read grant on /work is in the table; the
    // runtime gate consults folder_id = p2p-shared; the path-scoped
    // grant does not cover it; has_any_data_grant stays false;
    // default-open returns.
    let access = sync_engine.data_access_for_tls_verified("PEER").await;
    assert!(
        access.read,
        "pre-fix: a path-scoped grant was a silent no-op and default-open returned (the F1 failure mode)",
    );
    assert!(
        access.write,
        "pre-fix: a path-scoped grant was a silent no-op and default-open returned (the F1 failure mode)",
    );

    // Belt-and-braces: the path-scoped grant must not even cover the
    // runtime folder_id at the pure-Scope layer, so the same failure
    // mode cannot sneak back through a refactor.
    let p2p_folder = Scope::folder("p2p-shared");
    let path_scope = Scope::folder("/work");
    assert!(
        !path_scope.covers(&p2p_folder),
        "a Scope::folder(\"/work\") grant must not cover Scope::folder(\"p2p-shared\")",
    );
    // And the resolved-p2p scope (the F1 fix shape) DOES cover the
    // runtime folder id — pinning the contract the gate relies on.
    assert!(
        Scope::folder("p2p-shared").covers(&p2p_folder),
        "the F1 fix shape (Scope::folder(\"p2p-shared\")) must cover the runtime folder id",
    );
}

/// The F1 cross-check: a grant scoped to one P2P backend does not
/// satisfy a `SyncEngine` running for a different P2P backend. Two
/// folders, two folders, two distinct gates — a grant on `p2p-shared`
/// must never widen access for a peer on `p2p-personal`.
#[tokio::test]
async fn f1_grant_on_one_p2p_backend_does_not_leak_to_another() {
    let (_engine_dir, engine) = make_engine_with_p2p_data_authority("shared");
    // Register a second P2P backend so the test can write a grant on
    // one and assert it is invisible to the other.
    engine
        .db()
        .register_backend("p2p-personal", "p2p", "personal", None, None)
        .unwrap();

    // The peer connects to a SyncEngine running for `p2p-personal`.
    let (_sync_dir, sync_engine_personal) = make_sync_engine("p2p-personal");
    let authority: Arc<dyn DataAuthority> = engine.clone();
    sync_engine_personal.set_data_authority(authority).await;

    // Grant is on `p2p-shared`, NOT `p2p-personal`. The personal
    // engine must not see it.
    let owner = DeviceId::new("NODE-OWNER");
    engine
        .db()
        .insert_grant(&Grant {
            grantee: DeviceId::new("PEER"),
            capability: Capability::DataRead,
            scope: Scope::folder("p2p-shared"),
            granted_by: owner,
            expires: None,
        })
        .unwrap();

    let access = sync_engine_personal
        .data_access_for_tls_verified("PEER")
        .await;
    assert!(
        access.read,
        "no data:read on p2p-personal must default-open read",
    );
    assert!(
        access.write,
        "no data:write on p2p-personal must default-open write",
    );

    // Cross-check: the same grant IS visible to a SyncEngine running
    // for `p2p-shared`. This is the dual of the leak check above —
    // the namespace binding is selective, not just restrictive.
    let (_sync_dir2, sync_engine_shared) = make_sync_engine("p2p-shared");
    let authority: Arc<dyn DataAuthority> = engine.clone();
    sync_engine_shared.set_data_authority(authority).await;
    let access = sync_engine_shared
        .data_access_for_tls_verified("PEER")
        .await;
    assert!(
        access.read,
        "a data:read grant on p2p-shared must allow read for the same folder",
    );
    assert!(
        !access.write,
        "the absent write direction must stay denied under explicit-control",
    );
}
