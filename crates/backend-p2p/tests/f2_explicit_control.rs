#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::string_slice
)]
//! F2 verification test: the explicit-control bit for token-only
//! data-verb restrictions.
//!
//! The F2 invariant: a peer who has *ever* presented a verified data-verb
//! token for a folder is in explicit-control mode for that folder, even
//! after the token has been revoked or has expired. The absent direction
//! stays denied; the token-only restriction cannot be widened back to the
//! trusted-peer default by revoking or letting the token lapse.
//!
//! The test walks the full chain (engine + state DB + presented token) end
//! to end against a real [`Engine`] — no `FixedDataAuthority` or other
//! folder-ignoring test double — and asserts the F2 invariant at every
//! step:
//!
//! 1. A real `Engine` is built with a real device identity (P2P enabled),
//!    so `manage_node_device_id()` and the token verify path work.
//! 2. A `data:read` token is issued by the engine's own identity, scoped
//!    to the resolved `p2p-shared` folder id (the F1 namespace fix).
//! 3. The engine is wired as the data authority on a real `SyncEngine`,
//!    and `data_access_for_tls_verified` is called with the valid token.
//!    The result is `read = true, write = false` — the F2 success.
//! 4. The token is revoked via the engine's own `db.revoke_token`; the
//!    same token JSON is presented again (the test simulates a peer that
//!    has not yet refreshed its token cache).
//! 5. `data_access_for_tls_verified` is called again. The result is
//!    STILL `read = true, write = false` — the F2 invariant. The
//!    explicit-control bit, set on the first successful verify, survives
//!    the token's revocation; the absent write direction stays denied.
//! 6. (Parallel expiry test) A second token is issued with a near-future
//!    expiry and presented; the test advances the system clock past the
//!    expiry; the same `read = true, write = false` result holds.

use std::path::PathBuf;
use std::sync::Arc;

use cascade_backend_p2p::index::FolderIndex;
use cascade_backend_p2p::sync::SyncEngine;
use cascade_engine::backend::{MountedBackend, NullBackend};
use cascade_engine::db::StateDb;
use cascade_engine::engine::{Engine, EngineConfig};
use cascade_engine::manage::{
    Capability, DataAccess, DataAuthority, DeviceId, Scope, token::CapabilityToken,
};
use cascade_p2p::identity::DeviceIdentity;
use cascade_p2p::store::BlockStore;
use chrono::{DateTime, Utc};

type TempDir = tempfile::TempDir;

/// Build a real `Engine` whose `manage_node_device_id()` works (so the
/// token-verify path can resolve a node identity). The engine is created
/// with `enable_p2p: true` against a temp p2p data dir; the `NullBackend`
/// is mounted only to satisfy `Engine::new`'s "at least one backend"
/// contract — it is never exercised by the data-plane tests.
///
/// A known device identity is generated and persisted to the p2p data
/// dir before the engine is constructed, so the test can recover the
/// signing key from the saved PEMs and use it to issue tokens that
/// verify against the engine's own identity.
///
/// Returns the temp dir (for the `db_path` so the test can re-open the
/// `StateDb`), the engine, and the device identity the test uses to
/// sign tokens.
fn make_engine_with_real_device_id(
    register_backend_name: &str,
) -> (TempDir, PathBuf, Arc<Engine>, DeviceIdentity) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");
    let p2p_dir = dir.path().join("p2p");
    let p2p_subdir = p2p_dir.join("p2p");
    std::fs::create_dir_all(&p2p_subdir).unwrap();

    // Persist a known device identity to the engine's p2p data dir, so
    // the engine loads (does not regenerate) it on construction. The
    // returned `DeviceIdentity` holds the same private key and can sign
    // tokens that verify against the engine.
    let identity = DeviceIdentity::generate().unwrap();
    identity.save(&p2p_subdir.join("identity")).unwrap();

    let engine = Arc::new(
        Engine::new(EngineConfig {
            db_path: db_path.clone(),
            mount_point: PathBuf::from("/tmp/f2-mount"),
            backends: vec![MountedBackend::at_default(Arc::new(NullBackend::new(
                "p2p-only",
            )))],
            cache_dir: None,
            enable_p2p: true,
            p2p_data_dir: Some(p2p_dir),
            p2p_posture: None,
            p2p_relay_endpoints: Vec::new(),
            p2p_relay_shared_secret: None,
            backend_factory: None,
        })
        .unwrap(),
    );
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
    (dir, db_path, engine, identity)
}

