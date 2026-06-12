//! Unified frame-level transport for BEP message exchange.
//!
//! BEP messages are produced by [`crate::protocol::encode_message`] as
//! complete length-prefixed byte frames: the first four bytes carry the
//! body length and the remainder is the encoded message body. Three
//! lower-level wire technologies need to carry those frames in this
//! workspace:
//!
//! - direct TLS over TCP (via [`tokio_rustls::TlsStream`]),
//! - WebSocket-via-relay (via [`crate::relay::RelayConnection`]),
//! - UDP after a successful hole punch (via the socket owned by
//!   [`crate::traversal::UdpPunchTransport`]).
//!
//! The [`Transport`] trait unifies the three so the BEP session loop
//! does not need to know which underlying technology is in play. Each
//! adapter splits into a [`TransportReader`] / [`TransportWriter`] pair
//! so the session can drive a concurrent reader task and a writer task
//! independently — the existing direct-TLS path already follows that
//! pattern and the new relay/UDP paths match it.
//!
//! `Transport` operates at the frame level, not the byte level. Each
//! [`TransportWriter::send_frame`] call sends one complete BEP frame
//! and each [`TransportReader::recv_frame`] call returns one complete
//! BEP frame. Datagram-oriented adapters (relay, UDP) map one frame
//! to one underlying message; the stream-oriented TLS adapter reads
//! the length prefix and assembles the body into a single buffer
//! before returning. Either way the [`crate::framed::FramedSession`]
//! wrapper above this trait can drive a session over any of them.
//!
//! ## Why frame-level, not byte-level
//!
//! `AsyncRead + AsyncWrite` would be the obvious unification, but it
//! does not fit relay or UDP. Both are datagram-oriented and have
//! their own message boundaries: a relay binary frame is one BEP
//! message, and a UDP datagram is one BEP message. Forcing
//! byte-stream semantics on top would mean re-splitting a stream the
//! lower layer already split — wasted work and a fertile source of
//! framing bugs (split BEP frames across datagram boundaries, etc).
//! Working at the frame level keeps the abstraction honest.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::Mutex;
use tokio_rustls::TlsStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::framed::MAX_FRAME_BODY;
use crate::relay::RelayConnection;

/// Read half of a frame-level transport.
///
/// Each [`Self::recv_frame`] call returns exactly one BEP frame in
/// the `[4-byte big-endian length][body]` shape expected by
/// [`crate::protocol::decode_message`]. Returns `Ok(None)` on clean
/// transport close so the session loop can distinguish a graceful
/// shutdown from a real error.
#[async_trait]
pub trait TransportReader: Send {
    /// Receive one BEP frame.
    ///
    /// Returns `Ok(None)` when the peer closes the underlying
    /// transport cleanly.
    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>>;
}

/// Write half of a frame-level transport.
///
/// Each [`Self::send_frame`] call sends one complete BEP frame —
/// the bytes are the output of [`crate::protocol::encode_message`].
#[async_trait]
pub trait TransportWriter: Send {
    /// Send one BEP frame. `frame` is the complete length-prefixed
    /// encoded message.
    async fn send_frame(&mut self, frame: &[u8]) -> Result<()>;

    /// Shut down the write side of the underlying transport.
    ///
    /// Best-effort: callers treat shutdown errors as informational.
    /// Datagram-oriented transports (UDP, relay-via-drop) are
    /// expected to no-op here.
    async fn shutdown(&mut self) -> Result<()>;
}

/// Frame-level transport that splits into independent reader and
/// writer halves.
///
/// Implementations carry one [`crate::protocol::encode_message`]-shaped
/// frame per call. The pair of halves can be driven concurrently from
/// separate tasks — the direct-TLS session loop already does this,
/// and the new relay/UDP paths follow the same shape.
pub trait Transport: Send {
    /// Reader half type.
    type Reader: TransportReader + Send + 'static;
    /// Writer half type.
    type Writer: TransportWriter + Send + 'static;

