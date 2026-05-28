//! Peer connection orchestration.
//!
//! The manager tries a direct BEP TCP connection first and falls back to relay
//! servers when the direct path cannot be established.

use anyhow::{Context, Result};
use tokio::net::TcpStream;

use crate::discovery::DiscoveredPeer;
use crate::relay::{RelayClient, RelayConnection};

/// Manages connections to P2P peers.
#[derive(Debug, Clone)]
pub struct ConnectionManager {
    relay_urls: Vec<String>,
}

/// Established peer transport.
#[derive(Debug)]
pub enum PeerConnection {
    /// Direct TCP connection to the peer's BEP listener.
    Direct(TcpStream),
    /// WebSocket relay connection to the peer.
    Relay(Box<RelayConnection>),
}

impl ConnectionManager {
    /// Create a connection manager with ordered relay fallback URLs.
    pub fn new(relay_urls: Vec<String>) -> Self {
        Self { relay_urls }
    }

    /// Establish a connection to a peer, trying direct then relay.
    pub async fn connect(&self, peer: &DiscoveredPeer) -> Result<PeerConnection> {
        match TcpStream::connect(peer.address).await {
            Ok(stream) => Ok(PeerConnection::Direct(stream)),
            Err(direct_error) if self.relay_urls.is_empty() => {
                Err(direct_error).with_context(|| {
                    format!(
                        "connecting directly to peer {} at {}",
                        peer.device_id, peer.address
                    )
                })
            }
            Err(direct_error) => {
                let mut relay_errors = Vec::new();
                for relay_url in &self.relay_urls {
                    match RelayClient::connect(relay_url, &peer.device_id).await {
                        Ok(connection) => return Ok(PeerConnection::Relay(Box::new(connection))),
                        Err(error) => relay_errors.push(format!("{relay_url}: {error:#}")),
                    }
                }
                anyhow::bail!(
                    "direct connection to peer {} at {} failed: {direct_error}; relay fallback failed: {}",
                    peer.device_id,
                    peer.address,
                    relay_errors.join("; ")
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    #[tokio::test]
    async fn connect_uses_direct_tcp_when_peer_is_reachable() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let accept_task = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
        });
        let peer = DiscoveredPeer {
            device_id: "DEVICE".to_string(),
            address,
        };
        let manager = ConnectionManager::new(Vec::new());

        let connection = manager.connect(&peer).await.unwrap();

        match connection {
            PeerConnection::Direct(_) => {}
            PeerConnection::Relay(_) => panic!("expected direct connection"),
        }
        accept_task.await.unwrap();
    }

    #[tokio::test]
    async fn connect_falls_back_to_relay_when_direct_tcp_fails() {
        let unavailable_direct_address = unavailable_loopback_address().await;
        let relay_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_address = relay_listener.local_addr().unwrap();
        let relay_task = tokio::spawn(async move {
            let (stream, _) = relay_listener.accept().await.unwrap();
            let _websocket = accept_async(stream).await.unwrap();
        });
        let peer = DiscoveredPeer {
            device_id: "DEVICE".to_string(),
            address: unavailable_direct_address,
        };
        let manager = ConnectionManager::new(vec![format!("ws://{relay_address}")]);

        let connection = manager.connect(&peer).await.unwrap();

        match connection {
            PeerConnection::Direct(_) => panic!("expected relay connection"),
            PeerConnection::Relay(_) => {}
        }
        relay_task.await.unwrap();
    }

    async fn unavailable_loopback_address() -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        address
    }
}