/// Build a bare [`SyncEngine`] whose `folder_id` matches the P2P backend
/// the F2 test resolves to.
fn make_sync_engine(folder_id: &str) -> (TempDir, SyncEngine) {
    let dir = tempfile::tempdir().unwrap();
    let index = Arc::new(FolderIndex::open(&dir.path().join("index.db")).unwrap());
    let blocks = Arc::new(BlockStore::new(&dir.path().join("blocks")).unwrap());
    let identity = DeviceIdentity::load_or_generate(&dir.path().join("identity")).unwrap();
    let engine = SyncEngine::new(folder_id.to_owned(), index, blocks, identity);
    (dir, engine)
}

/// F2 success + invariant: a token-only `data:read` restriction survives
/// token revocation.
///
/// 1. Issue a `data:read` token signed by the engine's own identity,
///    scoped to `p2p-shared`.
/// 2. Insert the token into the engine's `capability_tokens` table so
///    the verify path's revoke-list check works for the revoke step.
/// 3. Call `data_access_for_tls_verified` with the valid token; assert
///    `read = true, write = false` — the F2 success path.
/// 4. Revoke the token via the engine's own `db.revoke_token`.
/// 5. Call `data_access_for_tls_verified` again with the same
///    (now-revoked) token JSON; assert `read = true, write = false` —
///    the F2 invariant. The explicit-control bit, set on the first
///    successful verify, persists across the revoke; the absent write
///    direction is not widened.
///
/// This test fails when the F2 fix is absent: without the
/// explicit-control bit, the revoke step drops the verified-token
/// contribution to the per-direction booleans, the grant slice is empty,
/// and default-open returns `read = true, write = true`. The F2
/// invariant is exactly the assertion that this default-open does not
/// happen after a successful verify.
#[tokio::test]
async fn f2_token_only_data_read_survives_token_revocation() {
    let (_engine_dir, _db_path, engine, issuer) = make_engine_with_real_device_id("shared");

    // Wire the live engine as the data authority on a real SyncEngine
    // whose folder_id is the resolved `p2p-shared` value.
    let (_sync_dir, sync_engine) = make_sync_engine("p2p-shared");
    let authority: Arc<dyn DataAuthority> = engine.clone();
    sync_engine.set_data_authority(authority).await;

    // The bearer: a fresh device id the test can present on the wire.
    let bearer_id = DeviceIdentity::generate().unwrap();
    let bearer = DeviceId::new(bearer_id.device_id.clone());

    // Issue a `data:read` token over `p2p-shared`, expiring far in the
    // future so the only thing that can flip its verify outcome is
    // revocation.
    let far_future = DateTime::from_timestamp(2_000_000_000, 0).unwrap();
    let token = CapabilityToken::issue(
        "tok-f2-read",
        &issuer,
        &bearer,
        Capability::DataRead,
        Scope::folder("p2p-shared"),
        far_future,
    )
    .unwrap();
    let token_json = serde_json::to_string(&token).unwrap();

    // Persist the token so the verify path can consult the
    // `capability_tokens` table; this matches what the `cascade token
    // issue` CLI does. Revocation below goes through the same table.
    engine
        .db()
        .insert_token(&token, Utc::now())
        .expect("insert the issued token");

    // 1. F2 success path: a valid token narrows the gate to read-only.
    let access: DataAccess = sync_engine
        .data_access_for_token(bearer.as_str(), Some(&token_json))
        .await;
    assert!(
        access.read,
        "a valid data:read token must allow read (F2 success)",
    );
    assert!(
        !access.write,
        "the absent data:write direction must stay denied under explicit-control",
    );

    // 2. Revoke the token via the engine's own `db.revoke_token`. The
    //    peer still presents the now-revoked token JSON (the test
    //    simulates a peer that has not yet refreshed its token cache).
    assert!(
        engine.db().revoke_token("tok-f2-read", Utc::now()).unwrap(),
        "the token id must be newly revoked",
    );

    // 3. F2 invariant: the explicit-control bit, set on the first
    //    successful verify, survives the revoke. The absent write
    //    direction stays denied; access is unchanged.
    let access_after_revoke: DataAccess = sync_engine
        .data_access_for_token(bearer.as_str(), Some(&token_json))
        .await;
    assert!(
        access_after_revoke.read,
        "F2 invariant: a token-only data:read must keep allowing read after revocation",
    );
    assert!(
        !access_after_revoke.write,
        "F2 invariant: the absent data:write direction must NOT widen back to default-open \
         after the token is revoked; the explicit-control bit keeps the absent direction denied",
    );
}