    /// Split into independent read and write halves.
    fn split(self) -> (Self::Reader, Self::Writer);
}

// ── TLS adapter ──

/// Adapter wrapping a direct TLS stream as a [`Transport`].
///
/// The TLS stream is byte-oriented, so the reader half consumes the
/// four length-prefix bytes, validates the body length against
/// [`MAX_FRAME_BODY`], and assembles the full frame in a single
/// allocation before returning it. The writer half writes the
/// caller-provided frame verbatim (its length prefix is already
/// present) and flushes before returning.
#[derive(Debug)]
pub struct TlsTransport {
    stream: TlsStream<TcpStream>,
}

impl TlsTransport {
    /// Wrap a fully-handshaken TLS stream.
    #[must_use]
    pub const fn new(stream: TlsStream<TcpStream>) -> Self {
        Self { stream }
    }
}

impl Transport for TlsTransport {
    type Reader = TlsTransportReader;
    type Writer = TlsTransportWriter;

    fn split(self) -> (Self::Reader, Self::Writer) {
        let (reader, writer) = tokio::io::split(self.stream);
        (
            TlsTransportReader { inner: reader },
            TlsTransportWriter { inner: writer },
        )
    }
}

/// Read half of [`TlsTransport`].
#[derive(Debug)]
pub struct TlsTransportReader {
    inner: ReadHalf<TlsStream<TcpStream>>,
}

#[async_trait]
impl TransportReader for TlsTransportReader {
    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>> {
        let mut len_buf = [0u8; 4];
        match self.inner.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e).context("reading TLS BEP frame length"),
        }
        let body_len = u32::from_be_bytes(len_buf) as usize;
        if body_len > MAX_FRAME_BODY {
            anyhow::bail!("BEP frame body length {body_len} exceeds limit {MAX_FRAME_BODY}");
        }
        let mut frame = vec![0u8; 4 + body_len];
        let (header, body) = frame.split_at_mut(4);
        header.copy_from_slice(&len_buf);
        self.inner
            .read_exact(body)
            .await
            .context("reading TLS BEP frame body")?;
        Ok(Some(frame))
    }
}

/// Write half of [`TlsTransport`].
#[derive(Debug)]
pub struct TlsTransportWriter {
    inner: WriteHalf<TlsStream<TcpStream>>,
}

#[async_trait]
impl TransportWriter for TlsTransportWriter {
    async fn send_frame(&mut self, frame: &[u8]) -> Result<()> {
        self.inner
            .write_all(frame)
            .await
            .context("writing TLS BEP frame")?;
        self.inner.flush().await.context("flushing TLS BEP frame")?;
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        self.inner
            .shutdown()
            .await
            .context("shutting down TLS transport")
    }
}

// ── Relay adapter ──

/// Adapter wrapping a [`RelayConnection`] as a [`Transport`].
///
/// One WebSocket binary message carries one BEP frame. The relay
/// client already preserves message boundaries — both the existing
/// [`RelayConnection::send`] / [`RelayConnection::recv`] surface and
/// the underlying tungstenite [`WebSocketStream`] do — so the
/// adapter is a one-frame-per-call passthrough.
///
/// The split moves ownership of the WebSocket out of the
/// [`RelayConnection`] wrapper and into an [`Arc<Mutex<...>>`] shared
/// by the reader and writer halves. tungstenite needs `&mut`
/// concurrency for `send` and `next`, so the mutex is unavoidable.
/// Practical impact is bounded: a single read or write at a time
/// matches the WebSocket protocol's framing requirement anyway.
pub struct RelayTransport {
    socket: Arc<Mutex<WebSocketStream<MaybeTlsStream<TcpStream>>>>,
}

impl std::fmt::Debug for RelayTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayTransport").finish_non_exhaustive()
    }
}

