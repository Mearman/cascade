//! WebSocket relay transport with end-to-end TLS.
//!
//! The relay is a blind byte-pipe — it sees only opaque ciphertext. The two
//! endpoints negotiate TLS *through* the relay tunnel, so even a malicious
//! relay operator cannot read or tamper with BEP traffic.
//!
//! Wire layers (inside out):
//!   BEP message → length-prefixed frame → TLS record → WebSocket binary message → relay

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

const RELAY_JOIN_PATH: &str = "join";
const RELAY_FRAME_LEN_SIZE: usize = 4;

/// Relay client — connects through a relay server when direct connection fails.
#[derive(Debug, Clone, Copy)]
pub struct RelayClient;

/// An active relayed connection.
#[derive(Debug)]
pub struct RelayConnection {
    socket: Mutex<WebSocketStream<MaybeTlsStream<TcpStream>>>,
}

impl RelayClient {
    /// Connect to a relay server and join a session for a target device.
    pub async fn connect(relay_url: &str, target_device_id: &str) -> Result<RelayConnection> {
        let join_url = relay_join_url(relay_url, target_device_id);
        let (socket, _) = connect_async(&join_url)
            .await
            .with_context(|| format!("connecting to relay {join_url}"))?;
        Ok(RelayConnection {
            socket: Mutex::new(socket),
        })
    }
}

impl RelayConnection {
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

fn relay_join_url(relay_url: &str, target_device_id: &str) -> String {
    format!(
        "{}/{RELAY_JOIN_PATH}/{target_device_id}",
        relay_url.trim_end_matches('/')
    )
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
mod tests {
    use super::*;

    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    #[tokio::test]
    async fn relay_connection_sends_and_receives_length_prefixed_messages() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_address = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            let message = websocket.next().await.unwrap().unwrap();
            match message {
                Message::Binary(frame) => {
                    assert_eq!(decode_relay_frame(&frame).unwrap(), b"hello".to_vec());
                }
                other => panic!("unexpected relay message: {other:?}"),
            }
            websocket
                .send(Message::Binary(
                    encode_relay_frame(b"world").unwrap().into(),
                ))
                .await
                .unwrap();
        });

        let connection = RelayClient::connect(&format!("ws://{relay_address}"), "TARGET")
            .await
            .unwrap();
        connection.send(b"hello").await.unwrap();
        let received = connection.recv().await.unwrap();

        assert_eq!(received, b"world".to_vec());
        server.await.unwrap();
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
