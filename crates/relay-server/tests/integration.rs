#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::string_slice
)]
//! End-to-end integration tests for the relay server.
//!
//! Each test spawns a real `RelayHandle` bound to `127.0.0.1:0`, drives one or
//! more `WebSocket` clients against it over loopback `TCP`, and asserts both
//! the wire-level behaviour (frames forwarded, connection closed, etc.) and
//! the published counters.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::Duration;

use cascade_p2p::relay::RelayClient;
use cascade_relay_server::auth::encode_handshake;
use cascade_relay_server::config::{RelayConfig, SHARED_SECRET_LEN};
use cascade_relay_server::server::{RelayHandle, spawn};
use futures_util::{SinkExt, StreamExt};
use rand::RngCore;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

/// Convenience type alias for the boxed client `WebSocket` stream the
/// tokio-tungstenite client returns.
type ClientWs = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Deterministic test shared secret. Real deployments draw 32 random bytes.
const fn known_secret() -> [u8; SHARED_SECRET_LEN] {
    let mut secret = [0u8; SHARED_SECRET_LEN];
    let mut idx = 0;
    while idx < SHARED_SECRET_LEN {
        // SAFETY: idx < 32 so the cast cannot truncate.
        #[allow(clippy::cast_possible_truncation)]
        let value = idx as u8;
        secret[idx] = value.wrapping_mul(7).wrapping_add(13);
        idx += 1;
    }
    secret
}

/// Build a `RelayConfig` bound to an OS-assigned ephemeral port.
fn config_for_test(
    secret: [u8; SHARED_SECRET_LEN],
    session_timeout: Duration,
    max_sessions: u32,
) -> RelayConfig {
    RelayConfig {
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        shared_secret: secret,
        session_timeout,
        max_sessions,
        metrics_bind: None,
        announce_bind: None,
    }
}

/// Connect a `WebSocket` client to `<addr>/join/<session_id>` and send the
/// `HMAC` handshake frame with the given device/session/secret.
async fn connect_and_handshake(
    addr: SocketAddr,
    session_id: &str,
    device_id: &str,
    secret: &[u8; SHARED_SECRET_LEN],
) -> ClientWs {
    let url = format!("ws://{addr}/join/{session_id}");
    let (mut ws, _resp) = connect_async(&url)
        .await
        .unwrap_or_else(|err| panic!("connecting to {url}: {err}"));
    let frame = encode_handshake(device_id, session_id, secret).expect("encoding handshake");
    ws.send(Message::Binary(frame.into()))
        .await
        .expect("sending handshake");
    ws
}

/// Read the next binary payload from a client. Panics if the connection
/// closes or yields a non-binary frame.
async fn recv_binary(ws: &mut ClientWs) -> Vec<u8> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Binary(payload))) => return payload.to_vec(),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            Some(Ok(Message::Close(close))) => panic!("unexpected close: {close:?}"),
            Some(Ok(other)) => panic!("unexpected frame: {other:?}"),
            Some(Err(err)) => panic!("transport error: {err}"),
            None => panic!("stream ended without a binary frame"),
        }
    }
}

/// Wait until the predicate returns true, polling every 25 ms up to `timeout`.
async fn wait_until<F: FnMut() -> bool>(mut predicate: F, timeout: Duration, label: &str) {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out after {timeout:?} waiting for: {label}");
}

/// Drain pending frames after the relay forwards a close. The first frame
/// received may be the peer's close echo, or a transport-level end-of-stream.
async fn expect_server_close(ws: &mut ClientWs) {
    let deadline = tokio::time::sleep(Duration::from_secs(1));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            biased;
            () = &mut deadline => panic!("server did not close the connection within 1s"),
            frame = ws.next() => match frame {
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                Some(Ok(Message::Binary(_) | Message::Text(_) | Message::Frame(_))) => {
                    panic!("expected close, got data frame");
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            },
        }
    }
}

/// Spawn a relay with the given config and assert it is up.
async fn spawn_relay(config: RelayConfig) -> RelayHandle {
    spawn(config).await.expect("spawning relay")
}

