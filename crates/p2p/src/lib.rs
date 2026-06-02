#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::string_slice
    )
)]
//! Cascade P2P engine — block-level file sharing between devices on a LAN.
//!
//! Based on Syncthing's Block Exchange Protocol (BEP) v1. The P2P engine sits
//! between the VFS and the cache layer as an optimisation: when a file is not
//! in the local cache, the engine first checks P2P peers on the LAN before
//! falling back to the cloud backend.
//!
//! # Architecture
//!
//! - **Block store** (`store`): Content-addressed storage for file blocks,
//!   sharded by hash prefix.
//! - **Block splitting** (`block`): Adaptive-size file splitting and SHA-256
//!   hashing.
//! - **BEP protocol** (`protocol`): Length-prefixed XDR message framing for
//!   peer communication.
//! - **Discovery** (`discovery`): UDP multicast LAN peer discovery on port
//!   21027.
//! - **WAN discovery** (`wan`): Fully P2P peer gossip with introducer referrals.
//!   No central discovery server — devices learn about each other through
//!   trusted peers.
//! - **NAT traversal** (`nat`): STUN Binding Requests for external address
//!   discovery and relay fallback decisions.
//! - **Relay** (`relay`): WebSocket transport for peers behind restrictive NAT.
//! - **Connection management** (`connection`): Direct TCP connection attempts
//!   with relay fallback.
//! - **Identity** (`identity`): Self-signed TLS certificate generation with
//!   base32-encoded device ID.

pub mod block;
pub mod candidate;
pub mod connection;
pub mod discovery;
pub mod framed;
pub mod identity;
pub mod nat;
pub mod protocol;
pub mod relay;
pub mod store;
pub mod transport;
pub mod traversal;
pub mod wan;

pub use traversal::{
    CandidatePair, Clock, ConnectivityStrategy, EstablishedFlow, NatType, PunchConfig, PunchError,
    PunchTransport, ReceivedProbe, SyncPunchAgreement, SystemClock, decide_connectivity,
    run_hole_punch,
};

use std::path::Path;

use anyhow::{Context, Result};

use block::FileBlocks;
use identity::DeviceIdentity;
use store::BlockStore;

/// Default P2P configuration directory (within the Cascade config dir).
const P2P_DIR: &str = "p2p";

/// Default BEP listen port.
const DEFAULT_LISTEN_PORT: u16 = 22000;

/// Top-level P2P engine composing all subsystems.
///
/// Relay fallback is driven by the backend's sync engine via
/// [`relay::RelayClient::connect_with_secret`], which has access to the
/// pre-shared HMAC secret needed to authenticate against the relay
/// server. The engine here covers identity, discovery, and the block
/// store.
#[derive(Debug)]
pub struct P2pEngine {
    /// This device's identity.
    identity: DeviceIdentity,
    /// Block store for content-addressed storage.
    block_store: BlockStore,
    /// TCP port for incoming BEP connections.
    listen_port: u16,
    /// Peer book for gossip-based P2P discovery.
    peer_book: wan::PeerBook,
}

impl P2pEngine {
    /// Create a new P2P engine rooted at the Cascade config directory.
    ///
    /// Initialises the block store and loads or generates a device identity.
    pub fn new(config_dir: &Path) -> Result<Self> {
        let p2p_dir = config_dir.join(P2P_DIR);
        let identity = DeviceIdentity::load_or_generate(&p2p_dir.join("identity"))
            .context("initialising device identity")?;
        let block_store = BlockStore::new(&p2p_dir).context("initialising block store")?;

        Ok(Self {
            identity,
            block_store,
            listen_port: DEFAULT_LISTEN_PORT,
            peer_book: wan::PeerBook::new(),
        })
    }

    /// Create with explicit identity and block store root (for testing).
    #[must_use]
    pub fn with_identity(identity: DeviceIdentity, block_store: BlockStore) -> Self {
        Self {
            identity,
            block_store,
            listen_port: DEFAULT_LISTEN_PORT,
            peer_book: wan::PeerBook::new(),
        }
    }

    /// This device's ID (base32-encoded SHA-256 of the TLS certificate).
    #[must_use]
    pub fn device_id(&self) -> &str {
        &self.identity.device_id
    }