impl RelayTransport {
    /// Wrap an authenticated relay connection.
    ///
    /// Consumes the [`RelayConnection`] so the underlying WebSocket
    /// is owned exclusively by the transport. The relay client API
    /// for sending the second-layer length prefix is bypassed —
    /// each WebSocket binary message *is* one BEP frame.
    pub fn new(connection: RelayConnection) -> Self {
        // RelayConnection's internal mutex is exactly the shape we
        // want — move it out by destructuring via the public method
        // is impossible without changing the type, so just wrap the
        // connection itself. Sending and receiving go through
        // RelayConnection's send/recv methods which add and strip
        // their own length prefix; that prefix is harmless (the BEP
        // frame already carries its own length, but the relay's
        // prefix lives on the wire wrapper, not inside the frame).
        Self {
            socket: Arc::new(Mutex::new(connection.into_socket())),
        }
    }
}

impl Transport for RelayTransport {
    type Reader = RelayTransportReader;
    type Writer = RelayTransportWriter;

    fn split(self) -> (Self::Reader, Self::Writer) {
        (
            RelayTransportReader {
                socket: Arc::clone(&self.socket),
            },
            RelayTransportWriter {
                socket: self.socket,
            },
        )
    }
}

/// Read half of [`RelayTransport`].
pub struct RelayTransportReader {
    socket: Arc<Mutex<WebSocketStream<MaybeTlsStream<TcpStream>>>>,
}

impl std::fmt::Debug for RelayTransportReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayTransportReader")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl TransportReader for RelayTransportReader {
    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>> {
        let mut socket = self.socket.lock().await;
        while let Some(message) = socket.next().await {
            let msg = message.context("receiving relay WebSocket message")?;
            match msg {
                Message::Binary(bytes) => return Ok(Some(bytes.to_vec())),
                Message::Close(_) => return Ok(None),
                Message::Ping(_) | Message::Pong(_) => {
                    // Skip keep-alives and wait for the next message.
                }
                Message::Text(_) | Message::Frame(_) => {
                    anyhow::bail!("relay sent non-binary BEP frame")
                }
            }
        }
        // Stream ended without a close frame — treat as clean EOF.
        Ok(None)
    }
}

/// Write half of [`RelayTransport`].
pub struct RelayTransportWriter {
    socket: Arc<Mutex<WebSocketStream<MaybeTlsStream<TcpStream>>>>,
}

impl std::fmt::Debug for RelayTransportWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayTransportWriter")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl TransportWriter for RelayTransportWriter {
    async fn send_frame(&mut self, frame: &[u8]) -> Result<()> {
        let mut socket = self.socket.lock().await;
        socket
            .send(Message::Binary(frame.to_vec().into()))
            .await
            .context("sending relay BEP frame")
    }

    async fn shutdown(&mut self) -> Result<()> {
        let mut socket = self.socket.lock().await;
        // Best-effort close — dropping the WebSocket is also valid.
        let _ = socket.close(None).await;
        Ok(())
    }
}

// ── UDP adapter (post-punch) ──

/// Adapter wrapping a punched UDP flow as a [`Transport`].
///
/// After a successful hole punch the caller holds the bound
/// [`UdpSocket`] (via [`crate::traversal::UdpPunchTransport`]) and the
/// peer's confirmed [`SocketAddr`]. The adapter sends each BEP frame
/// as one UDP datagram and matches each inbound datagram from the
/// remote address back to one BEP frame.
///
/// Datagrams from any other source are silently discarded — a real
/// post-punch flow may pick up stray traffic from other peers or
/// stale probes on the same bound port, and aborting the session on
/// the first stray packet would be brittle. Datagrams from the
/// matching source whose declared length does not match their actual
/// size are also discarded — a malformed frame from the right peer
/// is still better treated as "ignore and continue" than "tear down
/// the session". A `body_len` over [`MAX_FRAME_BODY`] is rejected
/// as a defence against allocation amplification.
///
/// Note: BEP frames exceeding the UDP MTU (~1472 bytes on a typical
/// Ethernet link, ~9000 bytes on jumbo frames) will be fragmented at
/// the IP layer, and any fragment loss drops the whole datagram. This
/// adapter is suitable for control traffic and small block transfers;
/// production deployments that need large block transfer over a
/// punched flow are expected to layer a reliable stream on top in a
/// later round (QUIC or DTLS+SCTP). The trait surface does not
/// change.
#[derive(Debug)]
pub struct UdpFlowTransport {
    socket: Arc<UdpSocket>,
    remote: SocketAddr,
}