#[tokio::test]
async fn relay_pairs_two_clients_and_shuttles_bytes() {
    let secret = known_secret();
    let handle = spawn_relay(config_for_test(secret, Duration::from_secs(30), 8)).await;

    let session_id = "test-session-1";

    // A connects and authenticates first so it is the parked side.
    let mut client_a = connect_and_handshake(handle.local_addr, session_id, "A", &secret).await;

    // Give the registry a moment to record A as parked before B arrives.
    wait_until(
        || handle.counters.auth_failures_total.load(Ordering::Relaxed) == 0,
        Duration::from_millis(250),
        "no auth failures recorded after A connects",
    )
    .await;

    let mut client_b = connect_and_handshake(handle.local_addr, session_id, "B", &secret).await;

    // The pairing happens as soon as B's session-runner notifies A. Wait
    // for the active counter to bump.
    wait_until(
        || handle.counters.sessions_active.load(Ordering::Relaxed) == 1,
        Duration::from_secs(1),
        "sessions_active reaches 1 after pair",
    )
    .await;

    // A -> B: send a 32-byte payload.
    let mut payload_ab = [0u8; 32];
    payload_ab[..12].copy_from_slice(b"Hello from A");
    client_a
        .send(Message::Binary(payload_ab.to_vec().into()))
        .await
        .expect("A sends");

    let received_b = tokio::time::timeout(Duration::from_secs(1), recv_binary(&mut client_b))
        .await
        .expect("B receives within 1s");
    assert_eq!(received_b, payload_ab.to_vec(), "B received A's payload");

    // B -> A: send a 64-byte payload.
    let mut payload_ba = [0u8; 64];
    for (idx, byte) in payload_ba.iter_mut().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let value = (idx as u8).wrapping_add(0x40);
        *byte = value;
    }
    client_b
        .send(Message::Binary(payload_ba.to_vec().into()))
        .await
        .expect("B sends");

    let received_a = tokio::time::timeout(Duration::from_secs(1), recv_binary(&mut client_a))
        .await
        .expect("A receives within 1s");
    assert_eq!(received_a, payload_ba.to_vec(), "A received B's payload");

    // Counters: one pair, no rejections, no timeouts, no auth failures.
    assert_eq!(
        handle
            .counters
            .sessions_paired_total
            .load(Ordering::Relaxed),
        1
    );
    assert_eq!(
        handle.counters.auth_failures_total.load(Ordering::Relaxed),
        0
    );
    assert_eq!(
        handle
            .counters
            .sessions_rejected_total
            .load(Ordering::Relaxed),
        0
    );
    let bytes_relayed = handle.counters.bytes_relayed_total.load(Ordering::Relaxed);
    assert_eq!(
        bytes_relayed,
        (payload_ab.len() + payload_ba.len()) as u64,
        "bytes_relayed_total matches the two payloads"
    );

    // Drop the clients and wait for `sessions_active` to settle back to zero.
    drop(client_a);
    drop(client_b);
    wait_until(
        || handle.counters.sessions_active.load(Ordering::Relaxed) == 0,
        Duration::from_secs(1),
        "sessions_active returns to zero after both clients drop",
    )
    .await;
}

#[tokio::test]
async fn relay_rejects_invalid_hmac() {
    let secret = known_secret();
    let handle = spawn_relay(config_for_test(secret, Duration::from_secs(5), 8)).await;

    // Forge a different secret — same length, different content.
    let mut wrong_secret = secret;
    for byte in &mut wrong_secret {
        *byte ^= 0xAA;
    }

    let url = format!("ws://{}/join/bad-hmac-session", handle.local_addr);
    let (mut ws, _resp) = connect_async(&url).await.expect("websocket handshake");
    let frame = encode_handshake("rogue", "bad-hmac-session", &wrong_secret)
        .expect("encoding handshake with wrong secret");
    ws.send(Message::Binary(frame.into()))
        .await
        .expect("sending bad handshake");

    expect_server_close(&mut ws).await;

    wait_until(
        || handle.counters.auth_failures_total.load(Ordering::Relaxed) >= 1,
        Duration::from_secs(1),
        "auth_failures_total >= 1",
    )
    .await;

    // No session was ever paired.
    assert_eq!(
        handle
            .counters
            .sessions_paired_total
            .load(Ordering::Relaxed),
        0
    );
}

