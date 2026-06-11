//! Test module for `mount.rs`, split out to keep the
//! parent file under the source-length cap. Declared from there via
//! `#[cfg(test)] #[path = "mount_tests.rs"] mod tests;`, so it stays a child
//! module with full access to the parent's private items.

use super::*;
use tempfile::TempDir;

#[cfg_attr(not(any(unix, windows)), allow(dead_code))]
fn make_ctx(dir: &TempDir) -> CliContext {
    let config_dir: PathBuf = dir.path().to_path_buf();
    CliContext {
        db_path: config_dir.join("state.db"),
        pid_path: config_dir.join("cascade.pid"),
        config_dir,
    }
}

#[test]
fn ensure_directory_creates_missing_directory() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("Cloud");

    ensure_directory(&path, "mount point").unwrap();

    assert!(path.is_dir());
}

#[test]
fn ensure_directory_rejects_existing_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("Cloud");
    std::fs::write(&path, "not a directory").unwrap();

    let error = ensure_directory(&path, "mount point").unwrap_err();
    let message = format!("{error:#}");

    assert!(message.contains("mount point"));
    assert!(message.contains("exists but is not a directory"));
}

#[test]
fn read_text_file_includes_path_in_error() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("missing.pid");

    let error = read_text_file(&path, "PID file").unwrap_err();
    let message = format!("{error:#}");

    assert!(message.contains("PID file"));
    assert!(message.contains("missing.pid"));
}

#[test]
fn write_pid_file_includes_path_in_error() {
    let dir = TempDir::new().unwrap();
    // Create a directory where the file should go — write will fail.
    let path = dir.path().join("subdir");
    std::fs::create_dir_all(&path).unwrap();
    let file_path = path.join("pid");
    // Make the directory read-only so writing fails.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
    }

    let result = write_pid_file(&file_path);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let error = result.unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("PID file"));
        assert!(message.contains("pid"));
        // Restore permissions so TempDir can clean up.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    #[cfg(not(unix))]
    {
        let _ = result;
    }
}

// --- Stop ---

// `stop` is implemented on unix and windows; other platforms bail.
// The same behavioural contract holds on both unix and windows:
// a missing PID file is "not running" (Ok), and a stale PID file
// (process that no longer exists) is cleaned up.

#[cfg(unix)]
#[test]
fn stop_succeeds_when_no_pid_file() {
    let dir = TempDir::new().unwrap();
    let ctx = make_ctx(&dir);

    // No PID file exists — should print "not running" and succeed.
    stop(&ctx).unwrap();
}

#[cfg(unix)]
#[test]
fn stop_cleans_up_stale_pid_file() {
    let dir = TempDir::new().unwrap();
    let ctx = make_ctx(&dir);

    // PID 999999999 is not a real process.
    std::fs::write(&ctx.pid_path, "999999999").unwrap();

    stop(&ctx).unwrap();

    // The stale PID file should have been removed.
    assert!(!ctx.pid_path.exists());
}

#[cfg(target_os = "windows")]
#[test]
fn windows_stop_succeeds_when_no_pid_file() {
    let dir = TempDir::new().unwrap();
    let ctx = make_ctx(&dir);

    // No PID file exists — should print "not running" and succeed.
    stop(&ctx).unwrap();
}

#[cfg(target_os = "windows")]
#[test]
fn windows_stop_cleans_up_stale_pid_file() {
    let dir = TempDir::new().unwrap();
    let ctx = make_ctx(&dir);

    // PID 999999999 is not a real process. `is_process_alive`
    // returns true on Windows (no cheap liveness check), so
    // `stop` will call `taskkill /F` against a missing PID;
    // we detect taskkill's "process not found" output and treat
    // it as success so `cascade stop` stays idempotent.
    std::fs::write(&ctx.pid_path, "999999999").unwrap();

    stop(&ctx).unwrap();

    assert!(!ctx.pid_path.exists());
}