impl UdpFlowTransport {
    /// Wrap the punched flow's socket and confirmed remote endpoint.
    ///
    /// Both arguments come from the [`crate::traversal::EstablishedFlow`]
    /// the hole-punch state machine returns plus the
    /// [`crate::traversal::UdpPunchTransport`] the caller drove the
    /// state machine over.
    #[must_use]
    pub const fn new(socket: Arc<UdpSocket>, remote: SocketAddr) -> Self {
        Self { socket, remote }
    }
}

impl Transport for UdpFlowTransport {
    type Reader = UdpFlowTransportReader;
    type Writer = UdpFlowTransportWriter;

    fn split(self) -> (Self::Reader, Self::Writer) {
        (
            UdpFlowTransportReader {
                socket: Arc::clone(&self.socket),
                remote: self.remote,
            },
            UdpFlowTransportWriter {
                socket: self.socket,
                remote: self.remote,
            },
        )
    }
}

/// Read half of [`UdpFlowTransport`].
#[derive(Debug)]
pub struct UdpFlowTransportReader {
    socket: Arc<UdpSocket>,
    remote: SocketAddr,
}

#[async_trait]
impl TransportReader for UdpFlowTransportReader {
    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>> {
        let mut buf = vec![0u8; 4 + MAX_FRAME_BODY];
        loop {
            let (read, from) = self
                .socket
                .recv_from(&mut buf)
                .await
                .context("receiving UDP BEP frame")?;
            if from != self.remote {
                tracing::trace!(
                    %from,
                    expected = %self.remote,
                    "discarding UDP datagram from unexpected source",
                );
                continue;
            }
            if read < 4 {
                tracing::trace!(
                    bytes = read,
                    "discarding short UDP datagram (no frame length prefix)",
                );
                continue;
            }
            // The buffer is sized to `4 + MAX_FRAME_BODY`, so the
            // `..4` and `..read` slices are trivially in bounds — but
            // express the extraction via `split_at` so the
            // workspace's `indexing_slicing` lint accepts the code
            // without an allow escape.
            let (header_slice, _rest) = buf.split_at(4);
            let mut len_buf = [0u8; 4];
            len_buf.copy_from_slice(header_slice);
            let body_len = u32::from_be_bytes(len_buf) as usize;
            if body_len > MAX_FRAME_BODY {
                anyhow::bail!(
                    "UDP BEP frame body length {body_len} exceeds limit {MAX_FRAME_BODY}",
                );
            }
            let expected = 4 + body_len;
            if read != expected {
                tracing::trace!(
                    actual = read,
                    expected,
                    "discarding UDP datagram whose length header does not match its size",
                );
                continue;
            }
            let (frame_slice, _tail) = buf.split_at(read);
            return Ok(Some(frame_slice.to_vec()));
        }
    }
}

/// Write half of [`UdpFlowTransport`].
#[derive(Debug)]
pub struct UdpFlowTransportWriter {
    socket: Arc<UdpSocket>,
    remote: SocketAddr,
}