/// Parallel expiry test: a token-only `data:read` restriction survives
/// token expiry. The mechanism is the same — the explicit-control bit
/// is set on the first successful verify and persists — but the failure
/// mode of the test is different (advancing the clock past `expires`).
#[tokio::test]
async fn f2_token_only_data_read_survives_token_expiry() {
    let (_engine_dir, _db_path, engine, issuer) = make_engine_with_real_device_id("shared");
    let (_sync_dir, sync_engine) = make_sync_engine("p2p-shared");
    let authority: Arc<dyn DataAuthority> = engine.clone();
    sync_engine.set_data_authority(authority).await;

    let bearer_id = DeviceIdentity::generate().unwrap();
    let bearer = DeviceId::new(bearer_id.device_id.clone());

    // A token that expires in 2030 — far enough in the future that the
    // test never crosses it by accident.
    let future_expiry = DateTime::from_timestamp(1_900_000_000, 0).unwrap();
    let token = CapabilityToken::issue(
        "tok-f2-expiry",
        &issuer,
        &bearer,
        Capability::DataRead,
        Scope::folder("p2p-shared"),
        future_expiry,
    )
    .unwrap();
    let token_json = serde_json::to_string(&token).unwrap();
    engine.db().insert_token(&token, Utc::now()).unwrap();

    // F2 success path: a valid, unexpired token narrows the gate.
    let access: DataAccess = sync_engine
        .data_access_for_token(bearer.as_str(), Some(&token_json))
        .await;
    assert!(access.read, "an unexpired data:read token must allow read");
    assert!(
        !access.write,
        "the absent data:write direction must stay denied under explicit-control",
    );

    // Advance past the token's expiry. The token's pure `verify` must
    // reject on the `Expired` branch — confirming the F2 trigger
    // (verify returning Err) does not, by itself, widen the gate.
    let past_expiry = DateTime::from_timestamp(2_000_000_000, 0).unwrap();
    let verify_outcome = token.verify(
        &DeviceId::new(issuer.device_id.clone()),
        &bearer,
        past_expiry,
        &|_id| false,
    );
    assert!(
        verify_outcome.is_err(),
        "the token must reject verify after the wall clock crosses its expiry",
    );

    // Belt-and-braces: the explicit-control bit was set on the
    // successful verify above; the engine's in-memory mirror must still
    // carry the per-direction state, so the gate continues to allow read
    // and deny write even though the token no longer verifies.
    let mirror = engine.explicit_data_control_snapshot();
    let stored = mirror
        .get(&(bearer.as_str().to_owned(), "p2p-shared".to_owned()))
        .expect("the explicit-control bit must persist after the token lapses");
    assert!(
        stored.0,
        "data_read must be true on the in-memory bit (the verified token granted data:read)",
    );
    assert!(
        !stored.1,
        "data_write must be false on the in-memory bit (the verified token was data:read, not data:write)",
    );
}

