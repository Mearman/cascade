//! Engine integration tests against the real state database.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::*;
use crate::backend::NullBackend;
use crate::manage::{DeviceId, Grant, Scope};
#[cfg(feature = "p2p")]
use crate::manage::Capability;
#[cfg(feature = "p2p")]
use cascade_p2p::protocol::{ManageCommand, ManageResult, ManageScope as WireScope};

fn make_test_engine() -> Engine {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");
    let config = EngineConfig {
        db_path,
        mount_point: PathBuf::from("/tmp/test-mount"),
        backends: vec![Arc::new(NullBackend::new("test"))],
        cache_dir: None,
        backend_factory: None,
        #[cfg(feature = "p2p")]
        enable_p2p: false,
        #[cfg(feature = "p2p")]
        p2p_data_dir: None,
        #[cfg(feature = "p2p")]
        p2p_posture: None,
        #[cfg(feature = "p2p")]
        p2p_relay_endpoints: Vec::new(),
        #[cfg(feature = "p2p")]
        p2p_relay_shared_secret: None,
    };
    let _ = config;
    Engine::new(config).unwrap()
}

#[tokio::test]
async fn engine_new_with_null_backend() {
    let engine = make_test_engine();

    let status = engine.status();
    // NullBackend's display_name is "P2P Only", registered with type "unknown".
    assert!(status.backends.iter().any(|b| b.contains("P2P Only")));
    assert!(!status.p2p_enabled);
    assert!(status.p2p_device_id.is_none());
}

#[tokio::test]
async fn engine_mount_unmount_backend() {
    let engine = make_test_engine();

    engine.mount_backend(PathBuf::from("Work"), Arc::new(NullBackend::new("work")));

    let tree = engine.vfs().read().unwrap();
    assert_eq!(tree.children().len(), 1);
    drop(tree);

    engine.unmount_backend(Path::new("Work"));

    let tree = engine.vfs().read().unwrap();
    assert!(tree.children().is_empty());
}

#[tokio::test]
async fn engine_pin_unpin_list() {
    let engine = make_test_engine();

    engine.pin("Documents/**", true).unwrap();

    let pins = engine.list_pins().unwrap();
    assert_eq!(pins.len(), 1);
    assert_eq!(pins[0].path_glob, "Documents/**");

    let removed = engine.unpin("Documents/**").unwrap();
    assert!(removed);

    let pins = engine.list_pins().unwrap();
    assert!(pins.is_empty());
}

#[tokio::test]
async fn engine_status_reflects_state() {
    let engine = make_test_engine();

    let status = engine.status();
    assert!(status.running);
    assert_eq!(status.backends.len(), 1);
    assert_eq!(status.cache_stats.online_count, 0);
}

#[tokio::test]
async fn engine_shutdown_signals_cancel() {
    let engine = make_test_engine();
    engine.shutdown();

    let status = engine.status();
    assert!(!status.running);
}

#[tokio::test]
async fn engine_start_and_shutdown() {
    let engine = make_test_engine();
    let handle = engine.start().unwrap();

    // Give the task a moment to start.
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    engine.shutdown();
    handle.cache_handle.abort();
}

#[tokio::test]
async fn engine_new_requires_at_least_one_backend() {
    let dir = tempfile::tempdir().unwrap();
    let result = Engine::new(EngineConfig {
        db_path: dir.path().join("state.db"),
        mount_point: PathBuf::from("/tmp/test-mount"),
        backends: vec![],
        cache_dir: None,
        backend_factory: None,
        #[cfg(feature = "p2p")]
        enable_p2p: false,
        #[cfg(feature = "p2p")]
        p2p_data_dir: None,
        #[cfg(feature = "p2p")]
        p2p_posture: None,
        #[cfg(feature = "p2p")]
        p2p_relay_endpoints: Vec::new(),
        #[cfg(feature = "p2p")]
        p2p_relay_shared_secret: None,
    });

    assert!(result.is_err());
}