#[async_trait]
impl TransportWriter for UdpFlowTransportWriter {
    async fn send_frame(&mut self, frame: &[u8]) -> Result<()> {
        let sent = self
            .socket
            .send_to(frame, self.remote)
            .await
            .with_context(|| format!("sending UDP BEP frame to {}", self.remote))?;
        if sent != frame.len() {
            anyhow::bail!(
                "short UDP BEP frame write: sent {sent} of {} bytes",
                frame.len()
            );
        }
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        // UDP has no graceful shutdown — drop the socket reference and
        // let the OS reclaim the binding when the last clone is gone.
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    use tokio::sync::mpsc;

    /// In-memory `Transport` pair backed by `mpsc` channels.
    ///
    /// One side's `send_frame` lands in the other side's `recv_frame` queue.
    /// Both sides shut down by dropping their senders. Used by unit tests for
    /// the BEP session loop — no sockets involved.
    #[derive(Debug)]
    pub struct ChannelTransport {
        tx: mpsc::UnboundedSender<Vec<u8>>,
        rx: mpsc::UnboundedReceiver<Vec<u8>>,
    }

    impl ChannelTransport {
        #[must_use]
        pub fn pair() -> (Self, Self) {
            let (a_tx, b_rx) = mpsc::unbounded_channel();
            let (b_tx, a_rx) = mpsc::unbounded_channel();
            (Self { tx: a_tx, rx: a_rx }, Self { tx: b_tx, rx: b_rx })
        }
    }

    impl Transport for ChannelTransport {
        type Reader = ChannelTransportReader;
        type Writer = ChannelTransportWriter;

        fn split(self) -> (Self::Reader, Self::Writer) {
            (
                ChannelTransportReader { rx: self.rx },
                ChannelTransportWriter { tx: self.tx },
            )
        }
    }

    #[derive(Debug)]
    pub struct ChannelTransportReader {
        rx: mpsc::UnboundedReceiver<Vec<u8>>,
    }

    #[async_trait]
    impl TransportReader for ChannelTransportReader {
        async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>> {
            Ok(self.rx.recv().await)
        }
    }

    #[derive(Debug)]
    pub struct ChannelTransportWriter {
        tx: mpsc::UnboundedSender<Vec<u8>>,
    }

    #[async_trait]
    impl TransportWriter for ChannelTransportWriter {
        async fn send_frame(&mut self, frame: &[u8]) -> Result<()> {
            self.tx
                .send(frame.to_vec())
                .map_err(|_| anyhow::anyhow!("channel transport closed"))
        }

        async fn shutdown(&mut self) -> Result<()> {
            Ok(())
        }
    }

    /// Two channel transports must round-trip frames bidirectionally.
    #[tokio::test]
    async fn channel_pair_round_trips_frames() {
        let (a, b) = ChannelTransport::pair();
        let (mut a_r, mut a_w) = a.split();
        let (mut b_r, mut b_w) = b.split();

        a_w.send_frame(b"hello").await.unwrap();
        let got = b_r.recv_frame().await.unwrap().unwrap();
        assert_eq!(got, b"hello");

        b_w.send_frame(b"world").await.unwrap();
        let got = a_r.recv_frame().await.unwrap().unwrap();
        assert_eq!(got, b"world");
    }

    /// Dropping the writer surfaces `Ok(None)` on the matching reader —
    /// the contract for graceful EOF.
    #[tokio::test]
    async fn channel_pair_eof_on_drop() {
        let (a, b) = ChannelTransport::pair();
        let (_a_r, a_w) = a.split();
        let (mut b_r, _b_w) = b.split();
        // Drop A's writer — recv on B sees EOF.
        drop(a_w);
        let got = b_r.recv_frame().await.unwrap();
        assert!(got.is_none(), "expected None on clean EOF");
    }

    /// `UdpFlowTransport` round-trips a frame on loopback. The remote
    /// address filter must accept the matching peer and discard any
    /// datagram from a third party. We do not synthesise a third-
    /// party packet here — the dedicated test below covers that —
    /// but this exercises the happy path end-to-end with a real UDP
    /// stack.
    #[tokio::test]
    async fn udp_flow_round_trips_one_frame() {
        let a = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let b = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let a_addr = a.local_addr().unwrap();
        let b_addr = b.local_addr().unwrap();
        let a_t = UdpFlowTransport::new(a, b_addr);
        let b_t = UdpFlowTransport::new(b, a_addr);
        let (_a_r, mut a_w) = a_t.split();
        let (mut b_r, _b_w) = b_t.split();

        let frame = build_synthetic_frame(b"ping payload");
        a_w.send_frame(&frame).await.unwrap();
        let got = b_r.recv_frame().await.unwrap().unwrap();
        assert_eq!(got, frame);
    }

    /// A datagram from an unexpected source must be silently discarded;
    /// the next datagram from the correct source must still arrive.
    /// Without the filter, the session would receive frames from any
    /// peer that happens to send to the same bound port.
    #[tokio::test]
    async fn udp_flow_filters_stray_source() {
        let a = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let stray = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let b = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let a_addr = a.local_addr().unwrap();
        let b_addr = b.local_addr().unwrap();

        let b_t = UdpFlowTransport::new(b, a_addr);
        let (mut b_r, _b_w) = b_t.split();
        let stray_frame = build_synthetic_frame(b"stray");
        let real_frame = build_synthetic_frame(b"real");
        // Send the stray first so it is on the queue before the real
        // frame arrives.
        stray.send_to(&stray_frame, b_addr).await.unwrap();
        a.send_to(&real_frame, b_addr).await.unwrap();

        let got = b_r.recv_frame().await.unwrap().unwrap();
        assert_eq!(got, real_frame, "the stray frame must be discarded");
    }

    /// Build a length-prefixed payload that resembles the on-wire
    /// shape `encode_message` produces — the prefix is the body
    /// length, the body is arbitrary bytes for this test.
    fn build_synthetic_frame(body: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(4 + body.len());
        let len = u32::try_from(body.len()).unwrap();
        frame.extend_from_slice(&len.to_be_bytes());
        frame.extend_from_slice(body);
        frame
    }

    /// `RelayTransport` wraps a real WebSocket pair and round-trips a
    /// BEP frame end to end through `FramedSession`. A TCP listener +
    /// tungstenite accept/connect produces a genuine WebSocket on
    /// loopback; the server echoes binary frames back so the client's
    /// `FramedSession<RelayTransport>` can send and receive without a
    /// full relay server.
    #[tokio::test]
    async fn relay_transport_round_trips_bep_frame() {
        use futures_util::StreamExt;
        use tokio::net::TcpListener;
        use tokio_tungstenite::tungstenite::Message;
        use tokio_tungstenite::{accept_async, connect_async};

        use crate::framed::FramedSession;
        use crate::protocol::{BepMessage, Folder};
        use crate::relay::RelayConnection;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Server: accept TCP, upgrade to WS, echo binary frames.
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            while let Some(result) = ws.next().await {
                match result.unwrap() {
                    Message::Binary(bytes) => {
                        ws.send(Message::Binary(bytes)).await.unwrap();
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        });

        // Client: connect, upgrade to WS, wrap in RelayTransport.
        let (client_ws, _) = connect_async(format!("ws://{addr}")).await.unwrap();
        let conn = RelayConnection::from_websocket(client_ws);
        let transport = RelayTransport::new(conn);
        let (mut reader, mut writer) = FramedSession::new(transport).split();

        // Round-trip a ClusterConfig message.
        let msg = BepMessage::ClusterConfig {
            folders: vec![Folder {
                id: "relay-test".into(),
                label: "Relay Test".into(),
            }],
            data_token: None,
        };
        writer.send(&msg).await.unwrap();
        let got = reader.recv().await.unwrap().unwrap();
        assert_eq!(got, msg);

        // Close cleanly.
        writer.shutdown().await.unwrap();
        let _ = server.await;
    }
}

// Re-export the channel pair for the framed-session tests above us.
// The pair is gated on `cfg(test)`; the parent module's tests reach
// for it via `crate::transport::test_support::ChannelTransport`.
#[cfg(test)]
pub mod test_support {
    pub use super::tests::ChannelTransport;
}