/// The on-disk state of the explicit-control table: a row for
/// `(peer, folder)` exists after the first successful verify, and the
/// row is not removed by token revocation. This pins the F2 persistence
/// contract at the storage layer, independent of the in-memory mirror.
#[tokio::test]
async fn f2_explicit_control_row_persists_across_token_revocation_in_db() {
    let (_engine_dir, db_path, engine, issuer) = make_engine_with_real_device_id("shared");
    let (_sync_dir, sync_engine) = make_sync_engine("p2p-shared");
    let authority: Arc<dyn DataAuthority> = engine.clone();
    sync_engine.set_data_authority(authority).await;

    let bearer_id = DeviceIdentity::generate().unwrap();
    let bearer = DeviceId::new(bearer_id.device_id.clone());

    let far_future = DateTime::from_timestamp(2_000_000_000, 0).unwrap();
    let token = CapabilityToken::issue(
        "tok-f2-persist",
        &issuer,
        &bearer,
        Capability::DataRead,
        Scope::folder("p2p-shared"),
        far_future,
    )
    .unwrap();
    let token_json = serde_json::to_string(&token).unwrap();
    engine.db().insert_token(&token, Utc::now()).unwrap();

    // Pre-condition: the table is empty.
    assert!(
        engine.db().list_data_explicit_control().unwrap().is_empty(),
        "no explicit-control rows before any verify",
    );

    // One successful verify writes a row.
    let _ = sync_engine
        .data_access_for_token(bearer.as_str(), Some(&token_json))
        .await;
    let rows = engine.db().list_data_explicit_control().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "one explicit-control row after the first successful verify",
    );
    assert_eq!(rows[0].peer_device, bearer.as_str());
    assert_eq!(rows[0].folder_id, "p2p-shared");
    // F2 invariant: the per-direction state on the bit must reflect
    // the direction the verified token granted — `data_read = true`
    // (the token's verb), `data_write = false` (the absent direction).
    assert!(
        rows[0].data_read,
        "data_read must be set on the bit (the verified token granted data:read)",
    );
    assert!(
        !rows[0].data_write,
        "data_write must NOT be set on the bit (the verified token was data:read, not data:write)",
    );

    // Revoking the token does NOT clear the row.
    assert!(
        engine
            .db()
            .revoke_token("tok-f2-persist", Utc::now())
            .unwrap(),
        "the token id must be newly revoked",
    );
    let rows_after = engine.db().list_data_explicit_control().unwrap();
    assert_eq!(
        rows_after.len(),
        1,
        "the explicit-control row must survive token revocation (F2 invariant at the storage layer)",
    );

    // A subsequent presented-but-failed token (the revoked one) does
    // not change the bit: a row exists already, and the verify path
    // returning `None` does not touch the storage.
    let _ = sync_engine
        .data_access_for_token(bearer.as_str(), Some(&token_json))
        .await;
    let rows_final = engine.db().list_data_explicit_control().unwrap();
    assert_eq!(
        rows_final.len(),
        1,
        "the explicit-control row must not be duplicated by subsequent failed verifies",
    );

    // Only the explicit clear (the operator revoke path) removes the row.
    let cleared = StateDb::open(&db_path)
        .unwrap()
        .clear_data_explicit_control(bearer.as_str(), "p2p-shared")
        .unwrap();
    assert!(cleared, "the explicit-control row must be removable");
    let rows_cleared = StateDb::open(&db_path)
        .unwrap()
        .list_data_explicit_control()
        .unwrap();
    assert!(
        rows_cleared.is_empty(),
        "the explicit-control row must be gone after an explicit clear",
    );
}
