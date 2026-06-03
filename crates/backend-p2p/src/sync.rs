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
use cascade_engine::manage::{DataAccess, DataAuthority, DeviceId, ManageDispatch};
use cascade_p2p::block::BlockHash;
use cascade_p2p::candidate::{Candidate, CandidateKind};
use cascade_p2p::connection::ConnectionManager;
use cascade_p2p::discovery::DiscoveredPeer;
use cascade_p2p::framed::{FramedPeer, FramedSession, SessionReader, SessionWriter};
use cascade_p2p::identity::DeviceIdentity;
use cascade_p2p::nat::{
    enumerate_host_candidates, is_globally_routable_ip, server_reflexive_candidate_from_addr,
};
use cascade_p2p::pipe::ByteMeter;
use cascade_p2p::protocol::{
    BepMessage, FileInfo, Folder, GossipPeer, MAX_RELAY_OFFER_ADDRESSES, ManageCommand,
    ManageErrorKind, ManageResult, ManageScope, Version,
};
use cascade_p2p::store::BlockStore;
use cascade_p2p::transport::{
    RelayTransport, Transport, TransportReader, TransportWriter, UdpFlowTransport,
};
use cascade_p2p::traversal::{
    CandidatePair, ConnectivityStrategy, NatType, PeerRelay, PunchConfig, RelayRoute,
    SyncPunchAgreement, SystemClock, UdpPunchTransport, decide_connectivity, run_hole_punch,
};
use cascade_p2p::wan::PeerBook;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock, mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::index::{FolderIndex, IndexEntry};
use crate::{RelayVolunteer, peer_relay};

/// Erased reader half used by the shared session loop.
///
/// `run_session_loop` is the single implementation behind both the
/// direct-TLS path (via [`FramedPeer`]) and the unified
/// [`FramedSession<T>`] path used after a successful hole punch or
/// relay handshake. Each transport has a different concrete reader
/// type, so the enum collapses them into one runtime-dispatched
/// surface — no monomorphisation explosion in the session loop, and
/// no `dyn Trait` for the TLS hot path either.
enum FramedHalfReader {
    /// Direct TLS reader produced by [`FramedPeer::split`].
    Tls(cascade_p2p::framed::FramedReader),
    /// Generic transport reader produced by [`FramedSession::split`].
    Session(Box<dyn AsyncBepReader>),
}

impl FramedHalfReader {
    async fn recv(&mut self) -> Result<Option<BepMessage>> {
        match self {
            Self::Tls(r) => r.recv().await,
            Self::Session(r) => r.recv_boxed().await,
        }
    }
}

/// Erased writer half used by the shared session loop. See
/// [`FramedHalfReader`] for the design rationale.
enum FramedHalfWriter {
    /// Direct TLS writer produced by [`FramedPeer::split`].
    Tls(cascade_p2p::framed::FramedWriter),
    /// Generic transport writer produced by [`FramedSession::split`].
    Session(Box<dyn AsyncBepWriter>),
}

impl FramedHalfWriter {
    async fn send(&mut self, msg: &BepMessage) -> Result<()> {
        match self {
            Self::Tls(w) => w.send(msg).await,
            Self::Session(w) => w.send_boxed(msg).await,
        }
    }

    async fn shutdown(&mut self) -> Result<()> {
        match self {
            Self::Tls(w) => w.shutdown().await,
            Self::Session(w) => w.shutdown_boxed().await,
        }
    }
}

/// How the peer principal a session attributes its frames to was established.
///
/// The management plane treats a session's `device_id` as the caller principal
/// when authorising [`BepMessage::ManageRequest`] frames. That is only sound when
/// the device id is cryptographically bound to the connection — proven by the
/// mutual-TLS handshake the [`ConnectionManager`] runs on the direct dial and
/// accept paths, where the device id is derived from the peer's certificate.
///
/// On the relayed and post-hole-punch paths the device id is merely a string
/// asserted on the wire (the relay volunteer names the `source_device`, the punch
/// agreement names the peer) and the inner transport carries plaintext BEP frames
/// with no inner handshake. A frame on such a session must NOT be allowed to act
/// as a management caller, or any party who can open a tunnel could spoof a
/// granted manager's device id.
///
/// The session loop carries this tag from each entry point down to the
/// management dispatch so the unverified paths are refused before a command runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallerAuthentication {
    /// The peer device id was proven by a mutual-TLS handshake — it is the
    /// SHA-256 of the peer's certificate, so the management plane may trust it
    /// as the caller principal.
    TlsVerified,
    /// The peer device id was asserted on the wire over a transport that ran no
    /// end-to-end peer TLS handshake (relay tunnel or post-hole-punch UDP). The
    /// management plane must not treat it as an authenticated caller.
    Unverified,
}

impl CallerAuthentication {
    /// Whether a session with this authentication may issue management
    /// commands. Only a TLS-verified principal qualifies.
    const fn permits_management(self) -> bool {
        matches!(self, Self::TlsVerified)
    }

    /// Whether a session with this authentication may write its index/blocks
    /// into this node (the data-plane accept direction). Only a TLS-verified
    /// principal qualifies: on a relayed or post-hole-punch session the device
    /// id is asserted on the wire, not certificate-bound, so a data grant keyed
    /// to that id must not be honoured — any party who can open a tunnel could
    /// otherwise spoof a write-granted peer's device id and push content. An
    /// unverified session is no-share for writes regardless of grants; its
    /// proposed rows are routed to the receive quarantine instead.
    const fn permits_data_write(self) -> bool {
        matches!(self, Self::TlsVerified)
    }
}

/// Object-safe trait the boxed reader half implements.
///
/// `SessionReader<R>` is generic over the underlying transport
/// reader; the session loop wants a single trait object so the enum
/// above stays narrow. The boxed wrapper below implements this trait
/// for every concrete reader the workspace ships.
#[async_trait::async_trait]
trait AsyncBepReader: Send {
    async fn recv_boxed(&mut self) -> Result<Option<BepMessage>>;
}

/// Object-safe trait the boxed writer half implements. See
/// [`AsyncBepReader`] for the rationale.
#[async_trait::async_trait]
trait AsyncBepWriter: Send {
    async fn send_boxed(&mut self, msg: &BepMessage) -> Result<()>;
    async fn shutdown_boxed(&mut self) -> Result<()>;
}

struct SessionReaderBoxed<R: TransportReader> {
    inner: SessionReader<R>,
}

impl<R: TransportReader> SessionReaderBoxed<R> {
    const fn new(inner: SessionReader<R>) -> Self {
        Self { inner }
    }
}

#[async_trait::async_trait]
impl<R: TransportReader + Send> AsyncBepReader for SessionReaderBoxed<R> {
    async fn recv_boxed(&mut self) -> Result<Option<BepMessage>> {
        self.inner.recv().await
    }
}

struct SessionWriterBoxed<W: TransportWriter> {
    inner: SessionWriter<W>,
}

impl<W: TransportWriter> SessionWriterBoxed<W> {
    const fn new(inner: SessionWriter<W>) -> Self {
        Self { inner }
    }
}

#[async_trait::async_trait]
impl<W: TransportWriter + Send> AsyncBepWriter for SessionWriterBoxed<W> {
    async fn send_boxed(&mut self, msg: &BepMessage) -> Result<()> {
        self.inner.send(msg).await
    }

    async fn shutdown_boxed(&mut self) -> Result<()> {
        self.inner.shutdown().await
    }
}

/// File-type code for regular files in BEP `FileInfo.file_type`.
const FILE_TYPE_FILE: u32 = 0;
/// File-type code for directories.
const FILE_TYPE_DIR: u32 = 1;

/// JSON-serialisable snapshot of a peer-proposed [`FileInfo`], stored in the
/// `data_receive_quarantine` table for a write-denied peer.
///
/// The wire [`FileInfo`] uses manual XDR encoding and is not itself serde-
/// serialisable, so this dedicated DTO captures exactly the fields needed to
/// surface a rejected local addition to the operator. It is deliberately a
/// faithful mirror of the proposal rather than a lossy summary, so an operator
/// inspecting the quarantine sees the peer's full claim. The block hashes are
/// hex-encoded for a compact, human-inspectable JSON form.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct QuarantinedFile {
    /// File path relative to the folder root.
    name: String,
    /// Total file size in bytes.
    size: u64,
    /// Last modification time (Unix seconds).
    modified: i64,
    /// Tombstone flag — a rejected delete proposal is preserved too.
    deleted: bool,
    /// Per-file version vector, as `(device_short_id, counter)` pairs.
    version: Vec<(u64, u64)>,
    /// Hex-encoded SHA-256 block hashes, in order.
    block_hashes: Vec<String>,
}

impl From<&FileInfo> for QuarantinedFile {
    fn from(file: &FileInfo) -> Self {
        Self {
            name: file.name.clone(),
            size: file.size,
            modified: file.modified,
            deleted: file.deleted,
            version: file.version.counters.clone(),
            block_hashes: file.block_hashes.iter().map(hex_encode_hash).collect(),
        }
    }
}

/// Hex-encode a 32-byte block hash into a lowercase string.
fn hex_encode_hash(hash: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    hash.iter()
        .fold(String::with_capacity(64), |mut acc, byte| {
            // Writing to a String never fails; the result is discarded deliberately.
            let _ = write!(acc, "{byte:02x}");
            acc
        })
}

/// Wall-clock timeout for a block request to a single peer.
const BLOCK_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Wall-clock timeout for a management request to a single peer. A managed
/// command runs the same handlers the local CLI drives, so it can take longer
/// than a single block fetch; the ceiling is set well above the block timeout
/// so a slow-but-honest command is not cut off, while a wedged peer still
/// fails loudly rather than hanging the manager indefinitely.
const MANAGE_REQUEST_TIMEOUT: Duration = Duration::from_mins(1);

/// [`MAX_RELAY_OFFER_ADDRESSES`] as a `usize` for length comparisons when
/// building an offer. Derived from the protocol constant so the volunteer's
/// self-imposed cap can never drift from the encoder's ceiling.
const MAX_RELAY_OFFER_ADDRESSES_USIZE: usize = MAX_RELAY_OFFER_ADDRESSES as usize;

/// Wall-clock window granted for a hole-punch attempt. Both peers must
/// have signalled `SyncPunch` and emitted their first burst before the
/// nonce expires. Five seconds matches the upper bound documented in
/// `docs/nat-hole-punching.md`.
const SYNC_PUNCH_WINDOW: Duration = Duration::from_secs(5);

/// Source of fresh per-process `SyncPunch` nonces. The atomic ensures
/// concurrent connection attempts get distinct nonces without
/// coordination; the value is opaque to the wire so monotonicity is
/// not required, only uniqueness within a session pair.
static SYNC_PUNCH_NONCE_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Allocate a fresh `SyncPunch` nonce.
fn next_sync_punch_nonce() -> u64 {
    SYNC_PUNCH_NONCE_COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Wall-clock milliseconds since the Unix epoch. Used to stamp
/// `SyncPunchAgreement::deadline_unix_ms`.
fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// `local_preference` assigned to the single `ServerReflexive` candidate
/// derived from the STUN external mapping. Set below the worst-case host
/// `local_preference` so any host candidate outranks the reflexive one
/// when both reach the same peer.
const SERVER_REFLEXIVE_LOCAL_PREFERENCE: u16 = 0;

/// Aggregate, deduplicate, and sort a local candidate set for gossip.
///
/// Inputs are the per-interface host candidates (typically from
/// [`enumerate_host_candidates`]), the STUN-derived external
/// `SocketAddr` (when [`cascade_p2p::nat::detect_nat_type_rfc5780`] has
/// produced one), and any extra candidates the caller has on hand —
/// `PeerReflexive` discovered during an earlier punch, or `Relayed`
/// allocated against a TURN-style relay. Today the engine has no source
/// for either, so the `extras` slice is empty in production, but the
/// helper is designed to fold them in without restructuring.
///
/// The output is deduplicated by `(address, kind)` and sorted by
/// descending `priority` so the receiving peer's
/// [`decide_connectivity`] picks the highest-priority pair first.
/// Duplicate inputs that share both an address and a kind collapse to
/// the first entry seen at that address+kind — priorities for repeats
/// are identical because [`Candidate::new`] is deterministic for a given
/// kind and local preference.
#[must_use]
fn aggregate_candidates(
    host_candidates: Vec<Candidate>,
    external: Option<SocketAddr>,
    extras: Vec<Candidate>,
) -> Vec<Candidate> {
    let mut combined = host_candidates;
    if let Some(external_addr) = external {
        combined.push(server_reflexive_candidate_from_addr(
            external_addr,
            SERVER_REFLEXIVE_LOCAL_PREFERENCE,
        ));
    }
    combined.extend(extras);

    // Sort by priority descending so the highest-ranked candidate sits
    // first. `sort_by_key` with `Reverse` is stable, so ties retain the
    // insertion order host-then-srflx-then-extras and the dedupe pass
    // below preserves that ordering when collapsing duplicates.
    combined.sort_by_key(|c| std::cmp::Reverse(c.priority));

    // Deduplicate by (address, kind). A `HashSet` would lose the sort
    // order; a linear scan keeps the priority-descending output stable.
    let mut seen: std::collections::HashSet<(SocketAddr, CandidateKind)> =
        std::collections::HashSet::new();
    combined.retain(|c| seen.insert((c.address, c.kind)));
    combined
}

/// Build the local candidate set advertised over `BepMessage::Candidates`.
///
/// Folds three sources together:
///
/// 1. Per-interface host candidates from [`enumerate_host_candidates`]
///    — every non-loopback, non-link-local address the OS knows about.
/// 2. The STUN-derived `ServerReflexive` candidate (when one is
///    available), giving peers on the public Internet a reachable
///    address through this host's `NAT`.
/// 3. Any extras the caller supplies — `PeerReflexive` candidates
///    discovered during a prior punch attempt or `Relayed` candidates
///    allocated against a TURN-style relay. Empty today.
///
/// The bound `SocketAddr` is used only to seed the port — its IP is
/// always the wildcard `0.0.0.0` or `::` when the listener binds to
/// `0`, which is not a useful candidate. The per-interface walk in
/// [`enumerate_host_candidates`] supplies the concrete addresses.
fn gather_local_candidates(
    bound_addr: SocketAddr,
    external: Option<SocketAddr>,
    extras: Vec<Candidate>,
) -> Vec<Candidate> {
    let host = enumerate_host_candidates(bound_addr.port());
    aggregate_candidates(host, external, extras)
}

/// Identity information for a connected peer.
#[derive(Debug, Clone)]
pub struct Peer {
    pub device_id: String,
    pub address: SocketAddr,
}

/// Handle to a live peer session — used to send messages and fetch blocks.
#[derive(Debug)]
struct PeerHandle {
    /// How the peer's device id was established for this session. Only a
    /// [`CallerAuthentication::TlsVerified`] session may be reused for the
    /// management plane: a relayed or post-hole-punch session asserts the
    /// device id on the wire without an end-to-end TLS handshake, so the
    /// managed node would refuse a `ManageRequest` arriving on it. The
    /// manager-side reuse check ([`SyncEngine::has_verified_peer`]) consults
    /// this so it never sends a management request down a session the node
    /// will reject, dialling a fresh direct session instead.
    caller_auth: CallerAuthentication,
    outbound: mpsc::UnboundedSender<BepMessage>,
    /// Outstanding block requests, keyed by the `request_id` allocated
    /// when the Request frame was sent. The responder echoes the id in
    /// the matching Response so the entry can be removed and the payload
    /// delivered to the right waiter.
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Vec<u8>>>>>,
    /// Outstanding management requests, keyed by the `request_id` allocated
    /// when the [`BepMessage::ManageRequest`] frame was sent. The managed
    /// node echoes the id in its [`BepMessage::ManageResponse`] so the entry
    /// can be removed and the typed [`ManageResult`] delivered to the
    /// waiting manager-side caller. Kept separate from `pending` because the
    /// two reply frames carry different payload types.
    manage_pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ManageResult>>>>,
    /// Source of fresh `request_id` values for outbound Request and
    /// `ManageRequest` frames. Shared across both frame families so a manage
    /// request can never collide with a concurrent block request id.
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

    /// Send a [`BepMessage::ManageRequest`] and await the managed node's
    /// matching [`BepMessage::ManageResponse`], correlated by the
    /// `request_id` carried in both frames.
    ///
    /// Returns the typed [`ManageResult`] the node reported — an authorisation
    /// denial surfaces as [`ManageResult::Err`] with
    /// [`ManageErrorKind::Unauthorised`], not as a transport error, so the
    /// caller can distinguish "you may not" from a command that ran and
    /// failed. A dropped session or a wedged peer fails loudly rather than
    /// hanging: the waiter is removed and a transport error is returned.
    async fn send_manage(
        &self,
        command: ManageCommand,
        scope: ManageScope,
        token: Option<String>,
    ) -> Result<ManageResult> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.manage_pending.lock().await;
            pending.insert(request_id, tx);
        }
        self.outbound
            .send(BepMessage::ManageRequest {
                request_id,
                command,
                scope,
                token,
            })
            .map_err(|_| anyhow::anyhow!("peer outbound channel closed"))?;

        match tokio::time::timeout(MANAGE_REQUEST_TIMEOUT, rx).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => {
                let mut pending = self.manage_pending.lock().await;
                pending.remove(&request_id);
                anyhow::bail!("peer session dropped before responding to management request");
            }
            Err(_) => {
                let mut pending = self.manage_pending.lock().await;
                pending.remove(&request_id);
                anyhow::bail!("peer management request timed out");
            }
        }
    }
}

