//! BEP (Block Exchange Protocol) message types and XDR codec.
//!
//! Messages are length-prefixed XDR: a 4-byte big-endian length followed by
//! the XDR-encoded message body. The message type is the first uint32 in the
//! body, allowing the decoder to dispatch to the correct deserialiser.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use anyhow::Result;

use crate::candidate::{Candidate, CandidateKind};

// ── Message type constants ──

const MSG_CLUSTER_CONFIG: u32 = 0;
const MSG_INDEX: u32 = 1;
const MSG_INDEX_UPDATE: u32 = 2;
const MSG_REQUEST: u32 = 3;
const MSG_RESPONSE: u32 = 4;
const MSG_PING: u32 = 5;
const MSG_CLOSE: u32 = 6;
const MSG_GOSSIP: u32 = 7;
const MSG_CANDIDATES: u32 = 8;
const MSG_SYNC_PUNCH: u32 = 9;
const MSG_OBSERVED_ADDRESS: u32 = 10;
const MSG_RELAY_OFFER: u32 = 11;
const MSG_RELAY_CONNECT: u32 = 12;
const MSG_RELAY_DATA: u32 = 13;
const MSG_RELAY_INBOUND: u32 = 14;
const MSG_MANAGE_REQUEST: u32 = 15;
const MSG_MANAGE_RESPONSE: u32 = 16;

/// Wire discriminant for a [`ManageResult::Ok`] outcome inside a
/// [`BepMessage::ManageResponse`] frame.
const MANAGE_RESULT_OK: u32 = 0;
/// Wire discriminant for a [`ManageResult::Err`] outcome inside a
/// [`BepMessage::ManageResponse`] frame.
const MANAGE_RESULT_ERR: u32 = 1;

/// Maximum number of candidates carried in a single
/// [`BepMessage::Candidates`] frame. Bounds the receiver's allocation
/// when a malicious or buggy peer sends a huge list. A device with
/// more than a handful of host, server-reflexive and relayed
/// addresses is unrealistic in practice; the cap leaves headroom
/// without being lavish.
const MAX_CANDIDATES_PER_FRAME: u32 = 64;

/// Address-family tag for [`encode_socket_addr`] / [`decode_socket_addr`].
const ADDR_FAMILY_IPV4: u8 = 4;
/// Address-family tag for [`encode_socket_addr`] / [`decode_socket_addr`].
const ADDR_FAMILY_IPV6: u8 = 6;

/// Maximum number of peers carried in a single `Gossip` frame. Caps the
/// receiver's memory cost when a malicious or buggy peer sends a huge
/// peer list. Well above the realistic peer-book size for a personal
/// mesh while still bounded.
const MAX_GOSSIP_PEERS: u32 = 10_000;

/// Maximum number of addresses per `GossipPeer`. A peer with more than
/// a small handful of reachable endpoints almost certainly indicates
/// either misconfiguration or an attempt to amplify the wire frame, so
/// the cap stays conservative.
const MAX_GOSSIP_ADDRESSES_PER_PEER: u32 = 32;

/// Maximum number of reachable addresses a [`BepMessage::RelayOffer`] may
/// advertise.
///
/// A volunteering relay realistically exposes a single public `host:port`,
/// occasionally a small handful (dual-stack, multiple interfaces). The cap
/// bounds receiver allocation against a malicious or buggy offer without
/// constraining any legitimate deployment. Exposed so a volunteer building an
/// offer caps its own advertised set to the same ceiling the encoder
/// enforces, rather than hardcoding a parallel number.
pub const MAX_RELAY_OFFER_ADDRESSES: u32 = 8;

// ── XDR primitives ──

fn encode_u32(buf: &mut Vec<u8>, val: u32) {
    buf.extend_from_slice(&val.to_be_bytes());
}

fn encode_u64(buf: &mut Vec<u8>, val: u64) {
    buf.extend_from_slice(&val.to_be_bytes());
}

fn encode_i64(buf: &mut Vec<u8>, val: i64) {
    buf.extend_from_slice(&val.to_be_bytes());
}

fn encode_opaque(buf: &mut Vec<u8>, data: &[u8]) -> Result<()> {
    let len = u32::try_from(data.len())
        .map_err(|_| anyhow::anyhow!("opaque data length {} exceeds u32", data.len()))?;
    encode_u32(buf, len);
    buf.extend_from_slice(data);
    let pad = (4 - (data.len() % 4)) % 4;
    buf.extend(std::iter::repeat_n(0u8, pad));
    Ok(())
}

fn encode_string(buf: &mut Vec<u8>, s: &str) -> Result<()> {
    encode_opaque(buf, s.as_bytes())
}

fn decode_u32(data: &[u8]) -> io::Result<(u32, &[u8])> {
    let (bytes, rest) = data
        .split_first_chunk::<4>()
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "need 4 bytes for uint32"))?;
    Ok((u32::from_be_bytes(*bytes), rest))
}

fn decode_u64(data: &[u8]) -> io::Result<(u64, &[u8])> {
    let (bytes, rest) = data
        .split_first_chunk::<8>()
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "need 8 bytes for uint64"))?;
    Ok((u64::from_be_bytes(*bytes), rest))
}

fn decode_i64(data: &[u8]) -> io::Result<(i64, &[u8])> {
    let (bytes, rest) = data
        .split_first_chunk::<8>()
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "need 8 bytes for int64"))?;
    Ok((i64::from_be_bytes(*bytes), rest))
}

fn decode_opaque(data: &[u8]) -> io::Result<(&[u8], &[u8])> {
    let (len, rest) = decode_u32(data)?;
    let len = usize::try_from(len).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if rest.len() < len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "opaque data truncated",
        ));
    }
    let (opaque_data, remainder) = rest.split_at(len);
    let pad = (4 - (len % 4)) % 4;
    let remainder = remainder.get(pad..).unwrap_or(&[]);
    Ok((opaque_data, remainder))
}

fn decode_string(data: &[u8]) -> io::Result<(String, &[u8])> {
    let (bytes, rest) = decode_opaque(data)?;
    let s = String::from_utf8(bytes.to_vec())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok((s, rest))
}

// ── Protocol types ──

/// A folder shared between peers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    /// Unique folder identifier.
    pub id: String,
    /// Human-readable label.
    pub label: String,
}

/// A version vector — one `(device_short_id, counter)` entry per device
/// that has ever modified the file. An empty vector means the row has
/// never been written.
///
/// Ordering rules (Syncthing-compatible):
/// - A *dominates* B when every counter in B is less than or equal to
///   the corresponding counter in A, and A has at least one entry that
///   is strictly greater than the matching entry in B (or present in A
///   and absent in B with a non-zero counter).
/// - Equal vectors (`a == b`) do not dominate one another.
/// - When neither dominates the other, the two versions are concurrent
///   — a conflict, in which case the caller must decide how to resolve.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Version {
    /// Sorted ascending by `device_short_id` for a stable wire encoding
    /// and deterministic comparisons.
    pub counters: Vec<(u64, u64)>,
}

impl Version {
    /// Increment this device's counter, inserting a new entry at
    /// counter 1 if the device is not yet present.
    pub fn bump(&mut self, device_short_id: u64) {
        if let Some(entry) = self
            .counters
            .iter_mut()
            .find(|(id, _)| *id == device_short_id)
        {
            entry.1 += 1;
        } else {
            self.counters.push((device_short_id, 1));
            self.counters.sort_by_key(|(id, _)| *id);
        }
    }

    /// `true` if `self` dominates `other`.
    ///
    /// `self` dominates `other` when every counter in `other` is less
    /// than or equal to the corresponding counter in `self`, and at
    /// least one entry in `self` is strictly greater than the matching
    /// entry in `other` (treating absent entries as zero). Equal
    /// vectors are not considered to dominate — use `==` for equality.
    #[must_use]
    pub fn dominates(&self, other: &Self) -> bool {
        let mut at_least_one_greater = false;
        for (other_id, other_ctr) in &other.counters {
            let self_ctr = self
                .counters
                .iter()
                .find(|(id, _)| id == other_id)
                .map_or(0, |(_, c)| *c);
            if self_ctr < *other_ctr {
                return false;
            }
            if self_ctr > *other_ctr {
                at_least_one_greater = true;
            }
        }
        // Any non-zero counter present in self but absent in other
        // implies self has additional history beyond other.
        for (self_id, self_ctr) in &self.counters {
            if *self_ctr > 0 && !other.counters.iter().any(|(id, _)| id == self_id) {
                at_least_one_greater = true;
            }
        }
        at_least_one_greater
    }

