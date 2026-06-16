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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use cascade_engine::manage::{DataAccess, DataAuthority, DeviceId, ManageDispatch};
use cascade_p2p::block::BlockHash;
use cascade_p2p::candidate::{Candidate, CandidateKind};
use cascade_p2p::connection::ConnectionManager;
use cascade_p2p::discovery::DiscoveredPeer;
use cascade_p2p::exec_stream::ExecStreamFrame;
use cascade_p2p::framed::{FramedPeer, FramedSession, SessionReader, SessionWriter};
use cascade_p2p::identity::DeviceIdentity;
use cascade_p2p::nat::{
    enumerate_host_candidates, is_globally_routable_ip, server_reflexive_candidate_from_addr,
};
use cascade_p2p::pipe::ByteMeter;
use cascade_p2p::protocol::{
    BepMessage, CapabilityDomain, FileInfo, Folder, GossipPeer, MAX_RELAY_OFFER_ADDRESSES,
    ManageCommand, ManageErrorKind, ManageResult, ManageScope, PROTOCOL_VERSION, Version,
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
pub(crate) enum FramedHalfReader {
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
pub(crate) enum FramedHalfWriter {
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
pub(crate) enum CallerAuthentication {
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
pub(crate) trait AsyncBepReader: Send {
    async fn recv_boxed(&mut self) -> Result<Option<BepMessage>>;
}

/// Object-safe trait the boxed writer half implements. See
/// [`AsyncBepReader`] for the rationale.
#[async_trait::async_trait]
pub(crate) trait AsyncBepWriter: Send {
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
    /// The capability domains the peer advertised in its opening
    /// [`BepMessage::Handshake`] frame. A peer that sent no Handshake (a
    /// pre-version peer) is treated as advertising only
    /// [`CapabilityDomain::Content`] and [`CapabilityDomain::Management`],
    /// the documented baseline for deployed peers before versioned
    /// negotiation was introduced. Used to gate outbound frame types (we must
    /// not send frames for a domain the peer did not advertise) and to refuse
    /// inbound frames whose domain the peer never declared.
    peer_domains: Vec<CapabilityDomain>,
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
    /// F3 readiness bit. Flips from `false` to `true` when the data
    /// authority is wired (the seam the daemon drives after the engine is
    /// constructed and before the BEP listener begins serving peers).
    /// The BEP listener's accept loop consults this bit and closes any
    /// inbound connection accepted while it is `false`, so the data plane
    /// does not serve peers during the startup window between
    /// `P2pBackend::open` and the engine's authority being installed.
    /// A pre-feature engine that never installs an authority keeps the
    /// bit at `false` only conceptually; the listener's port is still
    /// bound and the bit is consulted per-accept so the gate is closed
    /// by construction.
    data_plane_ready: Arc<AtomicBool>,
    /// Whether to advertise the [`CapabilityDomain::Exec`] domain in
    /// the opening [`BepMessage::Handshake`]. Flipped to `true` when the
    /// exec subsystem is wired into the engine (an `Arc<dyn ExecProvider>`
    /// is installed). A wasm / no-exec build or a node that has not yet
    /// installed the exec provider leaves this `false` and omits the domain
    /// from the handshake, so peers know not to send exec frames to it.
    ///
    /// Uses an `AtomicBool` behind an `Arc` for the same reason as
    /// `data_plane_ready`: the bit may be flipped after the engine is
    /// constructed and its clone distributed to running session loops.
    advertise_exec: Arc<AtomicBool>,
    /// Manager-side exec stream consumers, keyed by `(device_id, session_id)`.
    ///
    /// When a manager spawns a PTY or process on a remote node, the node
    /// streams the session's stdout/stderr back as
    /// [`BepMessage::ExecStream`] frames. This map holds the channel that
    /// delivers those frames to the manager-side consumer (the CLI's exec /
    /// shell commands). The session loop routes each inbound frame to the
    /// matching consumer and sends a [`BepMessage::ExecStreamAck`] back so
    /// the node's producer honours the backpressure window. A consumer that
    /// drops its receiver is removed from the map on the next frame.
    exec_stream_consumers:
        Arc<Mutex<HashMap<(String, u64), mpsc::UnboundedSender<ExecStreamFrame>>>>,
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
            // F3: the bit starts `true` (the pre-feature default — the BEP
            // listener serves peers as soon as a stream is accepted). A
            // deployment that wires the data authority post-construction
            // calls `set_data_plane_ready(false)` immediately after
            // `P2pBackend::open` and `set_data_plane_ready(true)` from
            // `set_data_authority`, closing the startup window between
            // those two seams. Bare engines (the pre-feature shape, and
            // integration tests that never wire an authority) leave the
            // bit at `true` and the listener serves as before.
            data_plane_ready: Arc::new(AtomicBool::new(true)),
            // Exec is not wired by default: a wasm build or a node that has
            // not yet installed the exec provider leaves this false. Flip it
            // via `set_advertise_exec(true)` after the exec subsystem is ready.
            advertise_exec: Arc::new(AtomicBool::new(false)),
            exec_stream_consumers: Arc::new(Mutex::new(HashMap::new())),
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
        // F3: installing the data authority closes the startup window.
        // The BEP listener's accept loop consults this bit and closes any
        // inbound connection accepted while it is `false`. Flipping it here
        // means the daemon's existing `wire_manage_dispatch` seam — which
        // already calls `set_data_authority` — is also the F3 readiness
        // seam, so a future refactor that drops one will surface the other.
        self.data_plane_ready.store(true, Ordering::Release);
    }

    /// F3 readiness accessor. Returns `true` immediately after
    /// construction (the pre-feature default — the BEP listener serves
    /// peers as soon as a stream is accepted). A deployment that wires
    /// the data authority post-construction calls
    /// [`Self::set_data_plane_ready_flag`] with `false` immediately
    /// after `P2pBackend::open` and then with `true` from
    /// [`Self::set_data_authority`], closing the startup window between
    /// those two seams. The BEP listener's accept loop is the primary
    /// consumer — it closes any stream accepted while this returns
    /// `false`.
    #[must_use]
    pub fn data_plane_ready(&self) -> bool {
        self.data_plane_ready.load(Ordering::Acquire)
    }

    /// F3 opt-in / opt-out. Set the readiness bit directly. A deployment
    /// that wires the data authority post-construction calls this with
    /// `false` immediately after `P2pBackend::open` to close the startup
    /// window, then [`Self::set_data_authority`] later flips it to `true`.
    /// Bare engines and pre-feature tests leave the bit at its default
    /// (`true`) and the listener serves as before.
    pub fn set_data_plane_ready_flag(&self, ready: bool) {
        self.data_plane_ready.store(ready, Ordering::Release);
    }

    /// Returns `true` when the exec subsystem is wired and the engine should
    /// include [`CapabilityDomain::Exec`] in its opening
    /// [`BepMessage::Handshake`] frame.
    #[must_use]
    pub fn advertise_exec(&self) -> bool {
        self.advertise_exec.load(Ordering::Acquire)
    }

    /// Set whether to include [`CapabilityDomain::Exec`] in future handshakes.
    ///
    /// The daemon calls this with `true` once the exec provider has been wired
    /// into the engine and is ready to serve exec verbs from remote peers. The
    /// flag may be set at any time; sessions already open are not
    /// retroactively updated.
    pub fn set_advertise_exec(&self, enabled: bool) {
        self.advertise_exec.store(enabled, Ordering::Release);
    }

    /// Returns the capability domains the connected peer advertised in its
    /// opening [`BepMessage::Handshake`]. Returns `None` when no session with
    /// `device_id` is registered.
    pub async fn peer_domains(&self, device_id: &str) -> Option<Vec<CapabilityDomain>> {
        let peers = self.peers.lock().await;
        peers.get(device_id).map(|h| h.peer_domains.clone())
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

    /// Public test helper: drive the same data-plane gate the internal
    /// `data_access_for` drives, but with a `TlsVerified` caller
    /// authentication. Exists so integration tests (F1 namespace, F2
    /// explicit-control) can walk the full chain from the live
    /// `DataAuthority` through `data_access` without taking a dependency
    /// on the private caller-authentication enum.
    pub async fn data_access_for_tls_verified(&self, peer: &str) -> DataAccess {
        self.data_access_for(peer, CallerAuthentication::TlsVerified)
            .await
    }

    /// Public test helper: drive the data-plane gate with a presented
    /// capability token. The token is staged in the engine's
    /// `presented_data_tokens` map exactly as the BEP `ClusterConfig`
    /// handler does, then the internal `data_access_for` is invoked
    /// with `TlsVerified` authentication so the verify path runs. Exists
    /// so the F2 explicit-control test can observe the full
    /// token-verify → record-bit → read-grant-set chain end to end.
    pub async fn data_access_for_token(
        &self,
        peer: &str,
        presented_token: Option<&str>,
    ) -> DataAccess {
        {
            let mut tokens = self.presented_data_tokens.lock().await;
            match presented_token {
                Some(token) => {
                    tokens.insert(peer.to_owned(), token.to_owned());
                }
                None => {
                    tokens.remove(peer);
                }
            }
        }
        self.data_access_for(peer, CallerAuthentication::TlsVerified)
            .await
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
                                // F3: the data-plane readiness bit must be set
                                // before any inbound connection proceeds past
                                // the accept. The window between
                                // `P2pBackend::open` and `set_data_authority`
                                // — when the engine is constructed but the
                                // data authority has not yet been installed —
                                // is the gap the F3 invariant closes: a
                                // connection that races this window would
                                // either commit data to the local index or
                                // block waiting for a token that never
                                // arrives. Drop the stream immediately; the
                                // remote peer sees a clean TCP close, not a
                                // hung half-handshake.
                                if !engine.data_plane_ready() {
                                    drop(stream);
                                    debug!(
                                        target: "cascade::backend::p2p",
                                        peer = %peer_addr,
                                        "F3: closing inbound connection during startup window \
                                         (data authority not yet installed)",
                                    );
                                    continue;
                                }
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
    ///
    /// The F3 readiness bit is intentionally **not** consulted on the
    /// outbound path. The bit guards the inbound listener (the side that
    /// would serve the peer its index and blocks): an inbound connection
    /// accepted during the startup window is closed without BEP
    /// negotiation. An outbound dial, in contrast, is initiated by *us*
    /// and ends in a BEP session the listener side will gate through the
    /// same bit. Refusing dials here would break the natural pattern of
    /// "dial after the engine is ready", where the engine's readiness is
    /// already proven by the caller having just installed the authority.
    /// The BEP layer still authenticates the peer, so a dial that races
    /// the window is rejected at handshake, not silently granted.
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
    /// On success the punched UDP socket is upgraded to a full BEP
    /// session via [`UdpFlowTransport`] → [`FramedSession`] →
    /// [`Self::run_transport_session`]. A failed punch logs the
    /// underlying `PunchError` and returns `Ok(())` so the caller can
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
                            peer_domains: h.peer_domains.clone(),
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
                peer_domains: h.peer_domains.clone(),
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

    /// Register a receiver for inbound exec-stream frames from `device_id`
    /// for `session_id`, returning the channel that delivers decoded frames.
    ///
    /// The session loop routes each [`BepMessage::ExecStream`] frame for the
    /// named `(device_id, session_id)` pair to this receiver as a typed
    /// [`ExecStreamFrame`], and sends a [`BepMessage::ExecStreamAck`] back so
    /// the node's producer keeps flowing. A manager calls this *before*
    /// sending the `PtySpawn` / `ProcSpawn` that mints `session_id`, so the
    /// first output frame does not race the registration. Dropping the
    /// receiver removes the entry on the next frame.
    pub async fn subscribe_exec_stream(
        &self,
        device_id: &str,
        session_id: u64,
    ) -> mpsc::UnboundedReceiver<cascade_p2p::exec_stream::ExecStreamFrame> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut consumers = self.exec_stream_consumers.lock().await;
        consumers.insert((device_id.to_owned(), session_id), tx);
        rx
    }

    /// Remove a previously-registered exec stream consumer. Called when the
    /// manager's exec / shell command exits, so a stale consumer does not
    /// absorb frames for a session that has ended.
    pub async fn unsubscribe_exec_stream(&self, device_id: &str, session_id: u64) {
        let mut consumers = self.exec_stream_consumers.lock().await;
        consumers.remove(&(device_id.to_owned(), session_id));
    }

    /// Send a `PtyWrite` management command to write `bytes` to the stdin of
    /// `session_id` on `device_id`. The bytes travel as a management frame,
    /// not as an [`BepMessage::ExecStream`] frame — the data plane carries
    /// node-to-manager output only; stdin rides the control path.
    ///
    /// `token` is the same capability token presented to the spawning
    /// `PtySpawn`: the node rebuilds the grant set from the on-node grants plus
    /// the presented token on *every* management request, so a token-only
    /// caller (no on-node grant) must re-present it here or the write is
    /// rejected as unauthorised. The node ignores the advertised wire scope for
    /// a session verb and authorises against the session's stored scope.
    pub async fn send_pty_write(
        &self,
        device_id: &str,
        session_id: u64,
        bytes: Vec<u8>,
        token: Option<String>,
    ) -> Result<ManageResult> {
        self.send_manage_request(
            device_id,
            ManageCommand::PtyWrite {
                session: session_id,
                bytes,
            },
            // The node authorises a session verb against the session's stored
            // scope, not this advertised value; the wire scope is a placeholder
            // the dispatcher does not gate session verbs on.
            ManageScope::Node,
            token,
        )
        .await
    }

    /// Send a `PtyResize` management command to resize the PTY of `session_id`
    /// on `device_id` to `cols` x `rows`. See [`send_pty_write`] for the
    /// `token` parameter.
    pub async fn send_pty_resize(
        &self,
        device_id: &str,
        session_id: u64,
        cols: u16,
        rows: u16,
        token: Option<String>,
    ) -> Result<ManageResult> {
        self.send_manage_request(
            device_id,
            ManageCommand::PtyResize {
                session: session_id,
                cols,
                rows,
            },
            ManageScope::Node,
            token,
        )
        .await
    }

    /// Send a `PtyKill` management command to signal `session_id` on
    /// `device_id` with `signal`. See [`send_pty_write`] for the `token`
    /// parameter.
    pub async fn send_pty_signal(
        &self,
        device_id: &str,
        session_id: u64,
        signal: i32,
        token: Option<String>,
    ) -> Result<ManageResult> {
        self.send_manage_request(
            device_id,
            ManageCommand::PtyKill {
                session: session_id,
                signal,
            },
            ManageScope::Node,
            token,
        )
        .await
    }
}

/// Build the path at which to persist a conflict copy of a row whose
/// content is about to be overwritten by an incoming concurrent write.
///
/// The format is `<stem>.conflict-<device_identifier>-<timestamp>.<ext>`
/// where the stem and extension are split on the LAST `.` in the
/// filename. A leading dot is treated as a hidden-file marker rather
/// Standard subdirectory under the backend data dir used by the sync
/// engine for any auxiliary state. Reserved for future use.
#[must_use]
pub fn sync_state_dir(base: &std::path::Path) -> PathBuf {
    base.join("sync")
}

// Conflict-copy path construction and device-id derivation helpers live in a
// child module to keep this file under the source-length cap. Re-imported here
// so the helpers stay in this module's namespace and `super::<helper>`
// resolution works unchanged for the sibling modules and tests that use them.
mod conflict_path;
use conflict_path::{
    conflict_copy_path, derive_device_short_id, entry_to_file_info, local_short_device_id,
    sanitise_for_path, unix_timestamp_seconds,
};

// Relay, hole-punch-relay, and session/message-loop methods of `SyncEngine`
// live in a child module to keep this file under the source-length cap. They
// remain part of the same `impl SyncEngine` surface.
mod relay_session;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
#[path = "sync_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "sync_sync_engine_send_check.rs"]
mod sync_engine_send_check;
