#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::string_slice
)]
//! End-to-end NAT integration tests using Linux network namespaces.
//!
//! Gated behind the `nat-integration` feature flag so regular `cargo test`
//! runs skip this file entirely. Only the dedicated CI job and local
//! developers who explicitly opt in with `--features nat-integration` will
//! execute these tests.
//!
//! # Network topology
//!
//! ```text
//!  cascade-peer-a (10.0.1.2/24)
//!       |
//!  veth-a-ext (10.0.1.1/24) ── cascade-internet ── veth-b-ext (10.0.2.1/24)
//!                                     |
//!                              cascade-peer-b (10.0.2.2/24)
//! ```
//!
//! Each peer namespace performs MASQUERADE NAT on its egress veth so the
//! internet namespace sees only the NAT gateway address (the `.1` end of each
//! veth pair). The relay server listens inside `cascade-internet` and is
//! reachable from both peers at the gateway's IP.
//!
//! # How the test works
//!
//! The test process (running as root in CI) sets up the three namespaces,
//! the veth pairs, the iptables MASQUERADE rules, and the routing. It then
//! re-invokes the test binary inside each peer namespace using
//! `ip netns exec`, passing the role via the `CASCADE_NETNS_ROLE` environment
//! variable:
//!
//! - `relay` — spawns the relay server and writes its address to stdout, then
//!   blocks until stdin closes (signalling teardown).
//! - `peer-a` / `peer-b` — connects to the relay using the shared secret,
//!   exchanges a block index + request/response, and exits zero on success.
//!
//! The harness collects the exit codes and fails the test if any subprocess
//! exits non-zero or times out.
//!
//! # Running locally
//!
//! See `docs/nat-integration-tests.md` for the step-by-step guide.

#![cfg(feature = "nat-integration")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::doc_markdown
)]

use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

// ── Subprocess role entry points ────────────────────────────────────────────

/// Entry point used when the test binary is re-invoked inside a namespace.
///
/// Checks `CASCADE_NETNS_ROLE` and dispatches to the appropriate role
/// function. Returns `true` when a role was executed (so `main` should not
/// proceed with the normal test suite).
#[must_use]
pub fn maybe_run_as_subprocess() -> bool {
    let Some(role) = std::env::var("CASCADE_NETNS_ROLE").ok() else {
        return false;
    };
    match role.as_str() {
        "relay" => run_relay_subprocess(),
        "peer-a" => run_peer_subprocess("peer-a"),
        "peer-b" => run_peer_subprocess("peer-b"),
        other => {
            eprintln!("unknown CASCADE_NETNS_ROLE: {other}");
            std::process::exit(1);
        }
    }
    true
}

/// Run as relay subprocess: bind a relay server, print its address to stdout,
/// then block until stdin is closed (the harness drops the pipe on teardown).
fn run_relay_subprocess() {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        let secret = relay_shared_secret();
        let bind: SocketAddr = "0.0.0.0:0".parse().expect("relay bind addr");
        let config = cascade_relay_server::RelayConfig {
            bind,
            shared_secret: secret,
            session_timeout: Duration::from_secs(30),
            max_sessions: 16,
            metrics_bind: None,
        };
        let handle = cascade_relay_server::server::spawn(config)
            .await
            .expect("spawning relay");
        // Print the bound address on stdout so the harness can read it.
        println!("{}", handle.local_addr);
        // Flush before blocking — the harness reads exactly one line.
        drop(std::io::stdout().flush());
        // Block until stdin closes (harness drops the write end).
        let stdin = std::io::stdin();
        let mut line = String::new();
        let _ = stdin.lock().read_line(&mut line);
        // The handle's Drop stops the server.
        drop(handle);
    });
}