    /// Merge `other` into `self`, taking the maximum of each device's
    /// counter. Entries present only in `other` are inserted.
    pub fn merge(&mut self, other: &Self) {
        for (other_id, other_ctr) in &other.counters {
            if let Some(entry) = self.counters.iter_mut().find(|(id, _)| id == other_id) {
                entry.1 = entry.1.max(*other_ctr);
            } else {
                self.counters.push((*other_id, *other_ctr));
            }
        }
        self.counters.sort_by_key(|(id, _)| *id);
    }
}

/// Description of a file's blocks as announced in Index messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    /// File name (relative to folder root).
    pub name: String,
    /// File type: 0 = file, 1 = directory.
    pub file_type: u32,
    /// Total file size in bytes.
    pub size: u64,
    /// Last modification time (Unix timestamp seconds).
    pub modified: i64,
    /// Per-row monotonic sequence number assigned by the sending peer's
    /// folder index. The receiver records the maximum sequence it has
    /// seen from each peer so that on reconnect only entries with a
    /// sequence greater than the last-seen value need to be sent — the
    /// delta-sync optimisation described in BEP.
    ///
    /// Sequence space is per-INDEX (i.e. per backend instance) on the
    /// sender, not strictly per-DEVICE. Since each device runs exactly
    /// one `FolderIndex` (defined in `cascade-backend-p2p`) in the
    /// current implementation, the two are equivalent here, but a
    /// future multi-folder-per-device design would need a per-(device,
    /// folder) tracking key.
    pub sequence: u64,
    /// Block size used for this file.
    pub block_size: u32,
    /// Tombstone flag. When `true`, the row records a delete event:
    /// the peer should mark its local copy deleted (subject to the
    /// version-vector comparison on `version`).
    pub deleted: bool,
    /// When `true`, the row's content is mid-write or otherwise in an
    /// inconsistent state on the sender. Receivers must NOT request
    /// blocks for this entry and must not upsert its content; the row
    /// is silently skipped at debug-log level.
    ///
    /// Currently only respected on receive — local producers do not
    /// emit `invalid: true` yet because the backend has no
    /// mid-write state for an `IndexEntry`. The wire field is in
    /// place so producers can be added later without a protocol bump.
    pub invalid: bool,
    /// When `true`, the sending device exists and knows about the file
    /// but cannot share its content (typically a permission-denied
    /// error reading the local row). Receivers must not request blocks
    /// for this entry and must not upsert its content.
    ///
    /// Currently only respected on receive — local producers do not
    /// emit `no_permissions: true` yet because the backend has no
    /// per-row permission-check infrastructure. The wire field is in
    /// place so producers can be added later without a protocol bump.
    pub no_permissions: bool,
    /// Per-file version vector. Used to detect concurrent edits that
    /// happened on disconnected peers — a strict generalisation of the
    /// previous `modified`-only LWW comparison.
    pub version: Version,
    /// SHA-256 hashes of each block, in order.
    pub block_hashes: Vec<[u8; 32]>,
}

/// A peer entry as it appears on the wire inside a [`BepMessage::Gossip`] frame.
///
/// Deliberately wire-typed (string addresses, explicit `last_seen`
/// field) so the on-disk [`crate::wan::PeerBook`] storage can evolve
/// without churning the BEP layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GossipPeer {
    /// Device ID (base32-encoded SHA-256 of the peer's TLS certificate).
    pub device_id: String,
    /// Known socket-addressable endpoints for this peer, serialised as
    /// `host:port` strings. Receivers parse each entry and silently
    /// drop ones they cannot resolve — DNS-style host names that are
    /// not yet routable from the receiver's network can show up here.
    pub addresses: Vec<String>,
    /// Unix-seconds timestamp at which the broadcaster *took the
    /// snapshot* — not necessarily when the peer was last actually
    /// reachable. `PeerBook` does not yet record per-peer
    /// last-contact time, so the broadcaster stamps every entry with
    /// `now` at send-time. A receiver should treat this as a
    /// monotone tie-breaker for concurrent gossip from multiple
    /// introducers, NOT as proof that the peer was live at this
    /// instant. When `PeerBook` grows a real last-seen field, this
    /// can be tightened to the per-peer observed timestamp.
    pub snapshot_unix_seconds: i64,
}

/// The extent a [`ManageCommand`] targets, as carried on the wire.
///
/// Mirrors the management-plane scope model in `cascade_engine::manage`: a
/// command is either node-wide or confined to a folder subtree identified by a
/// path prefix. Kept wire-typed (a plain enum over a path string) so the
/// protocol crate stays independent of the engine's richer `Scope` type — the
/// engine maps between the two at the dispatch boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManageScope {
    /// The whole node — every path.
    Node,
    /// A folder subtree identified by its path prefix.
    Folder {
        /// The folder path prefix this scope covers.
        path: String,
    },
}

/// Wire discriminant for a node-wide [`ManageScope`].
const MANAGE_SCOPE_NODE: u32 = 0;
/// Wire discriminant for a folder-scoped [`ManageScope`].
const MANAGE_SCOPE_FOLDER: u32 = 1;

/// A management command a manager asks a managed node to run, as carried on the
/// wire inside a [`BepMessage::ManageRequest`].
///
/// Each variant names a verb plus its arguments. The set is deliberately
/// closed and explicit rather than a free-form `(name, args)` pair: an
/// unrecognised verb fails to decode rather than reaching the dispatcher as an
/// untyped string, and the managed node's authorisation logic can map each
/// variant to the exact capability it requires. New capabilities slot in as new
/// variants with their own message-type-independent encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManageCommand {
    /// Read node status — mount state, cache usage, backend health, peer list.
    /// Requires the `status:read` capability.
    StatusRead,
    /// Pin a path glob, keeping matching files offline. Requires the
    /// `pin:write` capability.
    Pin {
        /// The path glob to pin.
        path_glob: String,
        /// Whether the pin recurses into subdirectories.
        recursive: bool,
    },
    /// Remove a pin rule. Requires the `pin:write` capability.
    Unpin {
        /// The path glob whose pin rule to remove.
        path_glob: String,
    },
    /// Run one cache eviction sweep. Requires the `cache:manage` capability.
    CacheEvict,
    /// Pre-warm a path glob so matching files are fetched on the next sync.
    /// Requires the `cache:manage` capability.
    CacheWarm {
        /// The path glob to warm.
        path_glob: String,
    },
    /// Push a `.cascade` config fragment to merge into the node's rule set.
    /// Requires the `config:push` capability over the fragment's target folder.
    ConfigPush {
        /// The serialisation format of `body`.
        format: ManageConfigFormat,
        /// The folder the fragment applies to — the scope the push targets and
        /// is authorised over. The fragment's pin and lifecycle rules are
        /// rooted here.
        folder: String,
        /// The raw config fragment in `format`.
        body: String,
    },
    /// Set a lifecycle policy on the node. Requires the `policy:set` capability
    /// over the policy's path.
    PolicySet {
        /// The path glob the policy applies to — also the scope it is
        /// authorised over.
        path_glob: String,
        /// Maximum file age before eviction, in seconds. Absent leaves the
        /// dimension unbounded.
        max_age_secs: Option<i64>,
        /// Maximum file size before eviction, in bytes. Absent leaves the
        /// dimension unbounded.
        max_file_size: Option<i64>,
        /// Priority — higher wins when policies overlap.
        priority: i32,
    },
    /// Register a backend on the node. Requires the dangerous `backend:manage`
    /// capability, granted explicitly for the backend's mount path (never by a
    /// node-wide wildcard).
    BackendAdd {
        /// The backend name (its identifier and config file stem).
        name: String,
        /// The backend type (`gdrive`, `s3`, `p2p`, …).
        backend_type: String,
        /// The VFS mount path the backend is mounted at — the scope this
        /// command is authorised over.
        mount_path: String,
        /// The backend's TOML config fragment, as a literal TOML document. The
        /// node parses and registers it exactly as the local wizard would.
        config_toml: String,
    },
    /// Remove a registered backend by name. Requires the dangerous
    /// `backend:manage` capability over the backend's mount path.
    BackendRemove {
        /// The backend name to remove.
        name: String,
        /// The VFS mount path the backend occupied — the scope this command is
        /// authorised over.
        mount_path: String,
    },
    /// Restart the daemon's background workers. Requires the dangerous
    /// `lifecycle:control` capability, granted explicitly for a folder scope.
    Restart,
    /// Stop the daemon's background workers. Requires the dangerous
    /// `lifecycle:control` capability, granted explicitly for a folder scope.
    Stop,
    /// Delegate a grant to another device. Requires the dangerous
    /// `grant:admin` capability over the grant's scope, AND the caller must
    /// itself hold a grant that is a superset of the one being delegated.
    GrantAdd {
        /// The grant being delegated.
        grant: ManageGrant,
    },
    /// Revoke a grant by its row id. Requires the dangerous `grant:admin`
    /// capability over the revoked grant's scope.
    GrantRevoke {
        /// The row id of the grant to revoke.
        grant_id: i64,
        /// The scope of the grant being revoked — the extent this command is
        /// authorised over.
        scope: ManageScope,
    },
}

