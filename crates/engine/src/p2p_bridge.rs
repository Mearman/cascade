//! Bridge between P2P engine and the Cascade sync/cache layer.
//!
//! The P2P bridge sits between the cache manager and the cloud backend.
//! When a file is requested, the bridge first checks whether any LAN peer
//! has the file's blocks available. If so, the data is reassembled locally
//! without hitting the cloud backend. Either way, downloaded files are
//! indexed into the local block store so other peers can fetch them.

use std::sync::Arc;

use anyhow::Result;
use cascade_p2p::DiscoveryReach;
use cascade_p2p::P2pEngine;
use cascade_p2p::block::{BlockHash, FileBlocks};
use cascade_p2p::store::BlockStore;
use tracing::{debug, warn};

use crate::db::StateDb;
use crate::types::{FileEntry, ItemId};

/// Configuration for the optimisation-layer P2P bridge.
///
/// These fields extend the bridge's reach beyond LAN defaults: they let a
/// cloud-backed node also participate in WAN peer discovery (with a specific
/// `DiscoveryReach` posture) and use a `cascade-relay` server for NAT
/// traversal. They mirror the fields a pure-P2P backend carries in its own
/// per-backend TOML, but live here for the engine's optimisation layer so
/// the two paths share a single source of intent.
#[derive(Debug, Clone, Default)]
pub struct P2pBridgeConfig {
    /// Discovery reach for the optimisation-layer P2P engine.
    ///
    /// `None` means the engine default (`private`) applies: trusted mesh,
    /// no global directory publication.
    pub posture: Option<DiscoveryReach>,
    /// Relay endpoint addresses for WAN NAT traversal.
    ///
    /// Each entry is a `cascade-relay` server socket address. Empty means no
    /// relay strategy is provisioned.
    pub relay_endpoints: Vec<std::net::SocketAddr>,
    /// 32-byte HMAC shared secret authenticating this node to the relay server.
    ///
    /// `None` means no relay secret is configured; a relay endpoint will be
    /// provisioned but dials will fail authentication at the relay side.
    pub relay_shared_secret: Option<[u8; 32]>,
}

/// Bridge between P2P engine and the sync/cache layer.
pub struct P2pBridge {
    p2p: P2pEngine,
    db: Arc<StateDb>,
    /// Configuration governing the optimisation-layer reach and relay.
    ///
    /// Stored for use when the bridge spawns its peer-session loop. Held
    /// here rather than in `P2pEngine` because the `cascade_p2p::P2pEngine`
    /// type predates posture/relay configuration and is used by the
    /// per-backend path too; keeping the configuration on the bridge keeps
    /// the two paths independent.
    pub config: P2pBridgeConfig,
}

impl std::fmt::Debug for P2pBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("P2pBridge")
            .field("device_id", &self.p2p.device_id())
            .field("posture", &self.config.posture)
            .field(
                "relay_endpoints",
                &format!("[{} endpoint(s)]", self.config.relay_endpoints.len()),
            )
            .finish_non_exhaustive()
    }
}

impl P2pBridge {
    /// Create a new P2P bridge with default configuration (private posture,
    /// no relay).
    pub fn new(p2p: P2pEngine, db: Arc<StateDb>) -> Self {
        Self {
            p2p,
            db,
            config: P2pBridgeConfig::default(),
        }
    }

    /// Create a new P2P bridge with explicit posture and relay configuration.
    pub const fn with_config(p2p: P2pEngine, db: Arc<StateDb>, config: P2pBridgeConfig) -> Self {
        Self { p2p, db, config }
    }

    /// Access the underlying P2P engine.
    #[must_use]
    pub const fn engine(&self) -> &P2pEngine {
        &self.p2p
    }

    /// Access the underlying block store.
    #[must_use]
    pub const fn block_store(&self) -> &BlockStore {
        self.p2p.block_store()
    }

    /// Try to fetch a file's content from local block storage.
    ///
    /// Looks up the file's block index in the DB, then reassembles the
    /// data from the local block store. Returns `None` if no block index
    /// exists for the file or if any block is missing from the store.
    pub async fn try_fetch_from_peers(&self, file: &FileEntry) -> Result<Option<Vec<u8>>> {
        let block_hashes = self.db.get_p2p_blocks(&file.id)?;
        if block_hashes.is_empty() {
            debug!(file = %file.name, "no P2P block index for file");
            return Ok(None);
        }

        let blocks = FileBlocks {
            size: file.size.unwrap_or(0),
            block_size: cascade_p2p::block::block_size_for_file(file.size.unwrap_or(0)),
            blocks: block_hashes.into_iter().map(BlockHash).collect(),
        };

        match self.p2p.reassemble(&blocks).await {
            Ok(data) => {
                debug!(
                    file = %file.name,
                    bytes = data.len(),
                    "reassembled file from local P2P block store"
                );
                Ok(Some(data))
            }
            Err(e) => {
                // Missing block or corrupt data — not fatal, just means
                // we fall back to the cloud backend.
                debug!(
                    file = %file.name,
                    error = %e,
                    "P2P reassemble failed, will fall back to cloud"
                );
                Ok(None)
            }
        }
    }

