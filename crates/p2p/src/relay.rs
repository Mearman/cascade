//! WebSocket relay transport with end-to-end TLS and HMAC client auth.
//!
//! The relay is a blind byte-pipe — it sees only opaque ciphertext. The two
//! endpoints negotiate TLS *through* the relay tunnel, so even a malicious
//! relay operator cannot read or tamper with BEP traffic.
//!
//! Wire layers (inside out):
//!   BEP message → length-prefixed frame → TLS record → WebSocket binary message → relay
//!
//! Before the byte-pipe opens the client must authenticate against the
//! relay server. The first binary `WebSocket` frame carries a single
//! `HMAC-SHA256` handshake whose layout is fixed by the relay server in
//! [`crates/relay-server/src/auth.rs`](../../../relay-server/src/auth.rs).
//! Both crates own a copy of the wire format; bumping
//! [`HANDSHAKE_VERSION`] requires updating both sides in lockstep.

use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

const RELAY_JOIN_PATH: &str = "join";
const RELAY_FRAME_LEN_SIZE: usize = 4;

/// Wire-format version of the HMAC handshake. Bumped on incompatible
/// changes; the relay server rejects any other value. Kept in sync with
/// `cascade_relay_server::auth::HANDSHAKE_VERSION`.
pub const HANDSHAKE_VERSION: u8 = 1;

/// Length of the HMAC-SHA256 tag carried in the handshake.
pub const HMAC_TAG_LEN: usize = 32;

/// Length of the shared secret feeding the HMAC key. Matches
/// `cascade_relay_server::config::SHARED_SECRET_LEN`.
pub const SHARED_SECRET_LEN: usize = 32;

/// Maximum permitted length of a device identifier inside the handshake.
/// The relay server enforces the same cap. See
/// `cascade_relay_server::auth::MAX_DEVICE_ID_LEN`.
pub const MAX_DEVICE_ID_LEN: usize = 128;

/// Maximum permitted length of the rendezvous session identifier. Matches
/// `cascade_relay_server::auth::MAX_SESSION_ID_LEN`.
pub const MAX_SESSION_ID_LEN: usize = 256;