/// The serialisation format of a [`ManageCommand::ConfigPush`] body.
///
/// Mirrors the four `.cascade` formats the parser accepts. Kept wire-typed so
/// the protocol crate stays independent of the config crate; the engine maps
/// each variant to the matching parser at the dispatch boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManageConfigFormat {
    /// Gitignore-style `.cascade`.
    Gitignore,
    /// `.cascade.toml`.
    Toml,
    /// `.cascade.yaml`.
    Yaml,
    /// `.cascade.json`.
    Json,
}

/// Wire discriminant for [`ManageConfigFormat::Gitignore`].
const MANAGE_CONFIG_FORMAT_GITIGNORE: u32 = 0;
/// Wire discriminant for [`ManageConfigFormat::Toml`].
const MANAGE_CONFIG_FORMAT_TOML: u32 = 1;
/// Wire discriminant for [`ManageConfigFormat::Yaml`].
const MANAGE_CONFIG_FORMAT_YAML: u32 = 2;
/// Wire discriminant for [`ManageConfigFormat::Json`].
const MANAGE_CONFIG_FORMAT_JSON: u32 = 3;

/// A capability grant as carried on the wire inside a
/// [`ManageCommand::GrantAdd`].
///
/// Mirrors `cascade_engine::manage::Grant` minus the `granted_by` field: a
/// delegated grant is always issued by the calling manager, so the managed node
/// stamps `granted_by` with the authenticated caller rather than trusting a
/// value off the wire. Kept wire-typed (a capability wire string, a
/// [`ManageScope`], an optional RFC 3339 expiry) so the protocol crate stays
/// free of the engine's domain enums.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManageGrant {
    /// The device the grant authorises, by device ID.
    pub grantee: String,
    /// The capability conferred, in its colon-delimited wire form.
    pub capability: String,
    /// The scope the capability applies over.
    pub scope: ManageScope,
    /// When the grant expires, as an RFC 3339 timestamp. Absent means never.
    pub expires: Option<String>,
}

/// Wire discriminant for [`ManageCommand::StatusRead`].
const MANAGE_CMD_STATUS_READ: u32 = 0;
/// Wire discriminant for [`ManageCommand::Pin`].
const MANAGE_CMD_PIN: u32 = 1;
/// Wire discriminant for [`ManageCommand::Unpin`].
const MANAGE_CMD_UNPIN: u32 = 2;
/// Wire discriminant for [`ManageCommand::CacheEvict`].
const MANAGE_CMD_CACHE_EVICT: u32 = 3;
/// Wire discriminant for [`ManageCommand::CacheWarm`].
const MANAGE_CMD_CACHE_WARM: u32 = 4;
/// Wire discriminant for [`ManageCommand::ConfigPush`].
const MANAGE_CMD_CONFIG_PUSH: u32 = 5;
/// Wire discriminant for [`ManageCommand::PolicySet`].
const MANAGE_CMD_POLICY_SET: u32 = 6;
/// Wire discriminant for [`ManageCommand::BackendAdd`].
const MANAGE_CMD_BACKEND_ADD: u32 = 7;
/// Wire discriminant for [`ManageCommand::BackendRemove`].
const MANAGE_CMD_BACKEND_REMOVE: u32 = 8;
/// Wire discriminant for [`ManageCommand::Restart`].
const MANAGE_CMD_RESTART: u32 = 9;
/// Wire discriminant for [`ManageCommand::Stop`].
const MANAGE_CMD_STOP: u32 = 10;
/// Wire discriminant for [`ManageCommand::GrantAdd`].
const MANAGE_CMD_GRANT_ADD: u32 = 11;
/// Wire discriminant for [`ManageCommand::GrantRevoke`].
const MANAGE_CMD_GRANT_REVOKE: u32 = 12;

/// Wire sentinel for an absent optional value (for example a `None` expiry or
/// an unbounded policy dimension). Paired with [`OPTION_SOME`].
const OPTION_NONE: u32 = 0;
/// Wire sentinel for a present optional value.
const OPTION_SOME: u32 = 1;

/// The outcome of a [`ManageCommand`], carried on the wire inside a
/// [`BepMessage::ManageResponse`].
///
/// `Ok` carries a short human-readable summary of what the command did (for
/// example a status snapshot or an eviction count); `Err` carries a typed
/// error code plus a message. The `Unauthorised` code is reserved for an
/// authorisation failure — the managed node refusing a command the caller's
/// grants do not cover — so a manager can distinguish "you may not" from a
/// command that ran and failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManageResult {
    /// The command ran successfully. Carries a short result summary.
    Ok {
        /// Human-readable summary of the command's effect.
        summary: String,
    },
    /// The command did not run, or ran and failed.
    Err {
        /// The typed error kind.
        kind: ManageErrorKind,
        /// A human-readable message describing the failure.
        message: String,
    },
}

/// The kind of failure carried by [`ManageResult::Err`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManageErrorKind {
    /// The caller's grants do not authorise the requested command over the
    /// requested scope. The command was not run; the attempt is still audited.
    Unauthorised,
    /// The command was authorised but failed while running.
    Failed,
}

/// Wire discriminant for [`ManageErrorKind::Unauthorised`].
const MANAGE_ERR_UNAUTHORISED: u32 = 0;
/// Wire discriminant for [`ManageErrorKind::Failed`].
const MANAGE_ERR_FAILED: u32 = 1;