/// Peer sync engine.
///
/// One instance per `P2pBackend`. Owns Arc-shared references to the
/// folder index and block store so background tasks can read/write them
/// without holding the backend itself.
///
/// [`Debug`] is implemented by hand rather than derived because the
/// management-plane dispatch port is an `Arc<dyn ManageDispatch>` trait object,
/// which does not itself implement [`Debug`].
#[derive(Clone)]
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
    /// Most recent local `NAT` classification — the value
    /// [`decide_connectivity`] reads for the local side. Default
    /// `NatType::Unknown` until startup detection completes
    /// (see `set_local_nat_type`). The interior mutex lets the
    /// background detection task update the value without taking
    /// ownership of the engine.
    local_nat_type: Arc<RwLock<NatType>>,
    /// Most recent STUN-derived external `SocketAddr` observed for this
    /// host. Populated by the same background detection task that
    /// publishes `local_nat_type`. `None` until detection runs (or when
    /// the host is on a public address, in which case the external
    /// address equals one of the host candidates and is redundant).
    /// Consumed by [`gather_local_candidates`] to emit a
    /// [`CandidateKind::ServerReflexive`] candidate alongside the host
    /// set.
    local_external_addr: Arc<RwLock<Option<SocketAddr>>>,
    /// Server-reflexive candidates learned via the peer-as-`STUN`
    /// mechanism — one per distinct address a peer told us it observed
    /// for our connection through a
    /// [`BepMessage::ObservedAddress`] frame. Populated by
    /// [`Self::set_observed_external_addr`] and folded into both the
    /// local candidate set advertised over `BepMessage::Candidates` and
    /// the self entry gossiped to peers, so `NAT`-derived external
    /// addresses land in the peer book and propagate via introducer gossip
    /// (active from [`crate::DiscoveryReach::Private`] upward).
    observed_external_candidates: Arc<RwLock<Vec<Candidate>>>,
    /// Known relay endpoints, in preference order. Fed verbatim into
    /// [`decide_connectivity`]. Empty means the relay strategy is
    /// unavailable — the traversal logic falls through to a best-effort
    /// hole punch.
    relay_endpoints: Arc<Vec<SocketAddr>>,
    /// Pre-shared HMAC secret authenticating against the relay pool.
    /// `None` means the relay path is provisioned but unusable
    /// (`decide_connectivity` may still pick `Relay` but
    /// `attempt_relay` skips the dial when no secret is set).
    relay_shared_secret: Option<[u8; 32]>,
    /// Hole-punching opt-out. When `false`, a `ConnectivityStrategy::HolePunch`
    /// is downgraded to direct-or-relay before any UDP burst is emitted.
    enable_hole_punch: bool,
    /// Bound address of the BEP listener once `start_listener` has
    /// returned successfully. Used as the local host candidate
    /// advertised in `BepMessage::Candidates`. `None` until the
    /// listener binds — outbound-only deployments thus advertise no
    /// host candidate at all, which is correct: they have no inbound
    /// path a peer could dial.
    local_listen_addr: Arc<RwLock<Option<SocketAddr>>>,
    /// Policy controlling whether this node volunteers as a peer relay.
    /// Combined with the detected local `NAT` type to decide whether to
    /// emit a `BepMessage::RelayOffer` to trusted peers.
    relay_volunteer: RelayVolunteer,
    /// Peer relays advertised to us by volunteers, keyed by the
    /// volunteer's device id. Fed into [`decide_connectivity`] so a
    /// reachable peer relay is preferred over an operated endpoint.
    /// Populated by [`Self::record_relay_offer`] when a
    /// `BepMessage::RelayOffer` arrives.
    peer_relays: Arc<RwLock<HashMap<String, PeerRelay>>>,
    /// Capacity governor for relay sessions this node bridges while
    /// volunteering. Enforces the configured session and bandwidth caps,
    /// rejecting new sessions past the limit rather than silently
    /// dropping them.
    relay_capacity: Arc<peer_relay::RelayCapacity>,
    /// Active relay bridges this node is volunteering. Each bridge is
    /// registered under BOTH bridged device ids (requester and target) so
    /// `RelayData` arriving from either side resolves the opposite side and
    /// return traffic flows. The two entries share one `Arc`, so the
    /// admission slot in the [`peer_relay::RelayBridge`] is released exactly
    /// once when the last reference drops. Populated when a
    /// `BepMessage::RelayConnect` is admitted; both entries are removed when
    /// either bridged session ends.
    relay_bridges: Arc<Mutex<HashMap<String, Arc<peer_relay::RelayBridge>>>>,
    /// Inner-session terminals this node owns as either the requester or the
    /// target of a relayed connection, keyed by the device id of the
    /// carrying session (the volunteer). When a `BepMessage::RelayData`
    /// frame arrives on a session whose device id is a key here, the payload
    /// is an inner BEP frame for the tunnelled session, so it is fed to the
    /// terminal's reader rather than forwarded. The requester registers a
    /// terminal in [`Self::attempt_peer_relay`]; the target registers one on
    /// [`BepMessage::RelayInbound`].
    relay_terminals: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<Vec<u8>>>>>,
    /// Management-plane dispatch port. When the inner `Option` is `Some`, an
    /// arriving [`BepMessage::ManageRequest`] is run through it — the peer's
    /// TLS-authenticated device id is the caller principal, the engine resolves
    /// that caller's grants, authorises, audits, and dispatches into the same
    /// command handlers the local CLI drives. `None` leaves the management plane
    /// disabled: a `ManageRequest` is refused with a typed
    /// [`ManageErrorKind::Unauthorised`] error rather than silently ignored, so
    /// a manager learns the node is not accepting remote administration.
    ///
    /// Held behind a shared `RwLock` rather than a plain field because the
    /// daemon wires this in *after* the engine has been constructed and its
    /// clones handed to the spawned listener and session loops (see
    /// [`Self::set_manage_dispatch`]). Interior mutability over a clone-shared
    /// `Arc` is what lets that late injection reach the loops already running on
    /// the cloned engines — a builder-style move setter could not.
    manage_dispatch: Arc<RwLock<Option<Arc<dyn ManageDispatch>>>>,
    /// Data-plane authority port. When the inner `Option` is `Some`, the BEP
    /// sync path gates serving our index and blocks to a peer on that peer's
    /// `data:read` access, and accepting a peer's index and blocks on its
    /// `data:write` access, for the folder — consulting the on-node data grants,
    /// the token revocation list, and any data-verb token the peer presented on
    /// its [`BepMessage::ClusterConfig`].
    ///
    /// When the port is **unset** (`None`) the path is *default-open*: every
    /// trusted peer keeps full bidirectional access, matching the pre-feature
    /// behaviour. That is also what keeps the bare-`SyncEngine` unit tests
    /// behaving as before — they never inject the port.
    ///
    /// Held behind the same shared `RwLock` as [`Self::manage_dispatch`] and for
    /// the same reason: the daemon wires it after the engine is constructed and
    /// its clones handed to the spawned loops (see [`Self::set_data_authority`]).
    data_authority: Arc<RwLock<Option<Arc<dyn DataAuthority>>>>,
    /// The `data:read`/`data:write` capability token a peer presented on its
    /// [`BepMessage::ClusterConfig`], keyed by the peer's device id. Captured
    /// when the `ClusterConfig` frame arrives and consulted at every data gate for
    /// that session, so a token-carried grant is folded into the access decision
    /// exactly as an on-node grant is. Removed when the session ends.
    presented_data_tokens: Arc<Mutex<HashMap<String, String>>>,
}

