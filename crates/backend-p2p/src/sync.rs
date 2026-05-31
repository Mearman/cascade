//! Peer sync orchestration for the P2P backend.
//!
//! Wires the existing `cascade-p2p` transport (TLS-authenticated peer
//! connections, BEP message framing) to the local [`FolderIndex`] and
//! [`BlockStore`]. The model is Syncthing-style:
//!
//! 1. On a successful connection (in or out), each side sends a
//!    [`BepMessage::ClusterConfig`] followed by [`BepMessage::Index`]
//!    enumerating every file row in the index — including tombstones —
//!    with its block hash list.
//! 2. Incoming `Index` and `IndexUpdate` frames are merged into the
//!    local index using a last-write-wins rule (compare `modified`
//!    timestamps; take the newer row). A row with `FileInfo.deleted`
//!    set marks the local entry as a tombstone instead of overwriting
//!    its blocks.
//! 3. Local writes (`upload`, `update`, `delete`) broadcast an
//!    `IndexUpdate` frame with the new row — tombstones included — to
//!    every connected peer.
//! 4. When a `Backend::download` call discovers blocks missing from
//!    the local [`BlockStore`], each connected peer is asked in turn
//!    via [`BepMessage::Request`] and the first matching block is kept.
//!
//! BEP v1 has no per-message correlation IDs, so block requests on a
//! single peer connection are serialised — at most one outstanding
//! [`BepMessage::Request`] per peer at any moment. Multiple peers can
//! be queried concurrently.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use cascade_p2p::block::BlockHash;
use cascade_p2p::connection::ConnectionManager;
use cascade_p2p::discovery::DiscoveredPeer;
use cascade_p2p::framed::FramedPeer;
use cascade_p2p::identity::DeviceIdentity;
use cascade_p2p::protocol::{BepMessage, FileInfo, Folder};
use cascade_p2p::store::BlockStore;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::index::{FolderIndex, IndexEntry};

/// File-type code for regular files in BEP `FileInfo.file_type`.
const FILE_TYPE_FILE: u32 = 0;
/// File-type code for directories.
const FILE_TYPE_DIR: u32 = 1;

/// Wall-clock timeout for a block request to a single peer.
const BLOCK_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Identity information for a connected peer.
#[derive(Debug, Clone)]
pub struct Peer {
    pub device_id: String,
    pub address: SocketAddr,
}

/// Handle to a live peer session — used to send messages and fetch blocks.
#[derive(Debug)]
struct PeerHandle {
    outbound: mpsc::UnboundedSender<BepMessage>,
    /// Oneshot senders for outstanding Request → Response correlation.
    /// BEP v1 has no message IDs, so we enforce strict request ordering.
    pending: Arc<Mutex<VecDeque<oneshot::Sender<Vec<u8>>>>>,
}

impl PeerHandle {
    /// Send a Request frame and await the next Response payload.
    async fn request_block(
        &self,
        folder: String,
        name: String,
        offset: u64,
        size: u32,
        hash: [u8; 32],
    ) -> Result<Vec<u8>> {
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.push_back(tx);
        }
        self.outbound
            .send(BepMessage::Request {
                folder,
                name,
                block_offset: offset,
                block_size: size,
                block_hash: hash,
            })
            .map_err(|_| anyhow::anyhow!("peer outbound channel closed"))?;

        match tokio::time::timeout(BLOCK_REQUEST_TIMEOUT, rx).await {
            Ok(Ok(data)) => Ok(data),
            Ok(Err(_)) => anyhow::bail!("peer session dropped before responding"),
            Err(_) => anyhow::bail!("peer block request timed out"),
        }
    }
}

/// Peer sync engine.
///
/// One instance per `P2pBackend`. Owns Arc-shared references to the
/// folder index and block store so background tasks can read/write them
/// without holding the backend itself.
#[derive(Debug, Clone)]
pub struct SyncEngine {
    folder_id: String,
    index: Arc<FolderIndex>,
    blocks: Arc<BlockStore>,
    identity: DeviceIdentity,
    /// Device IDs we are willing to talk to.
    trusted: Arc<Mutex<Vec<String>>>,
    peers: Arc<Mutex<HashMap<String, PeerHandle>>>,
}

