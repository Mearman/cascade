//! Async BEP message framing.
//!
//! Two layers live in this module:
//!
//! - [`FramedPeer`] wraps a direct TLS stream and yields a split
//!   read/write pair. It is the original entry point, predates the
//!   unified [`crate::transport::Transport`] abstraction, and is
//!   still used by the sync engine for the direct TCP+TLS path.
//! - [`FramedSession`] wraps any [`crate::transport::Transport`] —
//!   relay, punched UDP, or TLS — and yields a single send/recv
//!   surface speaking [`BepMessage`]. The post-punch and post-relay
//!   paths produce a `Transport` and drive a [`FramedSession`] over
//!   it; no second framing layer is needed because the trait already
//!   carries one BEP frame per call.
//!
//! Both layers agree on the same wire format: each frame is
//! `[4-byte big-endian length][body]` as produced by
//! [`crate::protocol::encode_message`] and consumed by
//! [`crate::protocol::decode_message`].

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio_rustls::TlsStream;

use crate::connection::PeerConnection;
use crate::protocol::{BepMessage, decode_message, encode_message};
use crate::transport::{Transport, TransportReader, TransportWriter};

/// Maximum permitted BEP frame body size (16 MiB).
///
/// Defensive cap — the largest BEP message is a Response carrying a single
/// block payload (≤1 MiB) plus a small frame envelope. 16 MiB leaves room
/// for future protocol additions without permitting unbounded allocation
/// from a malicious peer.
pub const MAX_FRAME_BODY: usize = 16 * 1024 * 1024;

/// Framed peer wrapping a direct TLS connection.
#[derive(Debug)]
pub struct FramedPeer {
    stream: TlsStream<TcpStream>,
}

impl FramedPeer {
    /// Wrap a [`PeerConnection`] for BEP framing.
    ///
    /// Only the direct TLS variant is currently supported. Relay
    /// connections require WebSocket framing and will be added later.
    pub fn from_connection(conn: PeerConnection) -> Result<Self> {
        match conn {
            PeerConnection::Direct(stream) => Ok(Self { stream: *stream }),
            PeerConnection::Relay(_) => {
                anyhow::bail!("BEP framing over relay is not yet supported")
            }
        }
    }

    /// Wrap a raw TLS stream (used by the listener after acceptance).
    #[must_use]
    pub const fn from_tls(stream: TlsStream<TcpStream>) -> Self {
        Self { stream }
    }

    /// Split into independent read and write halves.
    #[must_use]
    pub fn split(self) -> (FramedReader, FramedWriter) {
        let (r, w) = tokio::io::split(self.stream);
        (FramedReader { inner: r }, FramedWriter { inner: w })
    }
}

/// Read half of a framed peer connection.
#[derive(Debug)]
pub struct FramedReader {
    inner: ReadHalf<TlsStream<TcpStream>>,
}

impl FramedReader {
    /// Read the next BEP frame, or `Ok(None)` on clean EOF.
    pub async fn recv(&mut self) -> Result<Option<BepMessage>> {
        let mut len_buf = [0u8; 4];
        match self.inner.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e).context("reading BEP frame length"),
        }
        let body_len = u32::from_be_bytes(len_buf) as usize;
        if body_len > MAX_FRAME_BODY {
            anyhow::bail!("BEP frame body length {body_len} exceeds limit {MAX_FRAME_BODY}");
        }
        let mut frame = Vec::with_capacity(4 + body_len);
        frame.extend_from_slice(&len_buf);
        frame.resize(4 + body_len, 0);
        self.inner
            .read_exact(frame.get_mut(4..).unwrap_or_default())
            .await
            .context("reading BEP frame body")?;
        let msg = decode_message(&frame).context("decoding BEP frame")?;
        Ok(Some(msg))
    }
}

/// BEP-framed session over any [`Transport`].
///
/// `FramedSession` is the generic counterpart to [`FramedPeer`]: it
/// owns a [`Transport`] and exposes a [`SessionReader`] /
/// [`SessionWriter`] pair speaking [`BepMessage`] rather than raw
/// bytes. Each underlying [`Transport`] call carries exactly one BEP
/// frame, so the wrapper is a thin encode/decode layer with no
/// buffering of its own.
///
/// Use this when the underlying connectivity is something other than
/// the direct TLS path — punched UDP or relay-tunnelled WebSocket —
/// or when you want a single session API across all three. The TLS
/// path can still use the original [`FramedPeer`] without change;
/// both speak the same wire format.
pub struct FramedSession<T: Transport> {
    transport: T,
}

impl<T: Transport> std::fmt::Debug for FramedSession<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FramedSession").finish_non_exhaustive()
    }
}

impl<T: Transport> FramedSession<T> {
    /// Wrap a [`Transport`] for BEP-level send/recv.
    pub const fn new(transport: T) -> Self {
        Self { transport }
    }