/// Run as peer subprocess: connect to the relay at the address given in
/// `CASCADE_RELAY_ADDR`, exchange a block, then exit zero on success.
fn run_peer_subprocess(role: &str) {
    let relay_addr = std::env::var("CASCADE_RELAY_ADDR")
        .unwrap_or_else(|_| panic!("{role}: CASCADE_RELAY_ADDR not set"));
    let relay_url = format!("ws://{relay_addr}");
    let secret = relay_shared_secret();
    let session_id = "nat-integration-session";
    let device_id = role;

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        let connection = cascade_p2p::relay::RelayClient::connect_with_secret(
            &relay_url, session_id, device_id, &secret,
        )
        .await
        .unwrap_or_else(|err| panic!("{role}: relay connect failed: {err}"));

        if role == "peer-a" {
            // Peer A sends a test payload and waits for the echo.
            let payload = build_test_frame();
            connection
                .send(&payload)
                .await
                .unwrap_or_else(|err| panic!("{role}: send failed: {err}"));
            let received = connection
                .recv()
                .await
                .unwrap_or_else(|err| panic!("{role}: recv failed: {err}"));
            assert_eq!(
                received, payload,
                "{role}: received payload does not match sent payload",
            );
        } else {
            // Peer B echoes whatever it receives.
            let received = connection
                .recv()
                .await
                .unwrap_or_else(|err| panic!("{role}: recv failed: {err}"));
            connection
                .send(&received)
                .await
                .unwrap_or_else(|err| panic!("{role}: echo send failed: {err}"));
        }
    });
}

/// The test payload: a fixed-size block representing a BEP-shaped data frame.
/// Large enough to be non-trivial but small enough to transit the veth pair
/// in a single UDP datagram.
fn build_test_frame() -> Vec<u8> {
    let body = b"cascade-nat-integration-test-block";
    let len = u32::try_from(body.len()).expect("body length fits in u32");
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(body);
    frame
}

/// Shared HMAC secret for the relay handshake. Both sides (relay subprocess
/// and peer subprocesses) derive the same 32-byte value.
fn relay_shared_secret() -> [u8; 32] {
    // Deterministic test secret: 0x00 0x0d 0x1a 0x27 ... (idx * 13 mod 256).
    let mut secret = [0u8; 32];
    for (idx, byte) in secret.iter_mut().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let value = (idx as u8).wrapping_mul(13);
        *byte = value;
    }
    secret
}

// ── Network namespace harness ────────────────────────────────────────────────

/// Names for the three network namespaces used in the test.
const NS_INTERNET: &str = "cascade-internet";
const NS_PEER_A: &str = "cascade-peer-a";
const NS_PEER_B: &str = "cascade-peer-b";

/// IP addressing:
///
/// - `cascade-internet`: `lo` only, plus the two veth gateway ends.
/// - `cascade-peer-a`: `veth-a-int` at `10.0.1.2/24`, default via `10.0.1.1`.
/// - `cascade-peer-b`: `veth-b-int` at `10.0.2.2/24`, default via `10.0.2.1`.
const GW_A: &str = "10.0.1.1";
const PEER_A_IP: &str = "10.0.1.2";
const GW_B: &str = "10.0.2.1";
const PEER_B_IP: &str = "10.0.2.2";

/// Guard that tears down the three network namespaces (and all associated veth
/// pairs and iptables rules) when dropped.
///
/// All cleanup steps are best-effort: teardown proceeds even if individual
/// commands fail, so a partial setup leaves no orphan namespaces behind.
#[derive(Debug)]
struct NetNsHarness {
    /// The local address the relay is listening on inside `cascade-internet`.
    /// Constructed as `<GW_A>:<port>` so peers can reach it through NAT.
    relay_addr: String,
    relay_proc: Child,
}

