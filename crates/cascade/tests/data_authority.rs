#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::string_slice
)]
//! Integration tests for the `DataAuthority` trait implementation on `Engine`
//! (`crates/engine/src/engine/data_authority.rs`).
//!
//! These exercise `Engine::data_access` and `Engine::quarantine_received`
//! directly — the data-plane access gate the BEP sync path consults — rather
//! than going through `SyncEngine` (which the `backend-p2p` F2 test already
//! covers). The gate resolves a peer's directional read/write access from the
//! on-node data grants, the token revocation list, and any signed data-verb
//! token the peer presented, and records the F2 explicit-control bit on a
//! successful token verify.
//!
//! Each test builds a real `Engine` with `p2p` enabled so it has a device
//! identity to verify tokens against, and signs tokens with that same identity
//! so they verify. State is isolated per test via a `tempfile` `state.db`, and
//! all timestamp-dependent assertions use fixed `chrono::Utc` dates so the
//! results are deterministic.

use std::path::PathBuf;
use std::sync::Arc;

use cascade_engine::backend::NullBackend;
use cascade_engine::engine::{Engine, EngineConfig};
use cascade_engine::manage::token::CapabilityToken;
use cascade_engine::manage::{Capability, DataAccess, DataAuthority, DeviceId, Grant, Scope};
use cascade_p2p::identity::DeviceIdentity;
use chrono::{DateTime, TimeZone, Utc};

/// The BEP folder id the data-plane gate keys on throughout these tests. A
/// real folder scope (never node-wide) so the F4 node-wide filter does not
/// strip grants under test.
const FOLDER: &str = "p2p-shared";

/// A fixed reference instant well inside the validity window of every token
/// these tests issue. Using a fixed `now` keeps grant- and token-expiry
/// comparisons deterministic.
fn at(year: i32, month: u32, day: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, 0, 0, 0)
        .single()
        .expect("valid fixed date")
}

/// The "now" every test evaluates the gate at unless it is deliberately
/// probing expiry. Chosen to sit between the tokens' issue and expiry.
fn now() -> DateTime<Utc> {
    at(2026, 1, 1)
}