#[tokio::test]
async fn engine_with_multiple_backends() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(EngineConfig {
        db_path: dir.path().join("state.db"),
        mount_point: PathBuf::from("/tmp/test-mount"),
        backends: vec![
            Arc::new(NullBackend::new("root")),
            Arc::new(NullBackend::new("work")),
        ],
        cache_dir: None,
        backend_factory: None,
        #[cfg(feature = "p2p")]
        enable_p2p: false,
        #[cfg(feature = "p2p")]
        p2p_data_dir: None,
        #[cfg(feature = "p2p")]
        p2p_posture: None,
        #[cfg(feature = "p2p")]
        p2p_relay_endpoints: Vec::new(),
        #[cfg(feature = "p2p")]
        p2p_relay_shared_secret: None,
    })
    .unwrap();

    let tree = engine.vfs().read().unwrap();
    assert_eq!(tree.children().len(), 1);
    drop(tree);

    let status = engine.status();
    assert_eq!(status.backends.len(), 2);
}

// ── Management-plane dispatch against the real engine + DB ──

use chrono::Utc;

fn manager_id() -> DeviceId {
    DeviceId::new("MANAGER")
}

#[tokio::test]
async fn dispatch_authorised_pin_mutates_state_and_audits() {
    let engine = make_test_engine();
    // Grant the manager pin:write over /work, persisted in the real DB.
    engine
        .db()
        .insert_grant(&Grant {
            grantee: manager_id(),
            capability: Capability::PinWrite,
            scope: Scope::folder("/work"),
            granted_by: DeviceId::new("OWNER"),
            expires: None,
        })
        .unwrap();

    let result = engine
        .dispatch(
            &manager_id(),
            ManageCommand::Pin {
                path_glob: "/work/reports".to_owned(),
                recursive: true,
            },
            WireScope::Folder {
                path: "/work/reports".to_owned(),
            },
            None,
            Utc::now(),
        )
        .await;

    assert!(
        matches!(result, ManageResult::Ok { .. }),
        "authorised pin should succeed, got {result:?}",
    );
    // The side effect ran: the pin rule is now present.
    let pins = engine.list_pins().unwrap();
    assert!(
        pins.iter().any(|p| p.path_glob == "/work/reports"),
        "pin rule must have been recorded",
    );
    // The attempt was audited as allowed.
    let audit = engine.db().list_audit().unwrap();
    assert_eq!(audit.len(), 1);
    let row = audit.first().expect("one audit row");
    assert_eq!(row.entry.outcome, "allowed");
    assert_eq!(row.entry.actor_device, manager_id());
    assert_eq!(row.entry.capability, Capability::PinWrite);
}

#[tokio::test]
async fn dispatch_pin_outside_granted_scope_is_refused_against_real_engine() {
    // Scope-escape regression against the live Engine + real DB: the
    // manager holds pin:write over /work and advertises a wire scope of
    // /work, but the command's path_glob targets /personal. The pin must be
    // refused and no rule may land in the database.
    let engine = make_test_engine();
    engine
        .db()
        .insert_grant(&Grant {
            grantee: manager_id(),
            capability: Capability::PinWrite,
            scope: Scope::folder("/work"),
            granted_by: DeviceId::new("OWNER"),
            expires: None,
        })
        .unwrap();

    let result = engine
        .dispatch(
            &manager_id(),
            ManageCommand::Pin {
                path_glob: "/personal/secret".to_owned(),
                recursive: false,
            },
            WireScope::Folder {
                path: "/work".to_owned(),
            },
            None,
            Utc::now(),
        )
        .await;

    assert!(
        matches!(
            result,
            ManageResult::Err {
                kind: cascade_p2p::protocol::ManageErrorKind::Unauthorised,
                ..
            }
        ),
        "a pin escaping the granted scope must be refused, got {result:?}",
    );
    assert!(
        engine.list_pins().unwrap().is_empty(),
        "no pin rule may be created for a path outside the granted scope",
    );
    let audit = engine.db().list_audit().unwrap();
    assert_eq!(audit.len(), 1, "the denial is still audited");
    assert_eq!(
        audit.first().map(|r| r.entry.outcome.as_str()),
        Some("denied"),
    );
}