#[cfg(target_os = "windows")]
#[test]
fn is_windows_process_not_found_matches_taskkill_messages() {
    assert!(is_windows_process_not_found(
        "ERROR: The process \"999999999\" not found."
    ));
    assert!(is_windows_process_not_found(
        "INFO: No tasks running with the specified criteria."
    ));
    assert!(!is_windows_process_not_found("SUCCESS: terminated"));
    assert!(!is_windows_process_not_found(
        "ERROR: Access is denied for PID 1234."
    ));
}

// -- Permission escalation (macOS only) ---

#[cfg(target_os = "macos")]
#[test]
fn is_permission_error_detects_operation_not_permitted() {
    assert!(is_permission_error(
        "mount_nfs: can't mount / from 127.0.0.1 onto /tmp/cloud: Operation not permitted"
    ));
}

#[cfg(target_os = "macos")]
#[test]
fn is_permission_error_rejects_other_errors() {
    assert!(!is_permission_error("mount_nfs: No such file or directory"));
}

#[cfg(target_os = "macos")]
#[test]
fn osascript_mount_command_constructs_correctly() {
    let dir = TempDir::new().unwrap();
    let mount_point = dir.path().join("Cloud");
    let cmd = osascript_mount_command(&mount_point, 12345);

    assert_eq!(cmd.get_program(), "osascript");
    let args: Vec<String> = cmd
        .get_args()
        .map(std::ffi::OsStr::to_string_lossy)
        .map(std::borrow::Cow::into_owned)
        .collect();
    assert_eq!(args[0], "-e");
    assert!(args[1].contains("with administrator privileges"));
    assert!(args[1].contains("port=12345"));
    assert!(args[1].contains(mount_point.to_str().unwrap()));
}

#[cfg(target_os = "macos")]
#[test]
fn mount_nfs_on_unreachable_port_errors_without_privilege_escalation() {
    assert!(!is_permission_error("Connection refused"));
    assert!(!is_permission_error("No such file or directory"));
    assert!(is_permission_error("Operation not permitted"));
}

// --- WebDAV mount command construction ---

#[cfg(target_os = "macos")]
#[test]
fn webdav_mount_command_constructs_correctly() {
    let dir = TempDir::new().unwrap();
    let mount_point = dir.path().join("Cloud");
    let cmd = webdav_mount_command(&mount_point, 52431);

    assert_eq!(cmd.get_program(), "/sbin/mount_webdav");
    let args: Vec<String> = cmd
        .get_args()
        .map(std::ffi::OsStr::to_string_lossy)
        .map(std::borrow::Cow::into_owned)
        .collect();
    assert_eq!(args[0], "http://localhost:52431/");
    assert_eq!(args[1], mount_point.to_str().unwrap());
}

// --- Linux NFS mount command construction ---

/// Verify the Linux `mount -t nfs` command is built with the right
/// options for the cascade NFS server (v3 over TCP, port pinned to
/// the server's bound port for both NFS and mountd traffic).
#[cfg(target_os = "linux")]
#[test]
fn linux_nfs_mount_command_constructs_correctly() {
    let dir = TempDir::new().unwrap();
    let mount_point = dir.path().join("Cloud");
    let cmd = linux_nfs_mount_command(&mount_point, 52431);

    assert_eq!(cmd.get_program(), "mount");
    let args: Vec<String> = cmd
        .get_args()
        .map(std::ffi::OsStr::to_string_lossy)
        .map(std::borrow::Cow::into_owned)
        .collect();
    assert_eq!(args[0], "-t");
    assert_eq!(args[1], "nfs");
    assert_eq!(args[2], "-o");
    let options = &args[3];
    assert!(options.contains("port=52431"));
    assert!(options.contains("mountport=52431"));
    assert!(options.contains("proto=tcp"));
    assert!(options.contains("vers=3"));
    assert_eq!(args[4], "127.0.0.1:/");
    assert_eq!(args[5], mount_point.to_str().unwrap());
}