impl SyncEngine {
    /// Construct a sync engine with no peers and no listener running.
    pub fn new(
        folder_id: String,
        index: Arc<FolderIndex>,
        blocks: Arc<BlockStore>,
        identity: DeviceIdentity,
    ) -> Self {
        Self {
            folder_id,
            index,
            blocks,
            identity,
            trusted: Arc::new(Mutex::new(Vec::new())),
            peers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Add a trusted device ID. Future inbound connections from this
    /// device are accepted; outbound `connect_to` calls require the
    /// target to be in this set.
    pub async fn trust(&self, device_id: String) {
        let mut trusted = self.trusted.lock().await;
        if !trusted.contains(&device_id) {
            trusted.push(device_id);
        }
    }

    /// Our device ID.
    #[must_use]
    pub fn device_id(&self) -> &str {
        &self.identity.device_id
    }

    /// `true` if a session to `device_id` is currently active.
    pub async fn has_peer(&self, device_id: &str) -> bool {
        let peers = self.peers.lock().await;
        peers.contains_key(device_id)
    }

    /// `true` if `device_id` is in the trusted set.
    ///
    /// Used by the LAN discovery loop to skip announcements from
    /// untrusted devices before attempting an outbound connect.
    pub async fn is_trusted(&self, device_id: &str) -> bool {
        let trusted = self.trusted.lock().await;
        trusted.iter().any(|id| id == device_id)
    }

    /// Start accepting incoming TLS connections on `addr`.
    ///
    /// Returns the bound `SocketAddr` (useful when binding to port 0)
    /// and a `JoinHandle` for the listener task.
    pub async fn start_listener(
        &self,
        addr: SocketAddr,
    ) -> Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding P2P listener to {addr}"))?;
        let bound = listener
            .local_addr()
            .context("reading listener bound address")?;
        let engine = self.clone();
        let handle = tokio::spawn(async move {
            loop {
                let (stream, peer_addr) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(e) => {
                        warn!("P2P listener accept failed: {e}");
                        continue;
                    }
                };
                let engine = engine.clone();
                tokio::spawn(async move {
                    if let Err(e) = engine.handle_inbound(stream, peer_addr).await {
                        debug!("inbound peer {peer_addr} disconnected: {e:#}");
                    }
                });
            }
        });
        Ok((bound, handle))
    }

    /// Outbound: connect to a known peer and start a session.
    pub async fn connect_to(&self, peer: Peer) -> Result<()> {
        let trusted = self.trusted.lock().await.clone();
        if !trusted.contains(&peer.device_id) {
            anyhow::bail!("device {} is not trusted", peer.device_id);
        }
        let manager = ConnectionManager::new(self.identity.clone(), trusted, vec![]);
        let conn = manager
            .connect(&DiscoveredPeer {
                device_id: peer.device_id.clone(),
                address: peer.address,
            })
            .await
            .with_context(|| {
                format!("connecting to peer {} at {}", peer.device_id, peer.address)
            })?;
        let framed = FramedPeer::from_connection(conn)?;
        let engine = self.clone();
        let device_id = peer.device_id.clone();
        tokio::spawn(async move {
            if let Err(e) = engine.run_session(device_id.clone(), framed).await {
                debug!("outbound session to {device_id} ended: {e:#}");
            }
        });
        Ok(())
    }

    /// Inbound handler — completes the TLS handshake then runs a session.
    async fn handle_inbound(
        &self,
        stream: tokio::net::TcpStream,
        peer_addr: SocketAddr,
    ) -> Result<()> {
        let trusted = self.trusted.lock().await.clone();
        let manager = ConnectionManager::new(self.identity.clone(), trusted, vec![]);
        let (device_id, tls) = manager
            .accept(stream)
            .await
            .with_context(|| format!("accepting inbound from {peer_addr}"))?;
        info!("inbound P2P connection accepted from device {device_id}");
        let framed = FramedPeer::from_tls(tls);
        self.run_session(device_id, framed).await
    }