    /// Index a file's data into the local P2P block store and record the
    /// block index in the database so it can be looked up later.
    pub async fn index_file(&self, path: &str, data: &[u8]) -> Result<FileBlocks> {
        let blocks = self.p2p.index_data(data).await?;

        // Record block hashes in the DB. We use the path as a synthetic
        // file ID since we may not have a full FileEntry at index time.
        let file_id = ItemId::new("p2p", path);
        let hashes: Vec<[u8; 32]> = blocks.blocks.iter().map(|h| h.0).collect();
        self.db.index_p2p_blocks(&file_id, &hashes)?;

        debug!(
            path = path,
            blocks = blocks.block_count(),
            size = blocks.size,
            "indexed file for P2P sharing"
        );

        Ok(blocks)
    }

    /// Index a file's data using an existing `ItemId` from the DB.
    pub async fn index_file_by_id(&self, file_id: &ItemId, data: &[u8]) -> Result<FileBlocks> {
        let blocks = self.p2p.index_data(data).await?;

        let hashes: Vec<[u8; 32]> = blocks.blocks.iter().map(|h| h.0).collect();
        self.db.index_p2p_blocks(file_id, &hashes)?;

        debug!(
            file_id = %file_id,
            blocks = blocks.block_count(),
            size = blocks.size,
            "indexed file for P2P sharing"
        );

        Ok(blocks)
    }

    /// Check if we have all blocks for a given hash stored locally.
    pub fn has_blocks(&self, hash: &str) -> Result<bool> {
        // Parse the hex hash into a BlockHash.
        let bytes: Vec<u8> = (0..hash.len())
            .step_by(2)
            .map(|i| {
                let end = i + 2;
                let chunk = hash
                    .get(i..end)
                    .ok_or_else(|| anyhow::anyhow!("hash string index out of range at {i}"))?;
                u8::from_str_radix(chunk, 16).map_err(anyhow::Error::from)
            })
            .collect::<Result<Vec<_>, _>>()?;

        if bytes.len() != 32 {
            warn!(hash = hash, "invalid block hash length");
            return Ok(false);
        }

        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        let block_hash = BlockHash(arr);

        Ok(self.p2p.block_store().has_block(&block_hash))
    }

    /// Get this device's P2P ID.
    #[must_use]
    pub fn device_id(&self) -> &str {
        self.p2p.device_id()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_index_and_has_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let engine = P2pEngine::new(dir.path()).unwrap();
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        let bridge = P2pBridge::new(engine, db);

        let data = b"hello cascade p2p bridge test data";
        let blocks = bridge.index_file("test.txt", data).await.unwrap();
        assert!(!blocks.blocks.is_empty());

        // The first block hash should be present.
        let hash_hex = blocks.blocks[0].to_string();
        assert!(bridge.has_blocks(&hash_hex).unwrap());
    }

    #[tokio::test]
    async fn test_try_fetch_returns_none_when_no_index() {
        let dir = tempfile::tempdir().unwrap();
        let engine = P2pEngine::new(dir.path()).unwrap();
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        let bridge = P2pBridge::new(engine, db);

        let file = FileEntry::file(
            ItemId::new("gdrive", "nonexistent"),
            ItemId::new("gdrive", "root"),
            "test.txt".into(),
        );

        let result = bridge.try_fetch_from_peers(&file).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_device_id_non_empty() {
        let dir = tempfile::tempdir().unwrap();
        let engine = P2pEngine::new(dir.path()).unwrap();
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        let bridge = P2pBridge::new(engine, db);

        let device_id = bridge.device_id();
        assert!(!device_id.is_empty());
    }

    #[tokio::test]
    async fn test_index_and_fetch_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let engine = P2pEngine::new(dir.path()).unwrap();
        let db = Arc::new(StateDb::open_in_memory().unwrap());
        let bridge = P2pBridge::new(engine, db.clone());

        // Register backend so we can create a file entry.
        db.register_backend("p2p", "p2p", "P2P", None, None)
            .unwrap();

        let data = vec![0xAB; 128 * 1024 * 2 + 512]; // ~256 KB, spans multiple blocks.
        let file_id = ItemId::new("p2p", "test.bin");
        let parent_id = ItemId::new("p2p", "root");

        // Index the file.
        bridge.index_file_by_id(&file_id, &data).await.unwrap();

        // Now try to fetch it.
        let file = FileEntry::file(file_id.clone(), parent_id, "test.bin".into())
            .with_size(Some(data.len() as u64));

        let result = bridge.try_fetch_from_peers(&file).await.unwrap();
        let fetched = result.expect("should have fetched from local blocks");
        assert_eq!(fetched, data);
    }
}