/// BEP message types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BepMessage {
    /// Exchange folder configuration on connect.
    ///
    /// `data_token` optionally carries a signed capability token (in its JSON
    /// form) the connecting side presents to authorise directional data-plane
    /// access — a `data:read` or `data:write` grant bound to this device for a
    /// folder scope. The serving side verifies it (signed by it or a chain
    /// rooting in it, unexpired, not revoked, bearer == the authenticated peer)
    /// and folds the carried grant into its data-plane access decision. `None`
    /// means no token is presented; the peer is then authorised solely by the
    /// on-node data grants, defaulting to full bidirectional access for a
    /// trusted peer with no data grant configured.
    ClusterConfig {
        /// The folders the sender participates in.
        folders: Vec<Folder>,
        /// An optional signed capability token authorising directional data
        /// access for the sync session, in its JSON form.
        data_token: Option<String>,
    },
    /// Announce files and blocks.
    Index {
        folder: String,
        files: Vec<FileInfo>,
    },
    /// Incremental update when files change.
    IndexUpdate {
        folder: String,
        files: Vec<FileInfo>,
    },
    /// Request a specific block from a peer.
    Request {
        /// Monotonic per-peer correlation id chosen by the requester.
        /// The peer must echo this id in its [`BepMessage::Response`] so
        /// the requester can route the payload to the right waiter,
        /// allowing many concurrent requests on one connection.
        request_id: u64,
        folder: String,
        name: String,
        block_offset: u64,
        block_size: u32,
        block_hash: [u8; 32],
    },
    /// Send block data.
    Response {
        /// Echoes the `request_id` of the [`BepMessage::Request`] this
        /// response satisfies.
        request_id: u64,
        data: Vec<u8>,
    },
    /// Keepalive.
    Ping,
    /// Graceful connection teardown.
    Close { reason: String },
    /// Snapshot of the broadcaster's known peers, sent periodically to
    /// every connected peer so the receiver can learn about devices it
    /// is not directly configured for. Receivers merge the peers into
    /// their local peer book; unresolved address entries are silently
    /// dropped.
    Gossip { peers: Vec<GossipPeer> },
    /// Advertise reachable addresses to a remote peer before any data
    /// exchange. The recipient pairs these against its own local
    /// candidates and selects the highest-scoring pair to probe (see
    /// `Candidate::pairing_score` in `cascade-p2p`'s `candidate`
    /// module).
    ///
    /// Sent during connection setup, after the TLS handshake but before
    /// the first `BepMessage::Index`. Frame size is bounded by
    /// `MAX_CANDIDATES_PER_FRAME` to cap allocation cost on receive.
    Candidates {
        /// Reachable addresses, in arbitrary order. The priority field
        /// on each candidate determines the local ordering — receivers
        /// must NOT trust the wire order.
        candidates: Vec<Candidate>,
    },
    /// Synchronisation message exchanged by both peers immediately
    /// before a hole-punch probe burst. Both ends issue the same
    /// `nonce` and `deadline_unix_ms`; the second peer to receive the
    /// frame echoes its partner's values, then both schedule their
    /// probe bursts for `deadline_unix_ms`.
    ///
    /// This is the "sync" step of libp2p `DCUtR` (§3.2 of the `DCUtR`
    /// spec): the round-trip lets each peer estimate `RTT/2` and time
    /// its probes so they arrive at the remote `NAT` at approximately
    /// the same instant.
    SyncPunch {
        /// Random `64`-bit value chosen by the sender. The remote
        /// echoes it back unchanged so each side can correlate
        /// concurrent punch attempts on the same connection.
        nonce: u64,
        /// Wall-clock target for the probe burst, in milliseconds
        /// since the Unix epoch. Senders pick a deadline far enough
        /// in the future to cover `RTT/2`; receivers ignore the frame
        /// if the deadline has already passed.
        deadline_unix_ms: u64,
    },
    /// Tell the peer the source `SocketAddr` this side observed for the
    /// connection — the peer-as-`STUN` mechanism that lets a host learn
    /// its own reflexive (`NAT`-mapped) address with zero `STUN` servers.
    ///
    /// Sent by each side immediately after the TLS handshake completes:
    /// the address carried is the *remote* peer's source as seen by the
    /// local socket, so the receiver reads it back as its own externally
    /// observed address. The receiver folds it into a
    /// [`CandidateKind::ServerReflexive`] candidate (see
    /// `cascade_p2p::nat::server_reflexive_candidate_from_addr`) exactly
    /// as it would a `STUN`-derived `XOR-MAPPED-ADDRESS`.
    ObservedAddress(SocketAddr),
    /// Advertise this device as a willing relay to a trusted peer.
    ///
    /// Sent by a node whose detected `NAT` type is `Open` or `FullCone`
    /// and whose `relay_volunteer` policy is not `Off`, to peers it shares
    /// a folder with. The recipient records the sender (identified by the
    /// device id of the connection the offer arrives on) as a peer-relay
    /// candidate; when [`crate::decide_connectivity`] later calls for a
    /// relay, a reachable peer relay is preferred over an operated one.
    ///
    /// The carried addresses are the volunteer's reachable BEP endpoints —
    /// where a third peer dials to open a relay session. Bounded by
    /// `MAX_RELAY_OFFER_ADDRESSES` to cap receiver allocation.
    RelayOffer {
        /// Reachable BEP endpoints at which the volunteer accepts relay
        /// connections, in arbitrary order.
        addresses: Vec<SocketAddr>,
    },
    /// Ask a volunteering relay to bridge this session to `target_device`.
    ///
    /// Sent by a peer that has selected this volunteer as its relay. The
    /// volunteer pairs the requesting session with the session it holds
    /// to `target_device` and bridges the two with the shared byte-pipe
    /// (see [`crate::pipe::shuttle`]), exactly as the operated relay does.
    RelayConnect {
        /// Device id of the peer the requester wants to reach through this
        /// relay.
        target_device: String,
    },
    /// One opaque relayed frame travelling through a peer relay.
    ///
    /// The volunteering relay forwards the payload verbatim between the two
    /// bridged sessions without inspecting it. Each payload is a complete
    /// inner BEP frame (length prefix plus body) produced by the tunnel
    /// transport on the requester or target side; the relay treats it as
    /// opaque bytes.
    RelayData {
        /// Opaque relayed bytes — one inner BEP frame.
        payload: Vec<u8>,
    },
    /// Notify the target of a peer relay that a requester wants to open a
    /// tunnelled session through this relay.
    ///
    /// Sent by a volunteering relay to the target named in a
    /// [`BepMessage::RelayConnect`], on the relay's existing session to that
    /// target, once the bridge has been admitted. It carries the requester's
    /// device id so the target can stand up an inner BEP session terminal
    /// keyed by that requester and decapsulate the subsequent
    /// [`BepMessage::RelayData`] frames into it, rather than forwarding them
    /// onward. Without this signal the target cannot distinguish a tunnel it
    /// terminates from one it should relay.
    RelayInbound {
        /// Device id of the peer that initiated the relayed connection.
        source_device: String,
    },
    /// Ask the peer (the managed node) to run an administrative command.
    ///
    /// Carried over the already-TLS-authenticated peer connection, so the
    /// caller's device ID is established by the transport before the command is
    /// read — the managed node uses that identity as the caller principal,
    /// resolves the caller's grants, and only runs the command if a grant
    /// authorises `command` over `scope`. The command dispatches into the same
    /// internal handlers the local CLI drives; a manager can never do more than
    /// the local daemon could do to itself.
    ManageRequest {
        /// Correlation id chosen by the manager. The managed node echoes it in
        /// the matching [`BepMessage::ManageResponse`] so the manager can route
        /// the outcome to the right waiter.
        request_id: u64,
        /// The command to run.
        command: ManageCommand,
        /// The scope the command targets. Authorisation checks the caller's
        /// grants cover this scope.
        scope: ManageScope,
        /// An optional signed capability token presented to authorise the
        /// command, as the token's JSON form.
        ///
        /// When present, the managed node verifies the token (signed by this
        /// node or a delegation chain rooting in it, unexpired, not revoked,
        /// bearer matching the authenticated connection) and authorises the
        /// command against the token-carried grant in addition to any on-node
        /// grant. This lets a device act on authority issued offline, without a
        /// live grant row on the node. Kept as the opaque JSON string so this
        /// protocol crate stays free of the engine's token domain type; the
        /// engine deserialises and verifies it at the dispatch boundary.
        token: Option<String>,
    },
    /// Reply to a [`BepMessage::ManageRequest`].
    ///
    /// Echoes the request id and carries the [`ManageResult`] outcome. An
    /// authorisation failure is reported as a typed
    /// [`ManageErrorKind::Unauthorised`] error rather than silently dropping the
    /// request, so the manager learns its grants were insufficient.
    ManageResponse {
        /// Echoes the `request_id` of the [`BepMessage::ManageRequest`] this
        /// response answers.
        request_id: u64,
        /// The command outcome.
        result: ManageResult,
    },
}

impl BepMessage {
    const fn msg_type(&self) -> u32 {
        match self {
            Self::ClusterConfig { .. } => MSG_CLUSTER_CONFIG,
            Self::Index { .. } => MSG_INDEX,
            Self::IndexUpdate { .. } => MSG_INDEX_UPDATE,
            Self::Request { .. } => MSG_REQUEST,
            Self::Response { .. } => MSG_RESPONSE,
            Self::Ping => MSG_PING,
            Self::Close { .. } => MSG_CLOSE,
            Self::Gossip { .. } => MSG_GOSSIP,
            Self::Candidates { .. } => MSG_CANDIDATES,
            Self::SyncPunch { .. } => MSG_SYNC_PUNCH,
            Self::ObservedAddress(_) => MSG_OBSERVED_ADDRESS,
            Self::RelayOffer { .. } => MSG_RELAY_OFFER,
            Self::RelayConnect { .. } => MSG_RELAY_CONNECT,
            Self::RelayData { .. } => MSG_RELAY_DATA,
            Self::RelayInbound { .. } => MSG_RELAY_INBOUND,
            Self::ManageRequest { .. } => MSG_MANAGE_REQUEST,
            Self::ManageResponse { .. } => MSG_MANAGE_RESPONSE,
        }
    }
}