    /// Split into independent BEP-level read and write halves.
    pub fn split(self) -> (SessionReader<T::Reader>, SessionWriter<T::Writer>) {
        let (reader, writer) = self.transport.split();
        (
            SessionReader { inner: reader },
            SessionWriter { inner: writer },
        )
    }
}

/// Read half of a [`FramedSession`].
pub struct SessionReader<R: TransportReader> {
    inner: R,
}

impl<R: TransportReader> std::fmt::Debug for SessionReader<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionReader").finish_non_exhaustive()
    }
}

impl<R: TransportReader> SessionReader<R> {
    /// Receive one BEP message, or `Ok(None)` on clean EOF.
    pub async fn recv(&mut self) -> Result<Option<BepMessage>> {
        let Some(frame) = self
            .inner
            .recv_frame()
            .await
            .context("receiving BEP frame via transport")?
        else {
            return Ok(None);
        };
        let msg = decode_message(&frame).context("decoding BEP frame from transport")?;
        Ok(Some(msg))
    }
}

/// Write half of a [`FramedSession`].
pub struct SessionWriter<W: TransportWriter> {
    inner: W,
}

impl<W: TransportWriter> std::fmt::Debug for SessionWriter<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionWriter").finish_non_exhaustive()
    }
}

impl<W: TransportWriter> SessionWriter<W> {
    /// Encode and send one BEP message.
    pub async fn send(&mut self, msg: &BepMessage) -> Result<()> {
        let frame = encode_message(msg).context("encoding BEP frame for session")?;
        self.inner
            .send_frame(&frame)
            .await
            .context("sending BEP frame via transport")
    }

    /// Shut down the underlying transport writer.
    pub async fn shutdown(&mut self) -> Result<()> {
        self.inner
            .shutdown()
            .await
            .context("shutting down BEP session transport")
    }
}

/// Write half of a framed peer connection.
#[derive(Debug)]
pub struct FramedWriter {
    inner: WriteHalf<TlsStream<TcpStream>>,
}

impl FramedWriter {
    /// Encode and send a BEP frame.
    pub async fn send(&mut self, msg: &BepMessage) -> Result<()> {
        let frame = encode_message(msg).context("encoding BEP frame")?;
        self.inner
            .write_all(&frame)
            .await
            .context("writing BEP frame")?;
        self.inner.flush().await.context("flushing BEP frame")?;
        Ok(())
    }

