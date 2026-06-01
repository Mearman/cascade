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
//!    disconnected peers), the local row's content is preserved as a
//!    conflict copy at a sibling `<stem>.conflict-<id>-<ts>.<ext>`
//!    path, then the incoming row overwrites the original with the
//!    merged version vector so subsequent comparisons see both
//!    histories. The conflict-copy `<id>` is the friendly name
//!    configured for the LOCAL device when one is set, sanitised for
//!    filesystem use; otherwise it falls back to the first eight
//!    characters of the device id. A row with `FileInfo.deleted` set
//!    marks the local entry as a tombstone instead of overwriting its
//!    blocks.
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
use cascade_p2p::protocol::{BepMessage, FileInfo, Folder, GossipPeer, Version};
use cascade_p2p::store::BlockStore;
use cascade_p2p::wan::PeerBook;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock, mpsc, oneshot};
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
    /// Friendly name for the LOCAL device. When `Some`, used in place
    /// of the opaque short device id when stamping conflict-copy paths.
    /// Seeded from `P2pBackendConfig::device_name`.
    local_device_name: Option<String>,
    /// Friendly-name map `device_id → name`, seeded from the static
    /// peer config at startup. Only used today to give human-readable
    /// labels in logs and conflict-copy paths generated locally; peers
    /// do not exchange friendly names over the wire (that would require
    /// a protocol extension, which is deliberately out of scope).
    peer_names: Arc<RwLock<HashMap<String, String>>>,
    /// Device IDs we are willing to talk to.
    trusted: Arc<Mutex<Vec<String>>>,
    peers: Arc<Mutex<HashMap<String, PeerHandle>>>,
    /// Record of every peer we have successfully connected to (either
    /// direction), keyed by device ID. Populated as a local artefact for
    /// future gossip work; the transport itself is not wired yet.
    peer_book: Arc<RwLock<PeerBook>>,
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
            local_device_name: None,
            peer_names: Arc::new(RwLock::new(HashMap::new())),
            trusted: Arc::new(Mutex::new(Vec::new())),
            peers: Arc::new(Mutex::new(HashMap::new())),
            peer_book: Arc::new(RwLock::new(PeerBook::new())),
        }
    }

    /// Set the friendly name for the LOCAL device. Used by
    /// `persist_conflict_copy` when stamping conflict-copy paths;
    /// without it, the first eight characters of the device id are used
    /// as the fallback identifier.
    ///
    /// Builder-style so this can be threaded through `P2pBackend::open`
    /// without changing the public `SyncEngine::new` signature.
    #[must_use]
    pub fn with_local_device_name(mut self, name: Option<String>) -> Self {
        self.local_device_name = name;
        self
    }

    /// Seed the `device_id → friendly name` map from a list of
    /// `(device_id, name)` pairs. Existing entries are overwritten.
    /// Entries with empty names are ignored — an empty friendly name
    /// is treated as "no friendly name set".
    pub async fn seed_peer_names<I>(&self, entries: I)
    where
        I: IntoIterator<Item = (String, String)>,
    {
        let mut map = self.peer_names.write().await;
        for (device_id, name) in entries {
            if name.is_empty() {
                continue;
            }
            map.insert(device_id, name);
        }
    }

    /// Look up the friendly name previously seeded for `device_id`, if
    /// any. Returns `None` when the device id is unknown or has no
    /// friendly name set.
    pub async fn peer_name(&self, device_id: &str) -> Option<String> {
        let map = self.peer_names.read().await;
        map.get(device_id).cloned()
    }

    /// Friendly name for the LOCAL device, if configured.
    #[must_use]
    pub fn local_device_name(&self) -> Option<&str> {
        self.local_device_name.as_deref()
    }

    /// This device's short id, used as the entry key in version
    /// vectors. Stable across restarts.
    #[must_use]
    pub const fn device_short_id(&self) -> u64 {
        self.device_short_id
    }

    /// Read-only access to the peer book. Used by tests and future
    /// gossip work.
    #[must_use]
    pub const fn peer_book(&self) -> &Arc<RwLock<PeerBook>> {
        &self.peer_book
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
        self.record_peer(&peer.device_id, peer.address).await;
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
        self.record_peer(&device_id, peer_addr).await;
        let framed = FramedPeer::from_tls(tls);
        self.run_session(device_id, framed).await
    }

    /// Record a successful peer contact in the local `PeerBook`. A
    /// repeat call for the same device ID overwrites the recorded
    /// address with the latest one — that matches the realistic case
    /// of a peer reconnecting from a new IP. The contact time is
    /// stamped via [`PeerBook::mark_seen`] so subsequent gossip
    /// broadcasts can carry an accurate per-peer `last_seen` instead of
    /// falling back to the broadcast time.
    async fn record_peer(&self, device_id: &str, address: SocketAddr) {
        let mut book = self.peer_book.write().await;
        book.add_peer(device_id.to_string(), vec![address]);
        book.mark_seen(device_id, unix_timestamp_seconds());
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
        // Delta sync: only send rows whose row_version exceeds the
        // highest sequence we have previously sent to this peer (which
        // we approximate by the highest sequence the peer has reported
        // back to us — they are equal once the previous session
        // completed cleanly, and a conservative lower bound otherwise).
        // First connect to a peer sees `0` and falls through to a full
        // enumeration.
        let last_seen = self.index.get_peer_max_sequence(&device_id).unwrap_or(0);
        let snapshot = self.snapshot_since(last_seen)?;
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
            if let Err(e) = self.handle_message(&device_id, msg, &tx, &pending).await {
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

    /// Build a [`Vec<FileInfo>`] describing every row whose
    /// `row_version` exceeds `since`, including tombstones. Pass `0`
    /// for a full snapshot. Directory rows are skipped — BEP carries
    /// them implicitly via the parent path of each file.
    ///
    /// The sender's `row_version` is encoded as
    /// [`FileInfo::sequence`](cascade_p2p::protocol::FileInfo::sequence)
    /// in the emitted entries; the receiving peer uses the maximum
    /// sequence it observes to bound its next request.
    fn snapshot_since(&self, since: u64) -> Result<Vec<FileInfo>> {
        // The SQLite index stores `row_version` as i64. A `since` value
        // above `i64::MAX` indicates either a wrap-around bug or
        // corrupted state — silently saturating would send an empty
        // Index, hiding the underlying problem. Fail loudly instead.
        let since_i64 = i64::try_from(since)
            .with_context(|| format!("peer max sequence {since} exceeds i64::MAX"))?;
        let entries = self.index.entries_since(since_i64)?;
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
        peer_device_id: &str,
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
                self.merge_files(peer_device_id, &files)?;
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
            BepMessage::Gossip { peers } => {
                // Stamp the sender as just-seen — receiving a gossip
                // frame from them is direct proof of reachability.
                {
                    let mut book = self.peer_book.write().await;
                    book.mark_seen(peer_device_id, unix_timestamp_seconds());
                }
                self.merge_gossip(peer_device_id, peers).await;
                Ok(())
            }
        }
    }

    /// Merge an incoming gossip snapshot into the local peer book.
    ///
    /// Each wire address is parsed as a [`SocketAddr`]; entries that
    /// fail to parse (typically host-name addresses we cannot resolve
    /// here) are dropped silently rather than aborting the merge.
    /// Peers whose entire address list fails to parse are skipped
    /// entirely — there is no point recording a peer with no reachable
    /// endpoints. The broadcaster (`introducer_id`) is recorded for
    /// any newly-learned peer so it can be propagated correctly by
    /// the next outbound gossip frame.
    async fn merge_gossip(&self, introducer_id: &str, peers: Vec<GossipPeer>) {
        // PeerBook::merge_gossip enforces the self-exclusion guard, so
        // we just have to turn the wire-shape GossipPeer into the
        // PeerBook-shape (parse string addresses to SocketAddr,
        // dropping any that don't parse). Empty parsed-address lists
        // mean the peer is unreachable via the addresses they
        // advertised — skip them rather than recording an empty entry.
        let self_id = self.identity.device_id.clone();
        let wire_peers: Vec<cascade_p2p::wan::GossipPeer> = peers
            .into_iter()
            .filter_map(|peer| {
                let parsed: Vec<SocketAddr> = peer
                    .addresses
                    .iter()
                    .filter_map(|a| a.parse().ok())
                    .collect();
                if parsed.is_empty() {
                    None
                } else {
                    Some(cascade_p2p::wan::GossipPeer {
                        device_id: peer.device_id,
                        addresses: parsed,
                    })
                }
            })
            .collect();
        if wire_peers.is_empty() {
            return;
        }
        let message = cascade_p2p::wan::GossipMessage { peers: wire_peers };
        let mut book = self.peer_book.write().await;
        book.merge_gossip(introducer_id, &self_id, &message);
    }

    /// Build a [`BepMessage::Gossip`] payload from the current peer
    /// book, suitable for sending to connected peers.
    ///
    /// Excludes the local device id from the snapshot — peers do not
    /// need us to tell them about ourselves. Each entry's
    /// `snapshot_unix_seconds` carries the per-peer `last_seen` value
    /// stamped by [`PeerBook::mark_seen`] on the most recent confirmed
    /// contact (outbound connect, inbound accept, or any frame
    /// received). A peer that has been introduced via gossip but never
    /// reached directly is broadcast with `snapshot_unix_seconds = 0`.
    ///
    /// Returns an empty vector when no peers are known.
    pub async fn current_gossip_snapshot(&self) -> Vec<GossipPeer> {
        let book = self.peer_book.read().await;
        let self_id = self.device_id();
        book.peers()
            .values()
            .filter(|p| p.device_id != self_id)
            .map(|p| GossipPeer {
                device_id: p.device_id.clone(),
                addresses: p.addresses.iter().map(ToString::to_string).collect(),
                snapshot_unix_seconds: p.last_seen,
            })
            .collect()
    }

    /// Build a [`BepMessage::Gossip`] frame from the current peer book
    /// and send it to every connected peer.
    ///
    /// No-op when the snapshot is empty — sending an empty gossip frame
    /// every minute would just waste bandwidth.
    pub async fn broadcast_gossip(&self) {
        let snapshot = self.current_gossip_snapshot().await;
        if snapshot.is_empty() {
            return;
        }
        let msg = BepMessage::Gossip { peers: snapshot };
        let peers = self.peers.lock().await;
        for handle in peers.values() {
            let _ = handle.outbound.send(msg.clone());
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
    ///
    /// After processing the batch, the highest [`FileInfo::sequence`]
    /// observed is persisted via
    /// [`FolderIndex::set_peer_max_sequence`] for `peer_device_id`, so
    /// the next reconnect can request only the delta beyond what we
    /// have already seen. The stored value is `max(prior, observed)` —
    /// frames arriving out of order never regress the cursor.
    fn merge_files(&self, peer_device_id: &str, files: &[FileInfo]) -> Result<()> {
        for file in files {
            if file.file_type == FILE_TYPE_DIR {
                continue;
            }
            if file.file_type != FILE_TYPE_FILE {
                debug!("ignoring file_type {} for {}", file.file_type, file.name);
                continue;
            }
            if file.invalid || file.no_permissions {
                tracing::debug!(
                    target: "cascade::backend::p2p",
                    path = %file.name,
                    invalid = file.invalid,
                    no_permissions = file.no_permissions,
                    "skipping unhealthy index entry",
                );
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
        // Persist the highest sequence observed for this peer so the
        // next reconnect can ask only for the delta beyond it. Empty
        // batches are no-ops. The stored value is `max(prior, observed)`
        // — out-of-order delivery never regresses the cursor.
        if let Some(max_seq) = files.iter().map(|f| f.sequence).max() {
            let prior = self
                .index
                .get_peer_max_sequence(peer_device_id)
                .unwrap_or(0);
            let updated = prior.max(max_seq);
            if updated != prior {
                self.index.set_peer_max_sequence(peer_device_id, updated)?;
            }
        }
        Ok(())
    }

    /// Persist the local row at `original_path` as a conflict copy at
    /// a sibling path before the row is overwritten by an incoming
    /// concurrent write. The conflict copy is a snapshot of the local
    /// state — same content, same modified time, same version vector —
    /// but at a unique path so it does not collide on any peer.
    ///
    /// The path identifier comes from the friendly name configured for
    /// the LOCAL device when one is set (sanitised for filesystem use);
    /// otherwise it falls back to the first eight characters of the
    /// device id. An empty sanitised name also triggers the fallback
    /// so the resulting path is never `<stem>.conflict--<ts>.<ext>`.
    ///
    /// A row whose content is empty (zero size, no block hashes) is
    /// skipped — there's nothing meaningful to preserve.
    fn persist_conflict_copy(&self, original_path: &str, local: &IndexEntry) -> Result<()> {
        if local.size == 0 && local.block_hashes.is_empty() {
            return Ok(());
        }
        let identifier = self
            .local_device_name
            .as_deref()
            .map(sanitise_for_path)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| local_short_device_id(self.device_id()));
        let timestamp = unix_timestamp_seconds();
        let conflict_path = conflict_copy_path(original_path, &identifier, timestamp);
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
/// The format is `<stem>.conflict-<device_identifier>-<timestamp>.<ext>`
/// where the stem and extension are split on the LAST `.` in the
/// filename. A leading dot is treated as a hidden-file marker rather
/// than an extension separator, so `.gitignore` becomes
/// `.gitignore.conflict-<id>-<ts>` with no trailing extension.
///
/// `device_identifier` is the friendly device name when one is
/// configured, otherwise the first eight characters of the device id.
/// Callers are responsible for sanitising it via `sanitise_for_path`
/// before passing it in.
fn conflict_copy_path(original: &str, device_identifier: &str, timestamp: i64) -> String {
    let (parent, filename) = match original.rsplit_once('/') {
        Some((p, f)) => (Some(p), f),
        None => (None, original),
    };
    let (stem, ext) = split_filename(filename);
    let suffixed = if ext.is_empty() {
        format!("{stem}.conflict-{device_identifier}-{timestamp}")
    } else {
        format!("{stem}.conflict-{device_identifier}-{timestamp}.{ext}")
    };
    match parent {
        Some(p) => format!("{p}/{suffixed}"),
        None => suffixed,
    }
}

/// Sanitise a string for use as a filename component in a conflict-copy
/// path. Replaces any character that is unsafe or noisy in a filename
/// with a single `-` and lowercases the result. Forward slash,
/// backslash, dot, and whitespace are always replaced; a handful of
/// shell-significant characters and any remaining control character
/// are also normalised.
///
/// Replacement is one-for-one — runs of replaced characters become runs
/// of dashes — so `..` becomes `--` and `home/server` becomes
/// `home-server`. Collapsing would alias distinct inputs (`a..b` vs
/// `a-b`) which is undesirable when the identifier is meant to be
/// distinguishing.
///
/// An empty input produces an empty output — the caller is expected to
/// fall back to the short device id when this happens.
fn sanitise_for_path(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        let replaced = match ch {
            // Filesystem path separators and the extension separator.
            '/' | '\\' | '.'
            // Whitespace — keeps filenames terminal-friendly.
            | ' ' | '\t' | '\n' | '\r'
            // Shell metacharacters and control bytes get normalised too;
            // these would otherwise need quoting at every use site.
            | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '-',
            // Any other control character is replaced as well so the
            // result is safe to embed in shell output and filenames.
            other if other.is_control() => '-',
            other => other,
        };
        out.push(replaced);
    }
    out.to_lowercase()
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
        // Sequence space is per-INDEX (one FolderIndex per backend instance)
        // and the per-row `row_version` is monotonic across upserts and
        // tombstones, so it is exactly the per-device sequence number BEP
        // expects. See [`FileInfo::sequence`] for the per-index/per-device
        // equivalence note.
        sequence: u64::try_from(entry.row_version).unwrap_or(0),
        block_size,
        deleted: entry.deleted,
        // The backend has no mid-write or permission-denied state for
        // an `IndexEntry` today, so locally-produced rows always emit
        // these flags as false. The receive path respects the wire
        // fields when peers set them.
        invalid: false,
        no_permissions: false,
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
            .merge_files(
                "peer-test",
                &[FileInfo {
                    name: "doc.txt".into(),
                    file_type: FILE_TYPE_FILE,
                    size: 99,
                    modified: 1_000_000_000,
                    sequence: 0,
                    block_size: 128 * 1024,
                    deleted: false,
                    invalid: false,
                    no_permissions: false,
                    version: Version {
                        counters: vec![(1, 2)],
                    },
                    block_hashes: vec![[1u8; 32]],
                }],
            )
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
            .merge_files(
                "peer-test",
                &[FileInfo {
                    name: "doc.txt".into(),
                    file_type: FILE_TYPE_FILE,
                    size: 99,
                    modified: 2_000_000_000,
                    sequence: 0,
                    block_size: 128 * 1024,
                    deleted: false,
                    invalid: false,
                    no_permissions: false,
                    version: Version {
                        counters: vec![(1, 3)],
                    },
                    block_hashes: vec![[1u8; 32]],
                }],
            )
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
            .merge_files(
                "peer-test",
                &[FileInfo {
                    name: "doc.txt".into(),
                    file_type: FILE_TYPE_FILE,
                    size: 99, // would be different content, but vector equals — skip
                    modified: 2_000_000_000,
                    sequence: 0,
                    block_size: 128 * 1024,
                    deleted: false,
                    invalid: false,
                    no_permissions: false,
                    version: Version {
                        counters: vec![(1, 1), (2, 2)],
                    },
                    block_hashes: vec![[1u8; 32]],
                }],
            )
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

    #[test]
    fn conflict_copy_path_splits_on_last_dot() {
        // Compound extensions like `.tar.gz` split on the LAST dot:
        // stem = `archive.tar`, ext = `gz`. The conflict marker lands
        // between them. Two-component restoration (`gunzip` then
        // `tar -xf`) still recognises the file shape.
        assert_eq!(
            conflict_copy_path("archive.tar.gz", "7BHJ62FL", 1_700_000_000),
            "archive.tar.conflict-7BHJ62FL-1700000000.gz",
        );
    }

    #[test]
    fn conflict_copy_path_uses_friendly_name() {
        // A sanitised friendly name is passed positionally where the
        // short device id used to live — the format is unchanged, only
        // the source of the identifier differs.
        assert_eq!(
            conflict_copy_path("doc.txt", "work-laptop", 1_700_000_000),
            "doc.conflict-work-laptop-1700000000.txt",
        );
    }

    #[test]
    fn sanitise_for_path_handles_special_chars() {
        // The three cases called out by the design: whitespace, path
        // separators, and dots all replace one-for-one and lowercase.
        assert_eq!(sanitise_for_path("Work Laptop"), "work-laptop");
        assert_eq!(sanitise_for_path("home/server"), "home-server");
        assert_eq!(sanitise_for_path(".."), "--");
    }

    #[test]
    fn sanitise_for_path_lowercases_and_normalises_metacharacters() {
        // Mixed-case alphanumerics and apostrophes survive intact
        // except for the lowercasing pass; shell metacharacters,
        // colons, and backslashes normalise to single dashes
        // one-for-one. Replacement is not collapsed, so `C:\` becomes
        // `c--` (colon + backslash).
        assert_eq!(sanitise_for_path("Joe's MacBook"), "joe's-macbook");
        assert_eq!(sanitise_for_path("C:\\users\\joe"), "c--users-joe");
    }

    #[test]
    fn sanitise_for_path_empty_input_returns_empty() {
        // An empty input must produce an empty output so the caller
        // can detect the case and fall back to the short device id —
        // returning a placeholder here would defeat the fallback.
        assert_eq!(sanitise_for_path(""), "");
    }

    #[tokio::test]
    async fn persist_conflict_copy_uses_friendly_name_when_set() {
        let (_dir, engine) = make_engine("f").await;
        let engine = engine.with_local_device_name(Some("Work Laptop".to_string()));
        // Seed a local row so `merge_files` has something to displace.
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
        // Concurrent incoming write — neither vector dominates.
        engine
            .merge_files(
                "peer-test",
                &[FileInfo {
                    name: "doc.txt".into(),
                    file_type: FILE_TYPE_FILE,
                    size: 99,
                    modified: 2_000_000_000,
                    sequence: 0,
                    block_size: 128 * 1024,
                    deleted: false,
                    invalid: false,
                    no_permissions: false,
                    version: Version {
                        counters: vec![(2, 1)],
                    },
                    block_hashes: vec![[1u8; 32]],
                }],
            )
            .unwrap();
        // The displaced local row should be persisted at a sibling
        // path stamped with the sanitised friendly name, not the
        // opaque short device id.
        let conflict_row = engine
            .index
            .list_children("")
            .unwrap()
            .into_iter()
            .find(|e| e.path.starts_with("doc.conflict-work-laptop-"))
            .expect("conflict copy should use the friendly name");
        assert_eq!(
            std::path::Path::new(&conflict_row.path)
                .extension()
                .and_then(std::ffi::OsStr::to_str),
            Some("txt"),
            "conflict copy preserves the original extension",
        );
        assert_eq!(
            conflict_row.size, 10,
            "conflict copy keeps local content size"
        );
    }

    #[tokio::test]
    async fn persist_conflict_copy_falls_back_to_short_id_without_name() {
        let (_dir, engine) = make_engine("f").await;
        // No friendly name configured — `local_device_name` is `None`
        // by default — so the short device id must identify the
        // displaced side.
        assert!(engine.local_device_name().is_none());
        let short_id = local_short_device_id(engine.device_id());
        let conflict_prefix = format!("doc.conflict-{short_id}-");

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
            .merge_files(
                "peer-test",
                &[FileInfo {
                    name: "doc.txt".into(),
                    file_type: FILE_TYPE_FILE,
                    size: 99,
                    modified: 2_000_000_000,
                    sequence: 0,
                    block_size: 128 * 1024,
                    deleted: false,
                    invalid: false,
                    no_permissions: false,
                    version: Version {
                        counters: vec![(2, 1)],
                    },
                    block_hashes: vec![[1u8; 32]],
                }],
            )
            .unwrap();
        let conflict_row = engine
            .index
            .list_children("")
            .unwrap()
            .into_iter()
            .find(|e| e.path.starts_with(&conflict_prefix))
            .expect("conflict copy should use the short device id when no friendly name is set");
        assert_eq!(
            std::path::Path::new(&conflict_row.path)
                .extension()
                .and_then(std::ffi::OsStr::to_str),
            Some("txt"),
        );
    }

    #[tokio::test]
    async fn persist_conflict_copy_falls_back_when_friendly_name_sanitises_to_empty() {
        // A friendly name that consists entirely of replaced
        // characters sanitises to a string of dashes — non-empty —
        // and is still preferred over the short device id. The
        // genuine empty-string case (which would otherwise produce a
        // bare `.conflict--<ts>.` path) is the one we must guard.
        let (_dir, engine) = make_engine("f").await;
        let engine = engine.with_local_device_name(Some(String::new()));
        let short_id = local_short_device_id(engine.device_id());
        let conflict_prefix = format!("doc.conflict-{short_id}-");

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
            .merge_files(
                "peer-test",
                &[FileInfo {
                    name: "doc.txt".into(),
                    file_type: FILE_TYPE_FILE,
                    size: 99,
                    modified: 2_000_000_000,
                    sequence: 0,
                    block_size: 128 * 1024,
                    deleted: false,
                    invalid: false,
                    no_permissions: false,
                    version: Version {
                        counters: vec![(2, 1)],
                    },
                    block_hashes: vec![[1u8; 32]],
                }],
            )
            .unwrap();
        let conflict_row = engine
            .index
            .list_children("")
            .unwrap()
            .into_iter()
            .find(|e| e.path.starts_with(&conflict_prefix))
            .expect("empty friendly name must fall back to the short device id");
        assert_eq!(
            std::path::Path::new(&conflict_row.path)
                .extension()
                .and_then(std::ffi::OsStr::to_str),
            Some("txt"),
        );
    }

    #[tokio::test]
    async fn seed_peer_names_round_trips_via_peer_name_lookup() {
        let (_dir, engine) = make_engine("f").await;
        engine
            .seed_peer_names(vec![
                ("AAAAA".to_string(), "home-laptop".to_string()),
                // An empty value is ignored — the absence is preserved.
                ("BBBBB".to_string(), String::new()),
            ])
            .await;
        assert_eq!(
            engine.peer_name("AAAAA").await.as_deref(),
            Some("home-laptop"),
        );
        assert!(engine.peer_name("BBBBB").await.is_none());
        assert!(engine.peer_name("CCCCC").await.is_none());
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
            .merge_files(
                "peer-test",
                &[FileInfo {
                    name: "doc.txt".into(),
                    file_type: FILE_TYPE_FILE,
                    size: 99,
                    modified: 2_000_000_000,
                    sequence: 0,
                    block_size: 128 * 1024,
                    deleted: false,
                    invalid: false,
                    no_permissions: false,
                    version: Version {
                        counters: vec![(2, 1)],
                    },
                    block_hashes: vec![[1u8; 32]],
                }],
            )
            .unwrap();
        let after = engine.index.get("doc.txt").unwrap().unwrap();
        // The incoming row overwrites the original (matching the
        // ordering chosen by merge_files on the concurrent branch);
        // the version vector must merge both counters so a third peer
        // sees the full history. Separately, the conflict-copy path
        // covered by `merge_files_persists_conflict_copy` ensures the
        // displaced local content is preserved at a sibling path.
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
            .merge_files(
                "peer-test",
                &[FileInfo {
                    name: "doc.txt".into(),
                    file_type: FILE_TYPE_FILE,
                    size: 99,
                    modified: 100,
                    sequence: 0,
                    block_size: 128 * 1024,
                    deleted: false,
                    invalid: false,
                    no_permissions: false,
                    version: Version {
                        counters: vec![(2, 1)],
                    },
                    block_hashes: vec![[1u8; 32]],
                }],
            )
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
            .merge_files(
                "peer-test",
                &[FileInfo {
                    name: "subdir".into(),
                    file_type: FILE_TYPE_DIR,
                    size: 0,
                    modified: 1_000_000_000,
                    sequence: 0,
                    block_size: 128 * 1024,
                    deleted: false,
                    invalid: false,
                    no_permissions: false,
                    version: Version::default(),
                    block_hashes: vec![],
                }],
            )
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
            .merge_files(
                "peer-test",
                &[FileInfo {
                    name: "doc.txt".into(),
                    file_type: FILE_TYPE_FILE,
                    size: 0,
                    modified: 2_000_000_000,
                    sequence: 0,
                    block_size: 128 * 1024,
                    deleted: true,
                    invalid: false,
                    no_permissions: false,
                    version: Version {
                        counters: vec![(1, 2)],
                    },
                    block_hashes: vec![],
                }],
            )
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
            .merge_files(
                "peer-test",
                &[FileInfo {
                    name: "gone.txt".into(),
                    file_type: FILE_TYPE_FILE,
                    size: 0,
                    modified: 1_700_000_000,
                    sequence: 0,
                    block_size: 128 * 1024,
                    block_hashes: vec![],
                    deleted: true,
                    invalid: false,
                    no_permissions: false,
                    version: Version {
                        counters: vec![(7, 1)],
                    },
                }],
            )
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
            .merge_files(
                "peer-test",
                &[FileInfo {
                    name: "weird".into(),
                    file_type: 99,
                    size: 1,
                    modified: 1_000_000_000,
                    sequence: 0,
                    block_size: 128 * 1024,
                    deleted: false,
                    invalid: false,
                    no_permissions: false,
                    version: Version::default(),
                    block_hashes: vec![[0u8; 32]],
                }],
            )
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

    /// `connect_to` should record the dialled peer in our `PeerBook`.
    #[tokio::test]
    async fn peer_book_records_outbound_connections() {
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

        let book = engine_a.peer_book().read().await;
        let recorded = book
            .get(engine_b.device_id())
            .expect("B should be recorded in A's peer book");
        assert_eq!(recorded.addresses, vec![addr_b]);
        assert!(
            recorded.introduced_by.is_empty(),
            "manual contact should record no introducer"
        );
    }

    /// `handle_inbound` should record the accepted peer in our `PeerBook`.
    #[tokio::test]
    async fn peer_book_records_inbound_connections() {
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

        // The inbound handler records the peer asynchronously inside the
        // listener task; poll the peer book until A appears (or fail).
        let mut found = false;
        for _ in 0..40 {
            let book = engine_b.peer_book().read().await;
            if book.get(engine_a.device_id()).is_some() {
                found = true;
                break;
            }
            drop(book);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(found, "A should be recorded in B's peer book");
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

    #[tokio::test]
    async fn merge_files_advances_peer_max_sequence() {
        let (_dir, engine) = make_engine("f").await;
        engine
            .merge_files(
                "peer-x",
                &[
                    FileInfo {
                        name: "a.txt".into(),
                        file_type: FILE_TYPE_FILE,
                        size: 1,
                        modified: 0,
                        sequence: 7,
                        block_size: 128 * 1024,
                        deleted: false,
                        invalid: false,
                        no_permissions: false,
                        version: Version {
                            counters: vec![(1, 1)],
                        },
                        block_hashes: vec![[0u8; 32]],
                    },
                    FileInfo {
                        name: "b.txt".into(),
                        file_type: FILE_TYPE_FILE,
                        size: 1,
                        modified: 0,
                        sequence: 15,
                        block_size: 128 * 1024,
                        deleted: false,
                        invalid: false,
                        no_permissions: false,
                        version: Version {
                            counters: vec![(1, 1)],
                        },
                        block_hashes: vec![[0u8; 32]],
                    },
                ],
            )
            .unwrap();
        assert_eq!(engine.index.get_peer_max_sequence("peer-x").unwrap(), 15);
    }

    #[tokio::test]
    async fn merge_files_does_not_regress_peer_max_sequence() {
        let (_dir, engine) = make_engine("f").await;
        // Seed: peer reports a high watermark first.
        engine.index.set_peer_max_sequence("peer-x", 100).unwrap();
        // A later batch with a lower max sequence must NOT overwrite
        // the prior value — frame reordering should never regress
        // the cursor.
        engine
            .merge_files(
                "peer-x",
                &[FileInfo {
                    name: "late.txt".into(),
                    file_type: FILE_TYPE_FILE,
                    size: 1,
                    modified: 0,
                    sequence: 4,
                    block_size: 128 * 1024,
                    deleted: false,
                    invalid: false,
                    no_permissions: false,
                    version: Version {
                        counters: vec![(1, 1)],
                    },
                    block_hashes: vec![[0u8; 32]],
                }],
            )
            .unwrap();
        assert_eq!(
            engine.index.get_peer_max_sequence("peer-x").unwrap(),
            100,
            "out-of-order frames must not regress the cursor",
        );
    }

    #[tokio::test]
    async fn snapshot_since_filters_by_row_version() {
        // Three rows seeded into the index; entries_since(2) yields
        // only the third. Snapshot_since must mirror that.
        let (_dir, engine) = make_engine("f").await;
        for path in ["one.txt", "two.txt", "three.txt"] {
            engine
                .index
                .upsert(&IndexEntry {
                    path: path.into(),
                    is_dir: false,
                    size: 1,
                    modified: 0,
                    block_hashes: vec![0u8; 32],
                    deleted: false,
                    row_version: 0,
                    version: vec![(1, 1)],
                })
                .unwrap();
        }
        let delta = engine.snapshot_since(2).unwrap();
        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0].name, "three.txt");
        assert_eq!(delta[0].sequence, 3);
    }

    #[tokio::test]
    async fn merge_files_skips_invalid_entries() {
        let (_dir, engine) = make_engine("f").await;
        engine
            .merge_files(
                "peer-x",
                &[FileInfo {
                    name: "midwrite.txt".into(),
                    file_type: FILE_TYPE_FILE,
                    size: 99,
                    modified: 1_000_000_000,
                    sequence: 1,
                    block_size: 128 * 1024,
                    deleted: false,
                    invalid: true,
                    no_permissions: false,
                    version: Version {
                        counters: vec![(1, 1)],
                    },
                    block_hashes: vec![[1u8; 32]],
                }],
            )
            .unwrap();
        assert!(
            engine.index.get("midwrite.txt").unwrap().is_none(),
            "invalid entries must not be upserted",
        );
    }

    #[tokio::test]
    async fn merge_files_skips_no_permissions_entries() {
        let (_dir, engine) = make_engine("f").await;
        engine
            .merge_files(
                "peer-x",
                &[FileInfo {
                    name: "secret.txt".into(),
                    file_type: FILE_TYPE_FILE,
                    size: 99,
                    modified: 1_000_000_000,
                    sequence: 1,
                    block_size: 128 * 1024,
                    deleted: false,
                    invalid: false,
                    no_permissions: true,
                    version: Version {
                        counters: vec![(1, 1)],
                    },
                    block_hashes: vec![[1u8; 32]],
                }],
            )
            .unwrap();
        assert!(
            engine.index.get("secret.txt").unwrap().is_none(),
            "no_permissions entries must not be upserted",
        );
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