// ── Encoding ──

/// Encode a BEP message into a length-prefixed XDR frame.
///
/// Wire format: `[4-byte length][4-byte msg type][XDR body...]`
pub fn encode_message(msg: &BepMessage) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    encode_u32(&mut body, msg.msg_type());

    match msg {
        BepMessage::ClusterConfig {
            folders,
            data_token,
        } => {
            encode_u32(
                &mut body,
                u32::try_from(folders.len()).map_err(|_| anyhow::anyhow!("too many folders"))?,
            );
            for folder in folders {
                encode_string(&mut body, &folder.id)?;
                encode_string(&mut body, &folder.label)?;
            }
            encode_opt_string(&mut body, data_token.as_deref())?;
        }
        BepMessage::Index { folder, files } | BepMessage::IndexUpdate { folder, files } => {
            encode_string(&mut body, folder)?;
            encode_u32(
                &mut body,
                u32::try_from(files.len()).map_err(|_| anyhow::anyhow!("too many files"))?,
            );
            encode_file_infos(&mut body, files)?;
        }
        BepMessage::Request {
            request_id,
            folder,
            name,
            block_offset,
            block_size,
            block_hash,
        } => {
            encode_u64(&mut body, *request_id);
            encode_string(&mut body, folder)?;
            encode_string(&mut body, name)?;
            encode_u64(&mut body, *block_offset);
            encode_u32(&mut body, *block_size);
            encode_opaque(&mut body, block_hash)?;
        }
        BepMessage::Response { request_id, data } => {
            encode_u64(&mut body, *request_id);
            encode_opaque(&mut body, data)?;
        }
        BepMessage::Ping => {}
        BepMessage::Close { reason } => {
            encode_string(&mut body, reason)?;
        }
        BepMessage::Gossip { peers } => {
            let peer_count =
                u32::try_from(peers.len()).map_err(|_| anyhow::anyhow!("too many gossip peers"))?;
            encode_u32(&mut body, peer_count);
            for peer in peers {
                encode_string(&mut body, &peer.device_id)?;
                let addr_count = u32::try_from(peer.addresses.len())
                    .map_err(|_| anyhow::anyhow!("too many addresses per gossip peer"))?;
                encode_u32(&mut body, addr_count);
                for addr in &peer.addresses {
                    encode_string(&mut body, addr)?;
                }
                encode_i64(&mut body, peer.snapshot_unix_seconds);
            }
        }
        BepMessage::Candidates { candidates } => {
            let count = u32::try_from(candidates.len())
                .map_err(|_| anyhow::anyhow!("too many candidates"))?;
            if count > MAX_CANDIDATES_PER_FRAME {
                anyhow::bail!("candidate count {count} exceeds maximum {MAX_CANDIDATES_PER_FRAME}");
            }
            encode_u32(&mut body, count);
            for candidate in candidates {
                encode_candidate(&mut body, candidate)?;
            }
        }
        BepMessage::SyncPunch {
            nonce,
            deadline_unix_ms,
        } => {
            encode_u64(&mut body, *nonce);
            encode_u64(&mut body, *deadline_unix_ms);
        }
        BepMessage::ObservedAddress(addr) => {
            encode_socket_addr(&mut body, *addr)?;
        }
        BepMessage::RelayOffer { addresses } => {
            let count = u32::try_from(addresses.len())
                .map_err(|_| anyhow::anyhow!("too many relay offer addresses"))?;
            if count > MAX_RELAY_OFFER_ADDRESSES {
                anyhow::bail!(
                    "relay offer address count {count} exceeds maximum {MAX_RELAY_OFFER_ADDRESSES}"
                );
            }
            encode_u32(&mut body, count);
            for addr in addresses {
                encode_socket_addr(&mut body, *addr)?;
            }
        }
        BepMessage::RelayConnect { target_device } => {
            encode_string(&mut body, target_device)?;
        }
        BepMessage::RelayData { payload } => {
            encode_opaque(&mut body, payload)?;
        }
        BepMessage::RelayInbound { source_device } => {
            encode_string(&mut body, source_device)?;
        }
        BepMessage::ManageRequest {
            request_id,
            command,
            scope,
            token,
        } => {
            encode_u64(&mut body, *request_id);
            encode_manage_command(&mut body, command)?;
            encode_manage_scope(&mut body, scope)?;
            encode_opt_string(&mut body, token.as_deref())?;
        }
        BepMessage::ManageResponse { request_id, result } => {
            encode_u64(&mut body, *request_id);
            encode_manage_result(&mut body, result)?;
        }
    }

    let body_len = u32::try_from(body.len())
        .map_err(|_| anyhow::anyhow!("frame body too large for u32 length prefix"))?;
    let mut frame = Vec::with_capacity(4 + body.len());
    encode_u32(&mut frame, body_len);
    frame.extend_from_slice(&body);
    Ok(frame)
}

fn encode_file_infos(buf: &mut Vec<u8>, files: &[FileInfo]) -> Result<()> {
    for fi in files {
        encode_string(buf, &fi.name)?;
        encode_u32(buf, fi.file_type);
        encode_u64(buf, fi.size);
        encode_i64(buf, fi.modified);
        encode_u64(buf, fi.sequence);
        encode_u32(buf, fi.block_size);
        encode_u32(buf, u32::from(fi.deleted));
        encode_u32(buf, u32::from(fi.invalid));
        encode_u32(buf, u32::from(fi.no_permissions));
        encode_version(buf, &fi.version)?;
        encode_u32(
            buf,
            u32::try_from(fi.block_hashes.len())
                .map_err(|_| anyhow::anyhow!("too many block hashes"))?,
        );
        for hash in &fi.block_hashes {
            encode_opaque(buf, hash)?;
        }
    }
    Ok(())
}

fn encode_socket_addr(buf: &mut Vec<u8>, addr: SocketAddr) -> Result<()> {
    // Wire layout:
    //   [4 bytes family tag][4-byte port][opaque address bytes]
    // The address bytes are length-prefixed via `encode_opaque`, so
    // IPv4 and IPv6 are distinguished by both the family tag and the
    // opaque-length prefix — defence in depth against truncated frames.
    match addr.ip() {
        IpAddr::V4(v4) => {
            encode_u32(buf, u32::from(ADDR_FAMILY_IPV4));
            encode_u32(buf, u32::from(addr.port()));
            encode_opaque(buf, &v4.octets())?;
        }
        IpAddr::V6(v6) => {
            encode_u32(buf, u32::from(ADDR_FAMILY_IPV6));
            encode_u32(buf, u32::from(addr.port()));
            encode_opaque(buf, &v6.octets())?;
        }
    }
    Ok(())
}

fn decode_socket_addr(data: &[u8]) -> Result<(SocketAddr, &[u8])> {
    let (family_u32, rest) = decode_u32(data)?;
    let family = u8::try_from(family_u32).map_err(|_| anyhow::anyhow!("invalid address family"))?;
    let (port_u32, rest) = decode_u32(rest)?;
    let port = u16::try_from(port_u32).map_err(|_| anyhow::anyhow!("port out of range"))?;
    let (octets, rest) = decode_opaque(rest)?;
    match family {
        ADDR_FAMILY_IPV4 => {
            let bytes: [u8; 4] = octets
                .try_into()
                .map_err(|_| anyhow::anyhow!("IPv4 address must be 4 bytes"))?;
            Ok((
                SocketAddr::new(IpAddr::V4(Ipv4Addr::from(bytes)), port),
                rest,
            ))
        }
        ADDR_FAMILY_IPV6 => {
            let bytes: [u8; 16] = octets
                .try_into()
                .map_err(|_| anyhow::anyhow!("IPv6 address must be 16 bytes"))?;
            Ok((
                SocketAddr::new(IpAddr::V6(Ipv6Addr::from(bytes)), port),
                rest,
            ))
        }
        other => anyhow::bail!("unknown address family {other}"),
    }
}