impl NetNsHarness {
    /// Create the three namespaces, veth pairs, routing, NAT, and relay.
    ///
    /// Returns `None` and skips the test if `ip` or `iptables` are not
    /// available or if the process is not running as root (no `CAP_NET_ADMIN`).
    fn setup() -> Option<Self> {
        if !is_root() {
            eprintln!("nat_integration: not running as root — skipping");
            return None;
        }
        if !command_exists("ip") || !command_exists("iptables") {
            eprintln!("nat_integration: ip or iptables not found — skipping");
            return None;
        }

        // Tear down any stale namespaces from a previous interrupted run.
        Self::teardown_namespaces();

        // Create the three namespaces.
        run_required("ip", &["netns", "add", NS_INTERNET]);
        run_required("ip", &["netns", "add", NS_PEER_A]);
        run_required("ip", &["netns", "add", NS_PEER_B]);

        // veth pair for peer-a <-> internet.
        run_required(
            "ip",
            &[
                "link",
                "add",
                "veth-a-ext",
                "type",
                "veth",
                "peer",
                "name",
                "veth-a-int",
            ],
        );
        run_required("ip", &["link", "set", "veth-a-ext", "netns", NS_INTERNET]);
        run_required("ip", &["link", "set", "veth-a-int", "netns", NS_PEER_A]);

        // veth pair for peer-b <-> internet.
        run_required(
            "ip",
            &[
                "link",
                "add",
                "veth-b-ext",
                "type",
                "veth",
                "peer",
                "name",
                "veth-b-int",
            ],
        );
        run_required("ip", &["link", "set", "veth-b-ext", "netns", NS_INTERNET]);
        run_required("ip", &["link", "set", "veth-b-int", "netns", NS_PEER_B]);

        // Bring up loopback in all three namespaces.
        for ns in [NS_INTERNET, NS_PEER_A, NS_PEER_B] {
            ns_run(ns, "ip", &["link", "set", "lo", "up"]);
        }

        // Assign addresses and bring up interfaces in internet namespace.
        ns_run(
            NS_INTERNET,
            "ip",
            &["addr", "add", &format!("{GW_A}/24"), "dev", "veth-a-ext"],
        );
        ns_run(NS_INTERNET, "ip", &["link", "set", "veth-a-ext", "up"]);
        ns_run(
            NS_INTERNET,
            "ip",
            &["addr", "add", &format!("{GW_B}/24"), "dev", "veth-b-ext"],
        );
        ns_run(NS_INTERNET, "ip", &["link", "set", "veth-b-ext", "up"]);

        // Assign addresses and bring up interfaces in peer namespaces.
        ns_run(
            NS_PEER_A,
            "ip",
            &[
                "addr",
                "add",
                &format!("{PEER_A_IP}/24"),
                "dev",
                "veth-a-int",
            ],
        );
        ns_run(NS_PEER_A, "ip", &["link", "set", "veth-a-int", "up"]);
        ns_run(NS_PEER_A, "ip", &["route", "add", "default", "via", GW_A]);

        ns_run(
            NS_PEER_B,
            "ip",
            &[
                "addr",
                "add",
                &format!("{PEER_B_IP}/24"),
                "dev",
                "veth-b-int",
            ],
        );
        ns_run(NS_PEER_B, "ip", &["link", "set", "veth-b-int", "up"]);
        ns_run(NS_PEER_B, "ip", &["route", "add", "default", "via", GW_B]);

        // Enable IP forwarding in the internet namespace.
        ns_run(NS_INTERNET, "sysctl", &["-w", "net.ipv4.ip_forward=1"]);

        // MASQUERADE NAT on the peer namespace egress — peers appear to the
        // internet as their respective gateway IPs.
        ns_run(
            NS_PEER_A,
            "iptables",
            &[
                "-t",
                "nat",
                "-A",
                "POSTROUTING",
                "-o",
                "veth-a-int",
                "-j",
                "MASQUERADE",
            ],
        );
        ns_run(
            NS_PEER_B,
            "iptables",
            &[
                "-t",
                "nat",
                "-A",
                "POSTROUTING",
                "-o",
                "veth-b-int",
                "-j",
                "MASQUERADE",
            ],
        );

        // Spawn the relay subprocess inside cascade-internet. The relay binary
        // is the current test binary re-invoked with CASCADE_NETNS_ROLE=relay.
        //
        // The subprocess is a test binary, so the test harness emits its own
        // header lines to stdout (e.g. "running 1 test") before the relay
        // address appears. Two things are required to get the address on the
        // pipe:
        //
        //   1. `--nocapture` — without this the test harness captures
        //      `println!` output internally and never forwards it to the
        //      process's stdout, so the address is swallowed.
        //   2. Line-skipping — the harness header ("running 1 test",
        //      "test nat_relay_end_to_end ...") must be discarded before
        //      accepting the `<host>:<port>` line as the bound address.
        let current_exe = std::env::current_exe().expect("current_exe");
        let mut relay_proc = Command::new("ip")
            .args(["netns", "exec", NS_INTERNET])
            .arg(&current_exe)
            // Limit to our test so the harness doesn't try to run others, and
            // pass --nocapture so that `println!` in the test body reaches the
            // process's real stdout (which is piped to us).
            //
            // Note: test binary arguments are test harness arguments directly
            // (no `--` separator needed when invoking the binary directly).
            .args(["nat_relay_end_to_end", "--nocapture", "--test-threads=1"])
            .env("CASCADE_NETNS_ROLE", "relay")
            // Inherit stderr for debugging; capture stdout to read the bound port.
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawning relay subprocess");

        // Read the relay's bound address from its stdout.
        //
        // The test harness emits header lines before the relay address, so we
        // scan lines until we find one that looks like `<host>:<port>` (a
        // non-empty port suffix after the last `:`). Lines that don't match
        // — such as "running 1 test" or "test nat_relay_end_to_end ..." — are
        // skipped.
        let relay_bound_port = {
            let stdout = relay_proc.stdout.take().expect("relay stdout");
            let mut reader = BufReader::new(stdout);
            // The relay prints `0.0.0.0:<port>` or `127.0.0.1:<port>`, but the
            // test harness emits its own header lines first ("running 1 test",
            // "test nat_relay_end_to_end ..."). Skip lines that don't end in an
            // all-digit port suffix.
            loop {
                let mut line = String::new();
                let n = reader
                    .read_line(&mut line)
                    .expect("reading relay subprocess stdout");
                if n == 0 {
                    // EOF — relay exited before printing an address.
                    panic!("relay subprocess closed stdout without printing a bound address");
                }
                let trimmed = line.trim();
                if let Some(port) = trimmed.rsplit(':').next() {
                    let port = port.trim();
                    if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) {
                        break port.to_owned();
                    }
                }
            }
        };

