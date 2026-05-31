//! Peer sync orchestration for the P2P backend.
//!
//! Wires the existing `cascade-p2p` transport (TLS-authenticated peer
//! connections, BEP message framing) to the local [`FolderIndex`] and
//! [`BlockStore`]. The model is Syncthing-style:
//!
//! 1. On a successful connection (in or out), each side sends a
//!    [`BepMessage::ClusterConfig`] followed by [`BepMessage::Index`]
//!    enumerating every file row in the index — including tombstones —
//!    with its block hash list and per-file version vector.
//! 2. Incoming `Index` and `IndexUpdate` frames are merged into the
//!    local index using per-file version vector dominance. If the
//!    local row dominates the incoming version, the incoming row is
//!    ignored. If the incoming version dominates the local row, the
//!    incoming row wins. If neither dominates (concurrent edit on
//!    disconnected peers), a `tracing::warn!` records the conflict and
//!    the incoming row is accepted — the persisted-conflict-copy
//!    behaviour is a follow-up. A row with `FileInfo.deleted` set
//!    marks the local entry as a tombstone instead of overwriting
//!    its blocks.
//! 3. Local writes (`upload`, `update`, `delete`) bump the local
//!    device's counter in the row's version vector, then broadcast an
//!    `IndexUpdate` frame with the new row — tombstones included — to
//!    every connected peer.
//! 4. When a `Backend::download` call discovers blocks missing from
//!    the local [`BlockStore`], each connected peer is asked in turn
//!    via [`BepMessage::Request`] and the first matching block is kept.
//!
//! Each [`BepMessage::Request`] carries a monotonic per-peer `request_id`
//! chosen by the requester; the peer echoes it in the corresponding
//! [`BepMessage::Response`]. The requester routes the payload to the
//! matching waiter via a `HashMap<u64, oneshot::Sender>`, so multiple
//! block requests can be in flight on one connection without queueing
//! behind each other.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use cascade_p2p::block::BlockHash;
use cascade_p2p::connection::ConnectionManager;
use cascade_p2p::discovery::DiscoveredPeer;
use cascade_p2p::framed::FramedPeer;
use cascade_p2p::identity::DeviceIdentity;
use cascade_p2p::protocol::{BepMessage, FileInfo, Folder, Version};
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
    /// Outstanding block requests, keyed by the `request_id` allocated
    /// when the Request frame was sent. The responder echoes the id in
    /// the matching Response so the entry can be removed and the payload
    /// delivered to the right waiter.
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Vec<u8>>>>>,
    /// Source of fresh `request_id` values for outbound Request frames.
    next_request_id: Arc<AtomicU64>,
}