fn encode_candidate(buf: &mut Vec<u8>, candidate: &Candidate) -> Result<()> {
    // Wire layout:
    //   [4 bytes kind tag][4 bytes priority][socket address]
    encode_u32(buf, u32::from(candidate.kind.wire_tag()));
    encode_u32(buf, candidate.priority);
    encode_socket_addr(buf, candidate.address)
}

fn decode_candidate(data: &[u8]) -> Result<(Candidate, &[u8])> {
    let (kind_u32, rest) = decode_u32(data)?;
    let kind_tag =
        u8::try_from(kind_u32).map_err(|_| anyhow::anyhow!("invalid candidate kind tag"))?;
    let kind = CandidateKind::from_wire_tag(kind_tag)
        .ok_or_else(|| anyhow::anyhow!("unknown candidate kind {kind_tag}"))?;
    let (priority, rest) = decode_u32(rest)?;
    let (address, rest) = decode_socket_addr(rest)?;
    Ok((
        Candidate {
            address,
            kind,
            priority,
        },
        rest,
    ))
}

fn encode_version(buf: &mut Vec<u8>, version: &Version) -> Result<()> {
    encode_u32(
        buf,
        u32::try_from(version.counters.len())
            .map_err(|_| anyhow::anyhow!("version vector too long"))?,
    );
    for (id, ctr) in &version.counters {
        encode_u64(buf, *id);
        encode_u64(buf, *ctr);
    }
    Ok(())
}

fn encode_manage_scope(buf: &mut Vec<u8>, scope: &ManageScope) -> Result<()> {
    match scope {
        ManageScope::Node => encode_u32(buf, MANAGE_SCOPE_NODE),
        ManageScope::Folder { path } => {
            encode_u32(buf, MANAGE_SCOPE_FOLDER);
            encode_string(buf, path)?;
        }
    }
    Ok(())
}

fn encode_manage_command(buf: &mut Vec<u8>, command: &ManageCommand) -> Result<()> {
    match command {
        ManageCommand::StatusRead => encode_u32(buf, MANAGE_CMD_STATUS_READ),
        ManageCommand::Pin {
            path_glob,
            recursive,
        } => {
            encode_u32(buf, MANAGE_CMD_PIN);
            encode_string(buf, path_glob)?;
            encode_u32(buf, u32::from(*recursive));
        }
        ManageCommand::Unpin { path_glob } => {
            encode_u32(buf, MANAGE_CMD_UNPIN);
            encode_string(buf, path_glob)?;
        }
        ManageCommand::CacheEvict => encode_u32(buf, MANAGE_CMD_CACHE_EVICT),
        ManageCommand::CacheWarm { path_glob } => {
            encode_u32(buf, MANAGE_CMD_CACHE_WARM);
            encode_string(buf, path_glob)?;
        }
        ManageCommand::ConfigPush {
            format,
            folder,
            body,
        } => {
            encode_u32(buf, MANAGE_CMD_CONFIG_PUSH);
            encode_u32(buf, manage_config_format_tag(*format));
            encode_string(buf, folder)?;
            encode_string(buf, body)?;
        }
        ManageCommand::PolicySet {
            path_glob,
            max_age_secs,
            max_file_size,
            priority,
        } => {
            encode_u32(buf, MANAGE_CMD_POLICY_SET);
            encode_string(buf, path_glob)?;
            encode_opt_i64(buf, *max_age_secs);
            encode_opt_i64(buf, *max_file_size);
            encode_i32(buf, *priority);
        }
        ManageCommand::BackendAdd {
            name,
            backend_type,
            mount_path,
            config_toml,
        } => {
            encode_u32(buf, MANAGE_CMD_BACKEND_ADD);
            encode_string(buf, name)?;
            encode_string(buf, backend_type)?;
            encode_string(buf, mount_path)?;
            encode_string(buf, config_toml)?;
        }
        ManageCommand::BackendRemove { name, mount_path } => {
            encode_u32(buf, MANAGE_CMD_BACKEND_REMOVE);
            encode_string(buf, name)?;
            encode_string(buf, mount_path)?;
        }
        ManageCommand::Restart => encode_u32(buf, MANAGE_CMD_RESTART),
        ManageCommand::Stop => encode_u32(buf, MANAGE_CMD_STOP),
        ManageCommand::GrantAdd { grant } => {
            encode_u32(buf, MANAGE_CMD_GRANT_ADD);
            encode_manage_grant(buf, grant)?;
        }
        ManageCommand::GrantRevoke { grant_id, scope } => {
            encode_u32(buf, MANAGE_CMD_GRANT_REVOKE);
            encode_i64(buf, *grant_id);
            encode_manage_scope(buf, scope)?;
        }
    }
    Ok(())
}

/// The wire discriminant for a [`ManageConfigFormat`].
const fn manage_config_format_tag(format: ManageConfigFormat) -> u32 {
    match format {
        ManageConfigFormat::Gitignore => MANAGE_CONFIG_FORMAT_GITIGNORE,
        ManageConfigFormat::Toml => MANAGE_CONFIG_FORMAT_TOML,
        ManageConfigFormat::Yaml => MANAGE_CONFIG_FORMAT_YAML,
        ManageConfigFormat::Json => MANAGE_CONFIG_FORMAT_JSON,
    }
}

fn encode_i32(buf: &mut Vec<u8>, val: i32) {
    buf.extend_from_slice(&val.to_be_bytes());
}

/// Encode an `Option<i64>` as a one-word presence sentinel followed, when
/// present, by the value. Keeps an absent dimension distinct from a zero value.
fn encode_opt_i64(buf: &mut Vec<u8>, val: Option<i64>) {
    match val {
        None => encode_u32(buf, OPTION_NONE),
        Some(v) => {
            encode_u32(buf, OPTION_SOME);
            encode_i64(buf, v);
        }
    }
}

/// Encode an `Option<&str>` as a one-word presence sentinel followed, when
/// present, by the string.
fn encode_opt_string(buf: &mut Vec<u8>, val: Option<&str>) -> Result<()> {
    match val {
        None => encode_u32(buf, OPTION_NONE),
        Some(s) => {
            encode_u32(buf, OPTION_SOME);
            encode_string(buf, s)?;
        }
    }
    Ok(())
}

fn encode_manage_grant(buf: &mut Vec<u8>, grant: &ManageGrant) -> Result<()> {
    encode_string(buf, &grant.grantee)?;
    encode_string(buf, &grant.capability)?;
    encode_manage_scope(buf, &grant.scope)?;
    encode_opt_string(buf, grant.expires.as_deref())?;
    Ok(())
}

fn encode_manage_result(buf: &mut Vec<u8>, result: &ManageResult) -> Result<()> {
    match result {
        ManageResult::Ok { summary } => {
            encode_u32(buf, MANAGE_RESULT_OK);
            encode_string(buf, summary)?;
        }
        ManageResult::Err { kind, message } => {
            encode_u32(buf, MANAGE_RESULT_ERR);
            let kind_tag = match kind {
                ManageErrorKind::Unauthorised => MANAGE_ERR_UNAUTHORISED,
                ManageErrorKind::Failed => MANAGE_ERR_FAILED,
            };
            encode_u32(buf, kind_tag);
            encode_string(buf, message)?;
        }
    }
    Ok(())
}

// ── Decoding ──