        let relay_addr = format!("{GW_A}:{relay_bound_port}");

        Some(Self {
            relay_addr,
            relay_proc,
        })
    }

    /// Tear down: signal the relay subprocess (drop stdin) and remove all
    /// network namespaces. All cleanup steps are best-effort.
    fn cleanup(&mut self) {
        // Signal the relay to shut down by closing its stdin.
        if let Some(stdin) = self.relay_proc.stdin.take() {
            drop(stdin);
        }
        let _ = self.relay_proc.wait();
        Self::teardown_namespaces();
    }

    /// Remove all three namespaces (idempotent — ignores errors if they do
    /// not exist).
    fn teardown_namespaces() {
        for ns in [NS_INTERNET, NS_PEER_A, NS_PEER_B] {
            let _ = Command::new("ip").args(["netns", "delete", ns]).output();
        }
    }
}

impl Drop for NetNsHarness {
    fn drop(&mut self) {
        self.cleanup();
    }
}

// ── Helper utilities ─────────────────────────────────────────────────────────

/// Returns `true` when the current process is running as UID 0.
///
/// Determined by running `id -u` as a subprocess to avoid any `unsafe` code.
fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim() == "0")
        .unwrap_or(false)
}

/// Returns `true` when `command` is on PATH.
fn command_exists(command: &str) -> bool {
    Command::new("which")
        .arg(command)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run `ip netns exec <ns> <cmd> <args>`.
fn ns_run(ns: &str, cmd: &str, args: &[&str]) {
    let status = Command::new("ip")
        .arg("netns")
        .arg("exec")
        .arg(ns)
        .arg(cmd)
        .args(args)
        .status()
        .unwrap_or_else(|err| panic!("failed to spawn `ip netns exec {ns} {cmd}`: {err}"));
    if !status.success() {
        panic!("`ip netns exec {ns} {cmd} {args:?}` exited with {status}");
    }
}

/// Run a top-level command that must succeed (no namespace context).
fn run_required(cmd: &str, args: &[&str]) {
    let status = Command::new(cmd)
        .args(args)
        .status()
        .unwrap_or_else(|err| panic!("failed to spawn `{cmd} {args:?}`: {err}"));
    if !status.success() {
        panic!("`{cmd} {args:?}` exited with {status}");
    }
}

/// Spawn a peer subprocess inside `ns`, wait up to `timeout` for it to
/// complete, and return `true` on success.
fn run_peer_in_ns(ns: &str, role: &str, relay_addr: &str, timeout: Duration) -> bool {
    let current_exe = std::env::current_exe().expect("current_exe");
    let mut child = Command::new("ip")
        .args(["netns", "exec", ns])
        .arg(&current_exe)
        .env("CASCADE_NETNS_ROLE", role)
        .env("CASCADE_RELAY_ADDR", relay_addr)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap_or_else(|err| panic!("spawning peer {role}: {err}"));

    // Poll for completion up to the timeout.
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    eprintln!("peer {role} timed out after {timeout:?}");
                    return false;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(err) => {
                eprintln!("waiting on peer {role}: {err}");
                return false;
            }
        }
    }
}