    /// Drive a peer session: send our handshake, then read frames and
    /// respond. Returns when the read loop terminates.
    async fn run_session(&self, device_id: String, framed: FramedPeer) -> Result<()> {
        let (mut reader, mut writer) = framed.split();

        // Outbound channel — the writer task drains this.
        let (tx, mut rx) = mpsc::unbounded_channel::<BepMessage>();
        let pending: Arc<Mutex<VecDeque<oneshot::Sender<Vec<u8>>>>> =
            Arc::new(Mutex::new(VecDeque::new()));

        // Register handle.
        {
            let mut peers = self.peers.lock().await;
            peers.insert(
                device_id.clone(),
                PeerHandle {
                    outbound: tx.clone(),
                    pending: pending.clone(),
                },
            );
        }

        // Writer task — pump outbound messages.
        let device_for_writer = device_id.clone();
        let writer_task = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if let Err(e) = writer.send(&msg).await {
                    debug!("writer to {device_for_writer} failed: {e:#}");
                    return;
                }
            }
            let _ = writer.shutdown().await;
        });

        // Handshake: ClusterConfig then Index.
        tx.send(BepMessage::ClusterConfig {
            folders: vec![Folder {
                id: self.folder_id.clone(),
                label: self.folder_id.clone(),
            }],
        })
        .ok();
        let snapshot = self.snapshot_for_peer()?;
        tx.send(BepMessage::Index {
            folder: self.folder_id.clone(),
            files: snapshot,
        })
        .ok();

        // Read loop.
        let result = loop {
            let msg = match reader.recv().await {
                Ok(Some(m)) => m,
                Ok(None) => break Ok(()),
                Err(e) => break Err(e),
            };
            if let Err(e) = self.handle_message(msg, &tx, &pending).await {
                break Err(e);
            }
        };

        // Cleanup.
        {
            let mut peers = self.peers.lock().await;
            peers.remove(&device_id);
        }
        drop(tx);
        let _ = writer_task.await;
        result
    }

    /// Build a [`Vec<FileInfo>`] describing every row in the index,
    /// including tombstones. Directory rows are skipped — BEP carries
    /// them implicitly via the parent path of each file.
    fn snapshot_for_peer(&self) -> Result<Vec<FileInfo>> {
        let entries = self.index.entries_since(0)?;
        let mut files = Vec::with_capacity(entries.len());
        for entry in entries {
            if entry.is_dir {
                continue;
            }
            files.push(entry_to_file_info(&entry)?);
        }
        Ok(files)
    }

    /// Dispatch one incoming message.
    async fn handle_message(
        &self,
        msg: BepMessage,
        outbound: &mpsc::UnboundedSender<BepMessage>,
        pending: &Arc<Mutex<VecDeque<oneshot::Sender<Vec<u8>>>>>,
    ) -> Result<()> {
        match msg {
            BepMessage::ClusterConfig { .. } | BepMessage::Ping => Ok(()),
            BepMessage::Index { folder, files } | BepMessage::IndexUpdate { folder, files } => {
                if folder != self.folder_id {
                    debug!("ignoring frame for unknown folder {folder}");
                    return Ok(());
                }
                self.merge_files(&files)?;
                Ok(())
            }
            BepMessage::Request {
                folder,
                name: _,
                block_offset: _,
                block_size: _,
                block_hash,
            } => {
                if folder != self.folder_id {
                    outbound
                        .send(BepMessage::Response { data: Vec::new() })
                        .ok();
                    return Ok(());
                }
                let hash = BlockHash(block_hash);
                let data = self
                    .blocks
                    .get_block(&hash)
                    .await
                    .unwrap_or(None)
                    .unwrap_or_default();
                outbound.send(BepMessage::Response { data }).ok();
                Ok(())
            }
            BepMessage::Response { data } => {
                let waiter = {
                    let mut pending = pending.lock().await;
                    pending.pop_front()
                };
                if let Some(waiter) = waiter {
                    let _ = waiter.send(data);
                } else {
                    debug!("dropping unsolicited Response ({} bytes)", data.len());
                }
                Ok(())
            }
            BepMessage::Close { reason } => {
                debug!("peer closed connection: {reason}");
                anyhow::bail!("peer closed: {reason}")
            }
        }
    }

    /// Merge a peer-provided file list into the local index using LWW
    /// on `modified` timestamps. Newer rows replace local entries.
    /// Tombstones (`deleted == true`) mark the local row deleted
    /// instead of overwriting it.
    fn merge_files(&self, files: &[FileInfo]) -> Result<()> {
        for file in files {
            if file.file_type == FILE_TYPE_DIR {
                continue;
            }
            if file.file_type != FILE_TYPE_FILE {
                debug!("ignoring file_type {} for {}", file.file_type, file.name);
                continue;
            }
            let local = self.index.get(&file.name)?;
            if let Some(local) = &local {
                // Delete wins on equal timestamps; otherwise strict
                // greater-than means the local row is newer and the
                // incoming one is stale.
                if file.deleted {
                    if local.modified > file.modified {
                        continue;
                    }
                } else if local.modified >= file.modified {
                    continue;
                }
            }
            if file.deleted {
                if local.is_some() {
                    self.index.mark_deleted(&file.name)?;
                } else {
                    // Tombstone for a path we have never seen. Insert a
                    // synthetic deleted row so we can propagate the
                    // delete to peers that join later.
                    let entry = IndexEntry {
                        path: file.name.clone(),
                        is_dir: false,
                        size: 0,
                        modified: file.modified,
                        block_hashes: vec![],
                        deleted: true,
                        version: 0,
                    };
                    self.index.upsert(&entry)?;
                }
                continue;
            }
            let mut hash_blob = Vec::with_capacity(file.block_hashes.len() * 32);
            for h in &file.block_hashes {
                hash_blob.extend_from_slice(h);
            }
            let entry = IndexEntry {
                path: file.name.clone(),
                is_dir: false,
                size: file.size,
                modified: file.modified,
                block_hashes: hash_blob,
                deleted: false,
                version: 0,
            };
            self.index.upsert(&entry)?;
        }
        Ok(())
    }

    /// Broadcast an `IndexUpdate` for a single locally-changed entry.
    /// Tombstones (`entry.deleted`) are sent so peers can mirror the
    /// delete. Directory rows are still skipped — BEP carries
    /// directories implicitly via the parent path of each file.
    pub async fn broadcast_update(&self, entry: &IndexEntry) {
        if entry.is_dir {
            return;
        }
        let Ok(file_info) = entry_to_file_info(entry) else {
            return;
        };
        let msg = BepMessage::IndexUpdate {
            folder: self.folder_id.clone(),
            files: vec![file_info],
        };
        let peers = self.peers.lock().await;
        for handle in peers.values() {
            let _ = handle.outbound.send(msg.clone());
        }
    }

    /// Try every connected peer for `hash`, returning the first match.
    pub async fn fetch_block(
        &self,
        name: &str,
        block_index: usize,
        block_size: u32,
        hash: [u8; 32],
    ) -> Option<Vec<u8>> {
        let peers: Vec<(String, PeerHandle)> = {
            let map = self.peers.lock().await;
            map.iter()
                .map(|(id, h)| {
                    (
                        id.clone(),
                        PeerHandle {
                            outbound: h.outbound.clone(),
                            pending: h.pending.clone(),
                        },
                    )
                })
                .collect()
        };
        let offset = (block_index as u64) * u64::from(block_size);
        for (device_id, handle) in peers {
            match handle
                .request_block(
                    self.folder_id.clone(),
                    name.to_string(),
                    offset,
                    block_size,
                    hash,
                )
                .await
            {
                Ok(data) if !data.is_empty() && BlockHash::from_data(&data).0 == hash => {
                    return Some(data);
                }
                Ok(_) => debug!("peer {device_id} responded with empty/mismatched block"),
                Err(e) => debug!("peer {device_id} block request failed: {e:#}"),
            }
        }
        None
    }
}