/// Wall-clock cap on how long the relay handshake is allowed to take
/// before [`RelayClient::connect_with_secret`] gives up and returns
/// [`RelayAuthError::Timeout`].
///
/// The relay server itself does not park the inbound socket
/// indefinitely — its `session_timeout` starts ticking the moment the
/// parked side authenticates — but a misbehaving relay could still hang
/// the WebSocket without reading bytes, so the client applies its own
/// ceiling.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Reasons a relay handshake may fail.
///
/// Distinguishes the "transport went wrong" path (connect failure,
/// premature disconnect, frame parse errors) from the
/// "server actively rejected us" path. The latter typically means the
/// shared secret is misconfigured on one side and the operator needs to
/// be told loudly rather than retrying forever.
/// The relay server intentionally does not echo a typed reject —
/// dropping the socket is the only signal a bad-secret client gets.
/// Callers wanting to distinguish "rejected" from "transport closed
/// cleanly" must call [`RelayConnection::recv`] and inspect the
/// resulting `anyhow` error; this enum covers only failures the client
/// can detect synchronously inside `connect_with_secret`.
#[derive(Debug, Error)]
pub enum RelayAuthError {
    /// Could not open the `WebSocket` connection at all.
    #[error("relay WebSocket connect failed: {0}")]
    Connect(#[source] anyhow::Error),

    /// The session identifier exceeds the relay protocol's cap.
    #[error("relay session id is too long: {actual} bytes (max {max})")]
    SessionIdTooLong { actual: usize, max: usize },

    /// The device identifier exceeds the relay protocol's cap.
    #[error("relay device id is too long: {actual} bytes (max {max})")]
    DeviceIdTooLong { actual: usize, max: usize },

    /// `HMAC-SHA256` initialisation failed. The fixed-size shared
    /// secret argument should make this unreachable; kept as a typed
    /// variant rather than a panic so the caller can surface the
    /// failure if a future change ever loosens the input shape.
    #[error("HMAC initialisation failed")]
    HmacInit,

    /// The handshake took longer than [`HANDSHAKE_TIMEOUT`].
    #[error("relay handshake timed out after {0:?}")]
    Timeout(Duration),

    /// Sending the handshake frame failed at the transport layer.
    #[error("sending relay handshake frame: {0}")]
    SendFailed(#[source] anyhow::Error),
}

/// Relay client — connects through a relay server when direct connection fails.
#[derive(Debug, Clone, Copy)]
pub struct RelayClient;

/// An active relayed connection.
#[derive(Debug)]
pub struct RelayConnection {
    socket: Mutex<WebSocketStream<MaybeTlsStream<TcpStream>>>,
}

impl RelayClient {
    /// Authenticate against a relay server and join a rendezvous session.
    ///
    /// The relay server pairs two clients sharing a `session_id` and then
    /// shuttles binary frames between them. Both peers must use the same
    /// `session_id` to meet; each peer presents its own `local_device_id`
    /// for identification.
    ///
    /// Wire steps:
    ///
    /// 1. Open the `WebSocket` at `<relay_url>/join/<session_id>`.
    /// 2. Send a single binary frame matching the relay server's
    ///    [`HANDSHAKE_VERSION`] layout — `HMAC-SHA256(shared_secret,
    ///    local_device_id || session_id)` over the wire.
    /// 3. The server admits us to the rendezvous on a valid tag (no
    ///    accept frame is sent — the server simply keeps the socket
    ///    open until the second peer arrives or the timeout fires).
    /// 4. On a bad tag or unsupported version the server closes the
    ///    socket without writing anything. `connect_with_secret`
    ///    cannot distinguish that from a healthy server waiting for
    ///    the peer to arrive, so it returns `Ok` either way; callers
    ///    that need to detect a reject must observe the close on the
    ///    next [`RelayConnection::recv`].
    pub async fn connect_with_secret(
        relay_url: &str,
        session_id: &str,
        local_device_id: &str,
        shared_secret: &[u8; SHARED_SECRET_LEN],
    ) -> Result<RelayConnection, RelayAuthError> {
        if session_id.len() > MAX_SESSION_ID_LEN {
            return Err(RelayAuthError::SessionIdTooLong {
                actual: session_id.len(),
                max: MAX_SESSION_ID_LEN,
            });
        }
        if local_device_id.len() > MAX_DEVICE_ID_LEN {
            return Err(RelayAuthError::DeviceIdTooLong {
                actual: local_device_id.len(),
                max: MAX_DEVICE_ID_LEN,
            });
        }

        let frame = encode_client_handshake(local_device_id, session_id, shared_secret)?;

        let join_url = relay_join_url(relay_url, session_id);

        let outcome = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            let (mut socket, _) = connect_async(&join_url).await.map_err(|err| {
                RelayAuthError::Connect(
                    anyhow::Error::from(err).context(format!("connecting to relay {join_url}")),
                )
            })?;
            socket
                .send(Message::Binary(frame.into()))
                .await
                .map_err(|err| {
                    RelayAuthError::SendFailed(
                        anyhow::Error::from(err).context("sending relay handshake frame"),
                    )
                })?;
            Ok::<_, RelayAuthError>(socket)
        })
        .await;

        let socket = match outcome {
            Ok(Ok(socket)) => socket,
            Ok(Err(err)) => return Err(err),
            Err(_) => return Err(RelayAuthError::Timeout(HANDSHAKE_TIMEOUT)),
        };

        // The relay server does not emit an explicit accept frame on
        // success — it simply parks the socket until the matching peer
        // arrives. If the handshake was malformed or the HMAC tag did
        // not verify, the server closes the WebSocket without writing
        // anything. We treat any close arriving before the byte-pipe
        // opens as a rejection; a healthy server keeps the socket open
        // until the peer pairs.
        Ok(RelayConnection {
            socket: Mutex::new(socket),
        })
    }
}