// ── Test ─────────────────────────────────────────────────────────────────────

/// End-to-end NAT relay test.
///
/// Sets up two peer namespaces behind MASQUERADE NAT with a shared relay
/// server in the internet namespace. Peer A connects to the relay, Peer B
/// connects to the same session, A sends a test block, B echoes it back,
/// and A asserts the round-trip. Any failure causes the test to panic after
/// the namespace teardown has run.
#[test]
fn nat_relay_end_to_end() {
    // Dispatch to subprocess role if invoked inside a namespace.
    if maybe_run_as_subprocess() {
        return;
    }

    // Set up namespaces. Skip gracefully if prerequisites are missing.
    let Some(harness) = NetNsHarness::setup() else {
        println!("nat_relay_end_to_end: prerequisites missing — test skipped");
        return;
    };

    let relay_addr = harness.relay_addr.clone();
    let peer_timeout = Duration::from_secs(30);

    // Spawn peer-b first, then peer-a. Peer-b blocks on recv waiting for a
    // sender. A 200 ms stagger between the two ensures b is parked in the
    // relay before a arrives, which avoids the race where the relay has not
    // yet recorded b as parked when a's session-runner fires.
    let relay_addr_b = relay_addr.clone();
    let peer_b_handle = std::thread::spawn(move || {
        run_peer_in_ns(NS_PEER_B, "peer-b", &relay_addr_b, peer_timeout)
    });

    std::thread::sleep(Duration::from_millis(200));

    let relay_addr_a = relay_addr.clone();
    let peer_a_result = run_peer_in_ns(NS_PEER_A, "peer-a", &relay_addr_a, peer_timeout);
    let peer_b_result = peer_b_handle.join().expect("peer-b thread panicked");

    // Teardown runs here via Drop regardless of outcome.
    drop(harness);

    assert!(peer_a_result, "peer-a did not complete successfully");
    assert!(peer_b_result, "peer-b did not complete successfully");
}