    /// TCP port for incoming BEP connections.
    #[must_use]
    pub const fn listen_port(&self) -> u16 {
        self.listen_port
    }

    /// Set the BEP listen port.
    pub const fn set_listen_port(&mut self, port: u16) {
        self.listen_port = port;
    }

    /// Access the block store.
    #[must_use]
    pub const fn block_store(&self) -> &BlockStore {
        &self.block_store
    }

    /// Access the device identity.
    #[must_use]
    pub const fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    /// Index file data into the block store. Splits the data into blocks,
    /// stores each block, and returns the block description.
    pub async fn index_data(&self, data: &[u8]) -> Result<FileBlocks> {
        self.block_store
            .index_data(data)
            .await
            .context("indexing file data")
    }

    /// Reassemble file content from stored blocks.
    pub async fn reassemble(&self, blocks: &FileBlocks) -> Result<Vec<u8>> {
        self.block_store
            .reassemble(blocks)
            .await
            .context("reassembling file from blocks")
    }

    /// Broadcast a discovery announcement on the LAN.
    pub fn announce(&self) -> Result<()> {
        discovery::announce(self.device_id(), self.listen_port)
            .context("broadcasting discovery announcement")
    }

    /// Listen for peer discovery announcements.
    ///
    /// Blocks for the given duration, returning all discovered peers.
    pub fn discover_peers(timeout: std::time::Duration) -> Result<Vec<discovery::DiscoveredPeer>> {
        discovery::listen(timeout).context("listening for peer discovery")
    }

    /// Access the peer book for P2P gossip-based discovery.
    #[must_use]
    pub const fn peer_book(&self) -> &wan::PeerBook {
        &self.peer_book
    }

    /// Mutable access to the peer book.
    pub const fn peer_book_mut(&mut self) -> &mut wan::PeerBook {
        &mut self.peer_book
    }

    /// Establish a TLS-authenticated direct peer connection.
    ///
    /// Returns the source [`std::net::SocketAddr`] the local socket
    /// observed for the peer alongside the connection (see
    /// [`connection::ConnectionManager::connect`]).
    ///
    /// Relay fallback is driven one layer up by the backend's sync
    /// engine, which holds the shared HMAC secret needed by the relay
    /// client. This method is a thin wrapper around the connection
    /// manager and never attempts relay.
    pub async fn connect_peer(
        &self,
        peer: &discovery::DiscoveredPeer,
    ) -> Result<(std::net::SocketAddr, connection::PeerConnection)> {
        let trusted_ids: Vec<String> = self.peer_book.peers().keys().cloned().collect();
        connection::ConnectionManager::new(self.identity.clone(), trusted_ids)
            .connect(peer)
            .await
            .context("connecting to P2P peer")
    }

    /// Detect the local NAT type using a STUN server.
    pub async fn detect_nat_type(&self, stun_server: &str) -> Result<traversal::NatType> {
        nat::NatTraversal::detect_nat_type(stun_server)
            .await
            .context("detecting NAT type")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::BLOCK_128KB;

    #[tokio::test]
    async fn engine_creation() {
        let dir = tempfile::tempdir().unwrap();
        let engine = P2pEngine::new(dir.path()).unwrap();
        assert!(!engine.device_id().is_empty());
        assert_eq!(engine.listen_port(), DEFAULT_LISTEN_PORT);
    }

    #[tokio::test]
    async fn engine_index_and_reassemble() {
        let dir = tempfile::tempdir().unwrap();
        let engine = P2pEngine::new(dir.path()).unwrap();

        let data = vec![0xCC; BLOCK_128KB as usize * 2 + 512];
        let blocks = engine.index_data(&data).await.unwrap();

        assert_eq!(blocks.size, data.len() as u64);
        assert_eq!(blocks.block_count(), 3);

        let reassembled = engine.reassemble(&blocks).await.unwrap();
        assert_eq!(reassembled, data);
    }

    #[tokio::test]
    async fn engine_persistent_identity() {
        let dir = tempfile::tempdir().unwrap();
        let engine1 = P2pEngine::new(dir.path()).unwrap();
        let id1 = engine1.device_id().to_string();

        let engine2 = P2pEngine::new(dir.path()).unwrap();
        let id2 = engine2.device_id().to_string();

        // Same config dir should produce the same identity.
        assert_eq!(id1, id2);
    }
}