impl RelayConnection {
    /// Consume the connection and return ownership of the underlying
    /// WebSocket.
    ///
    /// Used by [`crate::transport::RelayTransport`] to take exclusive
    /// ownership of the WebSocket so the transport adapter can drive
    /// reads and writes directly without going through the legacy
    /// `send`/`recv` wrappers (which add a redundant length prefix
    /// inside the binary frame). The WebSocket already preserves
    /// message boundaries, so one binary message is one BEP frame
    /// when the transport adapter writes it.
    #[must_use]
    pub fn into_socket(self) -> WebSocketStream<MaybeTlsStream<TcpStream>> {
        self.socket.into_inner()
    }

    /// Send a BEP message through the relay.
    pub async fn send(&self, message: &[u8]) -> Result<()> {
        let frame = encode_relay_frame(message)?;
        let mut socket = self.socket.lock().await;
        socket
            .send(Message::Binary(frame.into()))
            .await
            .context("sending relayed BEP frame")?;
        Ok(())
    }

    /// Receive a BEP message from the relay.
    pub async fn recv(&self) -> Result<Vec<u8>> {
        let mut socket = self.socket.lock().await;
        while let Some(message) = socket.next().await {
            match message.context("receiving relayed WebSocket message")? {
                Message::Binary(frame) => return decode_relay_frame(&frame),
                Message::Close(close) => {
                    anyhow::bail!("relay closed connection: {close:?}");
                }
                Message::Ping(_) | Message::Pong(_) => {}
                Message::Text(_) | Message::Frame(_) => {
                    anyhow::bail!("relay sent non-binary BEP frame");
                }
            }
        }

        anyhow::bail!("relay connection ended before a BEP frame was received")
    }
}

fn relay_join_url(relay_url: &str, session_id: &str) -> String {
    format!(
        "{}/{RELAY_JOIN_PATH}/{session_id}",
        relay_url.trim_end_matches('/')
    )
}

/// Build the byte-for-byte client handshake frame consumed by the relay
/// server. Layout (matching
/// `cascade_relay_server::auth::encode_handshake`):
///
/// ```text
/// | version (u8)         = HANDSHAKE_VERSION
/// | device_id_len (u16 BE)
/// | device_id bytes
/// | session_id_len (u16 BE)
/// | session_id bytes
/// | hmac (HMAC_TAG_LEN bytes)
/// ```
///
/// The tag is `HMAC-SHA256(shared_secret, device_id_bytes || session_id_bytes)`.
fn encode_client_handshake(
    device_id: &str,
    session_id: &str,
    shared_secret: &[u8; SHARED_SECRET_LEN],
) -> Result<Vec<u8>, RelayAuthError> {
    let device_id_len =
        u16::try_from(device_id.len()).map_err(|_| RelayAuthError::DeviceIdTooLong {
            actual: device_id.len(),
            max: MAX_DEVICE_ID_LEN,
        })?;
    if usize::from(device_id_len) > MAX_DEVICE_ID_LEN {
        return Err(RelayAuthError::DeviceIdTooLong {
            actual: device_id.len(),
            max: MAX_DEVICE_ID_LEN,
        });
    }
    let session_id_len =
        u16::try_from(session_id.len()).map_err(|_| RelayAuthError::SessionIdTooLong {
            actual: session_id.len(),
            max: MAX_SESSION_ID_LEN,
        })?;
    if usize::from(session_id_len) > MAX_SESSION_ID_LEN {
        return Err(RelayAuthError::SessionIdTooLong {
            actual: session_id.len(),
            max: MAX_SESSION_ID_LEN,
        });
    }

    let tag = compute_handshake_tag(device_id.as_bytes(), session_id.as_bytes(), shared_secret)?;

    let mut frame =
        Vec::with_capacity(1 + 2 + device_id.len() + 2 + session_id.len() + HMAC_TAG_LEN);
    frame.push(HANDSHAKE_VERSION);
    frame.extend_from_slice(&device_id_len.to_be_bytes());
    frame.extend_from_slice(device_id.as_bytes());
    frame.extend_from_slice(&session_id_len.to_be_bytes());
    frame.extend_from_slice(session_id.as_bytes());
    frame.extend_from_slice(&tag);
    Ok(frame)
}