/// Decode a BEP message from a length-prefixed XDR frame.
///
/// Expects the full frame including the 4-byte length prefix.
pub fn decode_message(frame: &[u8]) -> Result<BepMessage> {
    let (body_len_u32, body) =
        decode_u32(frame).map_err(|e| anyhow::anyhow!("invalid frame length: {e}"))?;
    let body_len = usize::try_from(body_len_u32)
        .map_err(|_| anyhow::anyhow!("frame length too large for this platform"))?;
    if body.len() < body_len {
        anyhow::bail!(
            "frame body truncated: expected {body_len} bytes, got {}",
            body.len()
        );
    }
    let body = body
        .get(..body_len)
        .ok_or_else(|| anyhow::anyhow!("frame body slice out of bounds"))?;

    let (msg_type, rest) =
        decode_u32(body).map_err(|e| anyhow::anyhow!("invalid message type: {e}"))?;

    match msg_type {
        MSG_CLUSTER_CONFIG => decode_cluster_config(rest),
        MSG_INDEX => decode_index(rest),
        MSG_INDEX_UPDATE => decode_index_update(rest),
        MSG_REQUEST => decode_request(rest),
        MSG_RESPONSE => decode_response(rest),
        MSG_PING => Ok(BepMessage::Ping),
        MSG_CLOSE => decode_close(rest),
        MSG_GOSSIP => decode_gossip(rest),
        MSG_CANDIDATES => decode_candidates(rest),
        MSG_SYNC_PUNCH => decode_sync_punch(rest),
        MSG_OBSERVED_ADDRESS => decode_observed_address(rest),
        MSG_RELAY_OFFER => decode_relay_offer(rest),
        MSG_RELAY_CONNECT => decode_relay_connect(rest),
        MSG_RELAY_DATA => decode_relay_data(rest),
        MSG_RELAY_INBOUND => decode_relay_inbound(rest),
        MSG_MANAGE_REQUEST => decode_manage_request(rest),
        MSG_MANAGE_RESPONSE => decode_manage_response(rest),
        _ => anyhow::bail!("unknown message type: {msg_type}"),
    }
}

fn decode_candidates(data: &[u8]) -> Result<BepMessage> {
    let (count, mut rest) = decode_u32(data)?;
    if count > MAX_CANDIDATES_PER_FRAME {
        anyhow::bail!("candidate count {count} exceeds maximum {MAX_CANDIDATES_PER_FRAME}");
    }
    let mut candidates = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let (candidate, next) = decode_candidate(rest)?;
        candidates.push(candidate);
        rest = next;
    }
    Ok(BepMessage::Candidates { candidates })
}

fn decode_sync_punch(data: &[u8]) -> Result<BepMessage> {
    let (nonce, rest) = decode_u64(data)?;
    let (deadline_unix_ms, _) = decode_u64(rest)?;
    Ok(BepMessage::SyncPunch {
        nonce,
        deadline_unix_ms,
    })
}

fn decode_observed_address(data: &[u8]) -> Result<BepMessage> {
    let (addr, _) = decode_socket_addr(data)?;
    Ok(BepMessage::ObservedAddress(addr))
}

fn decode_relay_offer(data: &[u8]) -> Result<BepMessage> {
    let (count, mut rest) = decode_u32(data)?;
    if count > MAX_RELAY_OFFER_ADDRESSES {
        anyhow::bail!(
            "relay offer address count {count} exceeds maximum {MAX_RELAY_OFFER_ADDRESSES}"
        );
    }
    let mut addresses = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let (addr, next) = decode_socket_addr(rest)?;
        addresses.push(addr);
        rest = next;
    }
    Ok(BepMessage::RelayOffer { addresses })
}

fn decode_relay_connect(data: &[u8]) -> Result<BepMessage> {
    let (target_device, _) = decode_string(data)?;
    Ok(BepMessage::RelayConnect { target_device })
}

fn decode_relay_data(data: &[u8]) -> Result<BepMessage> {
    let (payload, _) = decode_opaque(data)?;
    Ok(BepMessage::RelayData {
        payload: payload.to_vec(),
    })
}

fn decode_relay_inbound(data: &[u8]) -> Result<BepMessage> {
    let (source_device, _) = decode_string(data)?;
    Ok(BepMessage::RelayInbound { source_device })
}

fn decode_manage_scope(data: &[u8]) -> Result<(ManageScope, &[u8])> {
    let (tag, rest) = decode_u32(data)?;
    match tag {
        MANAGE_SCOPE_NODE => Ok((ManageScope::Node, rest)),
        MANAGE_SCOPE_FOLDER => {
            let (path, rest) = decode_string(rest)?;
            Ok((ManageScope::Folder { path }, rest))
        }
        other => anyhow::bail!("unknown manage scope tag {other}"),
    }
}

fn decode_manage_command(data: &[u8]) -> Result<(ManageCommand, &[u8])> {
    let (tag, rest) = decode_u32(data)?;
    match tag {
        MANAGE_CMD_STATUS_READ => Ok((ManageCommand::StatusRead, rest)),
        MANAGE_CMD_PIN => {
            let (path_glob, rest) = decode_string(rest)?;
            let (recursive_flag, rest) = decode_u32(rest)?;
            Ok((
                ManageCommand::Pin {
                    path_glob,
                    recursive: recursive_flag != 0,
                },
                rest,
            ))
        }
        MANAGE_CMD_UNPIN => {
            let (path_glob, rest) = decode_string(rest)?;
            Ok((ManageCommand::Unpin { path_glob }, rest))
        }
        MANAGE_CMD_CACHE_EVICT => Ok((ManageCommand::CacheEvict, rest)),
        MANAGE_CMD_CACHE_WARM => {
            let (path_glob, rest) = decode_string(rest)?;
            Ok((ManageCommand::CacheWarm { path_glob }, rest))
        }
        MANAGE_CMD_CONFIG_PUSH => {
            let (format_tag, rest) = decode_u32(rest)?;
            let format = manage_config_format_from_tag(format_tag)?;
            let (folder, rest) = decode_string(rest)?;
            let (body, rest) = decode_string(rest)?;
            Ok((
                ManageCommand::ConfigPush {
                    format,
                    folder,
                    body,
                },
                rest,
            ))
        }
        MANAGE_CMD_POLICY_SET => {
            let (path_glob, rest) = decode_string(rest)?;
            let (max_age_secs, rest) = decode_opt_i64(rest)?;
            let (max_file_size, rest) = decode_opt_i64(rest)?;
            let (priority, rest) = decode_i32(rest)?;
            Ok((
                ManageCommand::PolicySet {
                    path_glob,
                    max_age_secs,
                    max_file_size,
                    priority,
                },
                rest,
            ))
        }
        MANAGE_CMD_BACKEND_ADD => {
            let (name, rest) = decode_string(rest)?;
            let (backend_type, rest) = decode_string(rest)?;
            let (mount_path, rest) = decode_string(rest)?;
            let (config_toml, rest) = decode_string(rest)?;
            Ok((
                ManageCommand::BackendAdd {
                    name,
                    backend_type,
                    mount_path,
                    config_toml,
                },
                rest,
            ))
        }
        MANAGE_CMD_BACKEND_REMOVE => {
            let (name, rest) = decode_string(rest)?;
            let (mount_path, rest) = decode_string(rest)?;
            Ok((ManageCommand::BackendRemove { name, mount_path }, rest))
        }
        MANAGE_CMD_RESTART => Ok((ManageCommand::Restart, rest)),
        MANAGE_CMD_STOP => Ok((ManageCommand::Stop, rest)),
        MANAGE_CMD_GRANT_ADD => {
            let (grant, rest) = decode_manage_grant(rest)?;
            Ok((ManageCommand::GrantAdd { grant }, rest))
        }
        MANAGE_CMD_GRANT_REVOKE => {
            let (grant_id, rest) = decode_i64(rest)?;
            let (scope, rest) = decode_manage_scope(rest)?;
            Ok((ManageCommand::GrantRevoke { grant_id, scope }, rest))
        }
        other => anyhow::bail!("unknown manage command tag {other}"),
    }
}

/// Parse a [`ManageConfigFormat`] from its wire discriminant.
fn manage_config_format_from_tag(tag: u32) -> Result<ManageConfigFormat> {
    match tag {
        MANAGE_CONFIG_FORMAT_GITIGNORE => Ok(ManageConfigFormat::Gitignore),
        MANAGE_CONFIG_FORMAT_TOML => Ok(ManageConfigFormat::Toml),
        MANAGE_CONFIG_FORMAT_YAML => Ok(ManageConfigFormat::Yaml),
        MANAGE_CONFIG_FORMAT_JSON => Ok(ManageConfigFormat::Json),
        other => anyhow::bail!("unknown manage config format tag {other}"),
    }
}

fn decode_i32(data: &[u8]) -> Result<(i32, &[u8])> {
    let (bytes, rest) = data
        .split_first_chunk::<4>()
        .ok_or_else(|| anyhow::anyhow!("need 4 bytes for int32"))?;
    Ok((i32::from_be_bytes(*bytes), rest))
}