#[tokio::test]
async fn dispatch_unauthorised_pin_makes_no_change_and_audits_denial() {
    let engine = make_test_engine();
    // Manager holds only status:read — a pin must be refused.
    engine
        .db()
        .insert_grant(&Grant {
            grantee: manager_id(),
            capability: Capability::StatusRead,
            scope: Scope::Node,
            granted_by: DeviceId::new("OWNER"),
            expires: None,
        })
        .unwrap();

    let result = engine
        .dispatch(
            &manager_id(),
            ManageCommand::Pin {
                path_glob: "/work".to_owned(),
                recursive: false,
            },
            WireScope::Folder {
                path: "/work".to_owned(),
            },
            None,
            Utc::now(),
        )
        .await;

    assert!(
        matches!(
            result,
            ManageResult::Err {
                kind: cascade_p2p::protocol::ManageErrorKind::Unauthorised,
                ..
            }
        ),
        "unauthorised pin must be refused, got {result:?}",
    );
    // No pin rule was created.
    assert!(
        engine.list_pins().unwrap().is_empty(),
        "an unauthorised request must not mutate state",
    );
    // The denial was still audited.
    let audit = engine.db().list_audit().unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(
        audit.first().map(|r| r.entry.outcome.as_str()),
        Some("denied"),
    );
}

#[tokio::test]
async fn dispatch_status_read_is_authorised_by_node_scope() {
    let engine = make_test_engine();
    engine
        .db()
        .insert_grant(&Grant {
            grantee: manager_id(),
            capability: Capability::StatusRead,
            scope: Scope::Node,
            granted_by: DeviceId::new("OWNER"),
            expires: None,
        })
        .unwrap();

    let result = engine
        .dispatch(
            &manager_id(),
            ManageCommand::StatusRead,
            WireScope::Node,
            None,
            Utc::now(),
        )
        .await;
    match result {
        ManageResult::Ok { summary } => {
            assert!(summary.contains("running="), "status summary: {summary}");
        }
        err @ ManageResult::Err { .. } => {
            panic!("status read should be authorised, got {err:?}")
        }
    }
}

// ── Duration / size parsing ──

#[test]
fn parse_duration_secs_units() {
    assert_eq!(operations::parse_duration_secs("30").unwrap(), 30);
    assert_eq!(operations::parse_duration_secs("45s").unwrap(), 45);
    assert_eq!(operations::parse_duration_secs("2m").unwrap(), 120);
    assert_eq!(operations::parse_duration_secs("3h").unwrap(), 3 * 3600);
    assert_eq!(operations::parse_duration_secs("7d").unwrap(), 7 * 86_400);
    assert_eq!(operations::parse_duration_secs("1w").unwrap(), 7 * 86_400);
}

#[test]
fn parse_duration_secs_rejects_bad_input() {
    assert!(operations::parse_duration_secs("").is_err());
    assert!(operations::parse_duration_secs("abc").is_err());
    assert!(operations::parse_duration_secs("10y").is_err());
}

#[test]
fn parse_size_bytes_units() {
    assert_eq!(operations::parse_size_bytes("512").unwrap(), 512);
    assert_eq!(operations::parse_size_bytes("512B").unwrap(), 512);
    assert_eq!(operations::parse_size_bytes("1KB").unwrap(), 1024);
    assert_eq!(operations::parse_size_bytes("2MB").unwrap(), 2 * 1024 * 1024);
    assert_eq!(operations::parse_size_bytes("1gb").unwrap(), 1024 * 1024 * 1024);
    assert_eq!(
        operations::parse_size_bytes("1TB").unwrap(),
        1024_i64 * 1024 * 1024 * 1024
    );
}

#[test]
fn parse_size_bytes_rejects_bad_input() {
    assert!(operations::parse_size_bytes("").is_err());
    assert!(operations::parse_size_bytes("big").is_err());
}

#[test]
fn root_under_always_roots_relative_to_folder() {
    assert_eq!(operations::root_under("/work", "reports"), "/work/reports");
    assert_eq!(operations::root_under("/work/", "reports"), "/work/reports");
    // An absolute-looking rule path is rooted UNDER the folder, never
    // treated as node-absolute — this is what stops a fragment escaping.
    assert_eq!(
        operations::root_under("/work", "/personal/secret"),
        "/work/personal/secret"
    );
    // Empty folder roots at the filesystem root.
    assert_eq!(operations::root_under("", "reports"), "/reports");
}