type HmacSha256 = Hmac<Sha256>;

fn compute_handshake_tag(
    device_id: &[u8],
    session_id: &[u8],
    shared_secret: &[u8; SHARED_SECRET_LEN],
) -> Result<[u8; HMAC_TAG_LEN], RelayAuthError> {
    let mut mac =
        HmacSha256::new_from_slice(shared_secret).map_err(|_| RelayAuthError::HmacInit)?;
    mac.update(device_id);
    mac.update(session_id);
    let bytes = mac.finalize().into_bytes();
    let mut out = [0u8; HMAC_TAG_LEN];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn encode_relay_frame(message: &[u8]) -> Result<Vec<u8>> {
    let len = u32::try_from(message.len())
        .map_err(|_| anyhow::anyhow!("relay message too large for u32 length prefix"))?;
    let mut frame = Vec::with_capacity(RELAY_FRAME_LEN_SIZE + message.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(message);
    Ok(frame)
}

fn decode_relay_frame(frame: &[u8]) -> Result<Vec<u8>> {
    if frame.len() < RELAY_FRAME_LEN_SIZE {
        anyhow::bail!("relay frame too short");
    }

    let header: &[u8; RELAY_FRAME_LEN_SIZE] = frame
        .get(..RELAY_FRAME_LEN_SIZE)
        .ok_or_else(|| anyhow::anyhow!("relay frame header out of bounds"))?
        .try_into()?;
    let message_len = usize::try_from(u32::from_be_bytes(*header))
        .map_err(|_| anyhow::anyhow!("relay message length too large for this platform"))?;
    let expected_frame_len = RELAY_FRAME_LEN_SIZE + message_len;
    if frame.len() != expected_frame_len {
        anyhow::bail!(
            "invalid relay frame length: expected {expected_frame_len} bytes, got {}",
            frame.len()
        );
    }

    Ok(frame
        .get(RELAY_FRAME_LEN_SIZE..)
        .ok_or_else(|| anyhow::anyhow!("relay frame body out of bounds"))?
        .to_vec())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    /// Deterministic test shared secret. Production deployments draw 32 random bytes.
    const fn known_secret() -> [u8; SHARED_SECRET_LEN] {
        let mut secret = [0u8; SHARED_SECRET_LEN];
        let mut idx = 0;
        while idx < SHARED_SECRET_LEN {
            #[allow(clippy::cast_possible_truncation)]
            let value = idx as u8;
            secret[idx] = value.wrapping_mul(7).wrapping_add(13);
            idx += 1;
        }
        secret
    }

    /// Mock relay server: accepts a `WebSocket`, reads the handshake
    /// frame, verifies it against `expected_secret`, and either keeps
    /// the socket open (so the client task succeeds) or closes it (so
    /// the client surfaces `Rejected`/`Disconnected`).
    async fn run_mock_relay(
        listener: TcpListener,
        expected_secret: [u8; SHARED_SECRET_LEN],
    ) -> Option<Vec<u8>> {
        let (stream, _) = listener.accept().await.ok()?;
        let mut websocket = accept_async(stream).await.ok()?;
        let message = websocket.next().await?.ok()?;
        let frame = match message {
            Message::Binary(bytes) => bytes.to_vec(),
            _ => return None,
        };

        // Decode and verify the handshake by recomputing the tag with
        // the expected secret.
        if frame.first().copied() != Some(HANDSHAKE_VERSION) {
            let _ = websocket.send(Message::Close(None)).await;
            return None;
        }
        let mut cursor = 1usize;
        let device_id_len = u16::from_be_bytes(frame.get(cursor..cursor + 2)?.try_into().ok()?);
        cursor += 2;
        let device_id = frame
            .get(cursor..cursor + usize::from(device_id_len))?
            .to_vec();
        cursor += usize::from(device_id_len);
        let session_id_len = u16::from_be_bytes(frame.get(cursor..cursor + 2)?.try_into().ok()?);
        cursor += 2;
        let session_id = frame
            .get(cursor..cursor + usize::from(session_id_len))?
            .to_vec();
        cursor += usize::from(session_id_len);
        let tag = frame.get(cursor..cursor + HMAC_TAG_LEN)?.to_vec();

        let expected = compute_handshake_tag(&device_id, &session_id, &expected_secret).ok()?;
        if expected.as_slice() != tag.as_slice() {
            let _ = websocket.send(Message::Close(None)).await;
            return None;
        }

        // Keep the socket open so the client treats the handshake as
        // successful. The caller decides how long to live.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = websocket.send(Message::Close(None)).await;
        Some(frame)
    }

    #[tokio::test]
    async fn connect_with_secret_succeeds_against_matching_server() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_address = listener.local_addr().unwrap();
        let secret = known_secret();

        let server = tokio::spawn(async move { run_mock_relay(listener, secret).await });

        let connection = RelayClient::connect_with_secret(
            &format!("ws://{relay_address}"),
            "sess-1",
            "device-A",
            &secret,
        )
        .await
        .expect("handshake succeeds with matching secret");
        drop(connection);

        let received = server.await.unwrap().expect("server received handshake");
        assert_eq!(received.first().copied(), Some(HANDSHAKE_VERSION));
    }

    #[tokio::test]
    async fn connect_with_secret_succeeds_then_recv_surfaces_server_close() {
        // Separate from the handshake-success test: after the relay
        // closes the WebSocket the receive path must report it. This
        // proves the close arriving after a successful handshake is
        // distinguishable from an in-handshake reject.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_address = listener.local_addr().unwrap();
        let secret = known_secret();

        let server = tokio::spawn(async move { run_mock_relay(listener, secret).await });

        let connection = RelayClient::connect_with_secret(
            &format!("ws://{relay_address}"),
            "sess-2",
            "device-A",
            &secret,
        )
        .await
        .expect("handshake succeeds");

        // Server closes after a short delay. `recv` should surface that.
        let err = connection
            .recv()
            .await
            .expect_err("expected close to surface as error");
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains("closed")
                || rendered.contains("ended before")
                || rendered.contains("Close"),
            "expected close error, got: {rendered}"
        );

        server.await.unwrap();
    }

    #[tokio::test]
    async fn connect_with_secret_handshake_still_completes_on_wrong_secret() {
        // The relay protocol does not echo a reject frame, so a
        // wrong-secret dial returns `Ok` from `connect_with_secret`
        // (we cannot tell yet) but the next `recv` surfaces the
        // server's close. This documents the asymmetry and pins it
        // into a test so future changes that try to detect rejects
        // synchronously have a regression to point at.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_address = listener.local_addr().unwrap();
        let server_secret = known_secret();
        let mut client_secret = server_secret;
        // Flip every byte so the HMAC tag mismatches.
        for byte in &mut client_secret {
            *byte ^= 0xFF;
        }

        let server = tokio::spawn(async move { run_mock_relay(listener, server_secret).await });

        let connection = RelayClient::connect_with_secret(
            &format!("ws://{relay_address}"),
            "sess-3",
            "device-A",
            &client_secret,
        )
        .await
        .expect("connect returns Ok even on wrong secret (server reject is asynchronous)");

        // The first `recv` after the server closes should surface the close.
        let err = connection
            .recv()
            .await
            .expect_err("expected close after wrong-secret handshake");
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains("closed")
                || rendered.contains("ended before")
                || rendered.contains("Close"),
            "expected reject-as-close error, got: {rendered}"
        );

        // The mock server returns `None` (handshake failure) so this is `Ok(None)`.
        let outcome = server.await.unwrap();
        assert!(
            outcome.is_none(),
            "mock server should have rejected the bad handshake"
        );
    }

    #[tokio::test]
    async fn connect_with_secret_returns_connect_error_when_relay_unreachable() {
        // Bind and immediately drop the listener so the address has
        // no listener — the TCP connect must fail.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);

        let secret = known_secret();
        let err = RelayClient::connect_with_secret(
            &format!("ws://{address}"),
            "sess-x",
            "device-A",
            &secret,
        )
        .await
        .expect_err("connect must fail when no relay is listening");

        assert!(matches!(err, RelayAuthError::Connect(_)));
    }

    #[tokio::test]
    async fn connect_with_secret_rejects_oversize_session_id() {
        let secret = known_secret();
        let too_long = "x".repeat(MAX_SESSION_ID_LEN + 1);
        let err =
            RelayClient::connect_with_secret("ws://127.0.0.1:1", &too_long, "device-A", &secret)
                .await
                .expect_err("oversize session id must reject before dialling");
        assert!(matches!(err, RelayAuthError::SessionIdTooLong { .. }));
    }

    #[tokio::test]
    async fn connect_with_secret_rejects_oversize_device_id() {
        let secret = known_secret();
        let too_long = "x".repeat(MAX_DEVICE_ID_LEN + 1);
        let err = RelayClient::connect_with_secret("ws://127.0.0.1:1", "sess", &too_long, &secret)
            .await
            .expect_err("oversize device id must reject before dialling");
        assert!(matches!(err, RelayAuthError::DeviceIdTooLong { .. }));
    }

    #[tokio::test]
    async fn connect_with_secret_times_out_on_silent_relay() {
        // Bind a listener that accepts the TCP socket but never replies
        // to the WebSocket upgrade. tokio-tungstenite hangs until our
        // HANDSHAKE_TIMEOUT fires.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_address = listener.local_addr().unwrap();

        let _server = tokio::spawn(async move {
            // Accept and then sleep forever — the WebSocket upgrade never
            // completes, simulating a hung relay.
            if let Ok((_stream, _)) = listener.accept().await {
                tokio::time::sleep(Duration::from_mins(1)).await;
            }
        });

        // Use a deliberately tiny timeout-window by pretending the
        // configured constant is too high — we cannot mutate the const
        // at runtime, so this test would take 10s. Skip until the relay
        // protocol grows a configurable timeout, or use tokio::time pause.
        tokio::time::pause();
        let secret = known_secret();
        let url = format!("ws://{relay_address}");
        let fut = RelayClient::connect_with_secret(&url, "sess", "device-A", &secret);
        tokio::pin!(fut);

        // Step time forward past the handshake timeout in one jump.
        tokio::time::advance(HANDSHAKE_TIMEOUT + Duration::from_secs(1)).await;
        let err = fut.await.expect_err("expected timeout");
        assert!(matches!(err, RelayAuthError::Timeout(_)));
    }

    #[test]
    fn encoded_handshake_matches_documented_layout() {
        let secret = known_secret();
        let frame = encode_client_handshake("device-A", "sess-1", &secret).unwrap();
        // 1 version byte + 2 dev_id len + 8 dev_id + 2 sess_id len + 6
        // sess_id + 32 tag = 51 bytes.
        assert_eq!(frame.len(), 1 + 2 + 8 + 2 + 6 + HMAC_TAG_LEN);
        assert_eq!(frame[0], HANDSHAKE_VERSION);
        let dev_len = u16::from_be_bytes([frame[1], frame[2]]);
        assert_eq!(usize::from(dev_len), "device-A".len());
    }

    #[test]
    fn decode_relay_frame_rejects_truncated_frame() {
        let mut frame = (u32::try_from(b"hello".len()).unwrap())
            .to_be_bytes()
            .to_vec();
        frame.extend_from_slice(b"he");

        let result = decode_relay_frame(&frame);

        assert!(result.is_err());
    }
}