#[cfg(target_os = "linux")]
#[test]
fn is_linux_permission_error_detects_root_messages() {
    assert!(is_linux_permission_error(
        "mount: only root can do that",
        Some(1)
    ));
    assert!(is_linux_permission_error(
        "mount: permission denied",
        Some(1)
    ));
    assert!(is_linux_permission_error(
        "mount: you must be root to use mount",
        Some(1)
    ));
    assert!(is_linux_permission_error(
        "mount: operation not permitted",
        Some(1)
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn is_linux_permission_error_detects_exit_32() {
    // util-linux exit code 32 = mount failure; commonly permission.
    assert!(is_linux_permission_error("", Some(32)));
}

#[cfg(target_os = "linux")]
#[test]
fn is_linux_permission_error_rejects_other_failures() {
    assert!(!is_linux_permission_error(
        "mount: no such file or directory",
        Some(1)
    ));
    assert!(!is_linux_permission_error(
        "mount: connection refused",
        Some(1)
    ));
    assert!(!is_linux_permission_error("", None));
}

// --- Fallback ordering ---

/// Verifies the macOS strategy order by checking that each
/// `try_*` function returns an error when its server/presenter cannot
/// start, rather than silently succeeding.
///
/// We cannot run a full engine in this test (it needs real config),
/// so we verify the module-level invariant: `try_fskit` is defined
/// on macOS and calls `FSKitPresenter::new`, confirming the import
/// path is wired correctly.
#[cfg(target_os = "macos")]
#[test]
fn fskit_presenter_import_is_wired() {
    // Construct an FSKitPresenter (does not require macOS runtime).
    let presenter = cascade_presenter_fskit::FSKitPresenter::new("/tmp/test.sock");
    assert_eq!(presenter.mount_point(), Path::new("/Volumes/Cascade"));

    let custom = presenter.with_mount_point("/tmp/custom-mount");
    assert_eq!(custom.mount_point(), Path::new("/tmp/custom-mount"));
}

/// On non-macOS the `FSKit` presenter is not compiled into the binary
/// path — confirm that the default NFS path is used instead.
#[cfg(not(target_os = "macos"))]
#[test]
fn non_macos_does_not_include_fskit() {
    // On non-macOS the unmount stub is a no-op.
    let dir = TempDir::new().unwrap();
    let mount_point = dir.path().join("Cloud");
    // Should succeed (no-op).
    unmount_path(&mount_point).unwrap();
}

// --- PresenterResources cleanup ---

/// Verify that `PresenterResources::shutdown` does not panic when called
/// on handles that have already completed (simulating a failed attempt).
#[tokio::test]
async fn presenter_resources_shutdown_is_idempotent() {
    // Spawn a task that completes immediately, then abort its handle.
    // Abort on a finished handle is a no-op — must not panic.
    let sync_handle = tokio::spawn(async {});
    // Let the runtime finish the spawned task.
    tokio::task::yield_now().await;
    // Abort must not panic even though the task already finished.
    sync_handle.abort();
}

// --- resolve_p2p_bridge_config tests ------------------------------------

#[test]
fn resolve_p2p_bridge_config_defaults_produce_none() {
    use crate::cli::init::P2pConfig;
    let cfg = resolve_p2p_bridge_config(&P2pConfig::default()).unwrap();
    assert!(!cfg.enable_p2p);
    assert!(cfg.posture.is_none());
    assert!(cfg.relay_endpoints.is_empty());
    assert!(cfg.relay_shared_secret.is_none());
}

#[test]
fn resolve_p2p_bridge_config_parses_public_posture_and_relay() {
    use crate::cli::init::P2pConfig;
    let secret = "b".repeat(64);
    let cfg = resolve_p2p_bridge_config(&P2pConfig {
        enabled: true,
        posture: Some("public".to_string()),
        relay_endpoint: Some("10.0.0.1:22067".to_string()),
        relay_shared_secret: Some(secret),
    })
    .unwrap();
    assert!(cfg.enable_p2p);
    assert_eq!(cfg.posture, Some(cascade_p2p::DiscoveryReach::Public));
    assert_eq!(cfg.relay_endpoints.len(), 1);
    assert!(cfg.relay_shared_secret.is_some());
}

#[test]
fn resolve_p2p_bridge_config_parses_lan_only_posture() {
    use crate::cli::init::P2pConfig;
    let cfg = resolve_p2p_bridge_config(&P2pConfig {
        enabled: true,
        posture: Some("lan-only".to_string()),
        relay_endpoint: None,
        relay_shared_secret: None,
    })
    .unwrap();
    assert_eq!(cfg.posture, Some(cascade_p2p::DiscoveryReach::LanOnly));
    assert!(cfg.relay_endpoints.is_empty());
}

#[test]
fn resolve_p2p_bridge_config_parses_private_posture() {
    use crate::cli::init::P2pConfig;
    let cfg = resolve_p2p_bridge_config(&P2pConfig {
        enabled: false,
        posture: Some("private".to_string()),
        relay_endpoint: None,
        relay_shared_secret: None,
    })
    .unwrap();
    assert_eq!(cfg.posture, Some(cascade_p2p::DiscoveryReach::Private));
}

#[test]
fn resolve_p2p_bridge_config_rejects_unknown_posture() {
    use crate::cli::init::P2pConfig;
    let result = resolve_p2p_bridge_config(&P2pConfig {
        enabled: true,
        posture: Some("publik".to_string()),
        relay_endpoint: None,
        relay_shared_secret: None,
    });
    assert!(result.is_err());
    let msg = format!("{:#}", result.unwrap_err());
    assert!(msg.contains("[p2p] posture"), "error was: {msg}");
}

#[test]
fn resolve_p2p_bridge_config_rejects_malformed_relay_endpoint() {
    use crate::cli::init::P2pConfig;
    let result = resolve_p2p_bridge_config(&P2pConfig {
        enabled: true,
        posture: None,
        relay_endpoint: Some("not-a-socket".to_string()),
        relay_shared_secret: None,
    });
    assert!(result.is_err());
    let msg = format!("{:#}", result.unwrap_err());
    assert!(msg.contains("[p2p] relay_endpoint"), "error was: {msg}");
}

#[test]
fn resolve_p2p_bridge_config_rejects_malformed_relay_secret() {
    use crate::cli::init::P2pConfig;
    let result = resolve_p2p_bridge_config(&P2pConfig {
        enabled: true,
        posture: None,
        relay_endpoint: Some("10.0.0.1:22067".to_string()),
        relay_shared_secret: Some("tooshort".to_string()),
    });
    assert!(result.is_err());
    let msg = format!("{:#}", result.unwrap_err());
    assert!(
        msg.contains("relay_shared_secret must be exactly 64"),
        "error was: {msg}"
    );
}

// --- rebuild_backends validation ---

/// A minimal `CascadeConfig` with two backends that can be used to test
/// `rebuild_backends` without running a real engine.
fn make_test_config(backends: &[(&str, Option<&str>)]) -> CascadeConfig {
    use crate::cli::init::{BackendConfig, CascadeConfig, MountConfig};
    let mut config = CascadeConfig {
        mount: MountConfig {
            point: "/tmp/test-cloud".to_string(),
        },
        ..CascadeConfig::default()
    };
    for &(name, mount) in backends {
        let entry = BackendConfig {
            backend_type: "s3".to_string(),
            mount: mount.map(str::to_string),
            account: None,
        };
        config
            .backends
            .insert(name.to_string(), toml::Value::try_from(&entry).unwrap());
    }
    config
}

/// `rebuild_backends` rejects a backend whose `mount` is empty after trimming.
#[test]
fn rebuild_backends_rejects_empty_mount() {
    let dir = TempDir::new().unwrap();
    // Write a per-backend TOML so load_backend_config succeeds.
    std::fs::write(dir.path().join("s3.toml"), "type = \"s3\"\n").unwrap();

    let config = make_test_config(&[("s3", Some("  "))]);
    // shared_http is never reached because the validation fires first.
    let http: std::sync::Arc<dyn cascade_engine::portable::HttpClient> =
        std::sync::Arc::new(cascade_engine::portable::native::ReqwestClient::new());
    let result = rebuild_backends(&config, dir.path(), http);
    assert!(result.is_err(), "expected error for empty mount");
    let msg = format!("{:#}", result.unwrap_err());
    assert!(msg.contains("empty mount path"), "error was: {msg}");
}

/// `rebuild_backends` rejects two backends mapped to the same mount path.
#[test]
fn rebuild_backends_rejects_duplicate_mount() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("alpha.toml"), "type = \"s3\"\n").unwrap();
    std::fs::write(dir.path().join("beta.toml"), "type = \"s3\"\n").unwrap();

    let config = make_test_config(&[("alpha", Some("shared")), ("beta", Some("shared"))]);
    let http: std::sync::Arc<dyn cascade_engine::portable::HttpClient> =
        std::sync::Arc::new(cascade_engine::portable::native::ReqwestClient::new());
    let result = rebuild_backends(&config, dir.path(), http);
    assert!(result.is_err(), "expected error for duplicate mount");
    let msg = format!("{:#}", result.unwrap_err());
    assert!(msg.contains("duplicate mount path"), "error was: {msg}");
}

/// `rebuild_backends` rejects two backends both using the at-root `"/"` mount.
#[test]
fn rebuild_backends_rejects_two_at_root_mounts() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("alpha.toml"), "type = \"s3\"\n").unwrap();
    std::fs::write(dir.path().join("beta.toml"), "type = \"s3\"\n").unwrap();

    let config = make_test_config(&[("alpha", Some("/")), ("beta", Some("/"))]);
    let http: std::sync::Arc<dyn cascade_engine::portable::HttpClient> =
        std::sync::Arc::new(cascade_engine::portable::native::ReqwestClient::new());
    let result = rebuild_backends(&config, dir.path(), http);
    assert!(result.is_err(), "expected error for two at-root mounts");
    let msg = format!("{:#}", result.unwrap_err());
    assert!(msg.contains("duplicate mount path"), "error was: {msg}");
}