#[test]
fn confine_rule_path_roots_absolute_paths_under_the_folder() {
    // The scope-escape blocker: an absolute rule path inside an authorised
    // `/work` push is confined to `/work/personal`, not leaked to a
    // node-absolute `/personal`. Confinement succeeds and returns the
    // rooted-under path.
    let scope = Scope::folder("/work".to_owned());
    let confined = operations::confine_rule_path("/work", "/personal/secret", &scope).unwrap();
    assert_eq!(confined, "/work/personal/secret");
    assert!(scope.covers(&Scope::folder(confined)));
}

#[test]
fn confine_rule_path_rejects_parent_traversal_escape() {
    // A `..` traversal that climbs out of the authorised folder must be
    // refused loudly rather than silently clamped or applied.
    let scope = Scope::folder("/work".to_owned());
    let err = operations::confine_rule_path("/work", "../personal", &scope)
        .expect_err("a traversal escaping the folder must be refused");
    assert!(
        err.to_string().contains("escapes the authorised folder"),
        "error should name the escape, got {err}",
    );
    // A deeper climb that lands above the root is equally refused.
    assert!(operations::confine_rule_path("/work", "../../etc", &scope).is_err());
}

// ── Engine-level command entry points against the real state DB ──

#[test]
fn config_push_applies_pins_and_policies_into_db() {
    let engine = make_test_engine();
    let body = r#"
        [[pin]]
        path = "reports"

        [[lifecycle]]
        path = "tmp"
        max_age = "7d"
        max_file_size = "1MB"
        priority = 3
    "#;
    let config = cascade_config::parse::toml::parse(body).unwrap();
    engine.config_push("/work", &config).unwrap();

    let pins = engine.db().list_pin_rules().unwrap();
    assert!(
        pins.iter().any(|p| p.path_glob == "/work/reports"),
        "relative pin path must be rooted under the pushed folder, got {pins:?}",
    );

    let policies = engine.db().list_lifecycle_policies().unwrap();
    let policy = policies
        .iter()
        .find(|p| p.path_glob == "/work/tmp")
        .expect("lifecycle policy rooted under the folder");
    assert_eq!(policy.max_age, Some(7 * 86_400));
    assert_eq!(policy.max_file_size, Some(1024 * 1024));
    assert_eq!(policy.priority, 3);
}

#[test]
fn config_push_roots_absolute_rule_paths_under_the_folder() {
    // Scope-escape blocker, end to end: a fragment authorised over /work
    // carries an absolute pin path `/personal` and an absolute lifecycle
    // path `/`. Both must be rooted UNDER /work — no `/personal` or bare
    // `/` row may land in the DB.
    let engine = make_test_engine();
    let body = r#"
        [[pin]]
        path = "/personal"

        [[lifecycle]]
        path = "/"
        max_age = "1d"
        priority = 1
    "#;
    let config = cascade_config::parse::toml::parse(body).unwrap();
    engine.config_push("/work", &config).unwrap();

    let pins = engine.db().list_pin_rules().unwrap();
    assert!(
        pins.iter().all(|p| p.path_glob.starts_with("/work")),
        "no pin rule may escape the authorised /work subtree, got {pins:?}",
    );
    assert!(
        pins.iter().any(|p| p.path_glob == "/work/personal"),
        "the absolute /personal path must be rooted under /work, got {pins:?}",
    );

    let policies = engine.db().list_lifecycle_policies().unwrap();
    assert!(
        policies.iter().all(|p| p.path_glob.starts_with("/work")),
        "no lifecycle policy may escape the authorised /work subtree, got {policies:?}",
    );
}

#[test]
fn config_push_with_traversal_escape_applies_nothing() {
    // A fragment whose rule path climbs out of the authorised folder via
    // `..` must reject the whole push and apply nothing — not even the
    // earlier, well-behaved rules in the same fragment.
    let engine = make_test_engine();
    let body = r#"
        [[pin]]
        path = "reports"

        [[pin]]
        path = "../personal"
    "#;
    let config = cascade_config::parse::toml::parse(body).unwrap();
    let err = engine
        .config_push("/work", &config)
        .expect_err("a traversal escape must reject the push");
    assert!(
        err.to_string().contains("escapes the authorised folder"),
        "error should name the escape, got {err}",
    );
    assert!(
        engine.db().list_pin_rules().unwrap().is_empty(),
        "no rule may be applied when any rule in the fragment escapes",
    );
}