#[tokio::test]
async fn relay_times_out_unpaired_session() {
    let secret = known_secret();
    let handle = spawn_relay(config_for_test(secret, Duration::from_millis(500), 8)).await;

    let mut client = connect_and_handshake(
        handle.local_addr,
        "lonely-session",
        "lonely-device",
        &secret,
    )
    .await;

    // No second peer arrives. The parked entry expires at +500ms; the
    // reaper runs every `max(250ms, timeout/4) = 250ms`. Sleep long
    // enough that several reaper ticks have fired post-expiry — 1500ms
    // gives four ticks of slack to survive slow CI runners.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    expect_server_close(&mut client).await;

    wait_until(
        || {
            handle
                .counters
                .sessions_timed_out_total
                .load(Ordering::Relaxed)
                >= 1
        },
        Duration::from_secs(2),
        "sessions_timed_out_total >= 1",
    )
    .await;
}

#[tokio::test]
async fn relay_pairs_under_concurrent_sessions() {
    let secret = known_secret();
    let handle = spawn_relay(config_for_test(secret, Duration::from_secs(30), 100)).await;
    let addr = handle.local_addr;

    let session_ids = ["session-a", "session-b", "session-c"];
    let mut rng = rand::rng();
    let mut payloads_ab: Vec<[u8; 100]> = Vec::with_capacity(session_ids.len());
    let mut payloads_ba: Vec<[u8; 100]> = Vec::with_capacity(session_ids.len());
    for _ in &session_ids {
        let mut ab = [0u8; 100];
        let mut ba = [0u8; 100];
        rng.fill_bytes(&mut ab);
        rng.fill_bytes(&mut ba);
        payloads_ab.push(ab);
        payloads_ba.push(ba);
    }

    // Spawn one task per session that drives both peers end-to-end.
    let mut tasks = Vec::with_capacity(session_ids.len());
    for (idx, session_id) in session_ids.iter().enumerate() {
        let secret_for_task = secret;
        let session = (*session_id).to_owned();
        let ab = payloads_ab[idx];
        let ba = payloads_ba[idx];
        tasks.push(tokio::spawn(async move {
            let mut client_a = connect_and_handshake(addr, &session, "A", &secret_for_task).await;
            // Tiny stagger so A is parked before B arrives.
            tokio::time::sleep(Duration::from_millis(10)).await;
            let mut client_b = connect_and_handshake(addr, &session, "B", &secret_for_task).await;

            client_a
                .send(Message::Binary(ab.to_vec().into()))
                .await
                .expect("A sends");
            let received_b =
                tokio::time::timeout(Duration::from_secs(2), recv_binary(&mut client_b))
                    .await
                    .expect("B receives");
            assert_eq!(
                received_b,
                ab.to_vec(),
                "B received A's payload for {session}"
            );

            client_b
                .send(Message::Binary(ba.to_vec().into()))
                .await
                .expect("B sends");
            let received_a =
                tokio::time::timeout(Duration::from_secs(2), recv_binary(&mut client_a))
                    .await
                    .expect("A receives");
            assert_eq!(
                received_a,
                ba.to_vec(),
                "A received B's payload for {session}"
            );

            drop(client_a);
            drop(client_b);
        }));
    }

    for task in tasks {
        task.await.expect("session task");
    }

    // After all six clients drop, sessions_active should return to zero and
    // sessions_paired_total should record one pair per session.
    wait_until(
        || handle.counters.sessions_active.load(Ordering::Relaxed) == 0,
        Duration::from_secs(2),
        "sessions_active settles to zero",
    )
    .await;
    assert_eq!(
        handle
            .counters
            .sessions_paired_total
            .load(Ordering::Relaxed),
        u64::try_from(session_ids.len()).expect("session count fits in u64"),
        "one pair per session"
    );
    assert_eq!(
        handle.counters.auth_failures_total.load(Ordering::Relaxed),
        0
    );
    assert_eq!(
        handle
            .counters
            .sessions_rejected_total
            .load(Ordering::Relaxed),
        0
    );
}