impl PeerHandle {
    /// Send a Request frame and await the matching Response payload,
    /// correlated by the `request_id` carried in both frames.
    async fn request_block(
        &self,
        folder: String,
        name: String,
        offset: u64,
        size: u32,
        hash: [u8; 32],
    ) -> Result<Vec<u8>> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(request_id, tx);
        }
        self.outbound
            .send(BepMessage::Request {
                request_id,
                folder,
                name,
                block_offset: offset,
                block_size: size,
                block_hash: hash,
            })
            .map_err(|_| anyhow::anyhow!("peer outbound channel closed"))?;

        match tokio::time::timeout(BLOCK_REQUEST_TIMEOUT, rx).await {
            Ok(Ok(data)) => Ok(data),
            Ok(Err(_)) => {
                // Responder dropped without sending — clean up the map
                // entry so it doesn't leak if the session is still alive.
                let mut pending = self.pending.lock().await;
                pending.remove(&request_id);
                anyhow::bail!("peer session dropped before responding");
            }
            Err(_) => {
                let mut pending = self.pending.lock().await;
                pending.remove(&request_id);
                anyhow::bail!("peer block request timed out");
            }
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
    /// 64-bit derivation of the device identity used as this device's
    /// entry key in version vectors. Stable across restarts because
    /// it is derived from the persistent `DeviceIdentity::device_id`.
    device_short_id: u64,
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
        let device_short_id = derive_device_short_id(&identity.device_id);
        Self {
            folder_id,
            index,
            blocks,
            identity,
            device_short_id,
            trusted: Arc::new(Mutex::new(Vec::new())),
            peers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// This device's short id, used as the entry key in version
    /// vectors. Stable across restarts.
    #[must_use]
    pub const fn device_short_id(&self) -> u64 {
        self.device_short_id
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
    ///
    /// The `cancel` receiver lets the caller break the accept loop when
    /// the owning backend is dropped. Without it, dropping the
    /// `JoinHandle` would detach the task rather than abort it, leaving
    /// the accept loop alive forever and keeping `Arc<FolderIndex>` and
    /// `Arc<BlockStore>` pinned through the engine clone.
    pub async fn start_listener(
        &self,
        addr: SocketAddr,
        mut cancel: tokio::sync::watch::Receiver<bool>,
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
                tokio::select! {
                    res = cancel.changed() => {
                        if res.is_err() || *cancel.borrow() {
                            debug!(
                                target: "cascade::backend::p2p",
                                "listener task cancelled",
                            );
                            break;
                        }
                    }
                    accept_result = listener.accept() => {
                        match accept_result {
                            Ok((stream, peer_addr)) => {
                                let engine = engine.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = engine.handle_inbound(stream, peer_addr).await {
                                        debug!(
                                            "inbound peer {peer_addr} disconnected: {e:#}",
                                        );
                                    }
                                });
                            }
                            Err(e) => {
                                warn!("P2P listener accept failed: {e}");
                                tokio::select! {
                                    () = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
                                    res = cancel.changed() => {
                                        if res.is_err() || *cancel.borrow() {
                                            debug!(
                                                target: "cascade::backend::p2p",
                                                "listener task cancelled during back-off",
                                            );
                                            return;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
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
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Vec<u8>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let next_request_id = Arc::new(AtomicU64::new(0));

        // Register handle.
        {
            let mut peers = self.peers.lock().await;
            peers.insert(
                device_id.clone(),
                PeerHandle {
                    outbound: tx.clone(),
                    pending: pending.clone(),
                    next_request_id: next_request_id.clone(),
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
        pending: &Arc<Mutex<HashMap<u64, oneshot::Sender<Vec<u8>>>>>,
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
                request_id,
                folder,
                name: _,
                block_offset: _,
                block_size: _,
                block_hash,
            } => {
                if folder != self.folder_id {
                    outbound
                        .send(BepMessage::Response {
                            request_id,
                            data: Vec::new(),
                        })
                        .ok();
                    return Ok(());
                }
                let hash = BlockHash(block_hash);
                let data = match self.blocks.get_block(&hash).await {
                    Ok(Some(data)) => data,
                    Ok(None) => {
                        // Block genuinely not in our store — send an empty
                        // response so the requester treats it as a miss and
                        // tries the next peer.
                        Vec::new()
                    }
                    Err(e) => {
                        // Block store error — log it, then send an empty
                        // response so the requester can fall through to the
                        // next peer. Returning Err here would tear down the
                        // whole session for one bad lookup.
                        tracing::warn!(
                            target: "cascade::backend::p2p",
                            block = %hash,
                            error = %e,
                            "block store get failed while serving peer request"
                        );
                        Vec::new()
                    }
                };
                outbound
                    .send(BepMessage::Response { request_id, data })
                    .ok();
                Ok(())
            }
            BepMessage::Response { request_id, data } => {
                let waiter = {
                    let mut pending = pending.lock().await;
                    pending.remove(&request_id)
                };
                if let Some(waiter) = waiter {
                    let _ = waiter.send(data);
                } else {
                    debug!(
                        "dropping unsolicited Response for unknown request_id {request_id} ({} bytes)",
                        data.len(),
                    );
                }
                Ok(())
            }
            BepMessage::Close { reason } => {
                debug!("peer closed connection: {reason}");
                anyhow::bail!("peer closed: {reason}")
            }
        }
    }

    /// Merge a peer-provided file list into the local index using
    /// per-file version vector dominance. The local row wins if it
    /// dominates the incoming version; the incoming row wins if it
    /// dominates the local. Equal vectors are no-ops. When neither
    /// dominates (concurrent edit on disconnected peers) the local row
    /// is preserved as a conflict copy at a sibling path before the
    /// incoming content overwrites it.
    ///
    /// A peer row with `FileInfo.deleted` set marks the local entry
    /// as a tombstone rather than overwriting its blocks.
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
            let incoming_version = file.version.clone();
            // The version vector to persist alongside the incoming row.
            // When there's no local row, or when the incoming row strictly
            // dominates, the incoming counters are correct as-is. On a
            // concurrent edit we must merge the two vectors so that a third
            // peer later receiving this row sees the union of history —
            // otherwise the local device's counter is silently dropped and
            // a subsequent dominance check against that peer regresses.
            let mut persisted_version = incoming_version.clone();
            if let Some(local_entry) = &local {
                let local_version = Version {
                    counters: local_entry.version.clone(),
                };
                if local_version == incoming_version {
                    // Identical version vectors — nothing to do.
                    continue;
                }
                if local_version.dominates(&incoming_version) {
                    // Local row is strictly newer; ignore incoming.
                    continue;
                }
                if !incoming_version.dominates(&local_version) {
                    // Neither dominates — concurrent edit. The version
                    // vector itself must merge so a third peer later
                    // receiving this row sees the union of history.
                    // Preserve the local row's content as a conflict
                    // copy at a sibling path before the incoming
                    // content overwrites the original.
                    persisted_version.merge(&local_version);
                    self.persist_conflict_copy(&file.name, local_entry)?;
                }
            }
            if file.deleted {
                let entry = IndexEntry {
                    path: file.name.clone(),
                    is_dir: false,
                    size: 0,
                    modified: file.modified,
                    block_hashes: vec![],
                    deleted: true,
                    row_version: 0,
                    version: persisted_version.counters,
                };
                self.index.upsert(&entry)?;
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
                row_version: 0,
                version: persisted_version.counters,
            };
            self.index.upsert(&entry)?;
        }
        Ok(())
    }

    /// Persist the local row at `original_path` as a conflict copy at
    /// a sibling path before the row is overwritten by an incoming
    /// concurrent write. The conflict copy is a snapshot of the local
    /// state — same content, same modified time, same version vector —
    /// but at a unique path so it does not collide on any peer.
    ///
    /// A row whose content is empty (zero size, no block hashes) is
    /// skipped — there's nothing meaningful to preserve.
    fn persist_conflict_copy(&self, original_path: &str, local: &IndexEntry) -> Result<()> {
        if local.size == 0 && local.block_hashes.is_empty() {
            return Ok(());
        }
        let short_id = local_short_device_id(self.device_id());
        let timestamp = unix_timestamp_seconds();
        let conflict_path = conflict_copy_path(original_path, &short_id, timestamp);
        let conflict_entry = IndexEntry {
            path: conflict_path.clone(),
            is_dir: false,
            size: local.size,
            modified: local.modified,
            block_hashes: local.block_hashes.clone(),
            deleted: false,
            row_version: 0,
            version: local.version.clone(),
        };
        self.index.upsert(&conflict_entry)?;
        info!(
            target: "cascade::backend::p2p",
            path = %original_path,
            conflict_copy = %conflict_path,
            "preserved local version as conflict copy",
        );
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
                            next_request_id: h.next_request_id.clone(),
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

/// Build the path at which to persist a conflict copy of a row whose
/// content is about to be overwritten by an incoming concurrent write.
///
/// The format is `<stem>.conflict-<short_device_id>-<timestamp>.<ext>`
/// where the stem and extension are split on the LAST `.` in the
/// filename. A leading dot is treated as a hidden-file marker rather
/// than an extension separator, so `.gitignore` becomes
/// `.gitignore.conflict-<id>-<ts>` with no trailing extension.
fn conflict_copy_path(original: &str, short_device_id: &str, timestamp: i64) -> String {
    let (parent, filename) = match original.rsplit_once('/') {
        Some((p, f)) => (Some(p), f),
        None => (None, original),
    };
    let (stem, ext) = split_filename(filename);
    let suffixed = if ext.is_empty() {
        format!("{stem}.conflict-{short_device_id}-{timestamp}")
    } else {
        format!("{stem}.conflict-{short_device_id}-{timestamp}.{ext}")
    };
    match parent {
        Some(p) => format!("{p}/{suffixed}"),
        None => suffixed,
    }
}

/// Split a filename into `(stem, extension)` on the LAST `.`. A leading
/// dot is treated as part of the stem (hidden-file convention), not as
/// an extension separator. An empty extension means there is no
/// extension to preserve.
fn split_filename(filename: &str) -> (&str, &str) {
    // Skip the leading dot for the purposes of finding the extension
    // separator — `.gitignore` is a stem, not a stem + ext. `split_at`
    // panics on an out-of-bounds index; the `min(filename.len())` guard
    // makes the bound trivially in range and avoids the workspace's
    // `indexing_slicing` lint.
    let search_start = usize::from(filename.starts_with('.'));
    let (_, search_slice) = filename.split_at(search_start.min(filename.len()));
    search_slice.rfind('.').map_or((filename, ""), |rel_idx| {
        let abs_idx = search_start + rel_idx;
        let (stem, dot_ext) = filename.split_at(abs_idx);
        // Strip the leading '.' from the extension half. `dot_ext` is
        // non-empty (it starts with the `.` we just located via
        // `rfind`).
        let (_, ext) = dot_ext.split_at(1);
        (stem, ext)
    })
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
        version: Version {
            counters: entry.version.clone(),
        },
        block_hashes: hashes,
    })
}

/// Derive this device's 64-bit short id from its persistent device id.
///
/// `DeviceIdentity::device_id` is a 52-character base32 SHA-256 of the
/// TLS certificate. Hashing again and folding to 8 bytes gives a
/// stable per-device u64 to use as the version vector entry key.
fn derive_device_short_id(device_id: &str) -> u64 {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(device_id.as_bytes());
    // SHA-256 is always 32 bytes; take the first 8 via `chunks_exact`
    // so the slice access satisfies the workspace's `indexing_slicing`
    // lint without requiring an `#[allow]` escape.
    let (head, _) = digest.as_slice().split_at(8);
    let mut buf = [0u8; 8];
    buf.copy_from_slice(head);
    u64::from_be_bytes(buf)
}

/// Return the first 8 characters of `device_id` for use as a short,
/// human-readable identifier in conflict-copy paths. `DeviceIdentity::device_id`
/// is a base32-encoded SHA-256 (52 chars), so 8 chars is plenty to
/// distinguish devices in practice without overflowing path budgets.
fn local_short_device_id(device_id: &str) -> String {
    let take = device_id.len().min(8);
    let (head, _) = device_id.split_at(take);
    head.to_string()
}

/// Current wall-clock time as seconds since the Unix epoch. Used to
/// stamp conflict-copy filenames so concurrent edits at the same path
/// produce distinct sibling paths.
fn unix_timestamp_seconds() -> i64 {
    let now = SystemTime::now();
    let secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    // Saturating cast — wall-clock seconds within i64 range for ~292B
    // years; the only way to hit the ceiling is a malformed clock, in
    // which case the saturating value is still a valid (if odd) sibling
    // path stamp.
    i64::try_from(secs).unwrap_or(i64::MAX)
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

        let (_cancel_tx_b, cancel_rx_b) = tokio::sync::watch::channel(false);
        let (addr_b, _b_task) = engine_b
            .start_listener("127.0.0.1:0".parse().unwrap(), cancel_rx_b)
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
            row_version: 0,
            version: Vec::new(),
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

        let (_cancel_tx_a, cancel_rx_a) = tokio::sync::watch::channel(false);
        let (addr_a, _a_task) = engine_a
            .start_listener("127.0.0.1:0".parse().unwrap(), cancel_rx_a)
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

    /// Two concurrent block requests against the same peer must each
    /// get a distinct `request_id` and must each receive the right
    /// payload — no Response can be misrouted by FIFO order.
    #[tokio::test]
    async fn request_block_uses_distinct_request_ids_concurrently() {
        let (_dir_a, engine_a) = make_engine("shared").await;
        let (_dir_b, engine_b) = make_engine("shared").await;

        engine_a.trust(engine_b.device_id().to_string()).await;
        engine_b.trust(engine_a.device_id().to_string()).await;

        // Two distinct blocks on A's store.
        let data_x = vec![0xAAu8; 4096];
        let data_y = vec![0xBBu8; 4096];
        let hash_x = BlockHash::from_data(&data_x);
        let hash_y = BlockHash::from_data(&data_y);
        engine_a.blocks.store_block(&hash_x, &data_x).await.unwrap();
        engine_a.blocks.store_block(&hash_y, &data_y).await.unwrap();

        let (_cancel_tx_a, cancel_rx_a) = tokio::sync::watch::channel(false);
        let (addr_a, _a_task) = engine_a
            .start_listener("127.0.0.1:0".parse().unwrap(), cancel_rx_a)
            .await
            .unwrap();
        engine_b
            .connect_to(Peer {
                device_id: engine_a.device_id().to_string(),
                address: addr_a,
            })
            .await
            .unwrap();

        // Wait for the peer handle to be registered.
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let peers = engine_b.peers.lock().await;
            if peers.contains_key(engine_a.device_id()) {
                break;
            }
        }

        // Fire both requests concurrently and assert both succeed with
        // the right bytes.
        let size = u32::try_from(data_x.len()).unwrap();
        let engine_b_x = engine_b.clone();
        let engine_b_y = engine_b.clone();
        let fut_x =
            tokio::spawn(async move { engine_b_x.fetch_block("x.bin", 0, size, hash_x.0).await });
        let fut_y =
            tokio::spawn(async move { engine_b_y.fetch_block("y.bin", 0, size, hash_y.0).await });

        let got_x = fut_x.await.unwrap().expect("expected X block");
        let got_y = fut_y.await.unwrap().expect("expected Y block");
        assert_eq!(got_x, data_x);
        assert_eq!(got_y, data_y);

        // The peer's id allocator must have advanced by at least two.
        let peers = engine_b.peers.lock().await;
        let handle = peers.get(engine_a.device_id()).unwrap();
        assert!(
            handle.next_request_id.load(Ordering::Relaxed) >= 2,
            "expected at least two ids consumed",
        );
    }

    #[tokio::test]
    async fn merge_files_skips_when_local_dominates() {
        let (_dir, engine) = make_engine("f").await;
        // Local row carries a strictly newer vector for device 1.
        engine
            .index
            .upsert(&IndexEntry {
                path: "doc.txt".into(),
                is_dir: false,
                size: 10,
                modified: 2_000_000_000,
                block_hashes: vec![0u8; 32],
                deleted: false,
                row_version: 0,
                version: vec![(1, 5)],
            })
            .unwrap();

        // Older-by-vector incoming row (`(1, 2)` < `(1, 5)`) must be ignored.
        engine
            .merge_files(&[FileInfo {
                name: "doc.txt".into(),
                file_type: FILE_TYPE_FILE,
                size: 99,
                modified: 1_000_000_000,
                block_size: 128 * 1024,
                deleted: false,
                version: Version {
                    counters: vec![(1, 2)],
                },
                block_hashes: vec![[1u8; 32]],
            }])
            .unwrap();
        let after = engine.index.get("doc.txt").unwrap().unwrap();
        assert_eq!(after.size, 10);
        assert_eq!(after.version, vec![(1, 5)]);
    }

    #[tokio::test]
    async fn merge_files_takes_dominating_peer() {
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
                row_version: 0,
                version: vec![(1, 1)],
            })
            .unwrap();
        // Incoming dominates: `(1, 1)` is strictly less than `(1, 3)`.
        engine
            .merge_files(&[FileInfo {
                name: "doc.txt".into(),
                file_type: FILE_TYPE_FILE,
                size: 99,
                modified: 2_000_000_000,
                block_size: 128 * 1024,
                deleted: false,
                version: Version {
                    counters: vec![(1, 3)],
                },
                block_hashes: vec![[1u8; 32]],
            }])
            .unwrap();
        let after = engine.index.get("doc.txt").unwrap().unwrap();
        assert_eq!(after.size, 99);
        assert_eq!(after.modified, 2_000_000_000);
        assert_eq!(after.block_hashes, vec![1u8; 32]);
        assert_eq!(after.version, vec![(1, 3)]);
    }

    #[tokio::test]
    async fn merge_files_noop_on_equal_vector() {
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
                row_version: 0,
                version: vec![(1, 1), (2, 2)],
            })
            .unwrap();
        engine
            .merge_files(&[FileInfo {
                name: "doc.txt".into(),
                file_type: FILE_TYPE_FILE,
                size: 99, // would be different content, but vector equals — skip
                modified: 2_000_000_000,
                block_size: 128 * 1024,
                deleted: false,
                version: Version {
                    counters: vec![(1, 1), (2, 2)],
                },
                block_hashes: vec![[1u8; 32]],
            }])
            .unwrap();
        let after = engine.index.get("doc.txt").unwrap().unwrap();
        assert_eq!(after.size, 10, "equal vectors are no-ops");
    }

    #[test]
    fn conflict_copy_path_preserves_extension() {
        assert_eq!(
            conflict_copy_path("docs/report.txt", "7BHJ62FL", 1_700_000_000),
            "docs/report.conflict-7BHJ62FL-1700000000.txt",
        );
    }

    #[test]
    fn conflict_copy_path_handles_no_extension() {
        assert_eq!(
            conflict_copy_path("README", "7BHJ62FL", 1_700_000_000),
            "README.conflict-7BHJ62FL-1700000000",
        );
    }

    #[test]
    fn conflict_copy_path_handles_dot_prefixed_filename() {
        // A leading dot is a hidden-file marker, not an extension
        // separator — the whole `.gitignore` is the stem, so no
        // extension is preserved.
        assert_eq!(
            conflict_copy_path(".gitignore", "7BHJ62FL", 1_700_000_000),
            ".gitignore.conflict-7BHJ62FL-1700000000",
        );
    }

    #[tokio::test]
    async fn merge_files_concurrent_edit_accepts_incoming() {
        let (_dir, engine) = make_engine("f").await;
        // Local row bumped by device 1; incoming row bumped by device 2.
        // Neither dominates — concurrent edit on disconnected peers.
        engine
            .index
            .upsert(&IndexEntry {
                path: "doc.txt".into(),
                is_dir: false,
                size: 10,
                modified: 1_000_000_000,
                block_hashes: vec![0u8; 32],
                deleted: false,
                row_version: 0,
                version: vec![(1, 1)],
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
                version: Version {
                    counters: vec![(2, 1)],
                },
                block_hashes: vec![[1u8; 32]],
            }])
            .unwrap();
        let after = engine.index.get("doc.txt").unwrap().unwrap();
        // Conflict-copy persistence is a follow-up; for now incoming
        // content wins, but the version vector must merge both counters so
        // a third peer sees the full history.
        assert_eq!(after.size, 99);
        assert!(
            after.version.iter().any(|(id, _)| *id == 1),
            "local device counter must survive the merge"
        );
        assert!(
            after.version.iter().any(|(id, _)| *id == 2),
            "remote device counter must be present after the merge"
        );
    }

    #[tokio::test]
    async fn merge_files_merges_version_vectors_on_conflict() {
        let (_dir, engine) = make_engine("f").await;
        // Seed local: version = [(1, 1)]
        engine
            .index
            .upsert(&IndexEntry {
                path: "doc.txt".into(),
                is_dir: false,
                size: 10,
                modified: 100,
                block_hashes: vec![0u8; 32],
                deleted: false,
                row_version: 0,
                version: vec![(1, 1)],
            })
            .unwrap();
        // Receive incoming with concurrent VV: [(2, 1)] — neither dominates.
        engine
            .merge_files(&[FileInfo {
                name: "doc.txt".into(),
                file_type: FILE_TYPE_FILE,
                size: 99,
                modified: 100,
                block_size: 128 * 1024,
                deleted: false,
                version: Version {
                    counters: vec![(2, 1)],
                },
                block_hashes: vec![[1u8; 32]],
            }])
            .unwrap();
        // After merge, the row must contain BOTH counters.
        let row = engine.index.get("doc.txt").unwrap().unwrap();
        assert!(
            row.version.iter().any(|(id, _)| *id == 1),
            "local device counter dropped"
        );
        assert!(
            row.version.iter().any(|(id, _)| *id == 2),
            "remote device counter missing"
        );
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
                version: Version::default(),
                block_hashes: vec![],
            }])
            .unwrap();
        assert!(engine.index.get("subdir").unwrap().is_none());
    }

    #[tokio::test]
    async fn merge_files_applies_dominating_tombstone() {
        let (_dir, engine) = make_engine("f").await;
        // Seed an undeleted local row with version `(1, 1)`.
        engine
            .index
            .upsert(&IndexEntry {
                path: "doc.txt".into(),
                is_dir: false,
                size: 10,
                modified: 1_000_000_000,
                block_hashes: vec![0u8; 32],
                deleted: false,
                row_version: 0,
                version: vec![(1, 1)],
            })
            .unwrap();
        // Incoming tombstone dominates with `(1, 2)`.
        engine
            .merge_files(&[FileInfo {
                name: "doc.txt".into(),
                file_type: FILE_TYPE_FILE,
                size: 0,
                modified: 2_000_000_000,
                block_size: 128 * 1024,
                deleted: true,
                version: Version {
                    counters: vec![(1, 2)],
                },
                block_hashes: vec![],
            }])
            .unwrap();
        let after = engine.index.get("doc.txt").unwrap().unwrap();
        assert!(after.deleted, "row should be marked deleted");
        assert_eq!(after.version, vec![(1, 2)]);
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
                version: Version {
                    counters: vec![(7, 1)],
                },
            }])
            .unwrap();
        let row = engine
            .index
            .get("gone.txt")
            .unwrap()
            .expect("tombstone row should exist");
        assert!(row.deleted);
        assert_eq!(row.modified, 1_700_000_000);
        assert_eq!(row.version, vec![(7, 1)]);
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
                version: Version::default(),
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
            row_version: 0,
            version: Vec::new(),
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
            row_version: 0,
            version: Vec::new(),
        };
        let err = entry_to_file_info(&entry).unwrap_err();
        assert!(err.to_string().contains("partial hash"));
    }

    /// The accept loop must observe the `cancel` watch and exit. Without
    /// this, dropping the `JoinHandle` would detach the task and leave
    /// the loop running forever, pinning the cloned engine (and its
    /// `Arc<FolderIndex>` / `Arc<BlockStore>`) past the backend's
    /// lifetime.
    #[tokio::test]
    async fn start_listener_exits_on_cancel() {
        let (_dir, engine) = make_engine("f").await;
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let (_bound, handle) = engine
            .start_listener("127.0.0.1:0".parse().unwrap(), cancel_rx)
            .await
            .unwrap();
        cancel_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("listener should exit within 2s of cancel")
            .expect("listener task should not panic");
    }
}