impl std::fmt::Debug for SyncEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `try_read` keeps `Debug` non-blocking; a momentary writer (a
        // `set_manage_dispatch` in flight) reports the port as not-yet-known
        // rather than deadlocking the formatter.
        let manage_enabled = self.manage_dispatch.try_read().map(|slot| slot.is_some());
        f.debug_struct("SyncEngine")
            .field("folder_id", &self.folder_id)
            .field("device_short_id", &self.device_short_id)
            .field("manage_enabled", &manage_enabled)
            .finish_non_exhaustive()
    }
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
            local_nat_type: Arc::new(RwLock::new(NatType::Unknown)),
            local_external_addr: Arc::new(RwLock::new(None)),
            observed_external_candidates: Arc::new(RwLock::new(Vec::new())),
            relay_endpoints: Arc::new(Vec::new()),
            relay_shared_secret: None,
            enable_hole_punch: true,
            local_listen_addr: Arc::new(RwLock::new(None)),
            relay_volunteer: RelayVolunteer::default(),
            peer_relays: Arc::new(RwLock::new(HashMap::new())),
            relay_capacity: Arc::new(peer_relay::RelayCapacity::new(
                crate::DEFAULT_MAX_RELAY_SESSIONS,
                crate::DEFAULT_MAX_RELAY_BANDWIDTH_BYTES_PER_SEC,
            )),
            relay_bridges: Arc::new(Mutex::new(HashMap::new())),
            relay_terminals: Arc::new(Mutex::new(HashMap::new())),
            manage_dispatch: Arc::new(RwLock::new(None)),
            data_authority: Arc::new(RwLock::new(None)),
            presented_data_tokens: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Builder-style setter for the management-plane dispatch port. When set, an
    /// arriving [`BepMessage::ManageRequest`] is authorised, audited, and
    /// dispatched through it; left unset, the management plane is disabled and
    /// requests are refused with a typed unauthorised error.
    ///
    /// Convenience for callers that hold the dispatch port at construction time
    /// (the tests). The daemon, which only obtains the port after building the
    /// engine, uses [`Self::set_manage_dispatch`] instead.
    #[must_use]
    pub fn with_manage_dispatch(self, dispatch: Arc<dyn ManageDispatch>) -> Self {
        // The builder runs before any clone is handed to a spawned loop, so a
        // blocking write here cannot contend with a live reader.
        if let Ok(mut slot) = self.manage_dispatch.try_write() {
            *slot = Some(dispatch);
        }
        self
    }

    /// Install (or replace) the management-plane dispatch port on a live engine.
    ///
    /// Unlike [`Self::with_manage_dispatch`], this takes `&self` and writes
    /// through the shared `RwLock`, so every clone of this engine — including the
    /// ones already handed to the spawned listener and per-peer session loops —
    /// observes the port. The daemon calls this once, after constructing the
    /// engine that implements [`ManageDispatch`], before remote managers connect.
    pub async fn set_manage_dispatch(&self, dispatch: Arc<dyn ManageDispatch>) {
        let mut slot = self.manage_dispatch.write().await;
        *slot = Some(dispatch);
    }

    /// Install (or replace) the data-plane authority port on a live engine.
    ///
    /// Mirrors [`Self::set_manage_dispatch`]: writing through the shared
    /// `RwLock` lets every clone of this engine — including the ones already
    /// driving spawned session loops — observe the port. The daemon calls this
    /// once, after constructing the engine that implements [`DataAuthority`],
    /// before peers exchange sync frames. Until then the path is default-open.
    pub async fn set_data_authority(&self, authority: Arc<dyn DataAuthority>) {
        let mut slot = self.data_authority.write().await;
        *slot = Some(authority);
    }

    /// Resolve the directional data-plane access a `peer` has for this engine's
    /// folder, as of now, for a session authenticated as `caller_auth`.
    ///
    /// When the [`DataAuthority`] port is **unset**, the access is *default-open*
    /// — `read = true, write = true` — so a bare engine and every pre-feature
    /// deployment keep full bidirectional sharing with trusted peers, on every
    /// transport including relayed and post-hole-punch sessions. This is the
    /// non-breaking default: directional enforcement only engages once the
    /// daemon wires the port.
    ///
    /// When the port is **set**, directional enforcement is active and the
    /// peer's device id is the principal the grants key on. An
    /// [`Unverified`](CallerAuthentication::Unverified) session (relayed or
    /// post-hole-punch) asserts that device id on the wire rather than proving it
    /// by mutual TLS, so a data grant keyed to it must not be honoured — the
    /// session is **no-share in both directions**. Any party who can open a
    /// tunnel could otherwise spoof a granted peer's device id. A
    /// [`TlsVerified`](CallerAuthentication::TlsVerified) session folds the
    /// on-node data grants, the revocation list, and any data-verb token the peer
    /// presented on its `ClusterConfig` into the decision via the port.
    ///
    /// A port error is **not** silently treated as full access: it fails closed
    /// to no-share in both directions so a faulty store cannot leak data to a
    /// peer that should be restricted, and the error is logged.
    async fn data_access_for(&self, peer: &str, caller_auth: CallerAuthentication) -> DataAccess {
        let authority = { self.data_authority.read().await.clone() };
        let Some(authority) = authority else {
            // Port unset: default-open on every transport — the pre-feature
            // behaviour, preserved unconditionally.
            return DataAccess {
                read: true,
                write: true,
            };
        };
        // Directional enforcement is engaged. An unverified session has no
        // trustworthy principal to authorise a directional grant against, so it
        // is no-share both ways — never consult the grants for a spoofable id.
        if !caller_auth.permits_data_write() {
            return DataAccess {
                read: false,
                write: false,
            };
        }
        let token = {
            let tokens = self.presented_data_tokens.lock().await;
            tokens.get(peer).cloned()
        };
        let device = DeviceId::new(peer.to_string());
        match authority
            .data_access(
                &device,
                &self.folder_id,
                token.as_deref(),
                chrono::Utc::now(),
            )
            .await
        {
            Ok(access) => access,
            Err(e) => {
                warn!(
                    target: "cascade::backend::p2p",
                    peer = %peer,
                    folder = %self.folder_id,
                    error = %e,
                    "data-plane authority lookup failed — failing closed to no-share",
                );
                DataAccess {
                    read: false,
                    write: false,
                }
            }
        }
    }

    /// Builder-style setter for the known relay endpoint pool. Threaded
    /// through `P2pBackend::open` from `P2pBackendConfig::relay_endpoints`.
    #[must_use]
    pub fn with_relay_endpoints(mut self, relays: Vec<SocketAddr>) -> Self {
        self.relay_endpoints = Arc::new(relays);
        self
    }

    /// Builder-style setter for the relay HMAC shared secret. `None`
    /// disables outbound relay connection attempts even when the relay
    /// pool is non-empty.
    #[must_use]
    pub const fn with_relay_shared_secret(mut self, secret: Option<[u8; 32]>) -> Self {
        self.relay_shared_secret = secret;
        self
    }

    /// Builder-style toggle for the `HolePunch` strategy. `false`
    /// downgrades `ConnectivityStrategy::HolePunch` to direct-or-relay
    /// before any UDP burst is emitted.
    #[must_use]
    pub const fn with_hole_punch_enabled(mut self, enabled: bool) -> Self {
        self.enable_hole_punch = enabled;
        self
    }

    /// Builder-style setter for the relay-volunteer policy. Threaded
    /// through `P2pBackend::open` from `P2pBackendConfig::relay_volunteer`.
    #[must_use]
    pub const fn with_relay_volunteer(mut self, policy: RelayVolunteer) -> Self {
        self.relay_volunteer = policy;
        self
    }

    /// Builder-style setter for the relay session and bandwidth caps.
    /// Threaded through `P2pBackend::open` from
    /// `P2pBackendConfig::max_relay_sessions` and `max_relay_bandwidth`.
    #[must_use]
    pub fn with_relay_session_caps(mut self, max_sessions: u32, max_bandwidth: u64) -> Self {
        self.relay_capacity = Arc::new(peer_relay::RelayCapacity::new(max_sessions, max_bandwidth));
        self
    }

    /// Record a relay offer advertised by a volunteering peer. The offer
    /// arrived on a connection from `device_id`; `addresses` are the
    /// reachable BEP endpoints the volunteer accepts relay connections on.
    /// Stored so [`decide_connectivity`] can prefer this peer relay over
    /// an operated endpoint. An offer with no addresses is recorded
    /// verbatim — `decide_connectivity` skips unreachable peer relays at
    /// selection time rather than dropping them here.
    pub async fn record_relay_offer(&self, device_id: String, addresses: Vec<SocketAddr>) {
        let mut relays = self.peer_relays.write().await;
        relays.insert(
            device_id.clone(),
            PeerRelay {
                device_id,
                addresses,
            },
        );
    }

    /// Snapshot of the peer relays advertised to us, in arbitrary order.
    /// Fed to [`decide_connectivity`] as the preferred relay pool.
    pub async fn peer_relays(&self) -> Vec<PeerRelay> {
        self.peer_relays.read().await.values().cloned().collect()
    }

    /// The configured relay-volunteer policy.
    #[must_use]
    pub const fn relay_volunteer(&self) -> RelayVolunteer {
        self.relay_volunteer
    }

    /// `true` when this node should advertise itself as a relay to
    /// trusted peers given its policy and current `NAT` classification.
    ///
    /// Only `Open` and `FullCone` nodes can usefully relay — a node
    /// behind a restrictive `NAT` cannot accept the inbound relay
    /// connections a bridge needs — so the offer is gated on both the
    /// operator's policy and the detected `NAT` type.
    pub async fn should_volunteer_as_relay(&self) -> bool {
        if matches!(self.relay_volunteer, RelayVolunteer::Off) {
            return false;
        }
        matches!(
            self.local_nat_type().await,
            NatType::Open | NatType::FullCone
        )
    }

    /// Reachable BEP endpoints to advertise in a [`BepMessage::RelayOffer`].
    ///
    /// A third peer dials one of these to open a relay session through us, so
    /// every address must be one a peer on the public Internet can reach. The
    /// set is the globally-routable subset of our local candidate set: the
    /// host candidates the listener exposes plus the STUN-derived and
    /// peer-as-`STUN` reflexive addresses, filtered to globally-routable IPs
    /// (private, loopback and link-local addresses are never reachable from
    /// off-LAN and would only mislead the receiver). Deduplicated, capped at
    /// [`cascade_p2p::protocol::MAX_RELAY_OFFER_ADDRESSES`] to match the
    /// encoder's ceiling. Empty when the listener is unbound or no routable
    /// address is known, in which case the caller suppresses the offer.
    async fn relay_offer_addresses(&self) -> Vec<SocketAddr> {
        let Some(local_addr) = *self.local_listen_addr.read().await else {
            return Vec::new();
        };
        let external = *self.local_external_addr.read().await;
        let extras = self.observed_external_candidates().await;
        let candidates = gather_local_candidates(local_addr, external, extras);

        let mut addresses: Vec<SocketAddr> = Vec::new();
        for candidate in candidates {
            if !is_globally_routable_ip(candidate.address.ip()) {
                continue;
            }
            if addresses.contains(&candidate.address) {
                continue;
            }
            addresses.push(candidate.address);
            if addresses.len() >= MAX_RELAY_OFFER_ADDRESSES_USIZE {
                break;
            }
        }
        addresses
    }

    /// Capacity governor for relay sessions this node bridges. Exposed so
    /// the inbound relay path can admit or reject sessions against the
    /// configured caps.
    #[must_use]
    pub const fn relay_capacity(&self) -> &Arc<peer_relay::RelayCapacity> {
        &self.relay_capacity
    }

    /// Update the local `NAT` classification observed by the most
    /// recent detection round. Used by the background task spawned in
    /// `P2pBackend::open`. Async because the underlying store is
    /// guarded by an async `RwLock`.
    pub async fn set_local_nat_type(&self, nat_type: NatType) {
        *self.local_nat_type.write().await = nat_type;
    }

    /// Most recent local `NAT` classification observed via STUN. Falls
    /// back to `NatType::Unknown` until the background detection task
    /// publishes a real reading.
    pub async fn local_nat_type(&self) -> NatType {
        *self.local_nat_type.read().await
    }

    /// Update the STUN-derived external `SocketAddr` observed by the
    /// most recent detection round. Used by the background task spawned
    /// in `P2pBackend::open` after [`detect_nat_type_rfc5780`] returns a
    /// non-`None` `external_socket_addr`. The recorded value seeds the
    /// `ServerReflexive` candidate emitted by the local candidate
    /// gathering path.
    ///
    /// [`detect_nat_type_rfc5780`]: cascade_p2p::nat::detect_nat_type_rfc5780
    pub async fn set_local_external_addr(&self, external: Option<SocketAddr>) {
        *self.local_external_addr.write().await = external;
    }

    /// Most recent STUN-derived external `SocketAddr` for this host.
    /// Returns `None` until the background detection task publishes a
    /// reading. The local candidate gathering path reads this value to
    /// decide whether to emit a `ServerReflexive` candidate alongside
    /// the host set.
    pub async fn local_external_addr(&self) -> Option<SocketAddr> {
        *self.local_external_addr.read().await
    }

    /// Record an externally observed source address learned from a peer
    /// via the peer-as-`STUN` mechanism (a
    /// [`BepMessage::ObservedAddress`] frame). The address is converted
    /// into a [`CandidateKind::ServerReflexive`] candidate using
    /// [`server_reflexive_candidate_from_addr`] — exactly the same shape
    /// a `STUN` `XOR-MAPPED-ADDRESS` would produce — and stored on the
    /// engine. Duplicate observations of the same address collapse to a
    /// single candidate so repeated frames from several peers do not
    /// inflate the advertised set.
    ///
    /// Non-globally-routable observations are dropped before storage
    /// (see [`is_globally_routable_ip`]). A peer on the same LAN observes
    /// this host's private, loopback, or link-local source; storing that
    /// as a reflexive candidate would advertise an unreachable address to
    /// off-LAN peers as a public mapping — exactly what real `STUN`
    /// never does. Rejecting at ingress keeps the advertised candidate
    /// set and the gossip self entry clean by construction rather than
    /// filtering at advertise time.
    ///
    /// The stored candidates feed both the local candidate set
    /// advertised over `BepMessage::Candidates` and the self entry
    /// gossiped to peers.
    pub async fn set_observed_external_addr(&self, observed: SocketAddr) {
        if !is_globally_routable_ip(observed.ip()) {
            debug!(
                target: "cascade::backend::p2p",
                %observed,
                "dropping non-globally-routable peer-as-STUN observation",
            );
            return;
        }
        let candidate =
            server_reflexive_candidate_from_addr(observed, SERVER_REFLEXIVE_LOCAL_PREFERENCE);
        let mut stored = self.observed_external_candidates.write().await;
        if !stored.iter().any(|c| c.address == candidate.address) {
            stored.push(candidate);
        }
    }

    /// Snapshot of the server-reflexive candidates learned via
    /// peer-as-`STUN`. Empty until at least one peer has echoed our
    /// observed source address back to us.
    pub async fn observed_external_candidates(&self) -> Vec<Candidate> {
        self.observed_external_candidates.read().await.clone()
    }

    /// The full local candidate set this device is currently reachable on.
    ///
    /// Combines the per-interface host candidates derived from the bound
    /// listener port, the STUN-derived external mapping, and the
    /// peer-as-`STUN` reflexive candidates — exactly the set the connect
    /// path advertises in a [`BepMessage::Candidates`] frame and the relay
    /// path filters for its offer. Exposed so the announce-server discovery
    /// loop can publish the same set to a rendezvous directory.
    ///
    /// Returns an empty vector when the listener is unbound, since a device
    /// with no listen address has no candidate to advertise.
    pub async fn local_candidates(&self) -> Vec<Candidate> {
        let Some(local_addr) = *self.local_listen_addr.read().await else {
            return Vec::new();
        };
        let external = *self.local_external_addr.read().await;
        let extras = self.observed_external_candidates().await;
        gather_local_candidates(local_addr, external, extras)
    }

    /// Known relay endpoints, in preference order. Returned as a slice
    /// over the shared `Arc` so callers can hand it to
    /// [`decide_connectivity`] without cloning. Empty when relay is
    /// not configured.
    #[must_use]
    pub fn relay_endpoints(&self) -> &[SocketAddr] {
        &self.relay_endpoints
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

    /// The address the BEP listener is bound to, once it has bound.
    ///
    /// `None` until [`Self::start_listener`] has bound a socket — outbound-only
    /// deployments never bind and so always return `None`. Useful to a caller
    /// that opened the backend with `listen_addr` set to port 0 and needs the
    /// OS-assigned port (the integration tests dial a node opened that way).
    pub async fn local_listen_addr(&self) -> Option<SocketAddr> {
        *self.local_listen_addr.read().await
    }

    /// `true` if a session to `device_id` is currently active.
    pub async fn has_peer(&self, device_id: &str) -> bool {
        let peers = self.peers.lock().await;
        peers.contains_key(device_id)
    }

    /// `true` if a session to `device_id` is active *and* its peer identity was
    /// proven by a mutual-TLS handshake (so the managed node will honour a
    /// management request on it).
    ///
    /// A relayed or post-hole-punch session is unverified — the device id is
    /// asserted on the wire, not certificate-bound — and the managed node
    /// refuses a `ManageRequest` arriving on it. The manager-side path uses this
    /// rather than [`Self::has_peer`] when deciding whether an existing session
    /// can carry a management command, so it never reuses a session the node
    /// would reject and instead dials a fresh direct session.
    pub async fn has_verified_peer(&self, device_id: &str) -> bool {
        let peers = self.peers.lock().await;
        peers
            .get(device_id)
            .is_some_and(|handle| handle.caller_auth.permits_management())
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
        // Stash the bound address so subsequent sessions can advertise
        // it as the local host candidate via `BepMessage::Candidates`.
        // Overwrites any prior value — only one listener runs per
        // engine in production.
        *self.local_listen_addr.write().await = Some(bound);
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
        let manager = ConnectionManager::new(self.identity.clone(), trusted);
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
        // Outbound dial: pass no observed address. The connector's
        // socket `peer_addr()` is just the address we dialled, never a
        // reflexive observation, so we must not echo it back as one. The
        // peer-as-STUN frame flows only from the accepting side below.
        tokio::spawn(async move {
            if let Err(e) = engine
                .run_framed_session(device_id.clone(), None, framed)
                .await
            {
                debug!("outbound session to {device_id} ended: {e:#}");
            }
        });
        Ok(())
    }

    /// Establish a peer connection using the [`ConnectivityStrategy`]
    /// chosen by [`decide_connectivity`] from the local and remote
    /// `NAT` classifications, the remote candidate list, and the
    /// configured relay pool.
    ///
    /// `Direct` uses the same TCP+TLS path as [`Self::connect_to`].
    /// `HolePunch` emits a `SyncPunch` frame, waits for the matching
    /// agreement from the peer (or builds a fresh one if our own peer
    /// book already carries one), then drives [`run_hole_punch`] over a
    /// freshly bound UDP socket. `Relay` opens a relay connection via
    /// [`cascade_p2p::relay::RelayClient`]. For v1 the post-punch and
    /// post-relay BEP transport upgrade is a stub log message — the
    /// goal here is to prove the wiring, not to drive a full session
    /// over the new transport.
    ///
    /// The engine's hole-punch flag (fed from the
    /// [`crate::DiscoveryReach`] exposure posture — off at `LanOnly`)
    /// downgrades a chosen `HolePunch` strategy to direct-or-relay before
    /// any UDP burst is emitted, so a LAN-confined deployment takes that
    /// path out of the equation without removing `decide_connectivity`
    /// from the loop.
    pub async fn connect_to_with_strategy(&self, peer: Peer) -> Result<()> {
        let trusted = self.trusted.lock().await.clone();
        if !trusted.contains(&peer.device_id) {
            anyhow::bail!("device {} is not trusted", peer.device_id);
        }

        let local_nat = self.local_nat_type().await;
        let (remote_nat, remote_candidates) = {
            let book = self.peer_book.read().await;
            let remote = book
                .last_known_nat_type(&peer.device_id)
                .unwrap_or(NatType::Unknown);
            let candidates = book
                .remote_candidates(&peer.device_id)
                .map(<[Candidate]>::to_vec)
                .unwrap_or_default();
            (remote, candidates)
        };

        // Feed the full relay pool to `decide_connectivity`. The
        // relay client now performs the HMAC handshake against the
        // server (see `cascade_p2p::relay::RelayClient::connect_with_secret`
        // and `crates/relay-server/src/auth.rs`) so the Relay arm is
        // viable for any peer whose direct + hole-punch paths fail.
        // `attempt_relay` still no-ops when no shared secret is
        // configured — the strategy can be picked but the dial fails
        // loudly rather than silently.
        let peer_relays = self.peer_relays().await;
        let mut strategy = decide_connectivity(
            local_nat,
            remote_nat,
            &remote_candidates,
            &peer_relays,
            &self.relay_endpoints,
        );

        // Honour the opt-out: a chosen HolePunch is downgraded to the
        // most reasonable fallback so the rest of the wiring runs as
        // normal. Prefer relay when one is configured and we hold a
        // shared secret — that matches the precedence the table uses
        // for symmetric pairs. Otherwise fall through to Direct so we
        // at least try the peer's reported address rather than giving
        // up.
        if !self.enable_hole_punch && matches!(strategy, ConnectivityStrategy::HolePunch { .. }) {
            // Prefer a reachable peer relay, then an operated endpoint
            // (only viable with a shared secret), then fall through to a
            // direct dial of the peer's reported address. This mirrors
            // the precedence `decide_connectivity` itself uses for the
            // relay arm.
            let peer_relay_route = peer_relays.iter().find_map(|relay| {
                relay.addresses.first().map(|addr| RelayRoute::Peer {
                    device_id: relay.device_id.clone(),
                    address: *addr,
                })
            });
            strategy = if let Some(route) = peer_relay_route {
                debug!(
                    target: "cascade::backend::p2p",
                    peer = %peer.device_id,
                    "hole-punch disabled — downgraded strategy to peer relay"
                );
                ConnectivityStrategy::Relay { route }
            } else if let (Some(&first_relay), true) = (
                self.relay_endpoints.first(),
                self.relay_shared_secret.is_some(),
            ) {
                debug!(
                    target: "cascade::backend::p2p",
                    peer = %peer.device_id,
                    relay = %first_relay,
                    "hole-punch disabled — downgraded strategy to operated relay"
                );
                ConnectivityStrategy::Relay {
                    route: RelayRoute::Operated {
                        endpoint: first_relay,
                    },
                }
            } else {
                debug!(
                    target: "cascade::backend::p2p",
                    peer = %peer.device_id,
                    "hole-punch disabled and no usable relay — downgraded strategy to direct"
                );
                ConnectivityStrategy::Direct { addr: peer.address }
            };
        }

        match strategy {
            ConnectivityStrategy::Direct { addr } => {
                debug!(
                    target: "cascade::backend::p2p",
                    peer = %peer.device_id,
                    %addr,
                    "connectivity strategy: direct"
                );
                self.connect_to(Peer {
                    device_id: peer.device_id,
                    address: addr,
                })
                .await
            }
            ConnectivityStrategy::HolePunch {
                remote_candidates: chosen_remote,
            } => self.attempt_hole_punch(&peer, &chosen_remote).await,
            ConnectivityStrategy::Relay { route } => match route {
                RelayRoute::Operated { endpoint } => self.attempt_relay(&peer, endpoint).await,
                RelayRoute::Peer {
                    device_id: relay_device,
                    address,
                } => self.attempt_peer_relay(&peer, &relay_device, address).await,
            },
        }
    }

    /// Negotiate a `SyncPunch` agreement with `peer` and drive
    /// [`run_hole_punch`] over a freshly bound UDP socket.
    ///
    /// For v1 the post-punch BEP transport upgrade is a stub log
    /// message — the goal is to prove the wiring (nonce exchange, UDP
    /// socket bind, state-machine invocation) without yet plumbing the
    /// resulting flow into the full BEP session. A successful
    /// `EstablishedFlow` is recorded at info; failures log the
    /// underlying `PunchError` and return `Ok(())` so the caller can
    /// move on to the next peer rather than tearing down its loop.
    async fn attempt_hole_punch(&self, peer: &Peer, remote_candidates: &[Candidate]) -> Result<()> {
        // Pick a remote candidate the punch state machine should
        // target. Highest priority wins — the same selection
        // `decide_connectivity` does for `Direct`. Empty means
        // `decide_connectivity` fell through with no remote candidates
        // and there's nothing punchable, which we surface as a debug
        // log rather than an error.
        let Some(remote_target) = remote_candidates.iter().max_by_key(|c| c.priority) else {
            debug!(
                target: "cascade::backend::p2p",
                peer = %peer.device_id,
                "hole-punch strategy chosen but no remote candidates known — skipping"
            );
            return Ok(());
        };

        // Build or read back the SyncPunch agreement. If we have
        // already received one from the peer (they signalled first),
        // honour their nonce so both sides probe with the same value.
        // Otherwise issue a fresh agreement and broadcast it so the
        // peer can match it on their side.
        let agreement = self
            .ensure_sync_punch_agreement(&peer.device_id)
            .await
            .with_context(|| format!("negotiating sync-punch with {}", peer.device_id))?;

        // Bind a UDP socket for the punch attempt. Port `0` lets the
        // OS pick an ephemeral port — the local-host candidate
        // emitted via gather_local_candidates lives on the BEP TCP
        // listener, but the UDP punch needs its own socket because
        // host candidates and punch transports do not share their
        // wire format.
        let transport = UdpPunchTransport::bind("0.0.0.0:0".parse()?)
            .await
            .context("binding UDP socket for hole punch")?;
        let local = transport
            .local_addr()
            .context("reading hole-punch socket local address")?;
        let pair = CandidatePair {
            local,
            remote: remote_target.address,
        };

        info!(
            target: "cascade::backend::p2p",
            peer = %peer.device_id,
            %local,
            remote = %remote_target.address,
            nonce = agreement.nonce,
            "driving hole-punch attempt"
        );
        match run_hole_punch(
            &transport,
            &pair,
            &agreement,
            &PunchConfig::default(),
            &SystemClock,
        )
        .await
        {
            Ok(flow) => {
                info!(
                    target: "cascade::backend::p2p",
                    peer = %peer.device_id,
                    local = %flow.local,
                    remote = %flow.remote,
                    established_at_unix_ms = flow.established_at_unix_ms,
                    "hole-punch succeeded — upgrading to BEP transport"
                );
                // Capture the bound socket from the punch transport
                // so the BEP-over-UDP adapter can reuse it without
                // reopening the binding. The peer is the confirmed
                // remote endpoint reported by the state machine.
                let udp_transport = UdpFlowTransport::new(transport.socket(), flow.remote);
                self.record_peer(&peer.device_id, flow.remote).await;
                let engine = self.clone();
                let device_id = peer.device_id.clone();
                tokio::spawn(async move {
                    if let Err(e) = engine
                        .run_transport_session(device_id.clone(), udp_transport)
                        .await
                    {
                        debug!(
                            target: "cascade::backend::p2p",
                            peer = %device_id,
                            error = %e,
                            "post-punch BEP session ended",
                        );
                    }
                });
                Ok(())
            }
            Err(e) => {
                debug!(
                    target: "cascade::backend::p2p",
                    peer = %peer.device_id,
                    error = %e,
                    "hole-punch attempt failed"
                );
                Ok(())
            }
        }
    }

    /// Ensure a `SyncPunch` agreement exists for `peer_device_id` and
    /// broadcast it via `BepMessage::SyncPunch`.
    ///
    /// When the peer book already carries a fresh agreement (the peer
    /// signalled first), reuse it. Otherwise allocate a new nonce,
    /// stamp a `SYNC_PUNCH_WINDOW`-second deadline, persist it, and
    /// send the frame so the peer can match it. Returns the agreement
    /// the caller should feed to `run_hole_punch`.
    async fn ensure_sync_punch_agreement(
        &self,
        peer_device_id: &str,
    ) -> Result<SyncPunchAgreement> {
        // Read-only check first to avoid taking the write lock when
        // the agreement is already in place. The deadline guard
        // mirrors `run_hole_punch`'s own check: an expired agreement
        // cannot succeed, so we treat it as absent and replace it.
        let now_ms = unix_now_ms();
        if let Some(existing) = self
            .peer_book
            .read()
            .await
            .current_punch_agreement(peer_device_id)
            && existing.deadline_unix_ms > now_ms
        {
            return Ok(existing);
        }

        // Allocate a fresh agreement and persist it. The deadline is
        // SYNC_PUNCH_WINDOW from now — long enough for the round trip
        // to the peer plus the punch state machine's bursts.
        let agreement = SyncPunchAgreement {
            nonce: next_sync_punch_nonce(),
            deadline_unix_ms: now_ms
                .saturating_add(u64::try_from(SYNC_PUNCH_WINDOW.as_millis()).unwrap_or(u64::MAX)),
        };
        {
            let mut book = self.peer_book.write().await;
            book.start_punch_with(peer_device_id, agreement);
        }

        // Broadcast on the existing peer session if one is up; the
        // peer matches the nonce on their side. If no session is
        // open the agreement still sits in our peer book so the next
        // connection setup carries it.
        let peers = self.peers.lock().await;
        if let Some(handle) = peers.get(peer_device_id) {
            let _ = handle.outbound.send(BepMessage::SyncPunch {
                nonce: agreement.nonce,
                deadline_unix_ms: agreement.deadline_unix_ms,
            });
        }

        Ok(agreement)
    }

    /// Open an HMAC-authenticated relay connection to `peer` via `relay`.
    ///
    /// The relay client now drives the full handshake against the
    /// `cascade-relay-server` (see
    /// [`cascade_p2p::relay::RelayClient::connect_with_secret`] and
    /// `crates/relay-server/src/auth.rs`). For v1 we connect through
    /// the relay (proving the address is reachable and the shared
    /// secret matches) and immediately log success — the post-relay
    /// BEP transport upgrade lives with the post-punch upgrade in the
    /// next round.
    ///
    /// The session id used for the rendezvous is the remote peer's
    /// device id: that matches the legacy `RelayClient::connect` API
    /// shape. A future round will agree the session id out of band so
    /// both peers meet at the same URL path.
    async fn attempt_relay(&self, peer: &Peer, relay: SocketAddr) -> Result<()> {
        let Some(shared_secret) = self.relay_shared_secret else {
            debug!(
                target: "cascade::backend::p2p",
                peer = %peer.device_id,
                %relay,
                "relay strategy chosen but no shared secret configured — skipping"
            );
            return Ok(());
        };
        let relay_url = format!("ws://{relay}");
        info!(
            target: "cascade::backend::p2p",
            peer = %peer.device_id,
            %relay,
            "opening relay connection — upgrading to BEP transport on success"
        );
        match cascade_p2p::relay::RelayClient::connect_with_secret(
            &relay_url,
            &peer.device_id,
            &self.identity.device_id,
            &shared_secret,
        )
        .await
        {
            Ok(conn) => {
                self.record_peer(&peer.device_id, relay).await;
                let relay_transport = RelayTransport::new(conn);
                let engine = self.clone();
                let device_id = peer.device_id.clone();
                tokio::spawn(async move {
                    if let Err(e) = engine
                        .run_transport_session(device_id.clone(), relay_transport)
                        .await
                    {
                        debug!(
                            target: "cascade::backend::p2p",
                            peer = %device_id,
                            error = %e,
                            "post-relay BEP session ended",
                        );
                    }
                });
                Ok(())
            }
            Err(e) => {
                debug!(
                    target: "cascade::backend::p2p",
                    peer = %peer.device_id,
                    %relay,
                    error = ?e,
                    "relay connection attempt failed"
                );
                Ok(())
            }
        }
    }

    /// Reach `peer` by tunnelling through a volunteering peer relay.
    ///
    /// Dials the volunteer's BEP listener at `relay_address`, completes
    /// the TLS handshake (the volunteer must be a trusted device), then
    /// sends a [`BepMessage::RelayConnect`] naming the target peer. The
    /// connection to the volunteer becomes a *carry* channel: it is not run
    /// as an ordinary BEP peer session. Instead the requester runs an inner
    /// BEP session toward the target over a [`peer_relay::PeerRelayTransport`]
    /// — each inner frame is wrapped in a [`BepMessage::RelayData`] envelope
    /// the volunteer forwards verbatim, and inbound `RelayData` is unwrapped
    /// back into inner frames. The volunteer admits the bridge against its
    /// own session cap and bridges the two carry channels, seeing only
    /// opaque payloads.
    ///
    /// This mirrors [`Self::attempt_relay`] for the operated path: there the
    /// requester runs the inner session over
    /// [`cascade_p2p::transport::RelayTransport`] and the relay server is a
    /// blind transport; here the volunteer plays that blind-transport role.
    /// The dial and session wiring run in a spawned task and a dial failure
    /// is logged rather than propagated, so the connection loop can move on
    /// to the next peer.
    async fn attempt_peer_relay(
        &self,
        peer: &Peer,
        relay_device: &str,
        relay_address: SocketAddr,
    ) -> Result<()> {
        let trusted = self.trusted.lock().await.clone();
        if !trusted.contains(&relay_device.to_owned()) {
            debug!(
                target: "cascade::backend::p2p",
                peer = %peer.device_id,
                relay = %relay_device,
                "peer relay chosen but the volunteer is not trusted — skipping"
            );
            return Ok(());
        }

        info!(
            target: "cascade::backend::p2p",
            peer = %peer.device_id,
            relay = %relay_device,
            %relay_address,
            "opening peer-relay connection — requesting bridge to target"
        );

        let manager = ConnectionManager::new(self.identity.clone(), trusted);
        let conn = match manager
            .connect(&DiscoveredPeer {
                device_id: relay_device.to_owned(),
                address: relay_address,
            })
            .await
        {
            Ok(conn) => conn,
            Err(e) => {
                debug!(
                    target: "cascade::backend::p2p",
                    peer = %peer.device_id,
                    relay = %relay_device,
                    %relay_address,
                    error = %e,
                    "peer-relay dial failed"
                );
                return Ok(());
            }
        };
        self.record_peer(relay_device, relay_address).await;

        let framed = FramedPeer::from_connection(conn)?;
        let (reader, mut writer) = framed.split();
        // Ask the volunteer to bridge us to the target before driving the
        // carry channel. The volunteer admits or rejects against its cap; a
        // rejection arrives as a `Close` the carry loop surfaces as EOF.
        writer
            .send(&BepMessage::RelayConnect {
                target_device: peer.device_id.clone(),
            })
            .await
            .with_context(|| {
                format!(
                    "sending RelayConnect for {} via relay {relay_device}",
                    peer.device_id
                )
            })?;

        let engine = self.clone();
        let relay_device_owned = relay_device.to_owned();
        let target_device = peer.device_id.clone();
        tokio::spawn(async move {
            if let Err(e) = engine
                .run_relay_carry_loop(
                    relay_device_owned.clone(),
                    target_device.clone(),
                    FramedHalfReader::Tls(reader),
                    FramedHalfWriter::Tls(writer),
                )
                .await
            {
                debug!(
                    target: "cascade::backend::p2p",
                    relay = %relay_device_owned,
                    target = %target_device,
                    error = %e,
                    "peer-relay carry channel ended",
                );
            }
        });
        Ok(())
    }

    /// Drive one end of a peer-relay tunnel.
    ///
    /// `carry_device` is the volunteer whose connection carries the tunnel;
    /// `inner_device` is the far endpoint the inner BEP session talks to (the
    /// target for a requester, the requester for a target). The carry
    /// connection is treated as a blind transport: its only job is to ferry
    /// [`BepMessage::RelayData`] frames. This loop:
    ///
    /// - registers an inner-session terminal keyed by `carry_device` so the
    ///   shared session machinery (and this loop's own reads) route inbound
    ///   `RelayData` payloads into the inner session;
    /// - spawns the inner BEP session over a
    ///   [`peer_relay::PeerRelayTransport`], keyed by `inner_device`, so the
    ///   full handshake, index exchange and block transfer run end to end
    ///   through the tunnel;
    /// - pumps the carry connection: outbound `RelayData` envelopes the inner
    ///   transport produces go out on the carry writer, and inbound frames
    ///   are unwrapped and fed to the terminal.
    ///
    /// The terminal and inner peer handle are removed when the carry channel
    /// closes.
    async fn run_relay_carry_loop(
        &self,
        carry_device: String,
        inner_device: String,
        mut carry_reader: FramedHalfReader,
        mut carry_writer: FramedHalfWriter,
    ) -> Result<()> {
        // Channel the inner transport writer pushes RelayData envelopes onto;
        // the carry writer task drains it to the volunteer connection.
        let (carry_tx, mut carry_rx) = mpsc::unbounded_channel::<BepMessage>();
        // Channel carrying inbound inner frames to the inner session reader.
        let (inner_tx, inner_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        // Register the terminal so any RelayData arriving for this carry
        // session is decapsulated into the inner session rather than being
        // treated as something to forward.
        {
            let mut terminals = self.relay_terminals.lock().await;
            terminals.insert(carry_device.clone(), inner_tx.clone());
        }

        // Carry writer task: drain RelayData envelopes onto the volunteer
        // connection.
        let carry_device_for_writer = carry_device.clone();
        let writer_task = tokio::spawn(async move {
            while let Some(msg) = carry_rx.recv().await {
                if let Err(e) = carry_writer.send(&msg).await {
                    debug!(
                        target: "cascade::backend::p2p",
                        relay = %carry_device_for_writer,
                        error = %e,
                        "peer-relay carry writer failed",
                    );
                    return;
                }
            }
            let _ = carry_writer.shutdown().await;
        });

        // Inner BEP session over the tunnel, keyed by the far endpoint.
        let transport = peer_relay::PeerRelayTransport::new(carry_tx, inner_rx);
        let engine = self.clone();
        let inner_device_for_session = inner_device.clone();
        let inner_task = tokio::spawn(async move {
            if let Err(e) = engine
                .run_transport_session(inner_device_for_session.clone(), transport)
                .await
            {
                debug!(
                    target: "cascade::backend::p2p",
                    peer = %inner_device_for_session,
                    error = %e,
                    "peer-relayed inner BEP session ended",
                );
            }
        });

        // Carry read loop: unwrap RelayData into inner frames, surface a
        // Close as EOF, ignore any other frame the volunteer should not be
        // sending on a carry channel.
        let result = loop {
            match carry_reader.recv().await {
                Ok(Some(BepMessage::RelayData { payload })) => {
                    if inner_tx.send(payload).is_err() {
                        // Inner session ended — stop pumping.
                        break Ok(());
                    }
                }
                Ok(Some(BepMessage::Close { reason })) => {
                    debug!(
                        target: "cascade::backend::p2p",
                        relay = %carry_device,
                        reason,
                        "peer relay closed the carry channel",
                    );
                    break Ok(());
                }
                Ok(Some(_other)) => {
                    debug!(
                        target: "cascade::backend::p2p",
                        relay = %carry_device,
                        "ignoring non-RelayData frame on peer-relay carry channel",
                    );
                }
                Ok(None) => break Ok(()),
                Err(e) => break Err(e),
            }
        };

        // Tear down: dropping inner_tx ends the inner session reader (EOF),
        // and removing the terminal stops future routing to it.
        {
            let mut terminals = self.relay_terminals.lock().await;
            terminals.remove(&carry_device);
        }
        drop(inner_tx);
        let _ = inner_task.await;
        let _ = writer_task.await;
        result
    }

    /// Forward one relayed frame from the session identified by
    /// `from_device` to the peer it is bridged to, metering the payload
    /// against the relay bandwidth budget.
    ///
    /// The bridge was registered symmetrically (under both bridged device
    /// ids) when the [`BepMessage::RelayConnect`] was admitted, so a frame
    /// from either bridged side resolves the opposite side and return
    /// traffic flows. The payload is re-wrapped into a
    /// [`BepMessage::RelayData`] frame and sent verbatim; the relay never
    /// inspects it. A frame arriving on a session with no bridge entry is
    /// dropped with a debug log rather than forwarded blindly.
    async fn forward_relay_data(&self, from_device: &str, payload: Vec<u8>) {
        let target_device = {
            let bridges = self.relay_bridges.lock().await;
            let Some(bridge) = bridges.get(from_device) else {
                debug!(
                    target: "cascade::backend::p2p",
                    from = %from_device,
                    "dropping RelayData on a session with no admitted bridge",
                );
                return;
            };
            let Some(target) = bridge.forward_target(from_device) else {
                debug!(
                    target: "cascade::backend::p2p",
                    from = %from_device,
                    "dropping RelayData — sender is not part of its bridge",
                );
                return;
            };
            target.to_owned()
        };
        // Account the forwarded payload against the bandwidth meter so the
        // configured ceiling reflects real relayed traffic.
        self.relay_capacity.record(payload.len() as u64);
        let peers = self.peers.lock().await;
        let Some(target) = peers.get(&target_device) else {
            debug!(
                target: "cascade::backend::p2p",
                from = %from_device,
                target = %target_device,
                "dropping RelayData — bridged peer is not connected",
            );
            return;
        };
        target.outbound.send(BepMessage::RelayData { payload }).ok();
    }

    /// Inbound handler — completes the TLS handshake then runs a session.
    async fn handle_inbound(
        &self,
        stream: tokio::net::TcpStream,
        peer_addr: SocketAddr,
    ) -> Result<()> {
        let trusted = self.trusted.lock().await.clone();
        let manager = ConnectionManager::new(self.identity.clone(), trusted);
        let (device_id, observed_peer_addr, tls) = manager
            .accept(stream)
            .await
            .with_context(|| format!("accepting inbound from {peer_addr}"))?;
        info!("inbound P2P connection accepted from device {device_id}");
        self.record_peer(&device_id, peer_addr).await;
        let framed = FramedPeer::from_tls(tls);
        // Inbound accept: the observed source address is the connecting
        // peer's genuine NAT-mapped source, so echo it back as a
        // peer-as-STUN observation.
        self.run_framed_session(device_id, Some(observed_peer_addr), framed)
            .await
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

    /// Drive a peer session over the direct TLS [`FramedPeer`].
    ///
    /// Kept as the primary entry point for TLS-direct sessions because
    /// `FramedPeer` predates the unified [`Transport`] abstraction and
    /// every existing caller passes one. Internally this routes
    /// through [`Self::run_session_loop`] so the post-punch UDP and
    /// post-relay WebSocket paths share the same handshake +
    /// read/write loop.
    ///
    /// `observed_peer_addr` is `Some` only on the accepting side, where
    /// the source address read off the live socket is the connecting
    /// peer's genuine `NAT`-mapped source — the only direction that
    /// yields a usable peer-as-`STUN` observation. The outbound dial
    /// passes `None` because the connector observes nothing but the
    /// address it already dialled.
    async fn run_framed_session(
        &self,
        device_id: String,
        observed_peer_addr: Option<SocketAddr>,
        framed: FramedPeer,
    ) -> Result<()> {
        let (reader, writer) = framed.split();
        // Every `run_framed_session` caller is a direct TLS path
        // (`handle_inbound` accept, `connect_to` dial) where the device id was
        // derived from the peer's certificate by the `ConnectionManager`
        // handshake. The principal is therefore cryptographically bound and may
        // act as a management caller.
        self.run_session_loop(
            device_id,
            observed_peer_addr,
            CallerAuthentication::TlsVerified,
            FramedHalfReader::Tls(reader),
            FramedHalfWriter::Tls(writer),
        )
        .await
    }

    /// Drive a peer session over an arbitrary [`Transport`].
    ///
    /// Used after a successful hole-punch (UDP) or relay handshake
    /// (WebSocket). Funnels the transport's read/write halves into
    /// the same handshake + dispatch loop as [`Self::run_framed_session`].
    fn run_transport_session<T>(
        &self,
        device_id: String,
        transport: T,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
    where
        T: Transport + 'static,
    {
        let (reader, writer) = FramedSession::new(transport).split();
        let engine = self.clone();
        // A post-punch UDP or relay transport carries no observed TCP
        // source address worth echoing back — the punched/relayed path
        // does not expose the peer's `NAT`-mapped origin in the same
        // way the direct TCP socket does. Skip the `ObservedAddress`
        // frame for these transports.
        //
        // This returns a boxed `Send` trait object rather than an opaque
        // `async fn` future on purpose. The peer-relay path re-enters the
        // session loop (`run_session_loop` → `handle_message` →
        // `spawn_relay_terminal` spawns a fresh `run_transport_session`),
        // which makes the concrete future type recursive and unprovably
        // `Send`. Erasing it to a named boxed-`dyn` type at this single edge —
        // the only place a session re-enters itself — cuts the cycle.
        Box::pin(async move {
            engine
                .run_session_loop(
                    device_id,
                    None,
                    // Post-punch UDP and relay-tunnel transports run no
                    // end-to-end peer TLS handshake: the device id is asserted
                    // on the wire (by the relay volunteer or the punch
                    // agreement), not proven by a certificate. The principal is
                    // therefore unverified and must not be trusted as a
                    // management caller — `handle_manage_request` refuses it.
                    CallerAuthentication::Unverified,
                    FramedHalfReader::Session(Box::new(SessionReaderBoxed::new(reader))),
                    FramedHalfWriter::Session(Box::new(SessionWriterBoxed::new(writer))),
                )
                .await
        })
    }

    /// Inner loop shared by every session entry point.
    ///
    /// Owns the handshake, the outbound writer task, the read loop and
    /// the cleanup. The reader/writer halves are erased behind
    /// [`FramedHalfReader`] / [`FramedHalfWriter`] enums so a single
    /// implementation can serve both the TLS and the
    /// `FramedSession<T>` paths without monomorphising the whole
    /// function body for every transport variant.
    async fn run_session_loop(
        &self,
        device_id: String,
        observed_peer_addr: Option<SocketAddr>,
        caller_auth: CallerAuthentication,
        mut reader: FramedHalfReader,
        mut writer: FramedHalfWriter,
    ) -> Result<()> {
        // Outbound channel — the writer task drains this.
        let (tx, mut rx) = mpsc::unbounded_channel::<BepMessage>();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Vec<u8>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let manage_pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ManageResult>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let next_request_id = Arc::new(AtomicU64::new(0));

        // Register handle.
        {
            let mut peers = self.peers.lock().await;
            peers.insert(
                device_id.clone(),
                PeerHandle {
                    caller_auth,
                    outbound: tx.clone(),
                    pending: pending.clone(),
                    manage_pending: manage_pending.clone(),
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
            // The serving side presents no token of its own — a data token
            // travels with the *bearer* presenting it (the `cascade remote
            // --token` path), and is consulted on the side serving the bearer.
            // A bare sync session asserts no token.
            data_token: None,
        })
        .ok();
        // Read gate: only serve our index to a peer that may read this folder.
        // A `data:read`-denied peer (write-only / no-share) is sent an empty
        // Index — the honest no-share surface — never the snapshot. We never
        // skip the frame silently: an empty Index keeps the handshake shape and
        // tells a write-only peer it has nothing to pull. Default-open when the
        // authority port is unset, so a trusted peer with no data grant still
        // receives the full snapshot.
        let snapshot = if self.data_access_for(&device_id, caller_auth).await.read {
            // Delta sync: only send rows whose row_version exceeds the
            // highest sequence we have previously sent to this peer (which
            // we approximate by the highest sequence the peer has reported
            // back to us — they are equal once the previous session
            // completed cleanly, and a conservative lower bound otherwise).
            // First connect to a peer sees `0` and falls through to a full
            // enumeration.
            let last_seen = self.index.get_peer_max_sequence(&device_id).unwrap_or(0);
            self.snapshot_since(last_seen)?
        } else {
            debug!(
                target: "cascade::backend::p2p",
                peer = %device_id,
                folder = %self.folder_id,
                "data:read denied — serving empty index to peer",
            );
            Vec::new()
        };
        tx.send(BepMessage::Index {
            folder: self.folder_id.clone(),
            files: snapshot,
        })
        .ok();

        // Peer-as-STUN: tell the peer the source address we observed for
        // this connection so it can learn its own reflexive (NAT-mapped)
        // address with no STUN server. Only sent over the direct TCP
        // path where the observed source is meaningful — post-punch and
        // relay transports pass `None`.
        if let Some(observed) = observed_peer_addr {
            tx.send(BepMessage::ObservedAddress(observed)).ok();
        }

        // Advertise our reachable candidates so the peer can pair them
        // against its own set in `decide_connectivity`. Only sent when
        // the BEP listener is bound — outbound-only deployments have no
        // host candidate worth advertising, and the receiver tolerates
        // a connection that never produces one (it falls through to
        // direct or relay). The lock guards are dropped before the
        // `tx.send` to avoid holding them across an await on the
        // unbounded channel (no actual await today, but the explicit
        // drop also satisfies clippy::significant_drop_in_scrutinee).
        // The external addr lookup is best-effort: if NAT detection has
        // not run yet (or has not yet observed an external mapping),
        // the gathered set falls back to host candidates only.
        let local_listen = *self.local_listen_addr.read().await;
        if let Some(local_addr) = local_listen {
            let external = *self.local_external_addr.read().await;
            // Fold the peer-as-STUN observed reflexive candidates in as
            // extras so they ride alongside the host and STUN-derived
            // candidates the peer pairs against.
            let extras = self.observed_external_candidates().await;
            let candidates = gather_local_candidates(local_addr, external, extras);
            if !candidates.is_empty() {
                tx.send(BepMessage::Candidates { candidates }).ok();
            }
        }

        // Volunteer as a relay if policy and NAT allow. A peer that records
        // this offer prefers us over an operated relay when its direct and
        // hole-punch paths fail. Sent after Candidates so the peer has our
        // reachable set first; gated on `should_volunteer_as_relay` so only
        // Open/FullCone nodes with a permissive policy advertise.
        if self.should_volunteer_as_relay().await {
            let addresses = self.relay_offer_addresses().await;
            if addresses.is_empty() {
                debug!(
                    target: "cascade::backend::p2p",
                    peer = %device_id,
                    "willing to relay but no routable BEP endpoint to advertise — skipping offer",
                );
            } else {
                tx.send(BepMessage::RelayOffer { addresses }).ok();
            }
        }

        // Read loop.
        let result = loop {
            let msg = match reader.recv().await {
                Ok(Some(m)) => m,
                Ok(None) => break Ok(()),
                Err(e) => break Err(e),
            };
            if let Err(e) = self
                .handle_message(&device_id, caller_auth, msg, &tx, &pending, &manage_pending)
                .await
            {
                break Err(e);
            }
        };

        // Cleanup.
        {
            let mut peers = self.peers.lock().await;
            peers.remove(&device_id);
        }
        // Drop any data-verb token the peer presented for this session so a
        // later session cannot inherit stale authority — the next session
        // re-presents its token on its own ClusterConfig.
        {
            let mut tokens = self.presented_data_tokens.lock().await;
            tokens.remove(&device_id);
        }
        // If this session was either half of a relay bridge we were
        // volunteering, tear the whole bridge down: remove both the entry
        // keyed by this device and the entry keyed by the bridge partner.
        // Dropping the last `Arc` to the shared `RelayBridge` releases the
        // admission slot, so the relay-session count tracks live bridges.
        {
            let mut bridges = self.relay_bridges.lock().await;
            if let Some(bridge) = bridges.remove(&device_id) {
                let partner = if bridge.requester == device_id {
                    bridge.target.clone()
                } else {
                    bridge.requester.clone()
                };
                bridges.remove(&partner);
            }
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
        caller_auth: CallerAuthentication,
        msg: BepMessage,
        outbound: &mpsc::UnboundedSender<BepMessage>,
        pending: &Arc<Mutex<HashMap<u64, oneshot::Sender<Vec<u8>>>>>,
        manage_pending: &Arc<Mutex<HashMap<u64, oneshot::Sender<ManageResult>>>>,
    ) -> Result<()> {
        match msg {
            BepMessage::Ping => Ok(()),
            BepMessage::ClusterConfig { data_token, .. } => {
                // Capture any data-verb capability token the peer presented for
                // this session. It is folded into every subsequent data-access
                // decision for this peer (read and write gates) exactly as an
                // on-node grant is. A `None` clears any prior token so a peer
                // cannot retain stale authority by reconnecting without one.
                let mut tokens = self.presented_data_tokens.lock().await;
                match data_token {
                    Some(token) => {
                        tokens.insert(peer_device_id.to_string(), token);
                    }
                    None => {
                        tokens.remove(peer_device_id);
                    }
                }
                Ok(())
            }
            BepMessage::Index { folder, files } | BepMessage::IndexUpdate { folder, files } => {
                if folder != self.folder_id {
                    debug!("ignoring frame for unknown folder {folder}");
                    return Ok(());
                }
                // Write gate: only merge a peer's index into our authoritative
                // index if it may write this folder. `data_access_for` already
                // denies an unverified session (relayed / post-punch — device id
                // asserted, not TLS-bound) both directions whenever directional
                // enforcement is engaged, so a spoofed device id can never push
                // content. A peer that may not write has its proposed rows
                // recorded as flagged local additions in the receive quarantine
                // and the frame is consumed without error, so the session stays
                // up. Default-open (port unset) keeps a trusted peer's writes
                // flowing as before, on every transport.
                if self
                    .data_access_for(peer_device_id, caller_auth)
                    .await
                    .write
                {
                    self.merge_files(peer_device_id, &files)?;
                } else {
                    self.quarantine_received(peer_device_id, &files).await;
                }
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
                // Read gate: a `data:read`-denied peer (write-only / no-share)
                // must learn nothing of our content. Reply with the same empty
                // Response a genuine block miss yields, so it cannot distinguish
                // "you may not read" from "no such block" — and we never reach
                // the block store on its behalf. Default-open (port unset) serves
                // the block as before.
                if !self.data_access_for(peer_device_id, caller_auth).await.read {
                    debug!(
                        target: "cascade::backend::p2p",
                        peer = %peer_device_id,
                        folder = %self.folder_id,
                        "data:read denied — refusing block request with empty response",
                    );
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
            BepMessage::Candidates { candidates } => {
                // Cache the remote candidate set on the peer book so
                // `decide_connectivity` can pair them against the
                // local set in any subsequent traversal attempt. Peers
                // send the complete set in every frame; no delta
                // protocol is needed.
                let mut book = self.peer_book.write().await;
                let count = candidates.len();
                book.set_remote_candidates(peer_device_id, candidates);
                debug!(
                    target: "cascade::backend::p2p",
                    peer = %peer_device_id,
                    count,
                    "stored remote candidates",
                );
                Ok(())
            }
            BepMessage::SyncPunch {
                nonce,
                deadline_unix_ms,
            } => {
                // Record the peer's agreement so a subsequent
                // `run_hole_punch` call reads back the negotiated
                // nonce. Mutual agreement is implicit: each side
                // records the other's frame; whichever side initiates
                // the punch reads the matched nonce out of its own
                // peer book.
                let agreement = SyncPunchAgreement {
                    nonce,
                    deadline_unix_ms,
                };
                let mut book = self.peer_book.write().await;
                book.start_punch_with(peer_device_id, agreement);
                debug!(
                    target: "cascade::backend::p2p",
                    peer = %peer_device_id,
                    nonce,
                    deadline_unix_ms,
                    "stored sync-punch agreement",
                );
                Ok(())
            }
            BepMessage::ObservedAddress(observed) => {
                // Peer-as-STUN: the peer is telling us the source address
                // it observed for our connection — our own reflexive
                // (NAT-mapped) address. Fold it into the server-reflexive
                // candidate set so it propagates through our advertised
                // candidates and gossip.
                self.set_observed_external_addr(observed).await;
                debug!(
                    target: "cascade::backend::p2p",
                    peer = %peer_device_id,
                    %observed,
                    "learned own reflexive address via peer-as-STUN",
                );
                Ok(())
            }
            BepMessage::RelayOffer { addresses } => {
                // A trusted peer is volunteering as a relay. Record it so
                // `decide_connectivity` can prefer this peer relay over an
                // operated endpoint on the next connection attempt.
                debug!(
                    target: "cascade::backend::p2p",
                    relay = %peer_device_id,
                    address_count = addresses.len(),
                    "recorded peer relay offer",
                );
                self.record_relay_offer(peer_device_id.to_owned(), addresses)
                    .await;
                Ok(())
            }
            BepMessage::RelayConnect { target_device } => {
                self.admit_relay_bridge(peer_device_id, target_device, outbound)
                    .await;
                Ok(())
            }
            BepMessage::RelayInbound { source_device } => {
                // A volunteer relay is telling us a requester wants to open a
                // tunnelled session to us through it. `peer_device_id` is the
                // volunteer carrying the tunnel; `source_device` is the
                // requester the inner session terminates at. Stand up a carry
                // loop terminal so subsequent `RelayData` on this session is
                // decapsulated into an inner BEP session toward the requester.
                self.spawn_relay_terminal(peer_device_id, source_device)
                    .await;
                Ok(())
            }
            BepMessage::RelayData { payload } => {
                // Two roles a RelayData frame can play on this session:
                //
                // 1. If this session carries a tunnel we terminate (a
                //    terminal is registered for it), the payload is an inner
                //    BEP frame — hand it to the terminal's reader.
                // 2. Otherwise we are the volunteer in the middle — forward
                //    the opaque payload to the bridged peer, metering it.
                let routed = {
                    let terminals = self.relay_terminals.lock().await;
                    terminals
                        .get(peer_device_id)
                        .map(|tx| tx.send(payload.clone()).is_ok())
                };
                match routed {
                    Some(true) => {}
                    Some(false) => {
                        debug!(
                            target: "cascade::backend::p2p",
                            carry = %peer_device_id,
                            "dropping RelayData — inner relay terminal has closed",
                        );
                    }
                    None => self.forward_relay_data(peer_device_id, payload).await,
                }
                Ok(())
            }
            BepMessage::ManageRequest {
                request_id,
                command,
                scope,
                token,
            } => {
                self.handle_manage_request(
                    peer_device_id,
                    caller_auth,
                    request_id,
                    command,
                    scope,
                    token,
                    outbound,
                )
                .await;
                Ok(())
            }
            BepMessage::ManageResponse { request_id, result } => {
                // The manager side: route the reply to the waiter registered by
                // `PeerHandle::send_manage` for this `request_id`. A frame whose
                // id has no waiter is either a duplicate, a reply that arrived
                // after its caller timed out, or a node that sent a response we
                // never asked for — log and drop rather than tear the session
                // down for an out-of-place reply.
                let waiter = {
                    let mut pending = manage_pending.lock().await;
                    pending.remove(&request_id)
                };
                match waiter {
                    Some(tx) => {
                        // The receiver may already be gone if the caller timed
                        // out between the remove above and this send; that is a
                        // benign race, so the send error is ignored.
                        tx.send(result).ok();
                    }
                    None => {
                        debug!(
                            target: "cascade::backend::p2p",
                            peer = %peer_device_id,
                            request_id,
                            "dropping ManageResponse with no waiting manage request",
                        );
                    }
                }
                Ok(())
            }
        }
    }

    /// Run an incoming [`BepMessage::ManageRequest`] and reply with a
    /// [`BepMessage::ManageResponse`].
    ///
    /// `peer_device_id` is the caller principal, but it may only be trusted as
    /// such when `caller_auth` is [`CallerAuthentication::TlsVerified`] — i.e.
    /// the device id was proven by the mutual-TLS handshake on a direct dial or
    /// accept. On relayed and post-hole-punch sessions the device id is merely
    /// asserted on the wire, so this method refuses the request with
    /// [`ManageErrorKind::Unauthorised`] before any grant is consulted; otherwise
    /// a party who could open a tunnel could spoof a granted manager's device id.
    ///
    /// A verified request is dispatched through the injected [`ManageDispatch`]
    /// port, which resolves the caller's grants, authorises, audits BEFORE
    /// applying any side effect, and runs the same command handlers the local
    /// CLI drives. When no dispatch port is configured the node is not accepting
    /// remote administration, so the request is refused with a typed
    /// [`ManageErrorKind::Unauthorised`] error rather than dropped.
    async fn handle_manage_request(
        &self,
        peer_device_id: &str,
        caller_auth: CallerAuthentication,
        request_id: u64,
        command: ManageCommand,
        scope: ManageScope,
        token: Option<String>,
        outbound: &mpsc::UnboundedSender<BepMessage>,
    ) {
        let result = if caller_auth.permits_management() {
            // Clone the Arc out under the read guard, then drop the guard before
            // the await so a long-running dispatch never holds the lock against a
            // concurrent `set_manage_dispatch`.
            let dispatch = self.manage_dispatch.read().await.clone();
            match dispatch {
                Some(dispatch) => {
                    let caller = DeviceId::new(peer_device_id.to_owned());
                    dispatch
                        .dispatch(&caller, command, scope, token, chrono::Utc::now())
                        .await
                }
                None => ManageResult::Err {
                    kind: ManageErrorKind::Unauthorised,
                    message: "node is not accepting remote management".to_owned(),
                },
            }
        } else {
            // The session's device id was asserted on the wire, not proven by a
            // TLS handshake. Refuse before consulting any grant so a spoofed
            // principal can never reach the dispatcher.
            debug!(
                target: "cascade::backend::p2p",
                peer = %peer_device_id,
                request_id,
                "refusing ManageRequest on a transport whose peer identity was not TLS-verified",
            );
            ManageResult::Err {
                kind: ManageErrorKind::Unauthorised,
                message: "management commands require a TLS-verified peer connection".to_owned(),
            }
        };
        outbound
            .send(BepMessage::ManageResponse { request_id, result })
            .ok();
    }

    /// Admit (or reject) a peer-relay bridge request from `requester` to
    /// `target`.
    ///
    /// Admission is gated on the configured session cap; past it the request
    /// is rejected with a `Close` rather than silently dropped, so the
    /// requester falls back to another relay or path. On admission the bridge
    /// is registered symmetrically under both device ids (so return traffic
    /// flows) and the target is told to stand up its inner-session terminal
    /// via a [`BepMessage::RelayInbound`] frame.
    async fn admit_relay_bridge(
        &self,
        requester: &str,
        target: String,
        outbound: &mpsc::UnboundedSender<BepMessage>,
    ) {
        // The target must be connected for the bridge to carry anything —
        // the volunteer relays between two live sessions it holds.
        let target_outbound = {
            let peers = self.peers.lock().await;
            peers.get(&target).map(|h| h.outbound.clone())
        };
        let Some(target_outbound) = target_outbound else {
            debug!(
                target: "cascade::backend::p2p",
                requester = %requester,
                target = %target,
                "rejecting peer-relay bridge — target is not connected to this relay",
            );
            outbound
                .send(BepMessage::Close {
                    reason: format!("relay target {target} not connected"),
                })
                .ok();
            return;
        };

        match self.relay_capacity.admit() {
            Ok(guard) => {
                info!(
                    target: "cascade::backend::p2p",
                    requester = %requester,
                    target = %target,
                    active = self.relay_capacity.active_sessions(),
                    "admitted peer-relay bridge request"
                );
                // Register the bridge under BOTH device ids, sharing one Arc
                // so the admission slot releases exactly once when the last
                // reference drops. Either bridged session ending removes both
                // entries (see `run_session_loop` cleanup).
                let bridge = Arc::new(peer_relay::RelayBridge {
                    requester: requester.to_owned(),
                    target: target.clone(),
                    guard,
                });
                {
                    let mut bridges = self.relay_bridges.lock().await;
                    bridges.insert(requester.to_owned(), Arc::clone(&bridge));
                    bridges.insert(target.clone(), bridge);
                }
                // Tell the target to terminate the inner session for the
                // requester. Without this the target would treat the inner
                // frames as something to forward and drop them.
                target_outbound
                    .send(BepMessage::RelayInbound {
                        source_device: requester.to_owned(),
                    })
                    .ok();
            }
            Err(err) => {
                debug!(
                    target: "cascade::backend::p2p",
                    requester = %requester,
                    target = %target,
                    error = %err,
                    "rejecting peer-relay bridge request — at capacity"
                );
                outbound
                    .send(BepMessage::Close {
                        reason: err.to_string(),
                    })
                    .ok();
            }
        }
    }

    /// Stand up an inner-session terminal for a relayed connection we are the
    /// target of.
    ///
    /// `carry_device` is the volunteer carrying the tunnel; `source_device`
    /// is the requester the inner session talks to. Spawns
    /// [`Self::run_relay_carry_loop`] driven by the carry session's own
    /// outbound channel and a fresh inbound channel; the loop registers the
    /// terminal keyed by `carry_device` and runs the inner BEP session.
    async fn spawn_relay_terminal(&self, carry_device: &str, source_device: String) {
        let carry_outbound = {
            let peers = self.peers.lock().await;
            peers.get(carry_device).map(|h| h.outbound.clone())
        };
        let Some(carry_outbound) = carry_outbound else {
            debug!(
                target: "cascade::backend::p2p",
                carry = %carry_device,
                source = %source_device,
                "ignoring RelayInbound — carrying session is gone",
            );
            return;
        };

        // Channel feeding inbound inner frames to the inner session reader.
        // Move the receiver straight into the transport so it is never held
        // as a local across an await (which would taint this future's
        // auto-traits); only the cloneable sender is registered.
        let (inner_tx, inner_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let transport = peer_relay::PeerRelayTransport::new(carry_outbound, inner_rx);
        {
            let mut terminals = self.relay_terminals.lock().await;
            terminals.insert(carry_device.to_owned(), inner_tx);
        }

        info!(
            target: "cascade::backend::p2p",
            carry = %carry_device,
            source = %source_device,
            "standing up peer-relay inner session terminal"
        );

        let engine = self.clone();
        let carry_device_owned = carry_device.to_owned();
        let source_for_log = source_device.clone();
        tokio::spawn(async move {
            if let Err(e) = engine.run_transport_session(source_device, transport).await {
                debug!(
                    target: "cascade::backend::p2p",
                    source = %source_for_log,
                    error = %e,
                    "peer-relay target inner session ended",
                );
            }
            // The inner session ended — drop the terminal so the carry
            // session stops routing to a dead channel.
            engine.remove_relay_terminal(&carry_device_owned).await;
        });
    }

    /// Remove the inner-session terminal registered for the carrying session
    /// `carry_device`, if any. Called when a relay terminal's inner session
    /// ends so the carry session stops routing `RelayData` to a dead channel.
    async fn remove_relay_terminal(&self, carry_device: &str) {
        self.relay_terminals.lock().await.remove(carry_device);
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

    /// External addresses for the LOCAL device, gathered from the
    /// `NAT`-detection signal: the `STUN`-derived `XOR-MAPPED-ADDRESS`
    /// (when detection produced one) plus every server-reflexive address
    /// learned via peer-as-`STUN` ([`BepMessage::ObservedAddress`]).
    ///
    /// Deduplicated, preserving first-seen order (`STUN` result first,
    /// then observed addresses). Empty until detection runs or a peer
    /// echoes an observed address back — an outbound-only host behind a
    /// `NAT` with no detection has no external address to advertise.
    async fn local_external_addresses(&self) -> Vec<SocketAddr> {
        let mut out: Vec<SocketAddr> = Vec::new();
        // Read each guarded value into a local before branching so the
        // RwLock guard is not held across the `if let` / loop body
        // (clippy::significant_drop_in_scrutinee).
        let stun = *self.local_external_addr.read().await;
        if let Some(stun) = stun {
            out.push(stun);
        }
        let observed = self.observed_external_candidates.read().await.clone();
        for candidate in observed {
            if !out.contains(&candidate.address) {
                out.push(candidate.address);
            }
        }
        out
    }

    /// Build a `BepMessage::Gossip` payload from the current peer
    /// book, suitable for sending to connected peers.
    ///
    /// Each known peer (other than the local device) is emitted with its
    /// `last_seen` value stamped by [`PeerBook::mark_seen`] on the most
    /// recent confirmed contact (outbound connect, inbound accept, or any
    /// frame received). A peer introduced via gossip but never reached
    /// directly is broadcast with `snapshot_unix_seconds = 0`.
    ///
    /// When the local device has learned its own external
    /// (`NAT`-derived) addresses — from `STUN` detection or peer-as-`STUN`
    /// observation — a self entry carrying those addresses is included so
    /// connected peers can relay our reachability to their own peers.
    /// Folding external addresses into the peer book lets them propagate
    /// via introducer gossip (active from [`crate::DiscoveryReach::Private`]
    /// upward). The self entry is stamped with the current time as a
    /// fresh-contact tie-breaker.
    ///
    /// Returns an empty vector when no peers are known and the local
    /// device has no external address to advertise.
    pub async fn current_gossip_snapshot(&self) -> Vec<GossipPeer> {
        let self_id = self.device_id().to_string();
        let mut snapshot: Vec<GossipPeer> = {
            let book = self.peer_book.read().await;
            book.peers()
                .values()
                .filter(|p| p.device_id != self_id)
                .map(|p| GossipPeer {
                    device_id: p.device_id.clone(),
                    addresses: p.addresses.iter().map(ToString::to_string).collect(),
                    snapshot_unix_seconds: p.last_seen,
                })
                .collect()
        };

        let own_external = self.local_external_addresses().await;
        if !own_external.is_empty() {
            snapshot.push(GossipPeer {
                device_id: self_id,
                addresses: own_external.iter().map(ToString::to_string).collect(),
                snapshot_unix_seconds: unix_timestamp_seconds(),
            });
        }
        snapshot
    }

    /// Build a `BepMessage::Gossip` frame from the current peer book
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
    /// After processing the batch, the highest `FileInfo::sequence`
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

    /// Record a write-denied peer's proposed file rows as flagged local
    /// additions in the receive quarantine, the receive-only conflict
    /// semantics: a peer we will not accept writes from has its edits kept
    /// (surfaced to the operator), never silently discarded, never merged into
    /// our authoritative index, and never pushed back to us as authoritative.
    ///
    /// Directory rows and unhealthy rows (`invalid` / `no_permissions`) are
    /// skipped — they are skipped by `merge_files` too, so there is nothing to
    /// preserve. Each retained row is serialised to JSON and handed to the
    /// data-plane authority port, which writes it to the `data_receive_quarantine`
    /// table keyed `(folder, peer, path)`; a newer proposal for a path replaces
    /// the older one.
    ///
    /// When the authority port is unset this is unreachable in practice (an
    /// unset port is default-open, so the write gate never denies), but for
    /// robustness an unset port here logs and drops rather than panicking.
    async fn quarantine_received(&self, peer_device_id: &str, files: &[FileInfo]) {
        let authority = { self.data_authority.read().await.clone() };
        let Some(authority) = authority else {
            debug!(
                target: "cascade::backend::p2p",
                peer = %peer_device_id,
                "write denied but no data-authority port to quarantine into — dropping proposal",
            );
            return;
        };
        let peer = DeviceId::new(peer_device_id.to_string());
        let observed_at = chrono::Utc::now();
        let mut quarantined = 0usize;
        for file in files {
            if file.file_type != FILE_TYPE_FILE {
                continue;
            }
            if file.invalid || file.no_permissions {
                continue;
            }
            let file_json = match serde_json::to_string(&QuarantinedFile::from(file)) {
                Ok(json) => json,
                Err(e) => {
                    warn!(
                        target: "cascade::backend::p2p",
                        peer = %peer_device_id,
                        path = %file.name,
                        error = %e,
                        "could not serialise rejected row for quarantine — dropping it",
                    );
                    continue;
                }
            };
            if let Err(e) = authority
                .quarantine_received(&peer, &self.folder_id, &file.name, &file_json, observed_at)
                .await
            {
                warn!(
                    target: "cascade::backend::p2p",
                    peer = %peer_device_id,
                    path = %file.name,
                    error = %e,
                    "could not record rejected row in receive quarantine",
                );
                continue;
            }
            quarantined += 1;
        }
        if quarantined > 0 {
            info!(
                target: "cascade::backend::p2p",
                peer = %peer_device_id,
                folder = %self.folder_id,
                count = quarantined,
                "recorded rejected local additions from write-denied peer",
            );
        }
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
        // Per-peer read gate: an `IndexUpdate` advertises a local change, so it
        // must only reach peers that may read this folder. A `data:read`-denied
        // peer (write-only / no-share) is skipped — it never learns of our edits.
        // Snapshot the peer ids and their session authentication first, then
        // evaluate access per peer outside the peers lock, because
        // `data_access_for` may itself take other locks (the authority port, the
        // presented-token map). Default-open (port unset) sends to every peer as
        // before.
        let peer_sessions: Vec<(String, CallerAuthentication)> = {
            let peers = self.peers.lock().await;
            peers
                .iter()
                .map(|(id, handle)| (id.clone(), handle.caller_auth))
                .collect()
        };
        for (peer_id, caller_auth) in peer_sessions {
            if !self.data_access_for(&peer_id, caller_auth).await.read {
                debug!(
                    target: "cascade::backend::p2p",
                    peer = %peer_id,
                    folder = %self.folder_id,
                    "data:read denied — skipping IndexUpdate broadcast to peer",
                );
                continue;
            }
            let peers = self.peers.lock().await;
            if let Some(handle) = peers.get(&peer_id) {
                let _ = handle.outbound.send(msg.clone());
            }
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
                            caller_auth: h.caller_auth,
                            outbound: h.outbound.clone(),
                            pending: h.pending.clone(),
                            manage_pending: h.manage_pending.clone(),
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

    /// Manager side: send a [`ManageCommand`] to the connected peer identified
    /// by `device_id` and return the managed node's typed [`ManageResult`].
    ///
    /// The peer must already hold a live session in the engine's peer map — the
    /// caller establishes it first via [`Self::connect_to`] /
    /// [`Self::connect_to_with_strategy`] over the connectivity ladder, so this
    /// method never opens a parallel transport of its own. The command rides the
    /// existing TLS-direct (or post-punch / relay) session as a
    /// [`BepMessage::ManageRequest`] frame, and the reply is the same
    /// [`BepMessage::ManageResponse`] the managed node's dispatcher produces.
    ///
    /// An authorisation denial on the managed node surfaces inside the returned
    /// `Ok(ManageResult::Err { kind: Unauthorised, .. })` — it is the node's
    /// considered answer, not a transport failure. A genuine transport failure
    /// (no session, dropped connection, timeout) is the `Err` arm of the outer
    /// `Result`.
    pub async fn send_manage_request(
        &self,
        device_id: &str,
        command: ManageCommand,
        scope: ManageScope,
        token: Option<String>,
    ) -> Result<ManageResult> {
        // Clone the handle out under the lock so the request — which awaits a
        // reply for up to `MANAGE_REQUEST_TIMEOUT` — does not hold the peer map
        // locked for the whole round-trip.
        let handle = {
            let peers = self.peers.lock().await;
            peers.get(device_id).map(|h| PeerHandle {
                caller_auth: h.caller_auth,
                outbound: h.outbound.clone(),
                pending: h.pending.clone(),
                manage_pending: h.manage_pending.clone(),
                next_request_id: h.next_request_id.clone(),
            })
        };
        let Some(handle) = handle else {
            anyhow::bail!("no live session to device {device_id}");
        };
        handle.send_manage(command, scope, token).await
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
        // expects. See `FileInfo::sequence` for the per-index/per-device
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

    fn make_engine(folder_id: &str) -> (tempfile::TempDir, SyncEngine) {
        let dir = tempdir().unwrap();
        let index = Arc::new(FolderIndex::open(&dir.path().join("idx.db")).unwrap());
        let blocks = Arc::new(BlockStore::new(&dir.path().join("blocks")).unwrap());
        let identity = DeviceIdentity::generate().unwrap();
        let engine = SyncEngine::new(folder_id.to_string(), index, blocks, identity);
        (dir, engine)
    }

    /// Two engines on loopback. A uploads a file, B should see it in
    /// its index after the `IndexUpdate` broadcast.
    #[tokio::test]
    async fn upload_propagates_via_index_update() {
        let (_dir_a, engine_a) = make_engine("shared");
        let (_dir_b, engine_b) = make_engine("shared");

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
        let (_dir_a, engine_a) = make_engine("shared");
        let (_dir_b, engine_b) = make_engine("shared");

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
        let (_dir_a, engine_a) = make_engine("shared");
        let (_dir_b, engine_b) = make_engine("shared");

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
        let (_dir, engine) = make_engine("f");
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
        let (_dir, engine) = make_engine("f");
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
        let (_dir, engine) = make_engine("f");
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
        let (_dir, engine) = make_engine("f");
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
        let (_dir, engine) = make_engine("f");
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
        let (_dir, engine) = make_engine("f");
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
        let (_dir, engine) = make_engine("f");
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
        let (_dir, engine) = make_engine("f");
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
        let (_dir, engine) = make_engine("f");
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
        let (_dir, engine) = make_engine("f");
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
        let (_dir, engine) = make_engine("f");
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
        let (_dir, engine) = make_engine("f");
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
        let (_dir, engine) = make_engine("f");
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
        let (_dir, engine) = make_engine("f");
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
        let (_dir_a, engine_a) = make_engine("shared");
        let (_dir_b, engine_b) = make_engine("shared");

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
        assert!(
            recorded.last_seen > 0,
            "outbound connect should stamp last_seen with the contact time",
        );
    }

    /// `handle_inbound` should record the accepted peer in our `PeerBook`.
    #[tokio::test]
    async fn peer_book_records_inbound_connections() {
        let (_dir_a, engine_a) = make_engine("shared");
        let (_dir_b, engine_b) = make_engine("shared");

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
        let mut found_last_seen: Option<i64> = None;
        for _ in 0..40 {
            let book = engine_b.peer_book().read().await;
            if let Some(entry) = book.get(engine_a.device_id()) {
                found_last_seen = Some(entry.last_seen);
                break;
            }
            drop(book);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let last_seen = found_last_seen.expect("A should be recorded in B's peer book");
        assert!(
            last_seen > 0,
            "inbound accept should stamp last_seen with the contact time",
        );
    }

    /// `current_gossip_snapshot` must carry the per-peer `last_seen`
    /// stamped on each `KnownPeer`, not a single broadcast-time
    /// timestamp. Build a book with two peers at known timestamps and
    /// confirm both come back through the snapshot.
    #[tokio::test]
    async fn broadcast_gossip_uses_per_peer_last_seen() {
        let (_dir, engine) = make_engine("f");
        {
            let mut book = engine.peer_book.write().await;
            book.add_peer(
                "DEVICE-A".to_string(),
                vec!["127.0.0.1:22000".parse().unwrap()],
            );
            book.mark_seen("DEVICE-A", 1_700_000_000);
            book.add_peer(
                "DEVICE-B".to_string(),
                vec!["127.0.0.1:22001".parse().unwrap()],
            );
            book.mark_seen("DEVICE-B", 1_700_005_000);
        }
        let snapshot = engine.current_gossip_snapshot().await;
        assert_eq!(snapshot.len(), 2);
        let by_id: HashMap<&str, &GossipPeer> =
            snapshot.iter().map(|p| (p.device_id.as_str(), p)).collect();
        assert_eq!(
            by_id.get("DEVICE-A").unwrap().snapshot_unix_seconds,
            1_700_000_000,
            "snapshot must carry the per-peer last_seen, not a global stamp",
        );
        assert_eq!(
            by_id.get("DEVICE-B").unwrap().snapshot_unix_seconds,
            1_700_005_000,
        );
    }

    /// A peer learned via gossip but never directly contacted has a
    /// `last_seen` of `0` and must be broadcast that way — we must not
    /// fabricate a contact time we cannot vouch for.
    #[tokio::test]
    async fn gossip_introduced_peers_broadcast_with_zero_last_seen() {
        let (_dir, engine) = make_engine("f");
        {
            let mut book = engine.peer_book.write().await;
            // Simulate a peer learned solely through gossip — never
            // confirmed reachable by us.
            let message = cascade_p2p::wan::GossipMessage {
                peers: vec![cascade_p2p::wan::GossipPeer {
                    device_id: "DEVICE-C".to_string(),
                    addresses: vec!["127.0.0.1:22002".parse().unwrap()],
                }],
            };
            book.merge_gossip("INTRODUCER", engine.device_id(), &message);
        }
        let snapshot = engine.current_gossip_snapshot().await;
        let entry = snapshot
            .iter()
            .find(|p| p.device_id == "DEVICE-C")
            .expect("gossip-introduced peer should appear in snapshot");
        assert_eq!(
            entry.snapshot_unix_seconds, 0,
            "uncontacted peers must broadcast last_seen = 0",
        );
    }

    /// Peer-as-STUN over a real loopback handshake: A dials B. Only the
    /// accepting side (B) observes a genuine NAT-mapped source — A's
    /// ephemeral outbound port — so only B sends an `ObservedAddress`
    /// frame. The connector (A) must NOT echo the address it dialled
    /// (B's own listening address) back to B, so the listener must never
    /// record its own listening address as a server-reflexive candidate.
    ///
    /// Over loopback the source B observes for A is a `127.0.0.0/8`
    /// address, which is not globally routable, so the scope filter in
    /// `set_observed_external_addr` drops it at ingress. The net effect
    /// is that neither side records a reflexive candidate — which is
    /// exactly correct: a same-host observation conveys no public
    /// reachability. (A routable observation is covered by
    /// `set_observed_external_addr_rejects_non_routable_sources`.)
    #[tokio::test]
    async fn observed_address_flows_only_from_acceptor() {
        let (_dir_a, engine_a) = make_engine("shared");
        let (_dir_b, engine_b) = make_engine("shared");

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

        // Let the handshake and any ObservedAddress frames settle.
        tokio::time::sleep(Duration::from_millis(300)).await;

        // The listener (B) must never record its own listening address as
        // a server-reflexive candidate. Before the fix, the connector
        // echoed the dialled address (exactly `addr_b`) back to B, which
        // folded it in as a bogus reflexive candidate. The connector now
        // sends nothing, so B holds no reflexive candidate at all.
        let b_reflexive = engine_b.observed_external_candidates().await;
        assert!(
            !b_reflexive.iter().any(|c| c.address == addr_b),
            "listener must not record its own listening address {addr_b} as a reflexive candidate, got {b_reflexive:?}",
        );
        assert!(
            b_reflexive.is_empty(),
            "acceptor sends no ObservedAddress on the outbound leg, so B records nothing, got {b_reflexive:?}",
        );

        // A's loopback source is observed by B and echoed back, but the
        // scope filter drops it because loopback is not globally routable.
        let a_reflexive = engine_a.observed_external_candidates().await;
        assert!(
            a_reflexive.is_empty(),
            "loopback observations are not globally routable and must be dropped, got {a_reflexive:?}",
        );
    }

    /// A reflexive candidate sourced from an observed address must appear
    /// in the broadcast gossip frame as a self entry, and a fresh
    /// receiver merging that frame must record the reflexive address in
    /// its peer book.
    #[tokio::test]
    async fn observed_reflexive_address_propagates_through_gossip() {
        let (_dir, engine) = make_engine("f");
        let observed: SocketAddr = "203.0.113.7:51820".parse().unwrap();
        engine.set_observed_external_addr(observed).await;

        // The self entry carrying the reflexive address must appear in
        // the snapshot the broadcaster sends.
        let snapshot = engine.current_gossip_snapshot().await;
        let self_entry = snapshot
            .iter()
            .find(|p| p.device_id == engine.device_id())
            .expect("self entry with reflexive address must be in the gossip snapshot");
        assert!(
            self_entry
                .addresses
                .iter()
                .any(|a| a == &observed.to_string()),
            "the observed reflexive address must be advertised, got {:?}",
            self_entry.addresses,
        );

        // A receiver merging that frame must record the address. The
        // receiver is a different device, so the self-exclusion guard in
        // PeerBook::merge_gossip does not drop the broadcaster's entry.
        let (_dir_rx, receiver) = make_engine("f");
        receiver.merge_gossip(engine.device_id(), snapshot).await;
        let book = receiver.peer_book().read().await;
        let recorded = book
            .get(engine.device_id())
            .expect("receiver should record the broadcaster from the gossip frame");
        assert!(
            recorded.addresses.contains(&observed),
            "receiver must merge the reflexive address into the peer book, got {:?}",
            recorded.addresses,
        );
    }

    /// Recording the same observed address more than once must not
    /// inflate the candidate set — repeated frames from several peers
    /// reporting the same reflexive address collapse to one candidate.
    #[tokio::test]
    async fn set_observed_external_addr_deduplicates() {
        let (_dir, engine) = make_engine("f");
        let observed: SocketAddr = "203.0.113.7:51820".parse().unwrap();
        engine.set_observed_external_addr(observed).await;
        engine.set_observed_external_addr(observed).await;
        assert_eq!(engine.observed_external_candidates().await.len(), 1);
    }

    /// A peer on the same LAN (or the local host) observes a private,
    /// loopback, or link-local source. Folding those into the reflexive
    /// set would advertise an unreachable address to off-LAN peers as a
    /// public mapping, so `set_observed_external_addr` must drop them at
    /// ingress and store nothing.
    #[tokio::test]
    async fn set_observed_external_addr_rejects_non_routable_sources() {
        let (_dir, engine) = make_engine("f");
        for raw in [
            "127.0.0.1:51820",      // IPv4 loopback
            "10.0.0.5:51820",       // RFC1918 10/8
            "172.16.3.4:51820",     // RFC1918 172.16/12
            "192.168.1.20:51820",   // RFC1918 192.168/16
            "169.254.10.10:51820",  // IPv4 link-local
            "0.0.0.0:51820",        // unspecified
            "[::1]:51820",          // IPv6 loopback
            "[fe80::1]:51820",      // IPv6 link-local
            "[fc00::1]:51820",      // IPv6 unique-local (fc00::/8)
            "[fd12:3456::1]:51820", // IPv6 unique-local (fd00::/8)
            "[::]:51820",           // IPv6 unspecified
        ] {
            let observed: SocketAddr = raw.parse().unwrap();
            engine.set_observed_external_addr(observed).await;
            assert!(
                engine.observed_external_candidates().await.is_empty(),
                "non-routable observed source {raw} must not be stored as a reflexive candidate",
            );
        }

        // A genuinely routable observation is still recorded.
        let routable: SocketAddr = "198.51.100.7:51820".parse().unwrap();
        engine.set_observed_external_addr(routable).await;
        let stored = engine.observed_external_candidates().await;
        assert_eq!(
            stored.len(),
            1,
            "a globally-routable observed source must be recorded, got {stored:?}",
        );
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
        let (_dir, engine) = make_engine("f");
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
        let (_dir, engine) = make_engine("f");
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
        let (_dir, engine) = make_engine("f");
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
        let (_dir, engine) = make_engine("f");
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
        let (_dir, engine) = make_engine("f");
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
        let (_dir, engine) = make_engine("f");
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

    // ── NAT traversal wiring ──

    /// Synthesise a host candidate for tests that need a concrete
    /// address+port without depending on the machine's real network
    /// interface list.
    fn fake_host_candidate(addr: SocketAddr, local_preference: u16) -> Candidate {
        Candidate::new(addr, CandidateKind::Host, local_preference)
    }

    #[test]
    fn aggregate_candidates_folds_host_set_and_external_addr() {
        // A typical run: two host candidates from the interface walk
        // plus one server-reflexive candidate derived from the STUN
        // mapping. All three should be present in the output, sorted
        // by descending priority.
        let host_a = fake_host_candidate("192.0.2.1:22000".parse().unwrap(), u16::MAX);
        let host_b = fake_host_candidate("192.0.2.2:22000".parse().unwrap(), u16::MAX - 1);
        let external: SocketAddr = "203.0.113.5:42000".parse().unwrap();

        let aggregated = aggregate_candidates(vec![host_a, host_b], Some(external), Vec::new());

        assert_eq!(
            aggregated.len(),
            3,
            "host + host + srflx survives the merge"
        );
        // Host candidates outrank server-reflexive by type preference
        // (126 vs 100) — both hosts must come before the srflx.
        assert_eq!(aggregated[0].kind, CandidateKind::Host);
        assert_eq!(aggregated[1].kind, CandidateKind::Host);
        assert_eq!(aggregated[2].kind, CandidateKind::ServerReflexive);
        assert_eq!(aggregated[2].address, external);
    }

    #[test]
    fn aggregate_candidates_sorts_by_descending_priority() {
        // The decision tree on the receiving end picks the highest
        // priority pair first; the gossiped order must reflect that so
        // a naïve scan does not have to re-sort.
        let host_high = fake_host_candidate("192.0.2.1:22000".parse().unwrap(), u16::MAX);
        let host_low = fake_host_candidate("192.0.2.2:22000".parse().unwrap(), 0);
        let external: SocketAddr = "203.0.113.5:42000".parse().unwrap();

        let aggregated = aggregate_candidates(
            vec![host_low, host_high], // Deliberately reversed input.
            Some(external),
            Vec::new(),
        );

        // The output must be priority-descending regardless of input
        // order — the highest-preference host first, the lowest host
        // second, and the server-reflexive last.
        assert!(aggregated[0].priority >= aggregated[1].priority);
        assert!(aggregated[1].priority >= aggregated[2].priority);
        assert_eq!(aggregated[0].address.ip().to_string(), "192.0.2.1");
        assert_eq!(aggregated[2].kind, CandidateKind::ServerReflexive);
    }

    #[test]
    fn aggregate_candidates_dedupes_by_address_and_kind() {
        // Two host inputs at the same address+kind collapse to one;
        // a server-reflexive at the same address but different kind
        // survives because the dedupe key is the pair, not the address
        // alone.
        let addr: SocketAddr = "192.0.2.1:22000".parse().unwrap();
        let host_a = fake_host_candidate(addr, u16::MAX);
        let host_a_dup = fake_host_candidate(addr, u16::MAX);

        let aggregated = aggregate_candidates(vec![host_a, host_a_dup], Some(addr), Vec::new());

        assert_eq!(
            aggregated.len(),
            2,
            "duplicate host collapses but the srflx at the same address survives"
        );
        let kinds: Vec<_> = aggregated.iter().map(|c| c.kind).collect();
        assert!(kinds.contains(&CandidateKind::Host));
        assert!(kinds.contains(&CandidateKind::ServerReflexive));
    }

    #[test]
    fn aggregate_candidates_handles_missing_external_addr() {
        // When NAT detection has not produced an external mapping yet
        // (or the host is on a public address), the aggregated set
        // contains only the host candidates and nothing else.
        let host = fake_host_candidate("192.0.2.1:22000".parse().unwrap(), u16::MAX);
        let aggregated = aggregate_candidates(vec![host], None, Vec::new());
        assert_eq!(aggregated.len(), 1);
        assert_eq!(aggregated[0].kind, CandidateKind::Host);
    }

    #[test]
    fn aggregate_candidates_folds_extras_into_output() {
        // PeerReflexive / Relayed entries supplied via `extras` must
        // appear alongside the host + srflx set, sorted into the
        // priority order. No extras flow in production yet, but the
        // helper must honour them so a future round can wire them up
        // without changing the aggregation contract.
        let host = fake_host_candidate("192.0.2.1:22000".parse().unwrap(), u16::MAX);
        let relay_addr: SocketAddr = "198.51.100.7:3478".parse().unwrap();
        let relayed = Candidate::new(relay_addr, CandidateKind::Relayed, 0);

        let aggregated = aggregate_candidates(vec![host], None, vec![relayed]);

        assert_eq!(aggregated.len(), 2);
        assert_eq!(aggregated[0].kind, CandidateKind::Host);
        assert_eq!(aggregated[1].kind, CandidateKind::Relayed);
        assert_eq!(aggregated[1].address, relay_addr);
    }

    #[tokio::test]
    async fn decide_connectivity_chooses_direct_when_both_peers_open() {
        // Two Open peers must end up Direct. Feeding a synthetic host
        // candidate through `aggregate_candidates` proves the decision
        // tree honours the priority sort: the dialler targets that
        // address rather than (say) a relay endpoint.
        let host_addr: SocketAddr = "127.0.0.1:22000".parse().unwrap();
        let candidates = aggregate_candidates(
            vec![fake_host_candidate(host_addr, u16::MAX)],
            None,
            Vec::new(),
        );
        let strategy = decide_connectivity(NatType::Open, NatType::Open, &candidates, &[], &[]);
        match strategy {
            ConnectivityStrategy::Direct { addr } => assert_eq!(addr, host_addr),
            other => panic!("expected Direct, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn local_external_addr_round_trips_via_setter() {
        // The background detection task calls `set_local_external_addr`
        // and the connection-time `gather_local_candidates` reads it
        // back. The round-trip is the contract under test here.
        let (_dir, engine) = make_engine("f");
        assert!(
            engine.local_external_addr().await.is_none(),
            "default is None until detection publishes a reading"
        );
        let external: SocketAddr = "203.0.113.5:42000".parse().unwrap();
        engine.set_local_external_addr(Some(external)).await;
        assert_eq!(engine.local_external_addr().await, Some(external));
    }

    #[test]
    fn decide_connectivity_chooses_relay_when_both_symmetric_with_relay() {
        // Symmetric ↔ Symmetric is doomed for direct punch — the table
        // routes through Relay when one is configured. Without a
        // relay, falls back to a best-effort punch (covered by the
        // upstream `cascade_p2p::traversal` tests).
        let relay: SocketAddr = "198.51.100.7:3478".parse().unwrap();
        let strategy =
            decide_connectivity(NatType::Symmetric, NatType::Symmetric, &[], &[], &[relay]);
        assert_eq!(
            strategy,
            ConnectivityStrategy::Relay {
                route: RelayRoute::Operated { endpoint: relay }
            }
        );
    }

    #[tokio::test]
    async fn recorded_peer_relay_is_preferred_over_operated_endpoint() {
        // A relay offer recorded on the engine must surface through
        // `peer_relays()` and, fed to `decide_connectivity` alongside an
        // operated endpoint for a Symmetric ↔ Symmetric pair, win.
        let (_dir, engine) = make_engine("f");
        let volunteer_addr: SocketAddr = "203.0.113.9:22000".parse().unwrap();
        engine
            .record_relay_offer("VOLUNTEER".to_owned(), vec![volunteer_addr])
            .await;

        let peer_relays = engine.peer_relays().await;
        assert_eq!(peer_relays.len(), 1);

        let operated: SocketAddr = "198.51.100.7:3478".parse().unwrap();
        let strategy = decide_connectivity(
            NatType::Symmetric,
            NatType::Symmetric,
            &[],
            &peer_relays,
            &[operated],
        );
        assert_eq!(
            strategy,
            ConnectivityStrategy::Relay {
                route: RelayRoute::Peer {
                    device_id: "VOLUNTEER".to_owned(),
                    address: volunteer_addr,
                }
            }
        );
    }

    /// End-to-end: a volunteer with a permissive policy and an `Open` NAT must
    /// actually emit a `RelayOffer` on session setup, and the connecting peer
    /// must record it so its `peer_relays()` becomes non-empty. This drives
    /// real framed TLS sessions over loopback — the gap the unit tests left,
    /// where `record_relay_offer` was only ever called directly.
    #[tokio::test]
    async fn volunteer_emits_relay_offer_on_session_setup() {
        let (_dir_v, volunteer) = make_engine("shared");
        let (_dir_a, requester) = make_engine("shared");

        volunteer.trust(requester.device_id().to_string()).await;
        requester.trust(volunteer.device_id().to_string()).await;

        // The volunteer is Open with the default Auto policy, so it should
        // advertise itself once its listener is bound (the offer addresses are
        // drawn from the bound host candidates, filtered to routable IPs).
        volunteer.set_local_nat_type(NatType::Open).await;

        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let (addr_v, _v_task) = volunteer
            .start_listener("127.0.0.1:0".parse().unwrap(), cancel_rx)
            .await
            .unwrap();
        // Seed a routable external mapping so the offer set is non-empty even
        // though the loopback listener address itself is not globally routable.
        volunteer
            .set_local_external_addr(Some("203.0.113.7:22000".parse().unwrap()))
            .await;

        requester
            .connect_to(Peer {
                device_id: volunteer.device_id().to_string(),
                address: addr_v,
            })
            .await
            .unwrap();

        // Wait for the volunteer's handshake (which emits the RelayOffer) to
        // reach the requester and populate its peer-relay map.
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let relays = requester.peer_relays().await;
            if let Some(relay) = relays.first() {
                assert_eq!(relay.device_id, volunteer.device_id());
                assert!(
                    relay
                        .addresses
                        .contains(&"203.0.113.7:22000".parse().unwrap()),
                    "offer must carry the volunteer's routable endpoint",
                );
                return;
            }
        }
        panic!("requester never recorded the volunteer's relay offer");
    }

    /// A node whose policy is `Off` must never advertise, even with an `Open`
    /// NAT and a routable endpoint — the connecting peer's `peer_relays()`
    /// stays empty over a full session.
    #[tokio::test]
    async fn off_policy_volunteer_emits_no_relay_offer_over_session() {
        let (_dir_v, volunteer) = make_engine("shared");
        let volunteer = volunteer.with_relay_volunteer(RelayVolunteer::Off);
        let (_dir_a, requester) = make_engine("shared");

        volunteer.trust(requester.device_id().to_string()).await;
        requester.trust(volunteer.device_id().to_string()).await;
        volunteer.set_local_nat_type(NatType::Open).await;

        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let (addr_v, _v_task) = volunteer
            .start_listener("127.0.0.1:0".parse().unwrap(), cancel_rx)
            .await
            .unwrap();
        volunteer
            .set_local_external_addr(Some("203.0.113.7:22000".parse().unwrap()))
            .await;

        requester
            .connect_to(Peer {
                device_id: volunteer.device_id().to_string(),
                address: addr_v,
            })
            .await
            .unwrap();

        // Give the handshake ample time, then confirm no offer was recorded.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            requester.peer_relays().await.is_empty(),
            "an Off-policy node must not advertise itself as a relay",
        );
    }

    /// Full two-hop relay: A reaches B by tunnelling through volunteer V, with
    /// no direct A↔B connection. Proves bytes traverse A → V → B AND back:
    /// the inner BEP handshake completes (ClusterConfig/Index both ways), an
    /// `IndexUpdate` from A lands in B's index (forward), and A fetches a block
    /// that only B holds (return), so a request/response round trip survives
    /// the bidirectional bridge. The volunteer's bandwidth meter must also see
    /// the relayed bytes — the live path it was previously bypassing.
    #[tokio::test]
    async fn two_hop_relay_carries_bep_session_both_ways() {
        let (_dir_v, volunteer) = make_engine("shared");
        let (_dir_a, engine_a) = make_engine("shared");
        let (_dir_b, engine_b) = make_engine("shared");

        // Full trust mesh — every device must accept the others' TLS.
        for peer in [engine_a.device_id(), engine_b.device_id()] {
            volunteer.trust(peer.to_string()).await;
        }
        engine_a.trust(volunteer.device_id().to_string()).await;
        engine_a.trust(engine_b.device_id().to_string()).await;
        engine_b.trust(volunteer.device_id().to_string()).await;
        engine_b.trust(engine_a.device_id().to_string()).await;

        // A block only B holds, so the return direction (B → A) is exercised
        // by a real Request/Response.
        let only_on_b = b"payload that only the target peer holds".repeat(4);
        let block_hash = BlockHash::from_data(&only_on_b);
        engine_b
            .blocks
            .store_block(&block_hash, &only_on_b)
            .await
            .unwrap();

        // The volunteer accepts inbound sessions; B and A both dial it.
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let (addr_v, _v_task) = volunteer
            .start_listener("127.0.0.1:0".parse().unwrap(), cancel_rx)
            .await
            .unwrap();

        // B connects to V first so the volunteer holds a live session to the
        // target before A asks to be bridged to it.
        engine_b
            .connect_to(Peer {
                device_id: volunteer.device_id().to_string(),
                address: addr_v,
            })
            .await
            .unwrap();
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if volunteer.has_peer(engine_b.device_id()).await {
                break;
            }
        }
        assert!(
            volunteer.has_peer(engine_b.device_id()).await,
            "volunteer must hold a session to the target before bridging",
        );

        // A asks the volunteer to bridge it to B. No direct A↔B link exists.
        engine_a
            .attempt_peer_relay(
                &Peer {
                    device_id: engine_b.device_id().to_string(),
                    address: "127.0.0.1:1".parse().unwrap(),
                },
                volunteer.device_id(),
                addr_v,
            )
            .await
            .unwrap();

        // Wait for the inner relayed session to register on both ends.
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if engine_a.has_peer(engine_b.device_id()).await
                && engine_b.has_peer(engine_a.device_id()).await
            {
                break;
            }
        }
        assert!(
            engine_a.has_peer(engine_b.device_id()).await,
            "A must hold an inner session to B through the relay",
        );
        assert!(
            engine_b.has_peer(engine_a.device_id()).await,
            "B must hold an inner session to A through the relay",
        );

        // Forward direction: an IndexUpdate from A must reach B's index.
        let entry = IndexEntry {
            path: "relayed.txt".to_string(),
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
        let mut forward_ok = false;
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if engine_b.index.get("relayed.txt").unwrap().is_some() {
                forward_ok = true;
                break;
            }
        }
        assert!(
            forward_ok,
            "A's IndexUpdate never reached B through the relay"
        );

        // Return direction: A fetches a block only B holds. This requires a
        // Request A → B and a Response B → A, both crossing the bridge.
        let fetched = engine_a
            .fetch_block(
                "relayed.txt",
                0,
                u32::try_from(only_on_b.len()).unwrap(),
                block_hash.0,
            )
            .await
            .expect("A must fetch B's block back through the relay");
        assert_eq!(fetched, only_on_b);

        // The volunteer must have metered the relayed bytes — proof the live
        // forwarding path, not a parallel helper, accounted the traffic.
        assert!(
            volunteer.relay_capacity().bytes_relayed() > 0,
            "the relay bandwidth meter must see the bridged bytes",
        );
    }

    #[tokio::test]
    async fn volunteers_only_when_policy_allows_and_nat_permits() {
        let (_dir, engine) = make_engine("f");

        // Default policy is Auto, but NAT defaults to Unknown — a node
        // that cannot relay must not advertise itself.
        assert_eq!(engine.relay_volunteer(), RelayVolunteer::Auto);
        assert!(
            !engine.should_volunteer_as_relay().await,
            "Unknown NAT must not volunteer"
        );

        // FullCone permits relaying under Auto.
        engine.set_local_nat_type(NatType::FullCone).await;
        assert!(engine.should_volunteer_as_relay().await);

        // Open permits relaying under Auto.
        engine.set_local_nat_type(NatType::Open).await;
        assert!(engine.should_volunteer_as_relay().await);

        // A restrictive NAT cannot relay even when Open earlier.
        engine.set_local_nat_type(NatType::Symmetric).await;
        assert!(!engine.should_volunteer_as_relay().await);
    }

    #[tokio::test]
    async fn volunteer_policy_off_never_advertises() {
        let (_dir, engine) = make_engine("f");
        let engine = engine.with_relay_volunteer(RelayVolunteer::Off);
        // Even with the most permissive NAT, Off means Off.
        engine.set_local_nat_type(NatType::Open).await;
        assert!(!engine.should_volunteer_as_relay().await);
    }

    #[tokio::test]
    async fn candidates_frame_updates_peer_book() {
        // Receiving a `BepMessage::Candidates` must store the wire
        // candidates on the peer book so the next traversal decision
        // can pair them against the local set.
        let (_dir, engine) = make_engine("f");
        let (tx, _rx) = mpsc::unbounded_channel();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Vec<u8>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let manage_pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ManageResult>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let remote_addr: SocketAddr = "203.0.113.1:22001".parse().unwrap();
        let candidates = vec![Candidate::new(remote_addr, CandidateKind::Host, 1024)];
        engine
            .handle_message(
                "PEER-A",
                CallerAuthentication::TlsVerified,
                BepMessage::Candidates {
                    candidates: candidates.clone(),
                },
                &tx,
                &pending,
                &manage_pending,
            )
            .await
            .unwrap();

        let book = engine.peer_book.read().await;
        let stored = book
            .remote_candidates("PEER-A")
            .expect("candidates should be stored under PEER-A");
        assert_eq!(stored, candidates.as_slice());
    }

    #[tokio::test]
    async fn sync_punch_frame_records_agreement_on_peer_book() {
        // Inbound `SyncPunch` must record the peer's nonce and
        // deadline. The matching `run_hole_punch` call reads them back
        // via `PeerBook::current_punch_agreement`.
        let (_dir, engine) = make_engine("f");
        let (tx, _rx) = mpsc::unbounded_channel();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Vec<u8>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let manage_pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ManageResult>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        engine
            .handle_message(
                "PEER-A",
                CallerAuthentication::TlsVerified,
                BepMessage::SyncPunch {
                    nonce: 0xCAFE_BABE,
                    deadline_unix_ms: 1_700_000_000_000,
                },
                &tx,
                &pending,
                &manage_pending,
            )
            .await
            .unwrap();

        let book = engine.peer_book.read().await;
        let agreement = book
            .current_punch_agreement("PEER-A")
            .expect("agreement should be stored under PEER-A");
        assert_eq!(agreement.nonce, 0xCAFE_BABE);
        assert_eq!(agreement.deadline_unix_ms, 1_700_000_000_000);
    }

    #[tokio::test]
    async fn local_nat_type_defaults_to_unknown_until_detection_publishes() {
        // No detection has run yet — the engine must report Unknown.
        // This is the conservative reading the strategy table treats
        // as "route through Relay (or best-effort punch)" rather than
        // a brittle optimistic Direct.
        let (_dir, engine) = make_engine("f");
        assert_eq!(engine.local_nat_type().await, NatType::Unknown);
    }

    #[tokio::test]
    async fn set_local_nat_type_publishes_to_strategy_input() {
        // The background detection task calls `set_local_nat_type` and
        // the connection-time `decide_connectivity` reads it back. The
        // round-trip is the contract under test here.
        let (_dir, engine) = make_engine("f");
        engine.set_local_nat_type(NatType::FullCone).await;
        assert_eq!(engine.local_nat_type().await, NatType::FullCone);
    }

    #[tokio::test]
    async fn ensure_sync_punch_agreement_reuses_fresh_peer_agreement() {
        // When the peer signals first, we honour their nonce instead
        // of allocating a new one — both sides must probe with the
        // same value or `run_hole_punch` will treat the matched probe
        // as a wrong-nonce stray and time out.
        let (_dir, engine) = make_engine("f");
        let peer_agreement = SyncPunchAgreement {
            nonce: 0xDEAD_BEEF,
            deadline_unix_ms: unix_now_ms() + 10_000,
        };
        {
            let mut book = engine.peer_book.write().await;
            book.start_punch_with("PEER-A", peer_agreement);
        }
        let got = engine.ensure_sync_punch_agreement("PEER-A").await.unwrap();
        assert_eq!(got.nonce, 0xDEAD_BEEF);
    }

    #[tokio::test]
    async fn ensure_sync_punch_agreement_replaces_expired() {
        // An expired agreement is treated as absent: a fresh nonce
        // and deadline are minted. Otherwise `run_hole_punch` would
        // reject the call with `DeadlinePassed` and burn a punch
        // budget on a doomed attempt. We stamp the stored nonce with
        // `u64::MAX` so the freshly-allocated one (drawn from the
        // monotonic process counter) cannot collide.
        let (_dir, engine) = make_engine("f");
        {
            let mut book = engine.peer_book.write().await;
            book.start_punch_with(
                "PEER-A",
                SyncPunchAgreement {
                    nonce: u64::MAX,
                    deadline_unix_ms: 0,
                },
            );
        }
        let got = engine.ensure_sync_punch_agreement("PEER-A").await.unwrap();
        assert!(got.deadline_unix_ms > unix_now_ms());
        assert_ne!(got.nonce, u64::MAX);
    }

    #[tokio::test]
    async fn connect_to_with_strategy_rejects_untrusted_peer() {
        // The trust check runs before any traversal logic — an
        // untrusted device must not get as far as candidate selection
        // or UDP socket binding.
        let (_dir, engine) = make_engine("f");
        let err = engine
            .connect_to_with_strategy(Peer {
                device_id: "STRANGER".to_string(),
                address: "127.0.0.1:1".parse().unwrap(),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not trusted"));
    }

    // ── Management-plane request handling ──

    use std::sync::Mutex as StdMutex;

    /// A fake [`ManageDispatch`] that records the caller principal it was
    /// invoked with and returns a canned result, so a test can assert the
    /// authenticated peer device id is threaded through as the caller and the
    /// dispatch outcome is reflected back in the reply frame.
    struct RecordingDispatch {
        seen_caller: StdMutex<Option<String>>,
        result: ManageResult,
    }

    impl RecordingDispatch {
        fn new(result: ManageResult) -> Self {
            Self {
                seen_caller: StdMutex::new(None),
                result,
            }
        }

        fn caller(&self) -> Option<String> {
            self.seen_caller.lock().ok().and_then(|c| c.clone())
        }
    }

    #[async_trait::async_trait]
    impl ManageDispatch for RecordingDispatch {
        async fn dispatch(
            &self,
            caller: &DeviceId,
            _command: ManageCommand,
            _scope: ManageScope,
            _token: Option<String>,
            _now: chrono::DateTime<chrono::Utc>,
        ) -> ManageResult {
            if let Ok(mut seen) = self.seen_caller.lock() {
                *seen = Some(caller.as_str().to_owned());
            }
            self.result.clone()
        }
    }

    #[tokio::test]
    async fn manage_request_uses_authenticated_peer_as_caller_and_replies() {
        let dispatch = Arc::new(RecordingDispatch::new(ManageResult::Ok {
            summary: "did the thing".to_owned(),
        }));
        let (_dir, engine) = make_engine("f");
        let engine = engine.with_manage_dispatch(dispatch.clone());

        let (tx, mut rx) = mpsc::unbounded_channel::<BepMessage>();
        engine
            .handle_manage_request(
                "PEER-DEVICE-ID",
                CallerAuthentication::TlsVerified,
                7,
                ManageCommand::StatusRead,
                ManageScope::Node,
                None,
                &tx,
            )
            .await;

        // The authenticated peer device id is the caller principal.
        assert_eq!(dispatch.caller().as_deref(), Some("PEER-DEVICE-ID"));
        // The reply echoes the request id and carries the dispatch outcome.
        match rx.try_recv() {
            Ok(BepMessage::ManageResponse { request_id, result }) => {
                assert_eq!(request_id, 7);
                assert_eq!(
                    result,
                    ManageResult::Ok {
                        summary: "did the thing".to_owned()
                    }
                );
            }
            other => panic!("expected a ManageResponse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn manage_request_without_dispatch_is_refused_unauthorised() {
        // No dispatch port configured — the node is not accepting remote
        // administration, so a request is refused with a typed unauthorised
        // error rather than silently dropped.
        let (_dir, engine) = make_engine("f");
        let (tx, mut rx) = mpsc::unbounded_channel::<BepMessage>();
        engine
            .handle_manage_request(
                "PEER-DEVICE-ID",
                CallerAuthentication::TlsVerified,
                3,
                ManageCommand::CacheEvict,
                ManageScope::Node,
                None,
                &tx,
            )
            .await;
        match rx.try_recv() {
            Ok(BepMessage::ManageResponse { request_id, result }) => {
                assert_eq!(request_id, 3);
                assert!(matches!(
                    result,
                    ManageResult::Err {
                        kind: ManageErrorKind::Unauthorised,
                        ..
                    }
                ));
            }
            other => panic!("expected an unauthorised ManageResponse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn manage_request_on_unverified_transport_is_refused_before_dispatch() {
        // Caller-spoofing regression: a relayed or post-hole-punch session
        // carries a wire-asserted device id with no end-to-end TLS handshake. A
        // ManageRequest arriving on such a session must be refused with
        // Unauthorised BEFORE the dispatch port is consulted, so a party that
        // can open a tunnel cannot assert a granted manager's device id and have
        // commands authorised under that spoofed principal. The dispatch double
        // is configured to return Ok, so the only way the reply is unauthorised
        // is if the gate refused the request without ever calling dispatch.
        let dispatch = Arc::new(RecordingDispatch::new(ManageResult::Ok {
            summary: "should never run".to_owned(),
        }));
        let (_dir, engine) = make_engine("f");
        let engine = engine.with_manage_dispatch(dispatch.clone());

        let (tx, mut rx) = mpsc::unbounded_channel::<BepMessage>();
        engine
            .handle_manage_request(
                // The attacker asserts a powerful manager's device id on the wire.
                "SPOOFED-MANAGER-DEVICE-ID",
                CallerAuthentication::Unverified,
                11,
                ManageCommand::CacheEvict,
                ManageScope::Node,
                None,
                &tx,
            )
            .await;

        // The dispatch port was never reached — no caller principal recorded.
        assert_eq!(
            dispatch.caller(),
            None,
            "an unverified session must not reach the dispatch port",
        );
        match rx.try_recv() {
            Ok(BepMessage::ManageResponse { request_id, result }) => {
                assert_eq!(request_id, 11);
                assert!(
                    matches!(
                        result,
                        ManageResult::Err {
                            kind: ManageErrorKind::Unauthorised,
                            ..
                        }
                    ),
                    "a ManageRequest on an unverified transport must be refused, got {result:?}",
                );
            }
            other => panic!("expected an unauthorised ManageResponse, got {other:?}"),
        }
    }

    // ── Data-plane directional sharing gates ──

    /// A configurable [`DataAuthority`] double. Returns a fixed [`DataAccess`]
    /// for every (peer, folder) and records the quarantine rows it was handed,
    /// so a test can assert both the access decision taken and the receive-only
    /// conflict handling.
    struct FixedDataAuthority {
        access: DataAccess,
        quarantined: StdMutex<Vec<(String, String, String)>>,
    }

    impl FixedDataAuthority {
        fn new(read: bool, write: bool) -> Arc<Self> {
            Arc::new(Self {
                access: DataAccess { read, write },
                quarantined: StdMutex::new(Vec::new()),
            })
        }

        /// The `(peer, path, file_json)` triples quarantined so far.
        fn quarantined(&self) -> Vec<(String, String, String)> {
            self.quarantined
                .lock()
                .map(|q| q.clone())
                .unwrap_or_default()
        }
    }

    #[async_trait::async_trait]
    impl DataAuthority for FixedDataAuthority {
        async fn data_access(
            &self,
            _peer: &DeviceId,
            _folder: &str,
            _presented_token: Option<&str>,
            _now: chrono::DateTime<chrono::Utc>,
        ) -> anyhow::Result<DataAccess> {
            Ok(self.access)
        }

        async fn quarantine_received(
            &self,
            peer: &DeviceId,
            _folder: &str,
            path: &str,
            file_json: &str,
            _observed_at: chrono::DateTime<chrono::Utc>,
        ) -> anyhow::Result<()> {
            if let Ok(mut q) = self.quarantined.lock() {
                q.push((
                    peer.as_str().to_owned(),
                    path.to_owned(),
                    file_json.to_owned(),
                ));
            }
            Ok(())
        }
    }

    /// A [`DataAuthority`] whose `data_access` always fails, to exercise the
    /// fail-closed branch of [`SyncEngine::data_access_for`].
    struct FailingDataAuthority;

    #[async_trait::async_trait]
    impl DataAuthority for FailingDataAuthority {
        async fn data_access(
            &self,
            _peer: &DeviceId,
            _folder: &str,
            _presented_token: Option<&str>,
            _now: chrono::DateTime<chrono::Utc>,
        ) -> anyhow::Result<DataAccess> {
            anyhow::bail!("data authority store is unavailable")
        }

        async fn quarantine_received(
            &self,
            _peer: &DeviceId,
            _folder: &str,
            _path: &str,
            _file_json: &str,
            _observed_at: chrono::DateTime<chrono::Utc>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn sample_file(name: &str) -> FileInfo {
        FileInfo {
            name: name.to_owned(),
            file_type: FILE_TYPE_FILE,
            size: 11,
            modified: 1_700_000_000,
            sequence: 1,
            block_size: 128 * 1024,
            deleted: false,
            invalid: false,
            no_permissions: false,
            version: Version::default(),
            block_hashes: vec![[7u8; 32]],
        }
    }

    #[tokio::test]
    async fn default_open_when_authority_unset_allows_both_directions() {
        // The non-breaking default: with no DataAuthority wired, a trusted peer
        // keeps full read-write access exactly as before the feature.
        let (_dir, engine) = make_engine("f");
        let access = engine
            .data_access_for("PEER-A", CallerAuthentication::TlsVerified)
            .await;
        assert!(access.read, "unset authority must default-open read");
        assert!(access.write, "unset authority must default-open write");
    }

    #[tokio::test]
    async fn default_open_holds_for_unverified_session_when_port_unset() {
        // The non-breaking default applies on every transport: an unverified
        // (relayed / post-punch) session keeps full access while the port is
        // unset, so the pre-feature relay sync behaviour is unchanged.
        let (_dir, engine) = make_engine("f");
        let access = engine
            .data_access_for("PEER-A", CallerAuthentication::Unverified)
            .await;
        assert!(
            access.read,
            "unset authority + unverified must default-open read"
        );
        assert!(
            access.write,
            "unset authority + unverified must default-open write"
        );
    }

    #[tokio::test]
    async fn unverified_session_is_no_share_once_port_is_set() {
        // Once directional enforcement engages, an unverified session has no
        // trustworthy principal — it is no-share both ways regardless of grants.
        let (_dir, engine) = make_engine("f");
        engine
            .set_data_authority(FixedDataAuthority::new(true, true))
            .await;
        let access = engine
            .data_access_for("PEER-A", CallerAuthentication::Unverified)
            .await;
        assert!(
            !access.read,
            "unverified session must not read once port is set"
        );
        assert!(
            !access.write,
            "unverified session must not write once port is set"
        );
    }

    #[tokio::test]
    async fn authority_failure_fails_closed_to_no_share() {
        // A faulty authority store must not leak data: the decision fails closed
        // to no-share in both directions rather than defaulting to full access.
        let (_dir, engine) = make_engine("f");
        engine
            .set_data_authority(Arc::new(FailingDataAuthority))
            .await;
        let access = engine
            .data_access_for("PEER-A", CallerAuthentication::TlsVerified)
            .await;
        assert!(!access.read, "authority error must deny read");
        assert!(!access.write, "authority error must deny write");
    }

    #[tokio::test]
    async fn write_denied_peer_is_quarantined_not_merged() {
        // Receive-only semantics: a peer we will not accept writes from has its
        // proposed rows recorded as flagged local additions, never merged into
        // the authoritative index, and the session is not torn down.
        let (_dir, engine) = make_engine("f");
        let authority = FixedDataAuthority::new(true, false);
        engine.set_data_authority(authority.clone()).await;

        let (tx, _rx) = mpsc::unbounded_channel();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Vec<u8>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let manage_pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ManageResult>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        engine
            .handle_message(
                "PEER-A",
                CallerAuthentication::TlsVerified,
                BepMessage::Index {
                    folder: "f".to_owned(),
                    files: vec![sample_file("drop.txt")],
                },
                &tx,
                &pending,
                &manage_pending,
            )
            .await
            .expect("a write-denied frame is consumed without error");

        // Not merged into our authoritative index.
        assert!(
            engine.index.get("drop.txt").unwrap().is_none(),
            "a write-denied peer's row must not be merged",
        );
        // Recorded in the quarantine, keyed by peer + path, carrying the row.
        let q = authority.quarantined();
        assert_eq!(q.len(), 1, "the rejected row must be quarantined");
        assert_eq!(q[0].0, "PEER-A");
        assert_eq!(q[0].1, "drop.txt");
        assert!(
            q[0].2.contains("drop.txt"),
            "the quarantined JSON must carry the proposed row, got {}",
            q[0].2,
        );
    }

    #[tokio::test]
    async fn write_allowed_peer_is_merged() {
        // A write-allowed peer merges as before, and nothing is quarantined.
        let (_dir, engine) = make_engine("f");
        let authority = FixedDataAuthority::new(true, true);
        engine.set_data_authority(authority.clone()).await;

        let (tx, _rx) = mpsc::unbounded_channel();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Vec<u8>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let manage_pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ManageResult>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        engine
            .handle_message(
                "PEER-A",
                CallerAuthentication::TlsVerified,
                BepMessage::Index {
                    folder: "f".to_owned(),
                    files: vec![sample_file("keep.txt")],
                },
                &tx,
                &pending,
                &manage_pending,
            )
            .await
            .unwrap();

        assert!(
            engine.index.get("keep.txt").unwrap().is_some(),
            "a write-allowed peer's row must be merged",
        );
        assert!(
            authority.quarantined().is_empty(),
            "nothing is quarantined when the peer may write",
        );
    }

    #[tokio::test]
    async fn unverified_session_cannot_write_even_with_write_grant() {
        // A relayed / post-punch session asserts its device id on the wire; a
        // data:write grant keyed to it must NOT be honoured, or a spoofed id
        // could push content. The rows are quarantined, never merged.
        let (_dir, engine) = make_engine("f");
        let authority = FixedDataAuthority::new(true, true);
        engine.set_data_authority(authority.clone()).await;

        let (tx, _rx) = mpsc::unbounded_channel();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Vec<u8>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let manage_pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ManageResult>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        engine
            .handle_message(
                "PEER-A",
                CallerAuthentication::Unverified,
                BepMessage::Index {
                    folder: "f".to_owned(),
                    files: vec![sample_file("spoof.txt")],
                },
                &tx,
                &pending,
                &manage_pending,
            )
            .await
            .unwrap();

        assert!(
            engine.index.get("spoof.txt").unwrap().is_none(),
            "an unverified session must not write even with a write grant",
        );
        assert_eq!(
            authority.quarantined().len(),
            1,
            "the unverified write is quarantined, not merged",
        );
    }

    #[tokio::test]
    async fn read_denied_peer_gets_empty_block_response() {
        // A read-denied peer that requests a block we hold is told "no such
        // block" uniformly: an empty Response, learning nothing of our content.
        let (_dir, engine) = make_engine("f");
        let authority = FixedDataAuthority::new(false, true);
        engine.set_data_authority(authority).await;

        // We DO hold the block — the gate must refuse before serving it.
        let data = b"secret payload".repeat(8);
        let hash = BlockHash::from_data(&data);
        engine.blocks.store_block(&hash, &data).await.unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Vec<u8>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let manage_pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ManageResult>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        engine
            .handle_message(
                "PEER-A",
                CallerAuthentication::TlsVerified,
                BepMessage::Request {
                    request_id: 3,
                    folder: "f".to_owned(),
                    name: "secret.txt".to_owned(),
                    block_offset: 0,
                    block_size: 128 * 1024,
                    block_hash: hash.0,
                },
                &tx,
                &pending,
                &manage_pending,
            )
            .await
            .unwrap();

        match rx.try_recv() {
            Ok(BepMessage::Response { request_id, data }) => {
                assert_eq!(request_id, 3);
                assert!(
                    data.is_empty(),
                    "a read-denied peer must get an empty Response even when we hold the block",
                );
            }
            other => panic!("expected an empty Response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_allowed_peer_gets_block() {
        // The companion to the above: a read-allowed peer is served the block.
        let (_dir, engine) = make_engine("f");
        let authority = FixedDataAuthority::new(true, true);
        engine.set_data_authority(authority).await;

        let data = b"shared payload".repeat(8);
        let hash = BlockHash::from_data(&data);
        engine.blocks.store_block(&hash, &data).await.unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Vec<u8>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let manage_pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ManageResult>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        engine
            .handle_message(
                "PEER-A",
                CallerAuthentication::TlsVerified,
                BepMessage::Request {
                    request_id: 4,
                    folder: "f".to_owned(),
                    name: "shared.txt".to_owned(),
                    block_offset: 0,
                    block_size: 128 * 1024,
                    block_hash: hash.0,
                },
                &tx,
                &pending,
                &manage_pending,
            )
            .await
            .unwrap();

        match rx.try_recv() {
            Ok(BepMessage::Response {
                request_id,
                data: got,
            }) => {
                assert_eq!(request_id, 4);
                assert_eq!(got, data, "a read-allowed peer must receive the block");
            }
            other => panic!("expected the block Response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cluster_config_captures_and_clears_presented_token() {
        // A data token on the peer's ClusterConfig is captured for the session;
        // a later ClusterConfig with no token clears it (no stale authority).
        let (_dir, engine) = make_engine("f");
        let (tx, _rx) = mpsc::unbounded_channel();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Vec<u8>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let manage_pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ManageResult>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        engine
            .handle_message(
                "PEER-A",
                CallerAuthentication::TlsVerified,
                BepMessage::ClusterConfig {
                    folders: vec![Folder {
                        id: "f".to_owned(),
                        label: "f".to_owned(),
                    }],
                    data_token: Some("token-json".to_owned()),
                },
                &tx,
                &pending,
                &manage_pending,
            )
            .await
            .unwrap();
        assert_eq!(
            engine
                .presented_data_tokens
                .lock()
                .await
                .get("PEER-A")
                .cloned(),
            Some("token-json".to_owned()),
        );

        engine
            .handle_message(
                "PEER-A",
                CallerAuthentication::TlsVerified,
                BepMessage::ClusterConfig {
                    folders: vec![],
                    data_token: None,
                },
                &tx,
                &pending,
                &manage_pending,
            )
            .await
            .unwrap();
        assert!(
            engine
                .presented_data_tokens
                .lock()
                .await
                .get("PEER-A")
                .is_none(),
            "a ClusterConfig with no token must clear the prior token",
        );
    }

    #[tokio::test]
    async fn read_only_peer_cannot_push_an_accepted_change() {
        // End-to-end over a live loopback session: peer B is read-only from A's
        // point of view (A serves B, but will not accept B's writes). B uploads
        // a file and broadcasts it; A must NOT merge it — B's edit stays local
        // on B and is quarantined on A.
        let (_dir_a, engine_a) = make_engine("shared");
        let (_dir_b, engine_b) = make_engine("shared");

        engine_a.trust(engine_b.device_id().to_string()).await;
        engine_b.trust(engine_a.device_id().to_string()).await;

        // A treats B as read-only: A serves B (read=true) but rejects B's
        // writes (write=false). B has no restriction on A (default-open).
        let a_authority = FixedDataAuthority::new(true, false);
        engine_a.set_data_authority(a_authority.clone()).await;

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

        tokio::time::sleep(Duration::from_millis(150)).await;

        let entry = IndexEntry {
            path: "from-b.txt".to_string(),
            is_dir: false,
            size: 11,
            modified: 1_700_000_000,
            block_hashes: vec![0u8; 32],
            deleted: false,
            row_version: 0,
            version: Vec::new(),
        };
        engine_b.index.upsert(&entry).unwrap();
        engine_b.broadcast_update(&entry).await;

        // Give A time to receive and (correctly) reject the IndexUpdate.
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert!(
            engine_a.index.get("from-b.txt").unwrap().is_none(),
            "a read-only peer must not be able to push an accepted change",
        );
        assert!(
            a_authority
                .quarantined()
                .iter()
                .any(|(_, path, _)| path == "from-b.txt"),
            "the rejected change must be quarantined, not discarded",
        );
    }

    #[tokio::test]
    async fn default_trusted_peer_still_syncs_both_ways() {
        // With no data grants configured (authority unset on both sides), two
        // trusted peers keep full bidirectional sync — the non-breaking default.
        let (_dir_a, engine_a) = make_engine("shared");
        let (_dir_b, engine_b) = make_engine("shared");

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

        tokio::time::sleep(Duration::from_millis(150)).await;

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

        for _ in 0..40 {
            if engine_b.index.get("hello.txt").unwrap().is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("default trusted peer did not receive the update");
    }
}

#[cfg(test)]
mod sync_engine_send_check {
    use super::*;
    #[allow(dead_code)]
    fn assert_traits() {
        fn is_send_sync<T: Send + Sync>() {}
        is_send_sync::<SyncEngine>();
    }
}