fn entry_to_file_info(entry: &IndexEntry) -> Result<FileInfo> {
    let block_size = cascade_p2p::block::block_size_for_file(entry.size);
    let mut hashes = Vec::with_capacity(entry.block_hashes.len() / 32);
    for chunk in entry.block_hashes.chunks(32) {
        let mut h = [0u8; 32];
        if chunk.len() != 32 {
            anyhow::bail!("malformed block_hashes column: trailing partial hash");
        }
        h.copy_from_slice(chunk);
        hashes.push(h);
    }
    Ok(FileInfo {
        name: entry.path.clone(),
        file_type: FILE_TYPE_FILE,
        size: entry.size,
        modified: entry.modified,
        block_size,
        deleted: entry.deleted,
        block_hashes: hashes,
    })
}

/// Standard subdirectory under the backend data dir used by the sync
/// engine for any auxiliary state. Reserved for future use.
#[must_use]
pub fn sync_state_dir(base: &std::path::Path) -> PathBuf {
    base.join("sync")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use cascade_p2p::identity::DeviceIdentity;
    use tempfile::tempdir;

    async fn make_engine(folder_id: &str) -> (tempfile::TempDir, SyncEngine) {
        let dir = tempdir().unwrap();
        let index = Arc::new(FolderIndex::open(&dir.path().join("idx.db")).unwrap());
        let blocks = Arc::new(BlockStore::new(&dir.path().join("blocks")).unwrap());
        let identity = DeviceIdentity::generate().unwrap();
        let engine = SyncEngine::new(folder_id.to_string(), index, blocks, identity);
        (dir, engine)
    }

    /// Two engines on loopback. A uploads a file, B should see it in
    /// its index after the IndexUpdate broadcast.
    #[tokio::test]
    async fn upload_propagates_via_index_update() {
        let (_dir_a, engine_a) = make_engine("shared").await;
        let (_dir_b, engine_b) = make_engine("shared").await;

        engine_a.trust(engine_b.device_id().to_string()).await;
        engine_b.trust(engine_a.device_id().to_string()).await;

        let (addr_b, _b_task) = engine_b
            .start_listener("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        engine_a
            .connect_to(Peer {
                device_id: engine_b.device_id().to_string(),
                address: addr_b,
            })
            .await
            .unwrap();

        // Let the handshake settle.
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Upload on A by inserting directly into A's index, then broadcast.
        let entry = IndexEntry {
            path: "hello.txt".to_string(),
            is_dir: false,
            size: 11,
            modified: 1_700_000_000,
            block_hashes: vec![0u8; 32],
            deleted: false,
            version: 0,
        };
        engine_a.index.upsert(&entry).unwrap();
        engine_a.broadcast_update(&entry).await;

        // Wait for B to receive the IndexUpdate.
        for _ in 0..40 {
            if engine_b.index.get("hello.txt").unwrap().is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("hello.txt did not appear in B's index");
    }

    /// Block-level fetch round trip. A has a block, B requests it.
    #[tokio::test]
    async fn fetch_block_from_peer() {
        let (_dir_a, engine_a) = make_engine("shared").await;
        let (_dir_b, engine_b) = make_engine("shared").await;

        engine_a.trust(engine_b.device_id().to_string()).await;
        engine_b.trust(engine_a.device_id().to_string()).await;

        // Pre-populate A's block store.
        let data = b"the quick brown fox jumps over the lazy dog".repeat(10);
        let hash = BlockHash::from_data(&data);
        engine_a.blocks.store_block(&hash, &data).await.unwrap();

        let (addr_a, _a_task) = engine_a
            .start_listener("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        engine_b
            .connect_to(Peer {
                device_id: engine_a.device_id().to_string(),
                address: addr_a,
            })
            .await
            .unwrap();

        // Let the handshake settle so the peer handle is registered.
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let peers = engine_b.peers.lock().await;
            if peers.contains_key(engine_a.device_id()) {
                break;
            }
        }

        let fetched = engine_b
            .fetch_block(
                "anything.bin",
                0,
                u32::try_from(data.len()).unwrap(),
                hash.0,
            )
            .await
            .expect("expected to fetch block from peer");
        assert_eq!(fetched, data);
    }

    #[tokio::test]
    async fn merge_files_skips_older_local() {
        let (_dir, engine) = make_engine("f").await;
        engine
            .index
            .upsert(&IndexEntry {
                path: "doc.txt".into(),
                is_dir: false,
                size: 10,
                modified: 2_000_000_000,
                block_hashes: vec![0u8; 32],
                deleted: false,
                version: 0,
            })
            .unwrap();

        // Older incoming row should not displace the local one.
        engine
            .merge_files(&[FileInfo {
                name: "doc.txt".into(),
                file_type: FILE_TYPE_FILE,
                size: 99,
                modified: 1_000_000_000,
                block_size: 128 * 1024,
                deleted: false,
                block_hashes: vec![[1u8; 32]],
            }])
            .unwrap();
        let after = engine.index.get("doc.txt").unwrap().unwrap();
        assert_eq!(after.size, 10);
        assert_eq!(after.modified, 2_000_000_000);
    }

    #[tokio::test]
    async fn merge_files_takes_newer_peer() {
        let (_dir, engine) = make_engine("f").await;
        engine
            .index
            .upsert(&IndexEntry {
                path: "doc.txt".into(),
                is_dir: false,
                size: 10,
                modified: 1_000_000_000,
                block_hashes: vec![0u8; 32],
                deleted: false,
                version: 0,
            })
            .unwrap();
        engine
            .merge_files(&[FileInfo {
                name: "doc.txt".into(),
                file_type: FILE_TYPE_FILE,
                size: 99,
                modified: 2_000_000_000,
                block_size: 128 * 1024,
                deleted: false,
                block_hashes: vec![[1u8; 32]],
            }])
            .unwrap();
        let after = engine.index.get("doc.txt").unwrap().unwrap();
        assert_eq!(after.size, 99);
        assert_eq!(after.modified, 2_000_000_000);
        assert_eq!(after.block_hashes, vec![1u8; 32]);
    }

    #[tokio::test]
    async fn merge_files_ignores_directory_entries() {
        let (_dir, engine) = make_engine("f").await;
        engine
            .merge_files(&[FileInfo {
                name: "subdir".into(),
                file_type: FILE_TYPE_DIR,
                size: 0,
                modified: 1_000_000_000,
                block_size: 128 * 1024,
                deleted: false,
                block_hashes: vec![],
            }])
            .unwrap();
        assert!(engine.index.get("subdir").unwrap().is_none());
    }

    #[tokio::test]
    async fn merge_files_applies_tombstone() {
        let (_dir, engine) = make_engine("f").await;
        // Seed an undeleted local row.
        engine
            .index
            .upsert(&IndexEntry {
                path: "doc.txt".into(),
                is_dir: false,
                size: 10,
                modified: 1_000_000_000,
                block_hashes: vec![0u8; 32],
                deleted: false,
                version: 0,
            })
            .unwrap();
        // Incoming tombstone with a newer timestamp should win.
        engine
            .merge_files(&[FileInfo {
                name: "doc.txt".into(),
                file_type: FILE_TYPE_FILE,
                size: 0,
                modified: 2_000_000_000,
                block_size: 128 * 1024,
                deleted: true,
                block_hashes: vec![],
            }])
            .unwrap();
        let after = engine.index.get("doc.txt").unwrap().unwrap();
        assert!(after.deleted, "row should be marked deleted");
    }

    #[tokio::test]
    async fn merge_files_creates_tombstone_for_unknown_path() {
        let (_dir, engine) = make_engine("f").await;
        // No prior upsert for "gone.txt".
        engine
            .merge_files(&[FileInfo {
                name: "gone.txt".into(),
                file_type: FILE_TYPE_FILE,
                size: 0,
                modified: 1_700_000_000,
                block_size: 128 * 1024,
                block_hashes: vec![],
                deleted: true,
            }])
            .unwrap();
        let row = engine
            .index
            .get("gone.txt")
            .unwrap()
            .expect("tombstone row should exist");
        assert!(row.deleted);
        assert_eq!(row.modified, 1_700_000_000);
    }

    #[tokio::test]
    async fn merge_files_tombstone_wins_on_tied_modified() {
        let (_dir, engine) = make_engine("f").await;
        let ts = 1_700_000_000;
        engine
            .index
            .upsert(&IndexEntry {
                path: "doc.txt".into(),
                is_dir: false,
                size: 10,
                modified: ts,
                block_hashes: vec![0u8; 32],
                deleted: false,
                version: 0,
            })
            .unwrap();
        engine
            .merge_files(&[FileInfo {
                name: "doc.txt".into(),
                file_type: FILE_TYPE_FILE,
                size: 0,
                modified: ts, // same timestamp
                block_size: 128 * 1024,
                block_hashes: vec![],
                deleted: true,
            }])
            .unwrap();
        let row = engine.index.get("doc.txt").unwrap().unwrap();
        assert!(row.deleted, "delete should win on tied modified");
    }

    #[tokio::test]
    async fn merge_files_skips_unknown_file_type() {
        let (_dir, engine) = make_engine("f").await;
        engine
            .merge_files(&[FileInfo {
                name: "weird".into(),
                file_type: 99,
                size: 1,
                modified: 1_000_000_000,
                block_size: 128 * 1024,
                deleted: false,
                block_hashes: vec![[0u8; 32]],
            }])
            .unwrap();
        assert!(engine.index.get("weird").unwrap().is_none());
    }

    #[tokio::test]
    async fn broadcast_update_skips_directories() {
        let (_dir, engine) = make_engine("f").await;
        // No peers connected; broadcast should be a quiet no-op for
        // dir entries (we just confirm no panic). Tombstones are now
        // broadcast normally and are exercised by the integration test.
        let dir = IndexEntry {
            path: "subdir".into(),
            is_dir: true,
            size: 0,
            modified: 0,
            block_hashes: vec![],
            deleted: false,
            version: 0,
        };
        engine.broadcast_update(&dir).await;
    }

    #[tokio::test]
    async fn entry_to_file_info_rejects_partial_hash() {
        let entry = IndexEntry {
            path: "bad.txt".into(),
            is_dir: false,
            size: 1,
            modified: 0,
            block_hashes: vec![0u8; 31], // not a multiple of 32
            deleted: false,
            version: 0,
        };
        let err = entry_to_file_info(&entry).unwrap_err();
        assert!(err.to_string().contains("partial hash"));
    }
}