#[test]
fn policy_set_inserts_a_lifecycle_policy() {
    let engine = make_test_engine();
    engine
        .policy_set("/work/*.tmp", Some(3600), None, 1)
        .unwrap();
    let policies = engine.db().list_lifecycle_policies().unwrap();
    let policy = policies
        .iter()
        .find(|p| p.path_glob == "/work/*.tmp")
        .expect("policy inserted");
    assert_eq!(policy.max_age, Some(3600));
    assert_eq!(policy.max_file_size, None);
}

#[test]
fn grant_add_and_revoke_round_trip_through_db() {
    use crate::manage::{Capability, Scope};
    let engine = make_test_engine();
    let g = Grant {
        grantee: DeviceId::new("SUBORDINATE"),
        capability: Capability::PinWrite,
        scope: Scope::folder("/work"),
        granted_by: manager_id(),
        expires: None,
    };
    engine.grant_add(&g).unwrap();
    let grants = engine.db().list_grants().unwrap();
    assert_eq!(grants.len(), 1);
    let record = grants.first().expect("one grant");
    assert_eq!(record.grant.grantee, DeviceId::new("SUBORDINATE"));
    assert_eq!(record.grant.granted_by, manager_id());

    let summary = engine.grant_revoke(record.id).unwrap();
    assert!(summary.contains("revoked"), "summary: {summary}");
    assert!(engine.db().list_grants().unwrap().is_empty());
}

#[tokio::test]
async fn backend_add_without_factory_fails_loudly() {
    // make_test_engine injects no factory, so a BackendAdd must error rather
    // than silently no-op.
    let engine = make_test_engine();
    let err = engine
        .backend_add("x", "gdrive", "/drive", "type = \"gdrive\"\n")
        .expect_err("backend_add must fail with no factory");
    assert!(
        err.to_string().contains("no backend factory"),
        "error should name the missing factory, got {err}",
    );
}

#[tokio::test]
async fn restart_rearms_running_state() {
    let engine = make_test_engine();
    engine.shutdown();
    assert!(!engine.status().running, "shutdown should stop the engine");
    let _handle = engine.restart().unwrap();
    assert!(
        engine.status().running,
        "restart must re-arm the running state",
    );
}

#[tokio::test]
async fn dispatch_grant_add_escalation_is_refused_end_to_end() {
    use crate::manage::{Capability, Scope};
    let engine = make_test_engine();
    // The manager may delegate (grant:admin over /work) but does NOT hold
    // pin:write — delegating it is an escalation and must be refused, with
    // no grant inserted and the denial audited.
    engine
        .db()
        .insert_grant(&Grant {
            grantee: manager_id(),
            capability: Capability::GrantAdmin,
            scope: Scope::folder("/work"),
            granted_by: DeviceId::new("OWNER"),
            expires: None,
        })
        .unwrap();

    let result = engine
        .dispatch(
            &manager_id(),
            ManageCommand::GrantAdd {
                grant: cascade_p2p::protocol::ManageGrant {
                    grantee: "SUBORDINATE".to_owned(),
                    capability: "pin:write".to_owned(),
                    scope: WireScope::Folder {
                        path: "/work".to_owned(),
                    },
                    expires: None,
                },
            },
            WireScope::Folder {
                path: "/work".to_owned(),
            },
            None,
            Utc::now(),
        )
        .await;

    assert!(
        matches!(
            result,
            ManageResult::Err {
                kind: cascade_p2p::protocol::ManageErrorKind::Unauthorised,
                ..
            }
        ),
        "escalating delegation must be refused, got {result:?}",
    );
    // Only the manager's own grant exists — no delegated grant was inserted.
    assert_eq!(engine.db().list_grants().unwrap().len(), 1);
    let audit = engine.db().list_audit().unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(
        audit.first().map(|r| r.entry.outcome.as_str()),
        Some("denied"),
    );
}