/// Decode an `Option<i64>` written by [`encode_opt_i64`].
fn decode_opt_i64(data: &[u8]) -> Result<(Option<i64>, &[u8])> {
    let (tag, rest) = decode_u32(data)?;
    match tag {
        OPTION_NONE => Ok((None, rest)),
        OPTION_SOME => {
            let (val, rest) = decode_i64(rest)?;
            Ok((Some(val), rest))
        }
        other => anyhow::bail!("invalid option sentinel {other}"),
    }
}

/// Decode an `Option<String>` written by [`encode_opt_string`].
fn decode_opt_string(data: &[u8]) -> Result<(Option<String>, &[u8])> {
    let (tag, rest) = decode_u32(data)?;
    match tag {
        OPTION_NONE => Ok((None, rest)),
        OPTION_SOME => {
            let (val, rest) = decode_string(rest)?;
            Ok((Some(val), rest))
        }
        other => anyhow::bail!("invalid option sentinel {other}"),
    }
}

fn decode_manage_grant(data: &[u8]) -> Result<(ManageGrant, &[u8])> {
    let (grantee, rest) = decode_string(data)?;
    let (capability, rest) = decode_string(rest)?;
    let (scope, rest) = decode_manage_scope(rest)?;
    let (expires, rest) = decode_opt_string(rest)?;
    Ok((
        ManageGrant {
            grantee,
            capability,
            scope,
            expires,
        },
        rest,
    ))
}

fn decode_manage_result(data: &[u8]) -> Result<(ManageResult, &[u8])> {
    let (tag, rest) = decode_u32(data)?;
    match tag {
        MANAGE_RESULT_OK => {
            let (summary, rest) = decode_string(rest)?;
            Ok((ManageResult::Ok { summary }, rest))
        }
        MANAGE_RESULT_ERR => {
            let (kind_tag, rest) = decode_u32(rest)?;
            let kind = match kind_tag {
                MANAGE_ERR_UNAUTHORISED => ManageErrorKind::Unauthorised,
                MANAGE_ERR_FAILED => ManageErrorKind::Failed,
                other => anyhow::bail!("unknown manage error kind tag {other}"),
            };
            let (message, rest) = decode_string(rest)?;
            Ok((ManageResult::Err { kind, message }, rest))
        }
        other => anyhow::bail!("unknown manage result tag {other}"),
    }
}

fn decode_manage_request(data: &[u8]) -> Result<BepMessage> {
    let (request_id, rest) = decode_u64(data)?;
    let (command, rest) = decode_manage_command(rest)?;
    let (scope, rest) = decode_manage_scope(rest)?;
    let (token, _) = decode_opt_string(rest)?;
    Ok(BepMessage::ManageRequest {
        request_id,
        command,
        scope,
        token,
    })
}

fn decode_manage_response(data: &[u8]) -> Result<BepMessage> {
    let (request_id, rest) = decode_u64(data)?;
    let (result, _) = decode_manage_result(rest)?;
    Ok(BepMessage::ManageResponse { request_id, result })
}

fn decode_cluster_config(data: &[u8]) -> Result<BepMessage> {
    let (count, mut data) = decode_u32(data)?;
    let mut folders = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let (id, rest) = decode_string(data)?;
        let (label, rest) = decode_string(rest)?;
        folders.push(Folder { id, label });
        data = rest;
    }
    let (data_token, _) = decode_opt_string(data)?;
    Ok(BepMessage::ClusterConfig {
        folders,
        data_token,
    })
}

fn decode_index(data: &[u8]) -> Result<BepMessage> {
    let (folder, rest) = decode_string(data)?;
    let (files, _) = decode_file_infos(rest)?;
    Ok(BepMessage::Index { folder, files })
}

fn decode_index_update(data: &[u8]) -> Result<BepMessage> {
    let (folder, rest) = decode_string(data)?;
    let (files, _) = decode_file_infos(rest)?;
    Ok(BepMessage::IndexUpdate { folder, files })
}

fn decode_request(data: &[u8]) -> Result<BepMessage> {
    let (request_id, data) = decode_u64(data)?;
    let (folder, data) = decode_string(data)?;
    let (name, data) = decode_string(data)?;
    let (block_offset, data) = decode_u64(data)?;
    let (block_size, data) = decode_u32(data)?;
    let (hash_bytes, _) = decode_opaque(data)?;
    if hash_bytes.len() != 32 {
        anyhow::bail!("block hash must be 32 bytes, got {}", hash_bytes.len());
    }
    let mut block_hash = [0u8; 32];
    block_hash.copy_from_slice(hash_bytes);
    Ok(BepMessage::Request {
        request_id,
        folder,
        name,
        block_offset,
        block_size,
        block_hash,
    })
}

fn decode_response(data: &[u8]) -> Result<BepMessage> {
    let (request_id, data) = decode_u64(data)?;
    let (raw, _) = decode_opaque(data)?;
    Ok(BepMessage::Response {
        request_id,
        data: raw.to_vec(),
    })
}

fn decode_close(data: &[u8]) -> Result<BepMessage> {
    let (reason, _) = decode_string(data)?;
    Ok(BepMessage::Close { reason })
}

fn decode_gossip(data: &[u8]) -> Result<BepMessage> {
    let (count, mut rest) = decode_u32(data)?;
    if count > MAX_GOSSIP_PEERS {
        anyhow::bail!("gossip peer count {count} exceeds maximum {MAX_GOSSIP_PEERS}");
    }
    let mut peers = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let (device_id, after_id) = decode_string(rest)?;
        let (addr_count, after_addr_count) = decode_u32(after_id)?;
        if addr_count > MAX_GOSSIP_ADDRESSES_PER_PEER {
            anyhow::bail!(
                "gossip peer `{device_id}` has {addr_count} addresses, exceeding maximum {MAX_GOSSIP_ADDRESSES_PER_PEER}",
            );
        }
        let mut addresses = Vec::with_capacity(addr_count as usize);
        let mut cursor = after_addr_count;
        for _ in 0..addr_count {
            let (addr, next) = decode_string(cursor)?;
            addresses.push(addr);
            cursor = next;
        }
        let (snapshot_unix_seconds, after_last_seen) = decode_i64(cursor)?;
        peers.push(GossipPeer {
            device_id,
            addresses,
            snapshot_unix_seconds,
        });
        rest = after_last_seen;
    }
    Ok(BepMessage::Gossip { peers })
}

fn decode_file_infos(data: &[u8]) -> Result<(Vec<FileInfo>, &[u8])> {
    let (count, mut data) = decode_u32(data)?;
    let mut files = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let (name, rest) = decode_string(data)?;
        let (file_type, rest) = decode_u32(rest)?;
        let (size, rest) = decode_u64(rest)?;
        let (modified, rest) = decode_i64(rest)?;
        let (sequence, rest) = decode_u64(rest)?;
        let (block_size, rest) = decode_u32(rest)?;
        let (deleted_flag, rest) = decode_u32(rest)?;
        let (invalid_flag, rest) = decode_u32(rest)?;
        let (no_permissions_flag, rest) = decode_u32(rest)?;
        let (version, rest) = decode_version(rest)?;
        let (hash_count, mut rest) = decode_u32(rest)?;
        let mut block_hashes = Vec::with_capacity(hash_count as usize);
        for _ in 0..hash_count {
            let (hash_bytes, remaining) = decode_opaque(rest)?;
            if hash_bytes.len() != 32 {
                anyhow::bail!("block hash must be 32 bytes, got {}", hash_bytes.len());
            }
            let mut hash = [0u8; 32];
            hash.copy_from_slice(hash_bytes);
            block_hashes.push(hash);
            rest = remaining;
        }
        files.push(FileInfo {
            name,
            file_type,
            size,
            modified,
            sequence,
            block_size,
            deleted: deleted_flag != 0,
            invalid: invalid_flag != 0,
            no_permissions: no_permissions_flag != 0,
            version,
            block_hashes,
        });
        data = rest;
    }
    Ok((files, data))
}

fn decode_version(data: &[u8]) -> Result<(Version, &[u8])> {
    let (count, mut rest) = decode_u32(data)?;
    let mut counters = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let (id, after_id) = decode_u64(rest)?;
        let (ctr, after_ctr) = decode_u64(after_id)?;
        counters.push((id, ctr));
        rest = after_ctr;
    }
    Ok((Version { counters }, rest))
}

#[cfg(test)]
#[path = "protocol_tests.rs"]
mod tests;