/// Build a real `Engine` with `p2p` enabled, returning the engine and the
/// device identity whose private key signs tokens that verify against it.
///
/// A known `DeviceIdentity` is generated and persisted into the engine's p2p
/// data dir *before* construction, so the engine loads that identity rather
/// than generating its own. The returned identity therefore holds the same
/// key the engine's `manage_node_device_id` resolves to, so tokens signed by
/// it verify. The temp dir is returned so it outlives the engine (dropping it
/// would delete `state.db` and the identity PEMs).
fn make_engine() -> (tempfile::TempDir, Arc<Engine>, DeviceIdentity) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");
    let p2p_dir = dir.path().join("p2p");
    let identity_dir = p2p_dir.join("p2p");
    std::fs::create_dir_all(&identity_dir).unwrap();

    let identity = DeviceIdentity::generate().unwrap();
    identity.save(&identity_dir.join("identity")).unwrap();

    let engine = Arc::new(
        Engine::new(EngineConfig {
            db_path,
            mount_point: PathBuf::from("/tmp/data-authority-mount"),
            backends: vec![Arc::new(NullBackend::new("p2p-only"))],
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
    (dir, engine, identity)
}

/// A fresh peer device id, distinct from the node's own identity.
fn fresh_peer() -> DeviceId {
    DeviceId::new(DeviceIdentity::generate().unwrap().device_id)
}

/// Issue a node-signed token for `bearer` over `FOLDER` with `capability`,
/// expiring at `expires`, and return it with its JSON form. `issuer` must be
/// the engine's own identity for the token to verify.
fn issue_token_json(
    issuer: &DeviceIdentity,
    bearer: &DeviceId,
    token_id: &str,
    capability: Capability,
    expires: DateTime<Utc>,
) -> (CapabilityToken, String) {
    let token = CapabilityToken::issue(
        token_id,
        issuer,
        bearer,
        capability,
        Scope::folder(FOLDER),
        expires,
    )
    .unwrap();
    let json = serde_json::to_string(&token).unwrap();
    (token, json)
}

/// Insert an on-node data grant for `peer` over `FOLDER`.
fn insert_data_grant(
    engine: &Engine,
    peer: &DeviceId,
    capability: Capability,
    expires: Option<DateTime<Utc>>,
) {
    let grant = Grant {
        grantee: peer.clone(),
        capability,
        scope: Scope::folder(FOLDER),
        granted_by: DeviceId::new("OWNER"),
        expires,
    };
    engine.db().insert_grant(&grant).unwrap();
}

// ── Default-open posture ──

/// A peer with no grant of any kind defaults to full bidirectional access:
/// the trusted-peer default, preserving the pre-feature behaviour.
#[tokio::test]
async fn no_grants_defaults_to_full_bidirectional_access() {
    let (_dir, engine, _issuer) = make_engine();
    let peer = fresh_peer();

    let access = engine
        .data_access(&peer, FOLDER, None, now())
        .await
        .unwrap();

    assert_eq!(
        access,
        DataAccess {
            read: true,
            write: true
        },
        "a peer with no grant and no token keeps full bidirectional access (default-open)",
    );
}

// ── Verified token grants access ──

/// A verified data token grants the peer the direction it carries. With no
/// on-node grant, `data_access` is default-open, so the token narrows rather
/// than widens — but the success path is that the verified verb is honoured.
#[tokio::test]
async fn verified_data_read_token_grants_read_denies_write() {
    let (_dir, engine, issuer) = make_engine();
    let peer = fresh_peer();
    let (token, json) = issue_token_json(
        &issuer,
        &peer,
        "tok-read-only",
        Capability::DataRead,
        at(2030, 1, 1),
    );
    engine.db().insert_token(&token, now()).unwrap();

    let access = engine
        .data_access(&peer, FOLDER, Some(&json), now())
        .await
        .unwrap();

    assert!(
        access.read,
        "a verified data:read token must grant the read direction",
    );
    assert!(
        !access.write,
        "a data:read token must NOT grant write — the absent direction is denied once the peer \
         is pinned into explicit-control by the verify",
    );
}

/// A `data:write` token grants write but denies read — the mirror of the
/// read-only case, confirming the gate keys on the token's actual verb.
#[tokio::test]
async fn verified_data_write_token_grants_write_denies_read() {
    let (_dir, engine, issuer) = make_engine();
    let peer = fresh_peer();
    let (token, json) = issue_token_json(
        &issuer,
        &peer,
        "tok-write-only",
        Capability::DataWrite,
        at(2030, 1, 1),
    );
    engine.db().insert_token(&token, now()).unwrap();

    let access = engine
        .data_access(&peer, FOLDER, Some(&json), now())
        .await
        .unwrap();

    assert!(
        access.write,
        "a verified data:write token must grant the write direction",
    );
    assert!(
        !access.read,
        "a data:write token must NOT grant read — the absent direction is denied",
    );
}

/// A verified token records the F2 explicit-control bit, pinning the peer into
/// directional control. The bit reflects the verb the token carried.
#[tokio::test]
async fn verified_token_records_explicit_control_state() {
    let (_dir, engine, issuer) = make_engine();
    let peer = fresh_peer();
    let (token, json) = issue_token_json(
        &issuer,
        &peer,
        "tok-records-bit",
        Capability::DataRead,
        at(2030, 1, 1),
    );
    engine.db().insert_token(&token, now()).unwrap();

    // Pre-condition: no explicit-control rows before any verify.
    assert!(
        engine.db().list_data_explicit_control().unwrap().is_empty(),
        "no explicit-control rows should exist before the first verify",
    );

    let _ = engine
        .data_access(&peer, FOLDER, Some(&json), now())
        .await
        .unwrap();

    let rows = engine.db().list_data_explicit_control().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "exactly one explicit-control row after the first successful verify",
    );
    assert_eq!(rows[0].peer_device, peer.as_str());
    assert_eq!(rows[0].folder_id, FOLDER);
    assert!(
        rows[0].data_read,
        "the bit must record data_read = true (the verified token's verb)",
    );
    assert!(
        !rows[0].data_write,
        "the bit must record data_write = false (the absent direction)",
    );
}

// ── Expiry and revocation deny ──

/// An expired on-node data grant denies its direction, despite no
/// explicit-control bit ever being set. The grant's presence still opts the
/// peer into explicit directional control, so the *other* direction is denied
/// too — a lapsed read grant yields no access at all.
#[tokio::test]
async fn expired_on_node_grant_denies_access() {
    let (_dir, engine, _issuer) = make_engine();
    let peer = fresh_peer();
    // A read grant that expired before `now`.
    insert_data_grant(&engine, &peer, Capability::DataRead, Some(at(2025, 1, 1)));

    let access = engine
        .data_access(&peer, FOLDER, None, now())
        .await
        .unwrap();

    assert!(
        !access.read,
        "an expired data:read grant must not allow read",
    );
    assert!(
        !access.write,
        "the lapsed read grant still opts the peer into explicit control, so the absent write \
         direction is denied rather than defaulting open",
    );
}

/// A revoked data-verb token is ignored by the gate: it confers nothing, so
/// the peer falls back to the no-token decision (default-open here). This is
/// the deny path for the token verification's revocation check.
#[tokio::test]
async fn revoked_token_confers_nothing() {
    let (_dir, engine, issuer) = make_engine();
    let peer = fresh_peer();
    let (token, json) = issue_token_json(
        &issuer,
        &peer,
        "tok-to-revoke",
        Capability::DataWrite,
        at(2030, 1, 1),
    );
    engine.db().insert_token(&token, now()).unwrap();
    // Revoke before the FIRST presentation, so no explicit-control bit is ever
    // set: the verify path must reject the revoked token outright.
    assert!(
        engine.db().revoke_token("tok-to-revoke", now()).unwrap(),
        "the token id must be newly revoked",
    );

    let access = engine
        .data_access(&peer, FOLDER, Some(&json), now())
        .await
        .unwrap();

    assert_eq!(
        access,
        DataAccess {
            read: true,
            write: true
        },
        "a revoked token confers nothing and sets no bit, so the peer keeps the default-open \
         decision — the revoked write grant did not narrow anything",
    );
    assert!(
        engine.db().list_data_explicit_control().unwrap().is_empty(),
        "a token that fails verification (revoked) must not record an explicit-control bit",
    );
}

/// A token presented at a `now` past its expiry verifies as expired and is
/// ignored: it confers nothing and sets no bit.
#[tokio::test]
async fn expired_token_confers_nothing() {
    let (_dir, engine, issuer) = make_engine();
    let peer = fresh_peer();
    // Token expires in early 2026; present it well after that.
    let (token, json) = issue_token_json(
        &issuer,
        &peer,
        "tok-short-lived",
        Capability::DataRead,
        at(2026, 2, 1),
    );
    engine.db().insert_token(&token, at(2026, 1, 1)).unwrap();

    let past_expiry = at(2026, 3, 1);
    let access = engine
        .data_access(&peer, FOLDER, Some(&json), past_expiry)
        .await
        .unwrap();

    assert_eq!(
        access,
        DataAccess {
            read: true,
            write: true
        },
        "a token presented after its expiry must verify as expired and confer nothing",
    );
    assert!(
        engine.db().list_data_explicit_control().unwrap().is_empty(),
        "an expired token must not record an explicit-control bit",
    );
}

// ── F2 invariant: explicit-control survives token loss ──

/// The F2 invariant via `Engine::data_access` directly: once a token verifies
/// and sets the explicit-control bit, revoking the token does not widen the
/// denied direction back to default-open. The bit is consulted on every call.
#[tokio::test]
async fn explicit_control_bit_survives_token_revocation() {
    let (_dir, engine, issuer) = make_engine();
    let peer = fresh_peer();
    let (token, json) = issue_token_json(
        &issuer,
        &peer,
        "tok-f2-survive",
        Capability::DataRead,
        at(2030, 1, 1),
    );
    engine.db().insert_token(&token, now()).unwrap();

    // First call: the token verifies, setting the bit (read-only).
    let first = engine
        .data_access(&peer, FOLDER, Some(&json), now())
        .await
        .unwrap();
    assert!(
        first.read && !first.write,
        "first verify narrows to read-only",
    );

    // Revoke the token; the peer presents the same (now-revoked) JSON.
    assert!(
        engine.db().revoke_token("tok-f2-survive", now()).unwrap(),
        "the token id must be newly revoked",
    );

    let after = engine
        .data_access(&peer, FOLDER, Some(&json), now())
        .await
        .unwrap();
    assert_eq!(
        after,
        DataAccess {
            read: true,
            write: false
        },
        "F2 invariant: the explicit-control bit set on the first verify keeps the absent write \
         direction denied even after the token is revoked — it does NOT widen to default-open",
    );
}

/// The same F2 invariant under expiry rather than revocation: after the token
/// lapses, the denied direction stays denied because the bit was set on the
/// earlier successful verify and is consulted on every call.
#[tokio::test]
async fn explicit_control_bit_survives_token_expiry() {
    let (_dir, engine, issuer) = make_engine();
    let peer = fresh_peer();
    // Issue a token that is valid at `verify_at` but expired at `lapsed_at`.
    let verify_at = at(2026, 1, 1);
    let lapsed_at = at(2026, 6, 1);
    let (token, json) = issue_token_json(
        &issuer,
        &peer,
        "tok-f2-expiry",
        Capability::DataWrite,
        at(2026, 3, 1),
    );
    engine.db().insert_token(&token, verify_at).unwrap();

    // First call while valid: bit is set (write-only).
    let first = engine
        .data_access(&peer, FOLDER, Some(&json), verify_at)
        .await
        .unwrap();
    assert!(
        first.write && !first.read,
        "first verify narrows to write-only",
    );

    // Second call after expiry: token no longer verifies, but the bit holds.
    let after = engine
        .data_access(&peer, FOLDER, Some(&json), lapsed_at)
        .await
        .unwrap();
    assert_eq!(
        after,
        DataAccess {
            read: false,
            write: true
        },
        "F2 invariant: the absent read direction stays denied after the token expires; the bit \
         set on the earlier verify is still consulted",
    );
}

// ── On-node grant unioning ──

/// Two on-node data grants for the same peer and folder — one read, one write
/// — union into a bidirectional decision. Either alone would deny the other
/// direction (explicit control); both present opens both.
#[tokio::test]
async fn two_directional_grants_union_to_bidirectional() {
    let (_dir, engine, _issuer) = make_engine();
    let peer = fresh_peer();
    insert_data_grant(&engine, &peer, Capability::DataRead, None);
    insert_data_grant(&engine, &peer, Capability::DataWrite, None);

    let access = engine
        .data_access(&peer, FOLDER, None, now())
        .await
        .unwrap();

    assert_eq!(
        access,
        DataAccess {
            read: true,
            write: true
        },
        "a read grant and a write grant for the same peer/folder union into full access",
    );
}

/// A single on-node read grant denies write: the presence of any data grant
/// opts the peer into explicit directional control, narrowing the
/// trusted-peer default.
#[tokio::test]
async fn single_read_grant_denies_write() {
    let (_dir, engine, _issuer) = make_engine();
    let peer = fresh_peer();
    insert_data_grant(&engine, &peer, Capability::DataRead, None);

    let access = engine
        .data_access(&peer, FOLDER, None, now())
        .await
        .unwrap();

    assert_eq!(
        access,
        DataAccess {
            read: true,
            write: false
        },
        "one read grant grants read and denies write (explicit directional control)",
    );
}

// ── Token verb / scope filters ──

/// A token carrying a non-data verb (e.g. `status:read`) is ignored by the
/// data-plane gate: it confers nothing and sets no explicit-control bit.
#[tokio::test]
async fn non_data_verb_token_is_ignored() {
    let (_dir, engine, issuer) = make_engine();
    let peer = fresh_peer();
    // A `status:read` token is well-formed and verifies, but is not a data
    // verb, so the data-plane gate ignores it.
    let (token, json) = issue_token_json(
        &issuer,
        &peer,
        "tok-status",
        Capability::StatusRead,
        at(2030, 1, 1),
    );
    engine.db().insert_token(&token, now()).unwrap();

    let access = engine
        .data_access(&peer, FOLDER, Some(&json), now())
        .await
        .unwrap();

    assert_eq!(
        access,
        DataAccess {
            read: true,
            write: true
        },
        "a non-data-verb token confers nothing to the data plane — the peer stays default-open",
    );
    assert!(
        engine.db().list_data_explicit_control().unwrap().is_empty(),
        "a non-data-verb token must not record an explicit-control bit",
    );
}

/// A token whose scope is node-wide is filtered out before folding (F4 defence
/// in depth): the verify path refuses a node-wide data token, so it confers
/// nothing and the peer stays default-open.
#[tokio::test]
async fn node_wide_scope_token_is_filtered() {
    let (_dir, engine, issuer) = make_engine();
    let peer = fresh_peer();
    // A data:read token scoped to the node root ("/") — node-wide in
    // everything but name. The verify path's F4 filter must reject it.
    let token = CapabilityToken::issue(
        "tok-node-wide",
        &issuer,
        &peer,
        Capability::DataRead,
        Scope::folder("/"),
        at(2030, 1, 1),
    )
    .unwrap();
    let json = serde_json::to_string(&token).unwrap();
    engine.db().insert_token(&token, now()).unwrap();

    let access = engine
        .data_access(&peer, FOLDER, Some(&json), now())
        .await
        .unwrap();

    assert_eq!(
        access,
        DataAccess {
            read: true,
            write: true
        },
        "a node-wide data token is filtered out (F4) and confers nothing",
    );
    assert!(
        engine.db().list_data_explicit_control().unwrap().is_empty(),
        "a filtered node-wide token must not record an explicit-control bit",
    );
}

/// A token signed by a *stranger* identity (not the engine's own) fails the
/// chain-root issuer check and confers nothing — the gate cannot be widened by
/// a token the node never issued.
#[tokio::test]
async fn token_signed_by_stranger_is_rejected() {
    let (_dir, engine, _issuer) = make_engine();
    let peer = fresh_peer();
    // Sign with an unrelated identity — the engine never issued this.
    let stranger = DeviceIdentity::generate().unwrap();
    let (token, json) = issue_token_json(
        &stranger,
        &peer,
        "tok-forged",
        Capability::DataRead,
        at(2030, 1, 1),
    );
    engine.db().insert_token(&token, now()).unwrap();

    let access = engine
        .data_access(&peer, FOLDER, Some(&json), now())
        .await
        .unwrap();

    assert_eq!(
        access,
        DataAccess {
            read: true,
            write: true
        },
        "a token whose chain root is not signed by this node confers nothing",
    );
    assert!(
        engine.db().list_data_explicit_control().unwrap().is_empty(),
        "a token that fails the issuer check must not record an explicit-control bit",
    );
}

/// A token whose bearer is a *different* device than the presenting peer fails
/// the bearer-binding check: presenting another peer's token confers nothing.
#[tokio::test]
async fn token_bearer_mismatch_is_rejected() {
    let (_dir, engine, issuer) = make_engine();
    let bearer = fresh_peer();
    let presenter = fresh_peer();
    let (token, json) = issue_token_json(
        &issuer,
        &bearer,
        "tok-wrong-bearer",
        Capability::DataRead,
        at(2030, 1, 1),
    );
    engine.db().insert_token(&token, now()).unwrap();

    // The token names `bearer`, but `presenter` presents it.
    let access = engine
        .data_access(&presenter, FOLDER, Some(&json), now())
        .await
        .unwrap();

    assert_eq!(
        access,
        DataAccess {
            read: true,
            write: true
        },
        "a token presented by a device other than its bearer confers nothing",
    );
}

// ── Delegation ──

/// A correctly-narrowed delegated token verifies and grants only the narrowed
/// direction. The chain roots in the engine's identity (issuer → delegator →
/// peer), and the gate honours the leaf's verb.
#[tokio::test]
async fn narrowed_delegated_token_grants_only_its_direction() {
    let (_dir, engine, issuer) = make_engine();

    // The intermediate delegator holds a data:read parent (so the child can
    // only ever be data:read), then mints a data:read child for the peer.
    let delegator = DeviceIdentity::generate().unwrap();
    let delegator_id = DeviceId::new(delegator.device_id.clone());
    let peer = fresh_peer();

    let expiry = at(2030, 1, 1);
    let parent = CapabilityToken::issue(
        "tok-parent",
        &issuer,
        &delegator_id,
        Capability::DataRead,
        Scope::folder(FOLDER),
        expiry,
    )
    .unwrap();

    // The delegator narrows: same verb, same scope, same expiry — a valid
    // (non-widening) child for the final peer.
    let child = parent
        .delegate(
            "tok-child",
            &delegator,
            &peer,
            Capability::DataRead,
            Scope::folder(FOLDER),
            expiry,
        )
        .unwrap();
    let child_json = serde_json::to_string(&child).unwrap();
    engine.db().insert_token(&child, now()).unwrap();

    let access = engine
        .data_access(&peer, FOLDER, Some(&child_json), now())
        .await
        .unwrap();

    assert!(
        access.read,
        "the narrowed delegated data:read token must grant read",
    );
    assert!(
        !access.write,
        "the delegated token carries only data:read, so write stays denied",
    );
}

/// A delegated token cannot widen authority: `delegate` refuses to mint a
/// child whose capability exceeds the parent's. This is the no-escalation
/// invariant the gate relies on — a widening token can never even be
/// constructed, let alone fold into a decision.
#[tokio::test]
async fn delegation_cannot_widen_capability() {
    let (_dir, _engine, issuer) = make_engine();
    let delegator = DeviceIdentity::generate().unwrap();
    let delegator_id = DeviceId::new(delegator.device_id.clone());
    let peer = fresh_peer();

    let expiry = at(2030, 1, 1);
    // Parent confers only data:read.
    let parent = CapabilityToken::issue(
        "tok-parent-read",
        &issuer,
        &delegator_id,
        Capability::DataRead,
        Scope::folder(FOLDER),
        expiry,
    )
    .unwrap();

    // Attempt to delegate data:write — a verb the parent never held.
    let widen = parent.delegate(
        "tok-child-write",
        &delegator,
        &peer,
        Capability::DataWrite,
        Scope::folder(FOLDER),
        expiry,
    );

    assert!(
        widen.is_err(),
        "delegating a capability the parent never held must be refused at mint time",
    );
}

/// A delegated token cannot widen *scope*: `delegate` refuses a child whose
/// folder scope is broader than the parent's subtree.
#[tokio::test]
async fn delegation_cannot_widen_scope() {
    let (_dir, _engine, issuer) = make_engine();
    let delegator = DeviceIdentity::generate().unwrap();
    let delegator_id = DeviceId::new(delegator.device_id.clone());
    let peer = fresh_peer();

    let expiry = at(2030, 1, 1);
    // Parent is scoped to a sub-folder.
    let parent = CapabilityToken::issue(
        "tok-parent-subfolder",
        &issuer,
        &delegator_id,
        Capability::DataRead,
        Scope::folder("p2p-shared/reports"),
        expiry,
    )
    .unwrap();

    // Attempt to delegate a broader scope (the parent's ancestor folder).
    let widen = parent.delegate(
        "tok-child-broad",
        &delegator,
        &peer,
        Capability::DataRead,
        Scope::folder("p2p-shared"),
        expiry,
    );

    assert!(
        widen.is_err(),
        "delegating a broader scope than the parent's subtree must be refused at mint time",
    );
}

// ── Quarantine ──

/// `quarantine_received` upserts a rejected peer write into the receive
/// quarantine with the exact `file_json` the peer sent, keyed by
/// `(folder, peer, path)`.
#[tokio::test]
async fn quarantine_received_records_exact_file_json() {
    let (_dir, engine, _issuer) = make_engine();
    let peer = fresh_peer();
    let path = "/work/notes.md";
    let file_json = r#"{"name":"notes.md","size":42,"modified":"2026-01-01T00:00:00Z"}"#;
    let observed_at = now();

    engine
        .quarantine_received(&peer, FOLDER, path, file_json, observed_at)
        .await
        .unwrap();

    let rows = engine.db().list_quarantine(FOLDER, peer.as_str()).unwrap();
    assert_eq!(
        rows.len(),
        1,
        "exactly one quarantined row after one upsert"
    );
    assert_eq!(rows[0].folder_id, FOLDER);
    assert_eq!(rows[0].peer_device, peer.as_str());
    assert_eq!(rows[0].path, path);
    assert_eq!(
        rows[0].file_json, file_json,
        "the quarantined row must carry the exact file_json the peer sent, unaltered",
    );
    assert_eq!(rows[0].observed_at, observed_at);
}

/// A newer proposal for the same `(folder, peer, path)` replaces the older
/// one, so the quarantine stays bounded by distinct paths rather than growing
/// per re-send.
#[tokio::test]
async fn quarantine_received_upsert_replaces_same_path() {
    let (_dir, engine, _issuer) = make_engine();
    let peer = fresh_peer();
    let path = "/work/report.txt";

    engine
        .quarantine_received(&peer, FOLDER, path, r#"{"v":1}"#, at(2026, 1, 1))
        .await
        .unwrap();
    engine
        .quarantine_received(&peer, FOLDER, path, r#"{"v":2}"#, at(2026, 1, 2))
        .await
        .unwrap();

    let rows = engine.db().list_quarantine(FOLDER, peer.as_str()).unwrap();
    assert_eq!(
        rows.len(),
        1,
        "a second proposal for the same path replaces the first rather than adding a row",
    );
    assert_eq!(
        rows[0].file_json, r#"{"v":2}"#,
        "the later proposal's file_json must win the upsert",
    );
    assert_eq!(
        rows[0].observed_at,
        at(2026, 1, 2),
        "the later proposal's observed_at must win the upsert",
    );
}