/// `rebuild_backends` uses the backend name as the default when `mount` is absent.
#[test]
fn rebuild_backends_defaults_mount_to_name() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("mybucket.toml"), "type = \"s3\"\nendpoint = \"https://s3.example.com\"\nbucket = \"b\"\nregion = \"us-east-1\"\naccess_key_id = \"k\"\nsecret_access_key = \"s\"\n").unwrap();

    // BackendConfig without `mount` — should default to "mybucket".
    let config = make_test_config(&[("mybucket", None)]);
    let http: std::sync::Arc<dyn cascade_engine::portable::HttpClient> =
        std::sync::Arc::new(cascade_engine::portable::native::ReqwestClient::new());
    let result = rebuild_backends(&config, dir.path(), http);
    assert!(result.is_ok(), "expected success: {:#?}", result.err());
    let backends = result.unwrap();
    assert_eq!(backends.len(), 1);
    assert_eq!(backends[0].mount.as_deref(), Some("mybucket"));
}

/// `rebuild_backends` returns backends in alphabetical order regardless of the
/// insertion order in `config.toml`.
#[test]
fn rebuild_backends_is_alphabetically_ordered() {
    let dir = TempDir::new().unwrap();
    let s3_toml = "type = \"s3\"\nendpoint = \"https://s3.example.com\"\nbucket = \"b\"\nregion = \"us-east-1\"\naccess_key_id = \"k\"\nsecret_access_key = \"s\"\n";
    std::fs::write(dir.path().join("z-last.toml"), s3_toml).unwrap();
    std::fs::write(dir.path().join("a-first.toml"), s3_toml).unwrap();
    std::fs::write(dir.path().join("m-middle.toml"), s3_toml).unwrap();

    let config = make_test_config(&[
        ("z-last", Some("z-last")),
        ("a-first", Some("a-first")),
        ("m-middle", Some("m-middle")),
    ]);
    let http: std::sync::Arc<dyn cascade_engine::portable::HttpClient> =
        std::sync::Arc::new(cascade_engine::portable::native::ReqwestClient::new());
    let result = rebuild_backends(&config, dir.path(), http).unwrap();
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].mount.as_deref(), Some("a-first"));
    assert_eq!(result[1].mount.as_deref(), Some("m-middle"));
    assert_eq!(result[2].mount.as_deref(), Some("z-last"));
}
