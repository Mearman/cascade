//! Async BEP message framing over a direct TLS peer connection.
//!
//! Wraps a `TlsStream` and yields a split read/write pair that speak
//! [`BepMessage`] frames. Each frame is `[4-byte big-endian length][body]`
//! as produced by [`super::protocol::encode_message`].

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio_rustls::TlsStream;

use crate::connection::PeerConnection;
use crate::protocol::{BepMessage, decode_message, encode_message};

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
            let (_device, tls) = manager.accept(stream).await.unwrap();
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