    /// Shut down the underlying write half.
    pub async fn shutdown(&mut self) -> Result<()> {
        self.inner
            .shutdown()
            .await
            .context("shutting down BEP frame writer")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::connection::ConnectionManager;
    use crate::discovery::DiscoveredPeer;
    use crate::identity::DeviceIdentity;
    use crate::protocol::Folder;

    /// Spin up an authenticated TLS pair on loopback and return the
    /// framed wrappers on both ends.
    async fn paired_framed() -> (FramedPeer, FramedPeer) {
        let identity = DeviceIdentity::generate().unwrap();
        let device_id = identity.device_id.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        let server_identity = identity.clone();
        let server_trusted = vec![device_id.clone()];
        let server = tokio::spawn(async move {
            let manager = ConnectionManager::new(server_identity, server_trusted);
            let (stream, _) = listener.accept().await.unwrap();
            let (_device, _observed, tls) = manager.accept(stream).await.unwrap();
            FramedPeer::from_tls(tls)
        });

        let client_manager = ConnectionManager::new(identity, vec![device_id.clone()]);
        let client_conn = client_manager
            .connect(&DiscoveredPeer { device_id, address })
            .await
            .unwrap();
        let client = FramedPeer::from_connection(client_conn).unwrap();
        let server = server.await.unwrap();
        (client, server)
    }

    #[tokio::test]
    async fn frame_round_trip() {
        let (client, server) = paired_framed().await;
        let (mut cr, mut cw) = client.split();
        let (mut sr, mut sw) = server.split();

        let cluster = BepMessage::ClusterConfig {
            folders: vec![Folder {
                id: "f1".into(),
                label: "Folder One".into(),
            }],
            data_token: None,
        };
        cw.send(&cluster).await.unwrap();
        let got = sr.recv().await.unwrap().unwrap();
        assert_eq!(got, cluster);

        let ping = BepMessage::Ping;
        sw.send(&ping).await.unwrap();
        let got = cr.recv().await.unwrap().unwrap();
        assert_eq!(got, ping);
    }

    #[tokio::test]
    async fn recv_returns_none_on_clean_close() {
        let (client, server) = paired_framed().await;
        let (mut sr, _sw) = server.split();
        let (_cr, mut cw) = client.split();
        cw.shutdown().await.unwrap();
        let got = sr.recv().await.unwrap();
        assert!(got.is_none());
    }

    /// A peer that announces a frame body larger than the allowed cap
    /// must be rejected by `recv` before any allocation happens.
    #[tokio::test]
    async fn recv_rejects_oversized_frame_header() {
        use tokio::io::AsyncWriteExt;
        let (client, server) = paired_framed().await;
        let (mut sr, _sw) = server.split();
        let (_cr, mut cw) = client.split();
        // Hand-craft a frame whose declared body is one byte over the cap.
        let bogus_len = u32::try_from(MAX_FRAME_BODY + 1).unwrap();
        cw.inner.write_all(&bogus_len.to_be_bytes()).await.unwrap();
        cw.inner.flush().await.unwrap();
        let err = sr.recv().await.unwrap_err();
        assert!(err.to_string().contains("exceeds limit"));
    }

    /// A `FramedSession` over the in-memory channel transport must
    /// round-trip the three BEP frames the M2 milestone needs to
    /// drive: the cluster handshake (analogous to "Hello"), a block
    /// `Request`, and the matching `Response`. This is the synthetic
    /// transport pair the milestone asks for — no real sockets are
    /// touched, so the test runs in microseconds and exercises only
    /// the encode/decode + Transport plumbing.
    #[tokio::test]
    async fn framed_session_round_trips_hello_request_response() {
        use crate::protocol::{FileInfo, Folder, Version};
        use crate::transport::test_support::ChannelTransport;

        let (client_t, server_t) = ChannelTransport::pair();
        let (mut client_r, mut client_w) = FramedSession::new(client_t).split();
        let (mut server_r, mut server_w) = FramedSession::new(server_t).split();

        // Hello-equivalent: ClusterConfig naming the shared folder.
        let hello = BepMessage::ClusterConfig {
            folders: vec![Folder {
                id: "shared".into(),
                label: "Shared".into(),
            }],
            data_token: None,
        };
        client_w.send(&hello).await.unwrap();
        let got = server_r.recv().await.unwrap().unwrap();
        assert_eq!(got, hello);

        // The server replies with an Index naming the file the client
        // is about to ask for. Keeps the test self-contained: a real
        // session would not necessarily emit the Index before the
        // Request, but for the BEP exchange wiring all that matters
        // is that each frame is parseable on the other side.
        let index = BepMessage::Index {
            folder: "shared".into(),
            files: vec![FileInfo {
                name: "doc.txt".into(),
                file_type: 0,
                size: 11,
                modified: 1_700_000_000,
                sequence: 1,
                block_size: 128 * 1024,
                deleted: false,
                invalid: false,
                no_permissions: false,
                version: Version {
                    counters: vec![(7, 1)],
                },
                block_hashes: vec![[42u8; 32]],
            }],
        };
        server_w.send(&index).await.unwrap();
        let got = client_r.recv().await.unwrap().unwrap();
        assert_eq!(got, index);

        // Request the only block.
        let request = BepMessage::Request {
            request_id: 1,
            folder: "shared".into(),
            name: "doc.txt".into(),
            block_offset: 0,
            block_size: 11,
            block_hash: [42u8; 32],
        };
        client_w.send(&request).await.unwrap();
        let got = server_r.recv().await.unwrap().unwrap();
        assert_eq!(got, request);

        // Server echoes the data.
        let response = BepMessage::Response {
            request_id: 1,
            data: b"hello world".to_vec(),
        };
        server_w.send(&response).await.unwrap();
        let got = client_r.recv().await.unwrap().unwrap();
        assert_eq!(got, response);
    }

    /// Dropping the peer's session must surface as `Ok(None)` on the
    /// surviving side — the same contract `FramedPeer` exposes for
    /// TLS clean close, propagated by the Transport adapter.
    #[tokio::test]
    async fn framed_session_recv_returns_none_on_drop() {
        use crate::transport::test_support::ChannelTransport;

        let (client_t, server_t) = ChannelTransport::pair();
        // Drop the client side entirely so no sender keeps the channel
        // alive. The server's recv must then surface `Ok(None)`.
        drop(client_t);
        let (mut server_r, _server_w) = FramedSession::new(server_t).split();
        let got = server_r.recv().await.unwrap();
        assert!(got.is_none(), "expected None on clean EOF");
    }

    /// Truncated mid-body must surface as a read error, not silently as
    /// EOF — otherwise a hostile peer could elicit a misparse by closing
    /// after the length prefix.
    #[tokio::test]
    async fn recv_errors_on_partial_body() {
        use tokio::io::AsyncWriteExt;
        let (client, server) = paired_framed().await;
        let (mut sr, _sw) = server.split();
        let (_cr, mut cw) = client.split();
        // Claim 16 bytes of body, send 4, then close.
        cw.inner.write_all(&16u32.to_be_bytes()).await.unwrap();
        cw.inner.write_all(&[0, 0, 0, 0]).await.unwrap();
        cw.shutdown().await.unwrap();
        let err = sr.recv().await.unwrap_err();
        assert!(
            err.to_string().contains("frame body")
                || err.to_string().contains("UnexpectedEof")
                || err.to_string().contains("eof"),
            "unexpected error: {err}"
        );
    }
}