/// End-to-end pairing test driven through the real `RelayClient` path.
///
/// The earlier tests scaffold the handshake by hand to keep the relay
/// server's wire format honest. This test exercises the production
/// client (`cascade_p2p::relay::RelayClient::connect_with_secret`)
/// instead, proving the HMAC authentication wired into the p2p crate
/// actually meets the relay server on the wire — without this case a
/// future tweak to either side's encoding could pass each crate's
/// own unit tests while breaking the cross-crate handshake.
#[tokio::test]
async fn relay_pairs_two_real_relay_clients_through_handshake() {
    let secret = known_secret();
    let handle = spawn_relay(config_for_test(secret, Duration::from_secs(30), 8)).await;

    let session_id = "real-client-pair";

    // A connects first and parks awaiting B. Drop the `RelayConnection`
    // returns nothing useful to assert until B arrives; we just hold it
    // so the server keeps the socket open.
    let secret_for_a = secret;
    let session_a = session_id.to_owned();
    let addr = handle.local_addr;
    let task_a = tokio::spawn(async move {
        let url = format!("ws://{addr}");
        let connection =
            RelayClient::connect_with_secret(&url, &session_a, "device-A", &secret_for_a)
                .await
                .expect("device A handshake");
        // A sends a payload then reads B's reply.
        connection.send(b"hello from A").await.expect("A sends");
        let received = connection.recv().await.expect("A recv");
        assert_eq!(received, b"world from B".to_vec(), "A received B's payload");
    });

    // Tiny pause so A is parked before B arrives. Without it the server
    // can momentarily see B before A's registry entry exists.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let secret_for_b = secret;
    let session_b = session_id.to_owned();
    let addr_b = handle.local_addr;
    let task_b = tokio::spawn(async move {
        let url = format!("ws://{addr_b}");
        let connection =
            RelayClient::connect_with_secret(&url, &session_b, "device-B", &secret_for_b)
                .await
                .expect("device B handshake");
        let received = connection.recv().await.expect("B recv");
        assert_eq!(received, b"hello from A".to_vec(), "B received A's payload");
        connection.send(b"world from B").await.expect("B sends");
    });

    // Wait for both clients to finish exchanging payloads.
    let timeout = Duration::from_secs(5);
    tokio::time::timeout(timeout, async {
        task_a.await.expect("A task");
        task_b.await.expect("B task");
    })
    .await
    .expect("pairing completed within timeout");

    // Counters: one pair, no rejections.
    wait_until(
        || {
            handle
                .counters
                .sessions_paired_total
                .load(Ordering::Relaxed)
                == 1
        },
        Duration::from_secs(1),
        "sessions_paired_total reaches 1",
    )
    .await;
    assert_eq!(
        handle.counters.auth_failures_total.load(Ordering::Relaxed),
        0
    );
    assert_eq!(
        handle
            .counters
            .sessions_rejected_total
            .load(Ordering::Relaxed),
        0
    );
}

/// Reject path: a `RelayClient` with the wrong shared secret authenticates
/// against the server and the server closes the socket. The current
/// client API surfaces this asynchronously — `connect_with_secret`
/// returns `Ok` (the WebSocket upgrade and handshake send succeed) and
/// the next `recv` reports the close. We pin that behaviour here so
/// future changes that try to detect a synchronous reject have a
/// regression to point at.
#[tokio::test]
async fn relay_real_client_with_wrong_secret_is_closed_by_server() {
    let server_secret = known_secret();
    let handle = spawn_relay(config_for_test(server_secret, Duration::from_secs(5), 8)).await;

    let mut wrong_secret = server_secret;
    for byte in &mut wrong_secret {
        *byte ^= 0x5A;
    }

    let url = format!("ws://{}", handle.local_addr);
    let connection =
        RelayClient::connect_with_secret(&url, "rejected-session", "device-x", &wrong_secret)
            .await
            .expect("connect_with_secret returns Ok even when the server later rejects");

    // The server closes the socket on bad HMAC; the client surfaces
    // that on the first `recv`.
    let err = connection
        .recv()
        .await
        .expect_err("server should close the socket after bad handshake");
    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("closed")
            || rendered.contains("ended before")
            || rendered.contains("Close"),
        "expected reject-as-close, got: {rendered}"
    );

    wait_until(
        || handle.counters.auth_failures_total.load(Ordering::Relaxed) >= 1,
        Duration::from_secs(1),
        "auth_failures_total >= 1",
    )
    .await;
    assert_eq!(
        handle
            .counters
            .sessions_paired_total
            .load(Ordering::Relaxed),
        0
    );
}
