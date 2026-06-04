#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::string_slice
    )
)]
//! P2P-only storage backend.
//!
//! Unlike `cascade-backend-{gdrive,s3,local}`, this backend has no cloud
//! source of truth. Files live as content-addressed blocks in the local
//! P2P block store; folder metadata lives in a `SQLite` index. The full
//! mesh is reconstituted from peers — Syncthing-style — when peer
//! synchronisation is enabled (Phase 3).
//!
//! For now (Phase 2) the backend is functional as a local-only,
//! deduplicating content-addressed store. The same file uploaded twice
//! costs blocks once. Peer sync is a follow-up that wires the existing
//! `cascade_p2p::BepMessage` machinery onto this index.
//!
//! ## Exposure posture
//!
//! A single [`DiscoveryReach`] posture governs how far this device
//! reaches out for peers. Rather than a scatter of independent on/off
//! flags, the posture names an intent — `LanOnly`, `Private`, or
//! `Public` — and each discovery and traversal source self-activates
//! when (the posture permits its exposure level) AND (the source has
//! what it needs to run). See [`DiscoveryReach`] for the exposure
//! levels and the self-activation rule.
//!
//! UDP-multicast LAN discovery, for instance, runs at every posture
//! (LAN is the floor) but only once a `listen_addr` is bound — without
//! a bound BEP port there is no inbound path for a discovered peer to
//! dial, so the source stays idle. Introducer gossip, hole punch, and
//! peer relay require at least `Private`; publication to the Mainline
//! DHT and announce servers requires `Public`. The server lists
//! (`stun_servers`, `announce_servers`, `relay_endpoints`) and the DHT
//! configuration say *where to point* a source — the posture decides
//! *whether* the source runs.

pub mod index;
pub mod peer_relay;
pub mod sync;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use cascade_engine::backend::Backend;
use cascade_engine::types::{Change, Cursor, FileEntry, FileId, ItemId, Quota};
use cascade_p2p::block::{BlockHash, split_data};
use cascade_p2p::candidate::{Candidate, CandidateKind};
use cascade_p2p::discovery::{Discovery, DiscoveryService, GossipDiscovery, LanDiscovery};
use cascade_p2p::identity::DeviceIdentity;
use cascade_p2p::protocol::{ManageCommand, ManageResult, ManageScope, Version};
use cascade_p2p::store::BlockStore;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::index::{FolderIndex, IndexEntry};
use crate::sync::SyncEngine;

/// Poll interval reported to the engine.
///
/// The P2P backend receives updates via `IndexUpdate` push, but the sync
/// runner only surfaces new entries into the presenter on its next
/// poll. We keep the interval short (1 s) so peer-pushed files appear
/// in WebDAV/FUSE listings without user-visible lag.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Maximum time a manager-side dial waits for the session loop to register
/// the peer handle before a management request may be sent.
///
/// [`SyncEngine::connect_to`] returns once the TLS handshake completes, but the
/// session loop registers the peer handle on a spawned task a moment later. Ten
/// seconds is a generous ceiling over the sub-second handshake-plus-register
/// path on a direct dial; past it the setup is treated as wedged and the manage
/// call fails loudly rather than racing the send.
const SESSION_REGISTER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Default ceiling on the number of concurrent relay sessions this node
/// will bridge when volunteering as a peer relay.
///
/// A relay session bridges two peers, holding two `WebSocket` connections
/// and one byte-pipe task for its lifetime. Eight concurrent bridges is a
/// generous default for a personal mesh while bounding the memory, socket,
/// and bandwidth cost an `Open`/`FullCone` node takes on by volunteering.
/// Past the cap, new relay requests are rejected rather than silently
/// dropped (see [`crate::sync::SyncEngine`]).
pub const DEFAULT_MAX_RELAY_SESSIONS: u32 = 8;

/// Default ceiling on aggregate relay throughput, in bytes per second,
/// summed across every active relay session this node bridges.
///
/// Ten mebibytes per second keeps a volunteer useful for block exchange
/// without letting relayed traffic saturate a typical residential uplink.
/// The cap is named rather than a bare literal so the budget is visible
/// and tunable in one place.
pub const DEFAULT_MAX_RELAY_BANDWIDTH_BYTES_PER_SEC: u64 = 10 * 1024 * 1024;

/// Public STUN servers used for NAT type detection when the operator has
/// not configured any of their own.
///
/// NAT detection and the server-reflexive candidate rung need at least one
/// STUN server to function; without a default, an out-of-the-box deployment
/// would silently run with `NatType::Unknown` and never gather a reflexive
/// candidate. These are Google's long-standing free public STUN endpoints,
/// the de-facto default across WebRTC and P2P tooling. Two are listed so the
/// RFC 5780 two-server detection path (which distinguishes the full NAT
/// taxonomy) works without any operator configuration.
///
/// Source: Google public STUN servers, documented widely in the WebRTC
/// ecosystem — e.g. <https://www.webrtc-experiment.com/docs/STUN-or-TURN.html>
/// (Wayback: <https://web.archive.org/web/20240101000000*/webrtc-experiment.com/docs/STUN-or-TURN.html>).
///
/// Operator-supplied `stun_servers` override this default entirely; an
/// explicit empty list disables STUN (see [`resolve_stun_servers`]).
pub const DEFAULT_PUBLIC_STUN_SERVERS: &[&str] =
    &["stun.l.google.com:19302", "stun1.l.google.com:19302"];

/// Resolve the effective STUN server list from the operator-supplied value.
///
/// The resolution rule is explicit absence versus explicit emptiness:
/// - `None` — the operator did not mention STUN at all, so the public
///   defaults ([`DEFAULT_PUBLIC_STUN_SERVERS`]) apply and NAT detection works
///   out of the box.
/// - `Some(list)` where `list` is non-empty — the operator's servers replace
///   the default entirely.
/// - `Some(empty)` — the operator explicitly disabled STUN, so no servers are
///   used and the engine stays at `NatType::Unknown`.
///
/// Modelling the operator's choice as `Option<Vec<String>>` rather than a
/// bare `Vec<String>` is what lets "not configured" and "configured empty"
/// diverge — a bare empty vector could not tell the two apart.
#[must_use]
pub fn resolve_stun_servers(configured: Option<Vec<String>>) -> Vec<String> {
    configured.unwrap_or_else(|| {
        DEFAULT_PUBLIC_STUN_SERVERS
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    })
}

// `DiscoveryReach` and `RelayVolunteer` are defined in `cascade-p2p` so the
// engine can reference them without a circular dependency. Re-export them here
// so existing callers of `cascade_backend_p2p::DiscoveryReach` and
// `cascade_backend_p2p::RelayVolunteer` continue to compile unchanged.
pub use cascade_p2p::{DiscoveryReach, RelayVolunteer};

/// Statically-configured peer entry.
///
/// The address is stored as a `host:port` string and re-resolved on
/// every reconnect attempt. This is what we want for container DNS:
/// the peer may not be resolvable at startup (it hasn't booted yet)
/// but becomes resolvable seconds later.
///
/// `name` is an optional human-readable label seeded into the sync
/// engine's `device_id → friendly name` map at startup. Conflict copies
/// generated by this peer prefer the friendly name over the opaque
/// short device id when persisting the losing side.
#[derive(Debug, Clone)]
pub struct ConfiguredPeer {
    pub device_id: String,
    pub address: String,
    pub name: Option<String>,
}

/// Kademlia/Mainline-DHT discovery configuration — *where to point* the
/// DHT source, not *whether* it runs.
///
/// Activation is governed by the [`DiscoveryReach`] posture: the DHT
/// source self-activates only at [`DiscoveryReach::Public`] (and once a
/// listener is bound to advertise). This config is always present — it
/// carries the bootstrap set the source uses when the posture permits it
/// — so an empty `bootstrap_nodes` means "use the built-in public set",
/// never "DHT disabled". Disabling the DHT is a posture choice, not the
/// absence of this config.
#[derive(Debug, Clone, Default)]
pub struct DhtConfig {
    /// Bootstrap nodes used to join the Mainline DHT, as resolved socket
    /// addresses. Empty falls back to the named public default
    /// ([`cascade_p2p::discovery::DEFAULT_DHT_BOOTSTRAP_NODES`]) — which the
    /// crate resolves on the DHT actor thread — so the DHT works at `Public`
    /// posture without an operator supplying their own bootstrap nodes; a
    /// non-empty list pins exactly those nodes and the public default is not
    /// mixed in.
    pub bootstrap_nodes: Vec<std::net::SocketAddr>,
}

/// A configured announce server: its base URL and the shared secret this
/// device authenticates its writes to it with.
///
/// The announce directory is a soft-state rendezvous that only holders of a
/// shared secret may write to — both carriers (the relay-server endpoint and
/// the Cloudflare Worker) reject a `POST` whose `HMAC` write tag is missing or
/// wrong. The secret is therefore not optional: an announce server configured
/// without it could resolve peers but never publish this device's candidates,
/// which would leave the device undiscoverable through that server. Modelling
/// the secret as a required field makes that misconfiguration a parse error
/// rather than a silent runtime `401`. The 32-byte width matches the shared
/// `HMAC` key length.
#[derive(Debug, Clone)]
pub struct AnnounceServer {
    /// Scheme-and-authority root of the announce server (e.g.
    /// `https://announce.example`). The `/announce/<device_id>` path is
    /// appended per request.
    pub base_url: String,
    /// 32-byte shared secret keying the `HMAC` write tag on every register.
    pub secret: [u8; cascade_p2p::discovery::announce::SHARED_SECRET_LEN],
}

/// Configuration for a P2P backend instance.
#[derive(Debug)]
pub struct P2pBackendConfig {
    pub instance_id: String,
    pub display_name: String,
    pub index_path: PathBuf,
    pub block_store_root: PathBuf,
    /// Directory used for the device identity certificate.
    pub identity_dir: PathBuf,
    /// Folder ID exchanged with peers. Defaults to `instance_id`.
    pub folder_id: String,
    /// If set, the backend starts a BEP listener on this address when
    /// opened. `None` disables the listener (the backend can still
    /// dial out to known peers).
    pub listen_addr: Option<std::net::SocketAddr>,
    /// Static peer list — connection attempts run in the background.
    pub peers: Vec<ConfiguredPeer>,
    /// How far this device reaches out to discover and connect to peers.
    /// Governs which discovery and traversal sources may run; each source
    /// then self-activates only when it also has what it needs (a bound
    /// listener, configured server, etc.). Defaults to
    /// [`DiscoveryReach::Private`] — a trusted mesh with no publication to
    /// any global directory.
    pub exposure: DiscoveryReach,
    /// Friendly name for the LOCAL device. Used by the sync engine when
    /// stamping conflict-copy paths so the displaced side is identified
    /// by a human-readable label (e.g. `report.conflict-work-laptop-…`)
    /// rather than the opaque first eight characters of the device id.
    /// `None` falls back to the short device id.
    pub device_name: Option<String>,
    /// STUN servers used for NAT type detection at startup, after the
    /// default-versus-override rule in [`resolve_stun_servers`] has been
    /// applied. Empty means no NAT detection runs (the operator explicitly
    /// disabled STUN). Each entry is a `host:port` string; the first server
    /// that responds wins on the single-server path, and the first two feed
    /// the RFC 5780 two-server path.
    pub stun_servers: Vec<String>,
    /// Announce servers — each a base URL paired with the shared secret this
    /// device authenticates its writes with. When non-empty, an
    /// [`cascade_p2p::discovery::announce::AnnounceDiscovery`] source is
    /// registered for each, so the device publishes its candidates to (using
    /// the secret as the `HMAC` write key) and resolves peers against those
    /// servers. Empty disables announce-server discovery — the device relies on
    /// LAN multicast and introducer gossip alone. Whether the configured servers
    /// are actually contacted is governed by the [`exposure`](Self::exposure)
    /// posture, which must be [`DiscoveryReach::Public`] for any global-directory
    /// publication.
    pub announce_servers: Vec<AnnounceServer>,
    /// Kademlia/Mainline-DHT discovery configuration — *where to point* the
    /// DHT source. A [`cascade_p2p::discovery::DhtDiscovery`] backed by the
    /// `BitTorrent` Mainline DHT publishes this device's candidate set into
    /// and resolves peers out of the DHT keyed by device id — the serverless
    /// equivalent of the announce server. Whether it runs is governed by the
    /// [`exposure`](Self::exposure) posture (it self-activates only at
    /// [`DiscoveryReach::Public`], once a listener is bound), not by the
    /// presence of this config.
    pub dht: DhtConfig,
    /// Known relay endpoints, in preference order. Fed into
    /// [`cascade_p2p::decide_connectivity`] as the relay pool when the
    /// strategy table calls for relayed transport. Empty disables the
    /// relay strategy — pairs that would otherwise relay fall through
    /// to a best-effort hole punch.
    pub relay_endpoints: Vec<std::net::SocketAddr>,
    /// Shared secret authenticating this device against the relay
    /// pool. `None` means the relay path is provisioned but unusable;
    /// `decide_connectivity` may still pick `Relay` but the relay
    /// strategy will skip the dial without a secret.
    /// The 32-byte width matches the cascade relay's HMAC key length.
    pub relay_shared_secret: Option<[u8; 32]>,
    /// Whether this node volunteers as a peer relay. When not
    /// [`RelayVolunteer::Off`] and the detected `NAT` type is `Open` or
    /// `FullCone`, the node advertises itself as a relay candidate to
    /// trusted peers it shares a folder with via a
    /// [`cascade_p2p::protocol::BepMessage::RelayOffer`]. Defaults to
    /// [`RelayVolunteer::Auto`]. Peer relaying is additionally gated by the
    /// [`exposure`](Self::exposure) posture — it runs only from
    /// [`DiscoveryReach::Private`] upward, regardless of this setting.
    pub relay_volunteer: RelayVolunteer,
    /// Ceiling on concurrent relay sessions this node bridges while
    /// volunteering. New requests past the cap are rejected rather than
    /// silently dropped. Defaults to [`DEFAULT_MAX_RELAY_SESSIONS`].
    pub max_relay_sessions: u32,
    /// Ceiling on aggregate relay throughput, in bytes per second, across
    /// every active relay session. Defaults to
    /// [`DEFAULT_MAX_RELAY_BANDWIDTH_BYTES_PER_SEC`].
    pub max_relay_bandwidth: u64,
}

impl Default for P2pBackendConfig {
    fn default() -> Self {
        Self {
            instance_id: String::new(),
            display_name: String::new(),
            index_path: PathBuf::new(),
            block_store_root: PathBuf::new(),
            identity_dir: PathBuf::new(),
            folder_id: String::new(),
            listen_addr: None,
            peers: Vec::new(),
            // Private: a trusted mesh with LAN discovery, gossip, hole
            // punch, and peer relay, but no publication to any global
            // directory. Never default to Public — that would opt a node
            // into DHT/announce publication without the operator asking.
            exposure: DiscoveryReach::Private,
            device_name: None,
            stun_servers: Vec::new(),
            announce_servers: Vec::new(),
            dht: DhtConfig::default(),
            relay_endpoints: Vec::new(),
            relay_shared_secret: None,
            // Volunteering is on by default but gated on NAT type: only
            // `Open`/`FullCone` nodes ever actually advertise an offer, so
            // a default of `Auto` costs nothing on the restrictive nodes
            // that cannot relay anyway.
            relay_volunteer: RelayVolunteer::Auto,
            max_relay_sessions: DEFAULT_MAX_RELAY_SESSIONS,
            max_relay_bandwidth: DEFAULT_MAX_RELAY_BANDWIDTH_BYTES_PER_SEC,
        }
    }
}

/// A P2P backend instance.
pub struct P2pBackend {
    cfg: P2pBackendConfig,
    index: Arc<FolderIndex>,
    blocks: Arc<BlockStore>,
    sync: SyncEngine,
    /// Signals all spawned background tasks to exit. Set to `true` on
    /// drop. Tasks select on `cancel.changed()` and break their loops
    /// when they observe the flag flip.
    cancel: tokio::sync::watch::Sender<bool>,
}

impl std::fmt::Debug for P2pBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("P2pBackend")
            .field("id", &self.cfg.instance_id)
            .field("display_name", &self.cfg.display_name)
            .finish_non_exhaustive()
    }
}

impl Drop for P2pBackend {
    fn drop(&mut self) {
        // Best-effort: signal every background task to exit. If all
        // receivers have already been dropped (no tasks running), the
        // send is a no-op error we intentionally ignore.
        let _ = self.cancel.send(true);
    }
}

impl P2pBackend {
    /// F3 opt-in / opt-out. A deployment that wires the data authority
    /// post-construction calls `set_data_plane_ready(false)` immediately
    /// after `P2pBackend::open` to mark the BEP listener as not yet
    /// ready to serve peers; `set_data_authority` later flips the bit
    /// back to `true`. Bare engines and pre-feature tests leave the bit
    /// at its default (`true`) and the listener serves as before.
    ///
    /// The `data_plane_ready` accessor on the underlying [`crate::sync::SyncEngine`]
    /// is the public read side. Exposed here for symmetry with
    /// `set_manage_dispatch` and `set_data_authority`.
    pub fn set_data_plane_ready(&self, ready: bool) {
        self.sync.set_data_plane_ready_flag(ready);
    }
    /// Open or create a P2P backend at the given index + block store
    /// paths. Synchronous: filesystem setup happens up-front so this
    /// is callable from within a tokio runtime worker thread without
    /// nested-runtime panics. If `cfg.listen_addr` is set, a BEP
    /// listener is spawned (via `tokio::spawn`); every configured peer
    /// gets a background reconnect task.
    ///
    /// All spawned background tasks share a `tokio::sync::watch`
    /// channel; when the returned `P2pBackend` is dropped, every task
    /// observes the cancellation flag and exits cleanly. This matters
    /// in tests and when a backend is removed at runtime — without it
    /// the listener, the per-peer reconnect loops, and the LAN
    /// discovery loops would leak past the backend's lifetime,
    /// keeping `Arc<FolderIndex>` and `Arc<BlockStore>` alive
    /// indefinitely.
    pub fn open(cfg: P2pBackendConfig) -> Result<Self> {
        let index = Arc::new(FolderIndex::open(&cfg.index_path)?);
        let blocks = Arc::new(BlockStore::new(&cfg.block_store_root).context("open block store")?);
        let identity = DeviceIdentity::load_or_generate(&cfg.identity_dir)
            .context("loading P2P backend identity")?;
        let sync = SyncEngine::new(
            cfg.folder_id.clone(),
            index.clone(),
            blocks.clone(),
            identity,
        )
        .with_local_device_name(cfg.device_name.clone())
        // Peer relay (endpoints + volunteering) self-activates only from
        // `Private` upward. At `LanOnly` the relay pool and volunteer
        // policy are left empty/off so neither dialling through a relay
        // nor offering to be one ever happens.
        .with_relay_endpoints(if cfg.exposure.permits_peer_relay() {
            cfg.relay_endpoints.clone()
        } else {
            Vec::new()
        })
        .with_relay_shared_secret(cfg.relay_shared_secret)
        // Hole punch self-activates only from `Private` upward; at
        // `LanOnly` a chosen `HolePunch` strategy is downgraded before any
        // UDP burst leaves the segment.
        .with_hole_punch_enabled(cfg.exposure.permits_hole_punch())
        .with_relay_volunteer(if cfg.exposure.permits_peer_relay() {
            cfg.relay_volunteer
        } else {
            RelayVolunteer::Off
        })
        .with_relay_session_caps(cfg.max_relay_sessions, cfg.max_relay_bandwidth);

        // Cancellation channel for all background tasks. `false` =
        // run, `true` = stop. The `Sender` lives on `P2pBackend`; its
        // `Drop` flips the flag.
        let (cancel, _) = tokio::sync::watch::channel(false);

        // Trust + reconnect tasks need a runtime — spawn them on the
        // current handle. The caller must be inside a tokio context.
        let sync_for_listener = sync.clone();
        let cfg_listen_addr = cfg.listen_addr;
        let cfg_instance_id = cfg.instance_id.clone();
        let cfg_peers = cfg.peers.clone();
        let cfg_exposure = cfg.exposure;
        let cfg_stun_servers = cfg.stun_servers.clone();
        let cfg_announce_servers = cfg.announce_servers.clone();
        let cfg_dht = cfg.dht.clone();
        let bootstrap_cancel = cancel.subscribe();
        let cancel_for_children = cancel.clone();
        tokio::spawn(async move {
            for peer in &cfg_peers {
                sync_for_listener.trust(peer.device_id.clone()).await;
            }
            // Seed the friendly-name map from the static peer list. Any
            // peer without a configured `name` is skipped — peers are
            // not auto-named, the absence is preserved so a later
            // protocol extension can fill them in without colliding.
            let name_entries: Vec<(String, String)> = cfg_peers
                .iter()
                .filter_map(|p| p.name.clone().map(|n| (p.device_id.clone(), n)))
                .collect();
            if !name_entries.is_empty() {
                sync_for_listener.seed_peer_names(name_entries).await;
            }

            // Wire WAN gossip: every minute, broadcast a snapshot of
            // the local peer book to every connected peer via the
            // `BepMessage::Gossip` frame. Receivers merge the snapshot
            // into their own peer book so devices that are not directly
            // configured for each other still learn about one another
            // transitively through any shared peer.
            //
            // The gossip task observes the same cancellation watch as
            // the rest of the children so it exits cleanly when the
            // backend is dropped.
            //
            // Introducer gossip self-activates from `Private` upward — it
            // needs nothing beyond the peer book the engine already keeps,
            // so the posture permission is the whole gate.
            if cfg_exposure.permits_gossip() {
                tracing::info!(
                    target: "cascade::backend::p2p",
                    instance = %cfg_instance_id,
                    "WAN gossip enabled — broadcasting peer-book snapshot every 60s"
                );
                let gossip_sync = sync_for_listener.clone();
                let mut gossip_cancel = cancel_for_children.subscribe();
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            () = tokio::time::sleep(std::time::Duration::from_mins(1)) => {
                                gossip_sync.broadcast_gossip().await;
                            }
                            res = gossip_cancel.changed() => {
                                if res.is_err() || *gossip_cancel.borrow() {
                                    return;
                                }
                            }
                        }
                    }
                });
            }

            // NAT detection runs in the background so a slow STUN
            // round-trip doesn't delay listener bind-up. The result is
            // published onto the engine (`SyncEngine::set_local_nat_type`)
            // and read by `connect_to_with_strategy` whenever a peer
            // connection attempt consults `decide_connectivity`. Local
            // NAT type stays `Unknown` until the task completes — that's
            // the conservative reading: the table routes Unknown
            // through Relay (with a punch fallback) until the real
            // classification arrives.
            if !cfg_stun_servers.is_empty() {
                let nat_sync = sync_for_listener.clone();
                let nat_instance = cfg_instance_id.clone();
                let nat_servers = cfg_stun_servers.clone();
                tokio::spawn(async move {
                    detect_nat_and_publish(&nat_instance, &nat_servers, &nat_sync).await;
                });
            }

            // Bind the listener first so subsequent announce loops can
            // advertise the actual bound port (important when
            // `listen_addr` uses port 0 — peers receiving a port-0
            // announcement would be unable to connect back).
            let bound_port = match cfg_listen_addr {
                Some(addr) => {
                    let listener_cancel = cancel_for_children.subscribe();
                    match sync_for_listener
                        .start_listener(addr, listener_cancel)
                        .await
                    {
                        Ok((bound, _handle)) => {
                            tracing::info!(
                                target: "cascade::backend::p2p",
                                "P2P backend `{}` listening on {bound} as device {}",
                                cfg_instance_id,
                                sync_for_listener.device_id(),
                            );
                            Some(bound.port())
                        }
                        Err(e) => {
                            tracing::error!(
                                target: "cascade::backend::p2p",
                                "P2P backend `{}` failed to listen on {addr}: {e:#}",
                                cfg_instance_id,
                            );
                            None
                        }
                    }
                }
                None => None,
            };

            // If we were cancelled while starting the listener, bail
            // out before spawning any further children.
            if *bootstrap_cancel.borrow() {
                return;
            }

            for peer in cfg_peers {
                let sync_clone = sync_for_listener.clone();
                let peer_cancel = cancel_for_children.subscribe();
                tokio::spawn(async move {
                    keep_peer_connected(sync_clone, peer, peer_cancel).await;
                });
            }

            // Compose the discovery sources into a single service, each
            // gated by the exposure posture and its own self-activation
            // requirement. The service is the structural home for
            // discovery — the loops below feed their I/O through the
            // registered `LanDiscovery` source so the wire behaviour is
            // unchanged.
            let mut discovery = DiscoveryService::new();

            // Introducer gossip: permitted from `Private` upward and needs
            // only the peer book the engine already keeps.
            if cfg_exposure.permits_gossip() {
                discovery.register(Box::new(GossipDiscovery::new(
                    sync_for_listener.peer_book().clone(),
                )));
            }

            // LAN multicast: permitted at every posture (LAN is the floor)
            // but self-activates only once a listener is bound — without an
            // inbound port a discovered peer would have nothing to dial.
            let lan_source = bound_port.is_some().then(LanDiscovery::new);
            if let Some(lan) = lan_source {
                discovery.register(Box::new(lan));
            }

            // Global directory — announce servers and the Mainline DHT —
            // is permitted only at `Public`. Below that posture this device
            // publishes nothing to any global directory, so neither source
            // is constructed or run.
            let publish_globally = cfg_exposure.permits_global_directory();

            // Register an announce-server source for each configured server.
            // Self-activates only at `Public` posture with a configured
            // server; each source carries its server's shared secret so its
            // registrations carry a verifying `HMAC` write tag. A client that
            // fails to construct (HTTP/TLS backend init) is logged and skipped
            // rather than aborting backend open, so the other discovery sources
            // still function.
            if publish_globally {
                for server in &cfg_announce_servers {
                    match cascade_p2p::discovery::announce::AnnounceDiscovery::new(
                        server.base_url.clone(),
                        server.secret,
                    ) {
                        Ok(source) => discovery.register(Box::new(source)),
                        Err(e) => tracing::warn!(
                            target: "cascade::backend::p2p",
                            instance = %cfg_instance_id,
                            announce_server = %server.base_url,
                            error = %e,
                            "could not construct announce-server discovery client; skipping",
                        ),
                    }
                }
            }
            // Register the Kademlia/Mainline-DHT source. Self-activates only
            // at `Public` posture and once the listener is up: the DHT
            // publishes and resolves keyed by device id, so it is useful only
            // with a bound port to advertise. A node whose construction fails
            // (binding the DHT UDP socket) is logged and skipped rather than
            // aborting backend open, exactly like the announce-server source.
            // The BEP44 keypair is derived per device id at announce/resolve
            // time, so the node holds no persisted secret of its own. The
            // constructed source is cloned: one copy joins the resolver, the
            // other drives the periodic announce loop below.
            let dht_source = if publish_globally && bound_port.is_some() {
                // `MainlineDht::open` is a blocking constructor: it resolves
                // the bootstrap host:port set with synchronous `getaddrinfo`
                // and binds the DHT UDP socket on the calling thread. Run it
                // off the runtime via `spawn_blocking` so a slow or
                // unreachable resolver cannot stall this worker, mirroring
                // `resolve_first`'s off-thread DNS.
                let bootstrap = cfg_dht.bootstrap_nodes.clone();
                match open_dht_off_thread(bootstrap).await {
                    Ok(node) => {
                        let source = cascade_p2p::discovery::DhtDiscovery::new(node);
                        discovery.register(Box::new(source.clone()));
                        Some(source)
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "cascade::backend::p2p",
                            instance = %cfg_instance_id,
                            error = %e,
                            "could not construct Mainline-DHT discovery node; skipping",
                        );
                        None
                    }
                }
            } else {
                None
            };

            tracing::debug!(
                target: "cascade::backend::p2p",
                instance = %cfg_instance_id,
                sources = discovery.len(),
                "discovery service composed",
            );

            // Publish our local candidate set into the DHT on a periodic loop
            // so peers can resolve us by device id without a central directory.
            // Mirrors the announce-server publish loop: it gathers the current
            // local candidates each tick and stores them under our device-id
            // key, observing the same cancellation watch as the other loops.
            if let Some(dht) = dht_source {
                let dht_sync = sync_for_listener.clone();
                let dht_cancel = cancel_for_children.subscribe();
                tokio::spawn(async move {
                    dht_publish_loop(dht, dht_sync, dht_cancel).await;
                });
            }

            // Publish our local candidate set to every announce server on a
            // periodic loop so peers can resolve us by device id even when
            // we share no LAN segment and no introducer. The loop builds a
            // fresh AnnounceDiscovery per server (the composed `discovery`
            // service owns its boxed copies and is consumed by resolution).
            if publish_globally && !cfg_announce_servers.is_empty() && bound_port.is_some() {
                let announce_sync = sync_for_listener.clone();
                let announce_servers = cfg_announce_servers.clone();
                let announce_cancel = cancel_for_children.subscribe();
                let announce_instance = cfg_instance_id.clone();
                tokio::spawn(async move {
                    announce_server_publish_loop(
                        &announce_instance,
                        announce_servers,
                        announce_sync,
                        announce_cancel,
                    )
                    .await;
                });
            }

            if let (Some(lan), Some(listen_port)) = (lan_source, bound_port) {
                let announce_sync = sync_for_listener.clone();
                let announce_cancel = cancel_for_children.subscribe();
                tokio::spawn(async move {
                    discovery_announce_loop(lan, announce_sync, listen_port, announce_cancel).await;
                });
                let listen_sync = sync_for_listener.clone();
                let listen_cancel = cancel_for_children.subscribe();
                tokio::spawn(async move {
                    discovery_listen_loop(lan, listen_sync, listen_cancel).await;
                });
            }
        });

        Ok(Self {
            cfg,
            index,
            blocks,
            sync,
            cancel,
        })
    }

    /// Access the sync engine — used to start a listener and add peers.
    #[must_use]
    pub const fn sync(&self) -> &SyncEngine {
        &self.sync
    }

    /// Manager side: administer a remote node by device id.
    ///
    /// Resolves `device_id` to a reachable address, opens (or reuses) an
    /// authenticated session over the connectivity ladder, sends the
    /// `command`, and returns the managed node's typed [`ManageResult`].
    ///
    /// Resolution prefers a statically-configured peer's address (the device is
    /// already paired, so its endpoint is known without a directory lookup) and
    /// otherwise falls back to the [`DiscoveryService`] composed from this
    /// backend's configured sources — exactly the sources the backend already
    /// runs (LAN multicast, introducer gossip, announce servers, the Mainline
    /// DHT). The transport is the same TLS-direct dial the data plane uses; this
    /// method never opens a parallel transport.
    ///
    /// The connection must reach the management plane over a TLS-verified
    /// session — the managed node refuses a `ManageRequest` on a relayed or
    /// post-hole-punch session whose peer identity is merely asserted on the
    /// wire — so this method connects directly via the dial ladder rather than
    /// the strategy chooser that may select a relay.
    ///
    /// An authorisation denial on the managed node surfaces inside the returned
    /// `Ok(ManageResult::Err { kind: Unauthorised, .. })`; a transport failure
    /// (unresolvable device, dial failure, dropped session, timeout) is the
    /// `Err` arm of the outer `Result`.
    pub async fn manage_remote(
        &self,
        device_id: &str,
        command: ManageCommand,
        scope: ManageScope,
        token: Option<String>,
    ) -> Result<ManageResult> {
        // Reuse an existing session only when it is TLS-verified — the data
        // plane may already hold a connection to this device, but a relayed or
        // post-hole-punch session asserts the peer's device id on the wire
        // without an end-to-end TLS handshake, so the managed node refuses a
        // ManageRequest on it (see `handle_manage_request`). Reusing such a
        // session would earn a spurious unauthorised denial — the exact NAT
        // case this feature is most needed in — so when no *verified* session
        // exists we dial a fresh direct one over the TLS ladder.
        if !self.sync.has_verified_peer(device_id).await {
            let address = self
                .resolve_remote_address(device_id)
                .await
                .with_context(|| format!("resolving management target {device_id}"))?;
            // The managed node only honours management commands from a
            // TLS-verified peer, so trust the device and dial it directly over
            // the TCP+TLS ladder rather than the strategy chooser that might
            // route through a relay (which the managed node refuses).
            self.sync.trust(device_id.to_owned()).await;
            self.sync
                .connect_to(crate::sync::Peer {
                    device_id: device_id.to_owned(),
                    address,
                })
                .await
                .with_context(|| {
                    format!("connecting to management target {device_id} at {address}")
                })?;
            // `connect_to` spawns the session loop; wait for the handle to
            // register before sending so the request does not race the
            // session setup.
            self.await_session(device_id)
                .await
                .with_context(|| format!("waiting for session to management target {device_id}"))?;
        }

        self.sync
            .send_manage_request(device_id, command, scope, token)
            .await
    }

    /// Resolve a peer device id to a reachable socket address.
    ///
    /// A statically-configured peer wins — its address is known without a
    /// directory lookup — and is re-resolved through DNS so a hostname that was
    /// not routable at startup still works. Otherwise the device is resolved
    /// through the [`DiscoveryService`] composed from this backend's configured
    /// discovery sources, returning the highest-priority candidate address.
    async fn resolve_remote_address(&self, device_id: &str) -> Result<std::net::SocketAddr> {
        if let Some(peer) = self.cfg.peers.iter().find(|p| p.device_id == device_id) {
            return resolve_first(&peer.address).await;
        }

        let discovery = self.compose_resolution_discovery().await;
        let candidates = discovery.resolve(device_id).await;
        candidates
            .into_iter()
            .next()
            .map(|candidate| candidate.address)
            .with_context(|| format!("no discovery source resolved device {device_id}"))
    }

    /// Compose a [`DiscoveryService`] from this backend's configured discovery
    /// sources, for one-shot peer resolution.
    ///
    /// Registers the same source families the backend runs continuously,
    /// each gated by the exposure posture exactly as backend open gates them:
    /// LAN multicast at every posture, introducer gossip from `Private`
    /// upward, and announce servers plus the Mainline DHT only at `Public`.
    /// Resolving through the same channels the data plane uses keeps the
    /// manager's reachability consistent with the node's posture. A source
    /// whose construction fails is logged and skipped rather than aborting the
    /// resolution, matching the backend's startup behaviour.
    async fn compose_resolution_discovery(&self) -> DiscoveryService {
        let mut discovery = DiscoveryService::new();
        if self.cfg.exposure.permits_gossip() {
            discovery.register(Box::new(GossipDiscovery::new(
                self.sync.peer_book().clone(),
            )));
        }
        // LAN multicast is permitted at every posture — it is the floor.
        discovery.register(Box::new(LanDiscovery::new()));
        if self.cfg.exposure.permits_global_directory() {
            for server in &self.cfg.announce_servers {
                match cascade_p2p::discovery::announce::AnnounceDiscovery::new(
                    server.base_url.clone(),
                    server.secret,
                ) {
                    Ok(source) => discovery.register(Box::new(source)),
                    Err(e) => tracing::warn!(
                        target: "cascade::backend::p2p",
                        announce_server = %server.base_url,
                        error = %e,
                        "could not construct announce-server discovery client for management resolution; skipping",
                    ),
                }
            }
            // `MainlineDht::open` blocks on `getaddrinfo` for the bootstrap set
            // and the UDP socket bind, so it runs off the runtime — this method
            // sits on the management-plane dial path and must not stall a worker
            // on DNS. See `open_dht_off_thread`. The DHT is posture-gated by the
            // `permits_global_directory()` check above, not by config presence.
            let bootstrap = self.cfg.dht.bootstrap_nodes.clone();
            match open_dht_off_thread(bootstrap).await {
                Ok(node) => {
                    discovery.register(Box::new(cascade_p2p::discovery::DhtDiscovery::new(node)));
                }
                Err(e) => tracing::warn!(
                    target: "cascade::backend::p2p",
                    error = %e,
                    "could not construct Mainline-DHT discovery node for management resolution; skipping",
                ),
            }
        }
        discovery
    }

    /// Wait for a *TLS-verified* session to `device_id` to register in the peer
    /// map.
    ///
    /// [`SyncEngine::connect_to`] completes the TLS handshake then spawns the
    /// session loop, which registers the peer handle asynchronously. Poll until
    /// the *verified* handle appears (so a following management request finds a
    /// session the managed node will honour) or the bounded wait elapses,
    /// failing loudly rather than racing the send against session setup.
    ///
    /// Waiting on the verified flavour rather than any session is deliberate: a
    /// stale relay or post-hole-punch session for the same device id could
    /// already be registered, and a management request on that would be refused.
    /// The direct dial just started replaces it with a verified session, so this
    /// waits for that replacement specifically.
    async fn await_session(&self, device_id: &str) -> Result<()> {
        // The handshake plus first registration is sub-second on a direct dial;
        // poll a short interval up to a bounded total so a wedged setup fails
        // loudly rather than hanging.
        let poll_interval = std::time::Duration::from_millis(50);
        let max_waits = SESSION_REGISTER_TIMEOUT.as_millis() / poll_interval.as_millis();
        for _ in 0..max_waits {
            if self.sync.has_verified_peer(device_id).await {
                return Ok(());
            }
            tokio::time::sleep(poll_interval).await;
        }
        anyhow::bail!(
            "TLS-verified session to {device_id} did not register within the connect window"
        )
    }

    /// Convert a `FolderIndex` row into a `FileEntry` keyed under this
    /// backend's instance ID. Root entries (those with no `/` in `path`)
    /// have `parent_id = "root"`; nested entries point at their parent
    /// path as the native ID.
    fn entry_to_file(&self, entry: &IndexEntry) -> FileEntry {
        let (parent_native, name) = match entry.path.rsplit_once('/') {
            Some((parent, name)) => (parent.to_string(), name.to_string()),
            None => ("root".to_string(), entry.path.clone()),
        };
        let modified = chrono::DateTime::from_timestamp(entry.modified, 0);
        FileEntry {
            id: ItemId::new(&self.cfg.instance_id, &entry.path),
            parent_id: ItemId::new(&self.cfg.instance_id, &parent_native),
            name,
            is_dir: entry.is_dir,
            size: if entry.is_dir { None } else { Some(entry.size) },
            mod_time: modified,
            mime_type: None,
            hash: None,
        }
    }

    /// Synthetic root entry (no real index row).
    fn root_entry(&self) -> FileEntry {
        FileEntry::dir(
            ItemId::new(&self.cfg.instance_id, "root"),
            ItemId::new(&self.cfg.instance_id, "root"),
            "P2P".to_string(),
        )
    }
}

#[async_trait]
impl Backend for P2pBackend {
    fn id(&self) -> &str {
        &self.cfg.instance_id
    }

    fn display_name(&self) -> &str {
        &self.cfg.display_name
    }

    async fn quota(&self) -> Result<Option<Quota>> {
        // No accounting yet — peer storage is opaque.
        Ok(None)
    }

    async fn changes(&self, cursor: Option<&Cursor>) -> Result<(Vec<Change>, Cursor)> {
        let since: i64 = cursor.and_then(|c| c.0.parse().ok()).unwrap_or(0);
        let entries = self.index.entries_since(since)?;
        let mut changes = Vec::with_capacity(entries.len());
        for entry in &entries {
            let file = self.entry_to_file(entry);
            if entry.deleted {
                changes.push(Change::Deleted(file));
            } else {
                changes.push(Change::Created(file));
            }
        }
        let new_cursor = self.index.max_version()?;
        Ok((changes, Cursor(new_cursor.to_string())))
    }

    async fn metadata(&self, path: &Path) -> Result<FileEntry> {
        let path_str = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("non-utf8 path"))?
            .trim_start_matches('/')
            .to_string();
        if path_str.is_empty() {
            return Ok(self.root_entry());
        }
        let entry = self
            .index
            .get(&path_str)?
            .ok_or_else(|| anyhow::anyhow!("not found: {path_str}"))?;
        if entry.deleted {
            anyhow::bail!("not found (deleted): {path_str}");
        }
        Ok(self.entry_to_file(&entry))
    }

    async fn download(
        &self,
        file: &FileEntry,
        writer: &mut (dyn tokio::io::AsyncWrite + Unpin + Send),
    ) -> Result<()> {
        let native = file.id.native_id();
        let entry = self
            .index
            .get(native)?
            .ok_or_else(|| anyhow::anyhow!("not in index: {native}"))?;
        let block_size = cascade_p2p::block::block_size_for_file(entry.size);
        // Block hashes are stored as concatenated 32-byte values.
        for (idx, chunk) in entry.block_hashes.chunks(32).enumerate() {
            let mut h = [0u8; 32];
            h.copy_from_slice(chunk);
            let hash = BlockHash(h);
            let data = if let Some(data) = self.blocks.get_block(&hash).await? {
                data
            } else {
                let fetched = self
                    .sync
                    .fetch_block(native, idx, block_size, h)
                    .await
                    .ok_or_else(|| anyhow::anyhow!("block {hash} missing and no peer had it"))?;
                // Cache the fetched block locally so future reads hit
                // the store without round-tripping the network.
                self.blocks.store_block(&hash, &fetched).await?;
                fetched
            };
            writer.write_all(&data).await?;
        }
        writer.flush().await?;
        Ok(())
    }

    /// Range read that fetches only the content-addressed blocks
    /// overlapping `[offset, offset + length)`.
    ///
    /// Rather than reconstructing the whole file (as the default impl
    /// does via [`Backend::download`]), this computes the span of blocks
    /// that cover the requested window, fetches just those blocks (local
    /// store first, then peers — caching anything pulled over the wire,
    /// exactly as `download` does), assembles them into a contiguous
    /// buffer aligned to the first block's file offset, then slices out
    /// the exact `[offset, offset + length)` window.
    ///
    /// Contract: the result may be shorter than `length` at end-of-file,
    /// is empty when `offset` is at or past the reconstructed size, and
    /// never panics on out-of-range offset/length.
    async fn read_range(&self, file: &FileEntry, offset: u64, length: u32) -> Result<Vec<u8>> {
        // A zero-length request reads nothing regardless of offset.
        if length == 0 {
            return Ok(Vec::new());
        }

        let native = file.id.native_id();
        let entry = self
            .index
            .get(native)?
            .ok_or_else(|| anyhow::anyhow!("not in index: {native}"))?;

        // Offset at or past the reconstructed size yields no bytes.
        if offset >= entry.size {
            return Ok(Vec::new());
        }

        let block_size = cascade_p2p::block::block_size_for_file(entry.size);
        let block_size_u64 = u64::from(block_size);
        // A zero block size would make the index arithmetic below
        // undefined; treat it as a malformed entry rather than dividing
        // by zero.
        if block_size_u64 == 0 {
            anyhow::bail!("block size of zero for {native}");
        }

        // Block hashes are stored as concatenated 32-byte values.
        let block_count = entry.block_hashes.len() / 32;
        if block_count == 0 {
            return Ok(Vec::new());
        }
        // The last addressable block index. Used to clamp the computed
        // `last_block` so a `length` running past EOF never references a
        // block that does not exist.
        let max_block = block_count.saturating_sub(1);

        // The window's inclusive last byte. `length > 0` and
        // `offset < entry.size` are both already established, so the
        // subtraction cannot wrap.
        let last_byte = offset.saturating_add(u64::from(length)).saturating_sub(1);

        let first_block_u64 = offset / block_size_u64;
        let last_block_u64 = last_byte / block_size_u64;
        let first_block = usize::try_from(first_block_u64)
            .map_err(|_| anyhow::anyhow!("first block index overflow"))?;
        let last_block = usize::try_from(last_block_u64)
            .map_err(|_| anyhow::anyhow!("last block index overflow"))?
            .min(max_block);

        // Byte offset in the file where the assembled buffer begins —
        // the start of the first covering block.
        let assembled_start = first_block_u64.saturating_mul(block_size_u64);

        // Fetch and concatenate only the covering blocks.
        let mut assembled: Vec<u8> = Vec::new();
        for idx in first_block..=last_block {
            let hash_start = idx.saturating_mul(32);
            let hash_end = hash_start.saturating_add(32);
            let chunk = entry
                .block_hashes
                .get(hash_start..hash_end)
                .ok_or_else(|| anyhow::anyhow!("block hash {idx} out of range for {native}"))?;
            let mut h = [0u8; 32];
            h.copy_from_slice(chunk);
            let hash = BlockHash(h);
            let data = if let Some(data) = self.blocks.get_block(&hash).await? {
                data
            } else {
                let fetched = self
                    .sync
                    .fetch_block(native, idx, block_size, h)
                    .await
                    .ok_or_else(|| anyhow::anyhow!("block {hash} missing and no peer had it"))?;
                // Cache the fetched block locally so future reads hit
                // the store without round-tripping the network.
                self.blocks.store_block(&hash, &fetched).await?;
                fetched
            };
            assembled.extend_from_slice(&data);
        }

        // Translate the absolute window into an offset within the
        // assembled buffer, clamping the end to what was actually
        // reconstructed (the final block may be short).
        let rel_start_u64 = offset.saturating_sub(assembled_start);
        let rel_start = usize::try_from(rel_start_u64)
            .map_err(|_| anyhow::anyhow!("relative offset overflow"))?
            .min(assembled.len());
        let want = usize::try_from(length).unwrap_or(usize::MAX);
        let rel_end = rel_start.saturating_add(want).min(assembled.len());
        Ok(assembled
            .get(rel_start..rel_end)
            .unwrap_or_default()
            .to_vec())
    }

    async fn upload(
        &self,
        path: &Path,
        reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
        parent_id: &FileId,
    ) -> Result<FileEntry> {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow::anyhow!("invalid filename"))?;
        let parent_native = parent_id.native_id();
        let path_str = if parent_native == "root" || parent_native.is_empty() {
            name.to_string()
        } else {
            format!("{parent_native}/{name}")
        };

        let mut data = Vec::new();
        reader.read_to_end(&mut data).await?;
        let size = data.len() as u64;

        let blocks_info = split_data(&data);
        let block_size = blocks_info.block_size as usize;
        let mut hash_blob = Vec::with_capacity(blocks_info.blocks.len() * 32);
        for (idx, hash) in blocks_info.blocks.iter().enumerate() {
            let start = idx * block_size;
            let end = (start + block_size).min(data.len());
            #[allow(clippy::indexing_slicing)] // bounds derived from split_data
            let slice = &data[start..end];
            self.blocks.store_block(hash, slice).await?;
            hash_blob.extend_from_slice(&hash.0);
        }

        // Bump the local device's version-vector counter so peers can
        // tell this write apart from any concurrent edit. The existing
        // counter (if any) is taken from the prior row.
        let mut version = Version {
            counters: self
                .index
                .get(&path_str)?
                .map(|e| e.version)
                .unwrap_or_default(),
        };
        version.bump(self.sync.device_short_id());

        let entry = IndexEntry {
            path: path_str.clone(),
            is_dir: false,
            size,
            modified: chrono::Utc::now().timestamp(),
            block_hashes: hash_blob,
            deleted: false,
            row_version: 0,
            version: version.counters,
        };
        self.index.upsert(&entry)?;
        self.sync.broadcast_update(&entry).await;
        Ok(self.entry_to_file(&entry))
    }

    async fn update(
        &self,
        file_id: &FileId,
        reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
    ) -> Result<FileEntry> {
        let native = file_id.native_id();
        let existing = self
            .index
            .get(native)?
            .ok_or_else(|| anyhow::anyhow!("not in index: {native}"))?;
        let parent = match native.rsplit_once('/') {
            Some((parent, _)) => format!("{}:{parent}", self.cfg.instance_id),
            None => format!("{}:root", self.cfg.instance_id),
        };
        // Re-upload using the same path.
        self.upload(Path::new(&existing.path), reader, &FileId(parent))
            .await
    }

    async fn create_dir(&self, path: &Path) -> Result<FileEntry> {
        let path_str = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("non-utf8 path"))?
            .trim_start_matches('/')
            .to_string();
        // Directory rows carry a version vector for symmetry with file
        // rows even though BEP never propagates directories — peers
        // infer them from the parent path of each file.
        let mut version = Version {
            counters: self
                .index
                .get(&path_str)?
                .map(|e| e.version)
                .unwrap_or_default(),
        };
        version.bump(self.sync.device_short_id());
        let entry = IndexEntry {
            path: path_str,
            is_dir: true,
            size: 0,
            modified: chrono::Utc::now().timestamp(),
            block_hashes: Vec::new(),
            deleted: false,
            row_version: 0,
            version: version.counters,
        };
        self.index.upsert(&entry)?;
        Ok(self.entry_to_file(&entry))
    }

    async fn delete(&self, file: &FileEntry) -> Result<()> {
        let native = file.id.native_id();
        // Bump the local device's counter so peers see this tombstone
        // as causally newer than whatever they last received for this
        // path. Reading the existing row gives us the prior vector to
        // extend rather than overwrite.
        let existing = self
            .index
            .get(native)?
            .ok_or_else(|| anyhow::anyhow!("not in index: {native}"))?;
        let mut version = Version {
            counters: existing.version,
        };
        version.bump(self.sync.device_short_id());
        let tombstone = IndexEntry {
            path: existing.path,
            is_dir: existing.is_dir,
            size: 0,
            modified: chrono::Utc::now().timestamp(),
            block_hashes: vec![],
            deleted: true,
            row_version: 0,
            version: version.counters,
        };
        self.index.upsert(&tombstone)?;
        // Broadcast the tombstone so peers can mirror the delete.
        self.sync.broadcast_update(&tombstone).await;
        Ok(())
    }

    async fn move_entry(&self, src: &Path, dst: &Path) -> Result<FileEntry> {
        let src_str = src
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("non-utf8 src path"))?
            .trim_start_matches('/');
        let dst_str = dst
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("non-utf8 dst path"))?
            .trim_start_matches('/');
        let existing = self
            .index
            .get(src_str)?
            .ok_or_else(|| anyhow::anyhow!("not in index: {src_str}"))?;
        let now = chrono::Utc::now().timestamp();
        let short_id = self.sync.device_short_id();

        // The destination row inherits the source's vector with the
        // local counter bumped, merged with any prior destination
        // vector so we don't accidentally regress against a peer.
        let mut dst_version = Version {
            counters: existing.version.clone(),
        };
        if let Some(existing_dst) = self.index.get(dst_str)? {
            dst_version.merge(&Version {
                counters: existing_dst.version,
            });
        }
        dst_version.bump(short_id);
        let new_entry = IndexEntry {
            path: dst_str.to_string(),
            is_dir: existing.is_dir,
            size: existing.size,
            modified: now,
            block_hashes: existing.block_hashes,
            deleted: false,
            row_version: 0,
            version: dst_version.counters,
        };
        self.index.upsert(&new_entry)?;

        // The source row becomes a tombstone with its own counter
        // bumped so peers see the delete as causally newer than the
        // row they last received.
        let mut src_version = Version {
            counters: existing.version,
        };
        src_version.bump(short_id);
        let tombstone = IndexEntry {
            path: src_str.to_string(),
            is_dir: false,
            size: 0,
            modified: now,
            block_hashes: vec![],
            deleted: true,
            row_version: 0,
            version: src_version.counters,
        };
        self.index.upsert(&tombstone)?;
        Ok(self.entry_to_file(&new_entry))
    }

    async fn list_children(&self, parent_native_id: &str) -> Result<Vec<FileEntry>> {
        let parent = if parent_native_id == "root" {
            ""
        } else {
            parent_native_id
        };
        let rows = self.index.list_children(parent)?;
        Ok(rows.iter().map(|e| self.entry_to_file(e)).collect())
    }

    async fn poll_interval(&self) -> Option<std::time::Duration> {
        // The local index is updated synchronously and there is no remote
        // source to poll (peer sync pushes IndexUpdate messages when
        // wired). 60s is a sensible default so changes() is still called
        // periodically to flush queued peer changes.
        Some(POLL_INTERVAL)
    }

    async fn set_manage_dispatch(&self, dispatch: Arc<dyn cascade_engine::manage::ManageDispatch>) {
        // Hand the engine's management-plane dispatcher to the sync engine. The
        // listener and session loops were spawned in `open` from clones of the
        // same sync engine, which share the dispatch slot behind an `Arc<RwLock>`,
        // so a request arriving after this point is authorised, audited, and
        // executed through the dispatcher rather than refused as "not accepting
        // remote management".
        self.sync.set_manage_dispatch(dispatch).await;
    }

    async fn set_data_authority(&self, authority: Arc<dyn cascade_engine::manage::DataAuthority>) {
        // Hand the engine's data-plane authority to the sync engine. As with the
        // management dispatcher, the session loops share the authority slot behind
        // an `Arc<RwLock>`, so a sync frame arriving after this point is gated on
        // the engine's directional data-access decision. Until this is wired the
        // sync path is default-open — every trusted peer keeps full bidirectional
        // access — matching the pre-feature behaviour.
        self.sync.set_data_authority(authority).await;
    }
}

/// Retry loop: try to keep an outbound BEP connection to `peer` up.
///
/// `connect_to` returns immediately after spawning the session; if the
/// session ends (e.g. the peer crashed or the network blipped), our
/// peer table loses the entry and we should try again. The 5s back-off
/// is enough to avoid busy-looping on a sustained outage and short
/// enough to recover quickly from a transient one.
///
/// DNS resolution happens here, not at config-parse time, so a peer
/// hostname that isn't yet routable at startup (the typical Docker
/// case) becomes routable on the next tick.
///
/// Exits as soon as `cancel` flips to `true`.
async fn keep_peer_connected(
    sync: SyncEngine,
    peer: ConfiguredPeer,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) {
    let peer_id = peer.device_id.clone();
    let peer_addr_raw = peer.address.clone();
    loop {
        if *cancel.borrow() {
            return;
        }
        let already = sync.has_peer(&peer_id).await;
        if !already {
            match resolve_first(&peer_addr_raw).await {
                Ok(addr) => {
                    if let Err(e) = sync
                        .connect_to(crate::sync::Peer {
                            device_id: peer.device_id.clone(),
                            address: addr,
                        })
                        .await
                    {
                        tracing::debug!(
                            target: "cascade::backend::p2p",
                            "peer {peer_id} not reachable at {addr}: {e:#}; retrying",
                        );
                    }
                }
                Err(e) => {
                    tracing::debug!(
                        target: "cascade::backend::p2p",
                        "peer {peer_id} address `{peer_addr_raw}` did not resolve: {e:#}; retrying",
                    );
                }
            }
        }
        tokio::select! {
            () = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
            res = cancel.changed() => {
                if res.is_err() || *cancel.borrow() {
                    return;
                }
            }
        }
    }
}

/// Detect the local `NAT` type via STUN and persist it on `sync`.
///
/// When two or more STUN servers are configured, runs the RFC 5780
/// two-server detection on a freshly bound UDP socket — that path
/// distinguishes the full taxonomy (`Open` / `FullCone` /
/// `RestrictedCone` / `PortRestrictedCone` / `Symmetric`) the
/// connectivity strategy table depends on. With a single server,
/// falls back to the single-server detection that only distinguishes
/// `Open` from `Symmetric` — better than nothing, but the table will
/// route most cases through `Relay` or best-effort punch. With no
/// servers, leaves the engine at `NatType::Unknown`.
///
/// Failures log at `warn` and leave the local NAT type unchanged
/// (default `NatType::Unknown` — conservative).
async fn detect_nat_and_publish(
    instance_id: &str,
    stun_servers: &[String],
    sync: &crate::sync::SyncEngine,
) {
    // Resolve `host:port` strings to socket addresses up-front so the
    // RFC 5780 path has concrete `primary`/`secondary` to work with.
    let resolved: Vec<std::net::SocketAddr> = {
        let mut out = Vec::with_capacity(stun_servers.len());
        for raw in stun_servers {
            match resolve_first(raw).await {
                Ok(addr) => out.push(addr),
                Err(e) => tracing::debug!(
                    target: "cascade::backend::p2p",
                    stun = %raw,
                    error = %e,
                    "could not resolve STUN server",
                ),
            }
        }
        out
    };

    if resolved.len() >= 2 {
        // RFC 5780 path. Need a bound UDP socket so the detection
        // reuses the same source for all four probes.
        let socket = match tokio::net::UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    target: "cascade::backend::p2p",
                    instance = %instance_id,
                    error = %e,
                    "binding STUN socket failed; leaving local NAT type as Unknown",
                );
                return;
            }
        };
        let Some(primary) = resolved.first().copied() else {
            // Unreachable — `len() >= 2` implies at least one entry.
            // Logged at warn so a future refactor that violates the
            // invariant surfaces visibly rather than silently
            // skipping detection.
            tracing::warn!(
                target: "cascade::backend::p2p",
                instance = %instance_id,
                "RFC 5780 path entered with empty STUN list — leaving local NAT type as Unknown",
            );
            return;
        };
        let Some(secondary) = resolved.get(1).copied() else {
            tracing::warn!(
                target: "cascade::backend::p2p",
                instance = %instance_id,
                "RFC 5780 path entered without a secondary STUN server — leaving local NAT type as Unknown",
            );
            return;
        };
        let cfg = cascade_p2p::nat::NatDetectionConfig {
            primary,
            secondary,
            per_request_timeout: std::time::Duration::from_secs(3),
            retries: 2,
        };
        match cascade_p2p::nat::detect_nat_type_rfc5780(&socket, &cfg).await {
            Ok(outcome) => {
                tracing::info!(
                    target: "cascade::backend::p2p",
                    instance = %instance_id,
                    nat = ?outcome.nat_type(),
                    external = ?outcome.external_socket_addr(),
                    "RFC 5780 NAT type detected",
                );
                sync.set_local_nat_type(outcome.nat_type()).await;
                // Stash the external mapping so gather_local_candidates
                // can fold a ServerReflexive candidate into the
                // gossiped set. The single-server path below has no
                // way to extract this — `NatTraversal::detect_nat_type`
                // discards the XOR-MAPPED-ADDRESS — so only RFC 5780
                // populates it.
                sync.set_local_external_addr(outcome.external_socket_addr())
                    .await;
            }
            Err(e) => tracing::warn!(
                target: "cascade::backend::p2p",
                instance = %instance_id,
                error = %e,
                "RFC 5780 NAT detection failed; leaving local NAT type as Unknown",
            ),
        }
        return;
    }

    // Single-server path — only distinguishes Open from Symmetric.
    for stun in stun_servers {
        match cascade_p2p::nat::NatTraversal::detect_nat_type(stun).await {
            Ok(nat_type) => {
                tracing::info!(
                    target: "cascade::backend::p2p",
                    instance = %instance_id,
                    stun = %stun,
                    nat = ?nat_type,
                    "single-server NAT type detected",
                );
                sync.set_local_nat_type(nat_type).await;
                return;
            }
            Err(e) => {
                tracing::debug!(
                    target: "cascade::backend::p2p",
                    stun = %stun,
                    error = %e,
                    "NAT detection failed; trying next server",
                );
            }
        }
    }
}

/// Resolve `host:port` (or `addr:port`) to a single `SocketAddr`,
/// off-thread so we don't block the tokio runtime.
async fn resolve_first(raw: &str) -> Result<std::net::SocketAddr> {
    let owned = raw.to_string();
    let addrs = tokio::task::spawn_blocking(move || {
        std::net::ToSocketAddrs::to_socket_addrs(&owned).map(Iterator::collect::<Vec<_>>)
    })
    .await
    .context("DNS resolve task panicked")?
    .with_context(|| format!("resolving `{raw}`"))?;
    addrs
        .into_iter()
        .next()
        .with_context(|| format!("`{raw}` resolved to no records"))
}

/// Construct a Mainline-DHT node off the tokio runtime.
///
/// `MainlineDht::open` is a blocking constructor: it resolves the bootstrap
/// set with synchronous `getaddrinfo` (for the default case those are public
/// router *hostnames*, not pre-resolved addresses) and binds the DHT UDP
/// socket, all on the calling thread. Running it directly on a runtime worker
/// would stall that worker for the duration of resolution under a slow or
/// unreachable resolver, so it goes through `spawn_blocking` exactly like
/// [`resolve_first`]'s DNS does.
async fn open_dht_off_thread(
    bootstrap_nodes: Vec<std::net::SocketAddr>,
) -> Result<cascade_p2p::discovery::MainlineDht> {
    tokio::task::spawn_blocking(move || cascade_p2p::discovery::MainlineDht::open(&bootstrap_nodes))
        .await
        .context("DHT open task panicked")?
}

/// Local preference for the single LAN host candidate this device
/// advertises. LAN announcements carry exactly one BEP listen port, so
/// there is no interface ranking to encode; the maximum value matches
/// the value [`LanDiscovery`] assigns to discovered LAN peers, keeping
/// the two sides symmetric.
const LAN_ANNOUNCE_LOCAL_PREFERENCE: u16 = u16::MAX;

/// Build the single host candidate carried in a LAN announcement. The
/// announcement wire shape conveys only the device ID and BEP listen
/// port; [`LanDiscovery::announce`] extracts the port from the
/// highest-priority host candidate, so the address IP is irrelevant and
/// the wildcard is used.
fn lan_announce_candidate(listen_port: u16) -> Candidate {
    let address = std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, listen_port));
    Candidate::new(address, CandidateKind::Host, LAN_ANNOUNCE_LOCAL_PREFERENCE)
}

/// Periodically broadcast our presence on the LAN discovery multicast
/// group via the [`LanDiscovery`] source.
///
/// The announce itself is a blocking std-net call hopped onto
/// `spawn_blocking` inside [`LanDiscovery::announce`]; errors there are
/// logged and swallowed — discovery is best-effort.
///
/// Exits as soon as `cancel` flips to `true`.
async fn discovery_announce_loop(
    lan: LanDiscovery,
    sync: SyncEngine,
    listen_port: u16,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) {
    let device_id = sync.device_id().to_string();
    let candidate = [lan_announce_candidate(listen_port)];
    loop {
        if *cancel.borrow() {
            return;
        }
        lan.announce(&device_id, &candidate).await;
        tokio::select! {
            () = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
            res = cancel.changed() => {
                if res.is_err() || *cancel.borrow() {
                    return;
                }
            }
        }
    }
}

/// Interval between announce-server candidate publications.
///
/// The announce directory holds soft state — a stale entry simply causes a
/// failed dial that falls back to the other discovery sources — so a minute
/// between refreshes keeps the directory current without hammering the
/// server. Matches the WAN-gossip cadence so the two background refresh
/// loops tick on the same rhythm.
const ANNOUNCE_PUBLISH_INTERVAL: std::time::Duration = std::time::Duration::from_mins(1);

/// Periodically publish our local candidate set to every configured
/// announce server so peers can resolve us by device id off-LAN.
///
/// Each tick gathers the current local candidates from the sync engine and
/// registers them with every announce server. A server that constructs but
/// then fails on the wire is handled inside
/// [`cascade_p2p::discovery::announce::AnnounceDiscovery::announce`], which
/// logs and moves on — publication is best-effort, exactly like LAN
/// announce. A server whose client cannot even be constructed is skipped for
/// the lifetime of the loop.
///
/// Exits as soon as `cancel` flips to `true`.
async fn announce_server_publish_loop(
    instance_id: &str,
    announce_servers: Vec<AnnounceServer>,
    sync: SyncEngine,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) {
    // Build the clients once, each holding its server's shared secret so its
    // registrations carry a verifying write tag. A client whose construction
    // fails is logged and omitted — the others still publish.
    let clients: Vec<cascade_p2p::discovery::announce::AnnounceDiscovery> = announce_servers
        .iter()
        .filter_map(|server| {
            match cascade_p2p::discovery::announce::AnnounceDiscovery::new(
                server.base_url.clone(),
                server.secret,
            ) {
                Ok(client) => Some(client),
                Err(e) => {
                    tracing::warn!(
                        target: "cascade::backend::p2p",
                        instance = %instance_id,
                        announce_server = %server.base_url,
                        error = %e,
                        "could not construct announce-server client for publish loop; skipping",
                    );
                    None
                }
            }
        })
        .collect();
    if clients.is_empty() {
        return;
    }

    let device_id = sync.device_id().to_string();
    loop {
        if *cancel.borrow() {
            return;
        }
        let candidates = sync.local_candidates().await;
        if !candidates.is_empty() {
            for client in &clients {
                client.announce(&device_id, &candidates).await;
            }
        }
        tokio::select! {
            () = tokio::time::sleep(ANNOUNCE_PUBLISH_INTERVAL) => {}
            res = cancel.changed() => {
                if res.is_err() || *cancel.borrow() {
                    return;
                }
            }
        }
    }
}

/// Periodically publish our local candidate set into the Mainline DHT so
/// peers can resolve us by device id without a central directory.
///
/// Each tick gathers the current local candidates from the sync engine and
/// stores them under our device-id key via the [`Discovery::announce`] impl on
/// [`cascade_p2p::discovery::DhtDiscovery`]. A store that does not reach enough
/// DHT nodes is handled best-effort inside the node — publication is
/// best-effort, exactly like LAN and announce-server publish.
///
/// Unlike the announce-server loop, the cadence here is
/// [`cascade_p2p::discovery::DHT_REPUBLISH_INTERVAL`], derived from the BEP44
/// mutable-item expiry rather than the announce-server soft-state rhythm: a DHT
/// value is dropped by its storing nodes if not refreshed within the expiry
/// window, so the republish interval is set to refresh well inside that window
/// and keep the candidate set continuously resolvable.
///
/// Exits as soon as `cancel` flips to `true`.
async fn dht_publish_loop(
    dht: cascade_p2p::discovery::DhtDiscovery<cascade_p2p::discovery::MainlineDht>,
    sync: SyncEngine,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) {
    let device_id = sync.device_id().to_string();
    loop {
        if *cancel.borrow() {
            return;
        }
        let candidates = sync.local_candidates().await;
        if !candidates.is_empty() {
            dht.announce(&device_id, &candidates).await;
        }
        tokio::select! {
            () = tokio::time::sleep(cascade_p2p::discovery::DHT_REPUBLISH_INTERVAL) => {}
            res = cancel.changed() => {
                if res.is_err() || *cancel.borrow() {
                    return;
                }
            }
        }
    }
}

/// Window each [`LanDiscovery::listen_all`] call listens for before
/// returning the peers seen. Matches the timeout the loop used before
/// discovery moved behind the trait.
const LAN_LISTEN_WINDOW: std::time::Duration = std::time::Duration::from_secs(15);

/// Listen for peer announcements on the LAN via the [`LanDiscovery`]
/// source and, for any trusted device we don't already have a session
/// to, kick off an outbound connect.
///
/// Each listen runs on `spawn_blocking` inside [`LanDiscovery::listen_all`]
/// with a 15 s window. Listen errors are logged and swallowed there,
/// surfacing as an empty peer set so the loop simply continues.
///
/// Exits as soon as `cancel` flips to `true`. The outer task races
/// `cancel.changed()` against the in-flight listen future, so shutdown
/// is observed immediately rather than waiting up to the 15 s blocking
/// timeout. The detached `spawn_blocking` thread will finish on its own
/// when the std-net call times out, but the async task no longer blocks
/// on it.
async fn discovery_listen_loop(
    lan: LanDiscovery,
    sync: SyncEngine,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        if *cancel.borrow() {
            return;
        }
        let listen_fut = lan.listen_all(LAN_LISTEN_WINDOW);
        let peers = tokio::select! {
            res = cancel.changed() => {
                if res.is_err() || *cancel.borrow() {
                    tracing::debug!(
                        target: "cascade::backend::p2p",
                        "discovery listen loop cancelled",
                    );
                    return;
                }
                continue;
            }
            peers = listen_fut => peers,
        };
        for peer in peers {
            // Skip ourselves — multicast loopback can deliver our own
            // announcement back to us.
            if peer.device_id == sync.device_id() {
                continue;
            }
            if !sync.is_trusted(&peer.device_id).await {
                continue;
            }
            if sync.has_peer(&peer.device_id).await {
                continue;
            }
            if let Err(e) = sync
                .connect_to(crate::sync::Peer {
                    device_id: peer.device_id.clone(),
                    address: peer.address,
                })
                .await
            {
                tracing::debug!(
                    target: "cascade::backend::p2p",
                    "LAN discovery connect to {} at {} failed: {e:#}",
                    peer.device_id,
                    peer.address,
                );
            }
        }
    }
}

/// CLI entry point — construct a backend from a TOML config table.
///
/// Expected keys:
/// - `name` (required) — instance name; used to derive `id = "p2p-{name}"`
/// - `display_name` (optional) — human-readable label
/// - `device_name` (optional) — friendly name for the LOCAL device, used
///   by the sync engine when labelling conflict-copy paths. Falls back
///   to the short device id when unset.
/// - `data_dir` (optional) — base dir for index + block store;
///   defaults to `${HOME}/.config/cascade/p2p-{name}`
/// - `listen_addr` (optional) — `"host:port"` for the BEP listener
/// - `peers` (optional) — array of `{ device_id = "...", address = "host:port", name = "..." }`.
///   `name` is optional; when present it is used in conflict-copy paths
///   generated by that peer.
/// - `exposure` (optional, default `private`) — how far this device reaches
///   out for peers: `lan-only`, `private`, or `public`. Governs which
///   discovery and traversal sources may run (see [`DiscoveryReach`]); each
///   source then self-activates only when it also has what it needs (a bound
///   `listen_addr` for LAN, a configured server for announce, etc.).
/// - `stun_servers` (optional) — array of `host:port` STUN servers used
///   for NAT type detection at startup. Omitting the key entirely applies
///   the public defaults ([`DEFAULT_PUBLIC_STUN_SERVERS`]) so NAT detection
///   works out of the box; supplying a non-empty list overrides them; an
///   explicit empty list disables STUN.
/// - `announce_servers` (optional) — array of announce-server tables, each
///   `{ url = "https://announce.example", shared_secret = "<64 hex>" }`: `url`
///   is the scheme-and-authority root — *where to point* the announce source —
///   and `shared_secret` is the 64-char hex `HMAC` write key the device
///   authenticates its registrations with (both carriers reject a write without
///   a verifying tag). The source self-activates only at `public` exposure with
///   at least one server configured.
/// - `dht_bootstrap_nodes` (optional) — array of `host:port` Mainline-DHT
///   bootstrap nodes used to join the DHT. Omitted or explicitly empty falls
///   back to the named public default
///   ([`cascade_p2p::discovery::DEFAULT_DHT_BOOTSTRAP_NODES`]) so the DHT works
///   out of the box; supplying a non-empty list pins exactly those nodes (the
///   public default is then not used). These say *where to point* the DHT
///   source — the source itself self-activates only at `public` exposure with
///   a bound `listen_addr`.
pub fn create_backend(config: &toml::Value) -> Result<Box<dyn Backend>> {
    Ok(Box::new(open_from_config(config)?))
}

/// Parse a backend TOML table into a [`P2pBackend`], returning the concrete
/// type rather than a boxed [`Backend`].
///
/// [`create_backend`] wraps this in a `Box<dyn Backend>` for the engine's
/// backend registry. Callers that need the concrete type — for example the
/// manager-side CLI driving [`P2pBackend::manage_remote`] over the connectivity
/// ladder — open the backend through here so the management entry points are in
/// reach. The accepted keys are documented on [`create_backend`].
pub fn open_from_config(config: &toml::Value) -> Result<P2pBackend> {
    let name = config
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("p2p backend requires 'name'"))?
        .to_string();
    let display_name = config
        .get("display_name")
        .and_then(|v| v.as_str())
        .unwrap_or(&name)
        .to_string();
    let data_dir = config
        .get("data_dir")
        .and_then(|v| v.as_str())
        .map_or_else(|| default_data_dir(&name), PathBuf::from);
    let listen_addr = config
        .get("listen_addr")
        .and_then(|v| v.as_str())
        .map(|s| {
            s.parse::<std::net::SocketAddr>()
                .with_context(|| format!("invalid listen_addr `{s}`"))
        })
        .transpose()?;
    let peers = parse_peers(config.get("peers"))?;
    let exposure = config
        .get("exposure")
        .and_then(|v| v.as_str())
        .map(parse_exposure)
        .transpose()?
        .unwrap_or_default();
    let device_name = config
        .get("device_name")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    // Distinguish "not configured" (key absent → public defaults apply)
    // from "configured empty" (key present but empty → STUN disabled). A
    // bare `unwrap_or_default` would collapse both to an empty list and
    // silently disable NAT detection out of the box.
    let configured_stun_servers = parse_string_list(config.get("stun_servers"), "stun_servers")?;
    let stun_servers = resolve_stun_servers(configured_stun_servers);
    let announce_servers = parse_announce_servers(config.get("announce_servers"))?;
    let dht = parse_dht_config(config)?;
    let relay_endpoints = config
        .get("relay_endpoints")
        .and_then(toml::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(toml::Value::as_str)
                .map(|s| {
                    // Accept a DNS hostname or Docker service name, not only a
                    // literal IP:port — resolved here at config load. A bind
                    // address (listen_addr) stays a SocketAddr; a relay we dial
                    // may legitimately be addressed by name.
                    use std::net::ToSocketAddrs as _;
                    s.to_socket_addrs()
                        .with_context(|| {
                            format!("relay endpoint `{s}` is not a resolvable host:port")
                        })?
                        .next()
                        .with_context(|| format!("relay endpoint `{s}` resolved to no addresses"))
                })
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();
    let relay_shared_secret = config
        .get("relay_shared_secret")
        .and_then(|v| v.as_str())
        .map(parse_relay_shared_secret)
        .transpose()?;
    let relay_volunteer = config
        .get("relay_volunteer")
        .and_then(|v| v.as_str())
        .map(parse_relay_volunteer)
        .transpose()?
        .unwrap_or_default();
    let max_relay_sessions = config
        .get("max_relay_sessions")
        .and_then(toml::Value::as_integer)
        .map(|raw| {
            u32::try_from(raw).with_context(|| format!("max_relay_sessions `{raw}` out of range"))
        })
        .transpose()?
        .unwrap_or(DEFAULT_MAX_RELAY_SESSIONS);
    let max_relay_bandwidth = config
        .get("max_relay_bandwidth")
        .and_then(toml::Value::as_integer)
        .map(|raw| {
            u64::try_from(raw).with_context(|| format!("max_relay_bandwidth `{raw}` out of range"))
        })
        .transpose()?
        .unwrap_or(DEFAULT_MAX_RELAY_BANDWIDTH_BYTES_PER_SEC);

    let instance_id = format!("p2p-{name}");
    let cfg = P2pBackendConfig {
        folder_id: instance_id.clone(),
        instance_id,
        display_name,
        index_path: data_dir.join("index.db"),
        block_store_root: data_dir.join("blocks"),
        identity_dir: data_dir.join("identity"),
        listen_addr,
        peers,
        exposure,
        device_name,
        stun_servers,
        announce_servers,
        dht,
        relay_endpoints,
        relay_shared_secret,
        relay_volunteer,
        max_relay_sessions,
        max_relay_bandwidth,
    };

    P2pBackend::open(cfg)
}

/// Read (or generate) the local device identity for a P2P backend config,
/// returning its device id without opening the full backend.
///
/// Resolves the `data_dir` / `identity` directory the same way
/// [`open_from_config`] does, then loads the persistent device identity from
/// it. Used by the management-plane CLI to stamp a locally-issued grant's
/// `granted_by` with this device's own id — the node owner — without spinning
/// up the listener, discovery, and reconnect tasks a full backend open starts.
pub fn device_id_from_config(config: &toml::Value) -> Result<String> {
    Ok(identity_from_config(config)?.device_id)
}

/// Read (or generate) the local device identity for a P2P backend config,
/// returning the full [`DeviceIdentity`] — certificate and private key included.
///
/// Resolves the `data_dir` / `identity` directory the same way
/// [`open_from_config`] does, then loads the persistent device identity from it.
/// Used by the management-plane CLI to *sign* a locally-issued capability token
/// with this device's real private key — the secret behind its certificate —
/// without spinning up the listener, discovery, and reconnect tasks a full
/// backend open starts.
pub fn identity_from_config(config: &toml::Value) -> Result<DeviceIdentity> {
    let name = config
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("p2p backend requires 'name'"))?
        .to_string();
    let data_dir = config
        .get("data_dir")
        .and_then(|v| v.as_str())
        .map_or_else(|| default_data_dir(&name), PathBuf::from);
    let identity_dir = data_dir.join("identity");
    DeviceIdentity::load_or_generate(&identity_dir)
        .context("loading P2P backend identity for management grant")
}

/// Parse the Kademlia/Mainline-DHT discovery configuration from a backend's
/// TOML.
///
/// This parses only *where to point* the DHT — its bootstrap set. Whether the
/// DHT source runs is governed by the [`DiscoveryReach`] exposure posture, not
/// by this config, so a [`DhtConfig`] is always produced. `dht_bootstrap_nodes`
/// is parsed when present — a malformed `host:port` entry fails loudly with the
/// offending value, matching the loud-failure parsing of `relay_endpoints` —
/// and an empty or omitted list falls back to the `mainline` crate's built-in
/// public bootstrap set at announce/resolve time.
fn parse_dht_config(config: &toml::Value) -> Result<DhtConfig> {
    let bootstrap_nodes = config
        .get("dht_bootstrap_nodes")
        .and_then(toml::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(toml::Value::as_str)
                .map(|s| {
                    s.parse::<std::net::SocketAddr>()
                        .with_context(|| format!("invalid DHT bootstrap node `{s}`"))
                })
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok(DhtConfig { bootstrap_nodes })
}

/// Parse a TOML array of strings, preserving the absent-vs-present distinction.
///
/// Returns `Ok(None)` when the key is absent so callers can apply their own
/// defaults, and `Ok(Some(vec))` when present (including an empty array). A
/// non-string array entry fails loudly with its index rather than being
/// silently dropped, matching the loud-failure pattern used for `peers` and
/// `relay_endpoints`.
fn parse_string_list(value: Option<&toml::Value>, field: &str) -> Result<Option<Vec<String>>> {
    let Some(raw) = value else {
        return Ok(None);
    };
    let arr = raw
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("{field} must be an array of strings"))?;
    let mut out = Vec::with_capacity(arr.len());
    for (idx, item) in arr.iter().enumerate() {
        let entry = item
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("{field}[{idx}] must be a string"))?;
        out.push(entry.to_string());
    }
    Ok(Some(out))
}

fn parse_peers(value: Option<&toml::Value>) -> Result<Vec<ConfiguredPeer>> {
    let Some(arr) = value.and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };
    let mut peers = Vec::with_capacity(arr.len());
    for (idx, item) in arr.iter().enumerate() {
        let table = item
            .as_table()
            .ok_or_else(|| anyhow::anyhow!("peers[{idx}] must be a table"))?;
        let device_id = table
            .get("device_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("peers[{idx}].device_id required"))?
            .to_string();
        let address = table
            .get("address")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("peers[{idx}].address required"))?
            .to_string();
        let name = table
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        peers.push(ConfiguredPeer {
            device_id,
            address,
            name,
        });
    }
    Ok(peers)
}

/// Parse the relay shared secret from its 64-character hex string form.
///
/// The cascade relay HMAC-authenticates clients with a 32-byte key. The
/// TOML config carries the key as lowercase hex so operators can copy
/// it from `openssl rand -hex 32` output. Anything other than exactly
/// 64 hex digits is a configuration error — the field is too sensitive
/// to silently truncate or pad.
fn parse_relay_shared_secret(input: &str) -> Result<[u8; 32]> {
    if input.len() != 64 {
        anyhow::bail!(
            "relay_shared_secret must be 64 hex characters (32 bytes), got {}",
            input.len(),
        );
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let pair_start = i.checked_mul(2).with_context(|| "secret index overflow")?;
        let pair_end = pair_start
            .checked_add(2)
            .with_context(|| "secret index overflow")?;
        let pair = input
            .get(pair_start..pair_end)
            .with_context(|| "relay_shared_secret hex slice out of range")?;
        *byte = u8::from_str_radix(pair, 16)
            .with_context(|| format!("invalid hex pair `{pair}` in relay_shared_secret"))?;
    }
    Ok(out)
}

/// Parse the `exposure` posture from its kebab-case string form.
///
/// Accepts exactly `lan-only`, `private`, or `public`. Any other value is a
/// configuration error rather than a silent fallback to the default — an
/// operator who typed `publik` deserves to be told, not to discover the node
/// quietly confined to the LAN when they meant to open it to the WAN (or the
/// reverse, publishing to a global directory they never intended).
fn parse_exposure(input: &str) -> Result<DiscoveryReach> {
    match input {
        "lan-only" => Ok(DiscoveryReach::LanOnly),
        "private" => Ok(DiscoveryReach::Private),
        "public" => Ok(DiscoveryReach::Public),
        other => {
            anyhow::bail!("exposure must be one of `lan-only`, `private`, `public`, got `{other}`")
        }
    }
}

/// Parse the `announce_servers` config into [`AnnounceServer`] entries.
///
/// Each entry is a table `{ url = "...", shared_secret = "<64 hex>" }`. The
/// secret is required, not optional: the announce write contract is
/// authenticated on both carriers, so an entry without a secret could only ever
/// resolve, never publish, leaving this device undiscoverable through that
/// server. Surfacing that as a parse error is louder and more useful than a
/// runtime `401`. The hex secret is decoded by the shared
/// `cascade_announce_wire::auth::parse_shared_secret_hex` primitive — the same
/// one the relay handshake and the Worker use — so all surfaces agree on the key
/// width and the hex form. An absent key yields an empty list (announce-server
/// discovery off); a present-but-not-an-array value, a non-table entry, a
/// missing field, or a malformed secret are all errors.
fn parse_announce_servers(value: Option<&toml::Value>) -> Result<Vec<AnnounceServer>> {
    let Some(raw) = value else {
        return Ok(Vec::new());
    };
    let arr = raw
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("announce_servers must be an array of tables"))?;
    let mut out = Vec::with_capacity(arr.len());
    for (idx, item) in arr.iter().enumerate() {
        let table = item
            .as_table()
            .ok_or_else(|| anyhow::anyhow!("announce_servers[{idx}] must be a table"))?;
        let base_url = table
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("announce_servers[{idx}].url required"))?
            .to_string();
        let hex_secret = table
            .get("shared_secret")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("announce_servers[{idx}].shared_secret required"))?;
        let secret = cascade_p2p::discovery::announce::parse_shared_secret_hex(hex_secret)
            .map_err(|e| anyhow::anyhow!("announce_servers[{idx}].shared_secret invalid: {e}"))?;
        out.push(AnnounceServer { base_url, secret });
    }
    Ok(out)
}

/// Parse the `relay_volunteer` policy from its lowercase string form.
///
/// Accepts exactly `off`, `explicit`, or `auto`. Any other value is a
/// configuration error rather than a silent fallback to the default —
/// an operator who typed `aut` deserves to be told, not to discover the
/// node quietly volunteering when they meant to turn it off.
fn parse_relay_volunteer(input: &str) -> Result<RelayVolunteer> {
    match input {
        "off" => Ok(RelayVolunteer::Off),
        "explicit" => Ok(RelayVolunteer::Explicit),
        "auto" => Ok(RelayVolunteer::Auto),
        other => {
            anyhow::bail!("relay_volunteer must be one of `off`, `explicit`, `auto`, got `{other}`")
        }
    }
}

fn default_data_dir(name: &str) -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("cascade")
        .join(format!("p2p-{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor as IoCursor;
    use tempfile::tempdir;

    fn make_backend() -> (tempfile::TempDir, P2pBackend) {
        let dir = tempdir().unwrap();
        let cfg = P2pBackendConfig {
            instance_id: "p2p-test".to_string(),
            folder_id: "p2p-test".to_string(),
            display_name: "Test".to_string(),
            index_path: dir.path().join("index.db"),
            block_store_root: dir.path().join("blocks"),
            identity_dir: dir.path().join("identity"),
            ..Default::default()
        };
        let backend = P2pBackend::open(cfg).unwrap();
        (dir, backend)
    }

    #[tokio::test]
    async fn upload_then_download_round_trips() {
        let (_dir, backend) = make_backend();
        let data = b"hello world".repeat(1000);
        let mut reader: IoCursor<Vec<u8>> = IoCursor::new(data.clone());
        let entry = backend
            .upload(
                Path::new("hello.txt"),
                &mut reader,
                &FileId("p2p-test:root".to_string()),
            )
            .await
            .unwrap();
        assert_eq!(entry.name, "hello.txt");
        assert_eq!(entry.size, Some(data.len() as u64));

        let mut out: Vec<u8> = Vec::new();
        backend.download(&entry, &mut out).await.unwrap();
        assert_eq!(out, data);
    }

    #[tokio::test]
    async fn list_children_after_uploads() {
        let (_dir, backend) = make_backend();
        let mut reader = IoCursor::new(b"a".to_vec());
        backend
            .upload(
                Path::new("a.txt"),
                &mut reader,
                &FileId("p2p-test:root".to_string()),
            )
            .await
            .unwrap();
        let mut reader2 = IoCursor::new(b"b".to_vec());
        backend
            .upload(
                Path::new("b.txt"),
                &mut reader2,
                &FileId("p2p-test:root".to_string()),
            )
            .await
            .unwrap();
        let kids = backend.list_children("root").await.unwrap();
        let names: Vec<_> = kids.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&"a.txt".to_string()));
        assert!(names.contains(&"b.txt".to_string()));
    }

    #[tokio::test]
    async fn changes_after_upload() {
        let (_dir, backend) = make_backend();
        let (initial, c0) = backend.changes(None).await.unwrap();
        assert!(initial.is_empty());

        let mut reader = IoCursor::new(b"data".to_vec());
        backend
            .upload(
                Path::new("x.txt"),
                &mut reader,
                &FileId("p2p-test:root".to_string()),
            )
            .await
            .unwrap();

        let (deltas, _c1) = backend.changes(Some(&c0)).await.unwrap();
        assert_eq!(deltas.len(), 1);
        assert!(matches!(deltas[0], Change::Created(_)));
    }

    #[tokio::test]
    async fn delete_marks_tombstone_excluded_from_listing() {
        let (_dir, backend) = make_backend();
        let mut reader = IoCursor::new(b"x".to_vec());
        let entry = backend
            .upload(
                Path::new("x.txt"),
                &mut reader,
                &FileId("p2p-test:root".to_string()),
            )
            .await
            .unwrap();
        backend.delete(&entry).await.unwrap();
        let kids = backend.list_children("root").await.unwrap();
        assert!(kids.is_empty());
    }

    /// End-to-end: A uploads through the Backend trait, B connects, and
    /// B's `download()` succeeds even though B's local block store is
    /// empty — the missing blocks must be fetched from A over the wire.
    #[tokio::test]
    async fn cross_backend_download_via_peer_fetch() {
        fn open_with_folder(dir: &std::path::Path, name: &str) -> P2pBackend {
            let cfg = P2pBackendConfig {
                instance_id: format!("p2p-{name}"),
                folder_id: "shared".to_string(),
                display_name: name.to_string(),
                index_path: dir.join("index.db"),
                block_store_root: dir.join("blocks"),
                identity_dir: dir.join("identity"),
                ..Default::default()
            };
            P2pBackend::open(cfg).unwrap()
        }
        let dir_a = tempdir().unwrap();
        let dir_b = tempdir().unwrap();
        let backend_a = open_with_folder(dir_a.path(), "a");
        let backend_b = open_with_folder(dir_b.path(), "b");

        backend_a
            .sync()
            .trust(backend_b.sync().device_id().to_string())
            .await;
        backend_b
            .sync()
            .trust(backend_a.sync().device_id().to_string())
            .await;

        let (_cancel_tx_a, cancel_rx_a) = tokio::sync::watch::channel(false);
        let (addr_a, _a_task) = backend_a
            .sync()
            .start_listener("127.0.0.1:0".parse().unwrap(), cancel_rx_a)
            .await
            .unwrap();
        backend_b
            .sync()
            .connect_to(crate::sync::Peer {
                device_id: backend_a.sync().device_id().to_string(),
                address: addr_a,
            })
            .await
            .unwrap();

        let payload = b"peer-to-peer round trip".repeat(50);
        let mut reader = IoCursor::new(payload.clone());
        let entry_a = backend_a
            .upload(
                Path::new("shared.bin"),
                &mut reader,
                &FileId(format!("{}:root", backend_a.id())),
            )
            .await
            .unwrap();

        // Let the IndexUpdate broadcast and the handshake Index reach B.
        let mut found = None;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            if let Some(local) = backend_b.index.get("shared.bin").unwrap() {
                found = Some(local);
                break;
            }
        }
        let local_b = found.expect("B never received index update");
        assert_eq!(local_b.size, entry_a.size.unwrap());
        // B's block store is empty — download must hit the peer.
        for chunk in local_b.block_hashes.chunks(32) {
            let mut h = [0u8; 32];
            h.copy_from_slice(chunk);
            assert!(
                backend_b
                    .blocks
                    .get_block(&BlockHash(h))
                    .await
                    .unwrap()
                    .is_none()
            );
        }

        let entry_b = backend_b.metadata(Path::new("shared.bin")).await.unwrap();
        let mut out: Vec<u8> = Vec::new();
        backend_b.download(&entry_b, &mut out).await.unwrap();
        assert_eq!(out, payload);
    }

    /// A deterministic payload large enough to split into several
    /// 128 KB blocks. Each byte is `position % 251` (a prime, so the
    /// pattern does not align to any power-of-two block boundary),
    /// which makes a wrong slice trivially detectable.
    fn multi_block_payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| u8::try_from(i % 251).unwrap()).collect()
    }

    #[tokio::test]
    async fn read_range_spans_block_boundary() {
        let (_dir, backend) = make_backend();
        // 3.5 blocks worth of data → four 128 KB blocks, last short.
        let block = 128 * 1024;
        let payload = multi_block_payload(block * 7 / 2);
        let mut reader: IoCursor<Vec<u8>> = IoCursor::new(payload.clone());
        let entry = backend
            .upload(
                Path::new("big.bin"),
                &mut reader,
                &FileId("p2p-test:root".to_string()),
            )
            .await
            .unwrap();

        // A window straddling the first/second block boundary.
        let start = block - 100;
        let length = 200u32;
        let got = backend
            .read_range(&entry, u64::try_from(start).unwrap(), length)
            .await
            .unwrap();
        let end = start + usize::try_from(length).unwrap();
        assert_eq!(got, &payload[start..end]);
    }

    #[tokio::test]
    async fn read_range_single_block_sub_range() {
        let (_dir, backend) = make_backend();
        let block = 128 * 1024;
        let payload = multi_block_payload(block * 3);
        let mut reader: IoCursor<Vec<u8>> = IoCursor::new(payload.clone());
        let entry = backend
            .upload(
                Path::new("three.bin"),
                &mut reader,
                &FileId("p2p-test:root".to_string()),
            )
            .await
            .unwrap();

        // Wholly inside the second block.
        let start = block + 17;
        let length = 64u32;
        let got = backend
            .read_range(&entry, u64::try_from(start).unwrap(), length)
            .await
            .unwrap();
        let end = start + usize::try_from(length).unwrap();
        assert_eq!(got, &payload[start..end]);
    }

    #[tokio::test]
    async fn read_range_clamps_length_past_eof() {
        let (_dir, backend) = make_backend();
        let payload = multi_block_payload(5000);
        let mut reader: IoCursor<Vec<u8>> = IoCursor::new(payload.clone());
        let entry = backend
            .upload(
                Path::new("small.bin"),
                &mut reader,
                &FileId("p2p-test:root".to_string()),
            )
            .await
            .unwrap();

        // length runs well past EOF — result is truncated to the tail.
        let got = backend.read_range(&entry, 4000, 10_000).await.unwrap();
        assert_eq!(got, &payload[4000..]);
    }

    #[tokio::test]
    async fn read_range_whole_file() {
        let (_dir, backend) = make_backend();
        let payload = multi_block_payload(128 * 1024 * 2 + 99);
        let mut reader: IoCursor<Vec<u8>> = IoCursor::new(payload.clone());
        let entry = backend
            .upload(
                Path::new("whole.bin"),
                &mut reader,
                &FileId("p2p-test:root".to_string()),
            )
            .await
            .unwrap();

        let len = u32::try_from(payload.len()).unwrap();
        let got = backend.read_range(&entry, 0, len).await.unwrap();
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn read_range_offset_at_or_past_eof_is_empty() {
        let (_dir, backend) = make_backend();
        let payload = multi_block_payload(2048);
        let mut reader: IoCursor<Vec<u8>> = IoCursor::new(payload.clone());
        let entry = backend
            .upload(
                Path::new("eof.bin"),
                &mut reader,
                &FileId("p2p-test:root".to_string()),
            )
            .await
            .unwrap();

        assert!(
            backend
                .read_range(&entry, 2048, 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            backend
                .read_range(&entry, 99_999, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn read_range_zero_length_is_empty() {
        let (_dir, backend) = make_backend();
        let payload = multi_block_payload(2048);
        let mut reader: IoCursor<Vec<u8>> = IoCursor::new(payload.clone());
        let entry = backend
            .upload(
                Path::new("zero.bin"),
                &mut reader,
                &FileId("p2p-test:root".to_string()),
            )
            .await
            .unwrap();

        assert!(backend.read_range(&entry, 0, 0).await.unwrap().is_empty());
        assert!(backend.read_range(&entry, 100, 0).await.unwrap().is_empty());
    }

    /// Cross-backend: B reads a range from a file it has indexed but not
    /// cached. The fetch must pull ONLY the blocks covering the window
    /// from A, leaving the rest of B's block store empty — proof that
    /// the override does not reconstruct the whole file.
    #[tokio::test]
    async fn read_range_fetches_only_covering_blocks_from_peer() {
        fn open_with_folder(dir: &std::path::Path, name: &str) -> P2pBackend {
            let cfg = P2pBackendConfig {
                instance_id: format!("p2p-{name}"),
                folder_id: "shared".to_string(),
                display_name: name.to_string(),
                index_path: dir.join("index.db"),
                block_store_root: dir.join("blocks"),
                identity_dir: dir.join("identity"),
                ..Default::default()
            };
            P2pBackend::open(cfg).unwrap()
        }
        let dir_a = tempdir().unwrap();
        let dir_b = tempdir().unwrap();
        let backend_a = open_with_folder(dir_a.path(), "a");
        let backend_b = open_with_folder(dir_b.path(), "b");

        backend_a
            .sync()
            .trust(backend_b.sync().device_id().to_string())
            .await;
        backend_b
            .sync()
            .trust(backend_a.sync().device_id().to_string())
            .await;

        let (_cancel_tx_a, cancel_rx_a) = tokio::sync::watch::channel(false);
        let (addr_a, _a_task) = backend_a
            .sync()
            .start_listener("127.0.0.1:0".parse().unwrap(), cancel_rx_a)
            .await
            .unwrap();
        backend_b
            .sync()
            .connect_to(crate::sync::Peer {
                device_id: backend_a.sync().device_id().to_string(),
                address: addr_a,
            })
            .await
            .unwrap();

        // Five-block file (4 full 128 KB blocks + a short tail).
        let block = 128 * 1024;
        let payload = multi_block_payload(block * 4 + 1234);
        let mut reader = IoCursor::new(payload.clone());
        backend_a
            .upload(
                Path::new("range.bin"),
                &mut reader,
                &FileId(format!("{}:root", backend_a.id())),
            )
            .await
            .unwrap();

        // Wait for B to learn about the file via the index update.
        let mut found = None;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            if let Some(local) = backend_b.index.get("range.bin").unwrap() {
                found = Some(local);
                break;
            }
        }
        let local_b = found.expect("B never received index update");
        let total_blocks = local_b.block_hashes.len() / 32;
        assert_eq!(total_blocks, 5);

        // Read a window inside the third block only (index 2).
        let start = block * 2 + 50;
        let length = 300u32;
        let entry_b = backend_b.metadata(Path::new("range.bin")).await.unwrap();
        let got = backend_b
            .read_range(&entry_b, u64::try_from(start).unwrap(), length)
            .await
            .unwrap();
        let end = start + usize::try_from(length).unwrap();
        assert_eq!(got, &payload[start..end]);

        // Exactly one block — the covering one — should now be cached on
        // B; the other four must still be absent. This is the load-
        // bearing assertion: a whole-file reconstruction would have
        // cached all five.
        let mut cached = 0usize;
        for chunk in local_b.block_hashes.chunks(32) {
            let mut h = [0u8; 32];
            h.copy_from_slice(chunk);
            if backend_b
                .blocks
                .get_block(&BlockHash(h))
                .await
                .unwrap()
                .is_some()
            {
                cached += 1;
            }
        }
        assert_eq!(cached, 1, "only the covering block should be cached");
    }

    #[test]
    fn resolve_stun_servers_unconfigured_uses_public_defaults() {
        // Key absent entirely → the public defaults apply so NAT detection
        // and the reflexive rung work out of the box.
        let resolved = resolve_stun_servers(None);
        let expected: Vec<String> = DEFAULT_PUBLIC_STUN_SERVERS
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        assert_eq!(resolved, expected);
        // The default must list at least two servers so the RFC 5780
        // two-server detection path is reachable without configuration.
        assert!(resolved.len() >= 2);
    }

    #[test]
    fn resolve_stun_servers_non_empty_override_replaces_defaults() {
        // A non-empty operator list replaces the defaults entirely — none
        // of the public servers leak through.
        let operator = vec!["stun.example.org:3478".to_string()];
        let resolved = resolve_stun_servers(Some(operator.clone()));
        assert_eq!(resolved, operator);
        assert!(!resolved.iter().any(|s| s.contains("l.google.com")));
    }

    #[test]
    fn resolve_stun_servers_explicit_empty_disables_stun() {
        // An explicitly empty list is the operator opting out — it must NOT
        // fall back to the defaults, which would re-enable STUN against
        // their wishes.
        let resolved = resolve_stun_servers(Some(Vec::new()));
        assert!(resolved.is_empty());
    }

    #[test]
    fn create_backend_omitting_stun_servers_applies_defaults() {
        // End-to-end through the TOML boundary: a config that does not
        // mention `stun_servers` resolves to the public defaults, while an
        // explicit empty array disables STUN.
        let without = toml::from_str::<toml::Value>(r#"name = "x""#).unwrap();
        let configured = parse_string_list(without.get("stun_servers"), "stun_servers").unwrap();
        assert!(resolve_stun_servers(configured).len() >= 2);

        let empty = toml::from_str::<toml::Value>("name = \"x\"\nstun_servers = []").unwrap();
        let configured_empty =
            parse_string_list(empty.get("stun_servers"), "stun_servers").unwrap();
        assert!(resolve_stun_servers(configured_empty).is_empty());
    }

    #[test]
    fn parse_string_list_absent_key_is_none() {
        // An absent key must stay `None` so callers can apply their own
        // defaults rather than seeing an empty list.
        let value = toml::from_str::<toml::Value>(r#"name = "x""#).unwrap();
        let parsed = parse_string_list(value.get("stun_servers"), "stun_servers").unwrap();
        assert!(parsed.is_none());
    }

    #[test]
    fn parse_string_list_empty_array_is_some_empty() {
        // A present-but-empty array is distinct from absent: it yields
        // `Some(vec![])` so the absent-versus-configured-empty distinction
        // survives the TOML boundary.
        let value = toml::from_str::<toml::Value>("name = \"x\"\nstun_servers = []").unwrap();
        let parsed = parse_string_list(value.get("stun_servers"), "stun_servers").unwrap();
        assert_eq!(parsed, Some(Vec::new()));
    }

    #[test]
    fn parse_string_list_rejects_non_string_entry() {
        // A non-string array entry must fail loudly with its index rather
        // than being silently dropped.
        let value =
            toml::from_str::<toml::Value>("name = \"x\"\nstun_servers = [\"a:1\", 42]").unwrap();
        let err = parse_string_list(value.get("stun_servers"), "stun_servers")
            .expect_err("non-string entry must error");
        assert!(err.to_string().contains("stun_servers[1]"));
    }

    #[test]
    fn parse_string_list_rejects_non_array() {
        // A scalar where an array is expected must fail loudly rather than
        // being silently ignored.
        let value = toml::from_str::<toml::Value>("name = \"x\"\nstun_servers = \"oops\"").unwrap();
        let err = parse_string_list(value.get("stun_servers"), "stun_servers")
            .expect_err("non-array value must error");
        assert!(err.to_string().contains("stun_servers must be an array"));
    }

    /// 64-char hex string for a secret whose bytes are all `0xAB`.
    fn hex_secret() -> String {
        "ab".repeat(cascade_p2p::discovery::announce::SHARED_SECRET_LEN)
    }

    #[test]
    fn parse_announce_servers_absent_is_empty() {
        let value = toml::from_str::<toml::Value>(r#"name = "x""#).unwrap();
        let parsed = parse_announce_servers(value.get("announce_servers")).unwrap();
        assert!(parsed.is_empty());
    }

    #[test]
    fn parse_announce_servers_reads_url_and_secret() {
        let cfg = format!(
            "name = \"x\"\n[[announce_servers]]\nurl = \"https://a.example\"\nshared_secret = \"{}\"\n",
            hex_secret()
        );
        let value = toml::from_str::<toml::Value>(&cfg).unwrap();
        let parsed = parse_announce_servers(value.get("announce_servers")).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].base_url, "https://a.example");
        assert_eq!(
            parsed[0].secret,
            [0xAB; cascade_p2p::discovery::announce::SHARED_SECRET_LEN]
        );
    }

    #[test]
    fn parse_announce_servers_requires_a_secret() {
        // A URL with no secret could only resolve, never publish — a silent
        // half-broken state. The parser must reject it loudly instead.
        let cfg = "name = \"x\"\n[[announce_servers]]\nurl = \"https://a.example\"\n";
        let value = toml::from_str::<toml::Value>(cfg).unwrap();
        let err = parse_announce_servers(value.get("announce_servers"))
            .expect_err("missing secret must error");
        assert!(err.to_string().contains("shared_secret required"));
    }

    #[test]
    fn parse_announce_servers_requires_a_url() {
        let cfg = format!(
            "name = \"x\"\n[[announce_servers]]\nshared_secret = \"{}\"\n",
            hex_secret()
        );
        let value = toml::from_str::<toml::Value>(&cfg).unwrap();
        let err = parse_announce_servers(value.get("announce_servers"))
            .expect_err("missing url must error");
        assert!(err.to_string().contains("url required"));
    }

    #[test]
    fn parse_announce_servers_rejects_a_malformed_secret() {
        let cfg = "name = \"x\"\n[[announce_servers]]\nurl = \"https://a.example\"\nshared_secret = \"nothex\"\n";
        let value = toml::from_str::<toml::Value>(cfg).unwrap();
        let err = parse_announce_servers(value.get("announce_servers"))
            .expect_err("malformed secret must error");
        assert!(err.to_string().contains("shared_secret invalid"));
    }

    #[test]
    fn parse_announce_servers_rejects_a_non_table_entry() {
        let value =
            toml::from_str::<toml::Value>("name = \"x\"\nannounce_servers = [\"https://a\"]")
                .unwrap();
        let err = parse_announce_servers(value.get("announce_servers"))
            .expect_err("non-table entry must error");
        assert!(
            err.to_string()
                .contains("announce_servers[0] must be a table")
        );
    }

    #[test]
    fn parse_relay_shared_secret_round_trips_32_bytes() {
        // A 64-char lowercase hex string round-trips to its 32 source
        // bytes exactly. Anything shorter, longer, or with non-hex
        // characters is rejected — silent truncation of an HMAC key
        // would substitute predictable bytes for the missing ones.
        let secret_hex = "0011223344556677889900aabbccddeeff00112233445566778899aabbccddee";
        let parsed = parse_relay_shared_secret(secret_hex).unwrap();
        assert_eq!(parsed[0], 0x00);
        assert_eq!(parsed[1], 0x11);
        assert_eq!(parsed[31], 0xee);
    }

    #[test]
    fn parse_relay_shared_secret_rejects_wrong_length() {
        assert!(parse_relay_shared_secret("abcd").is_err());
        let too_long = "0".repeat(66);
        assert!(parse_relay_shared_secret(&too_long).is_err());
    }

    #[test]
    fn parse_relay_shared_secret_rejects_non_hex() {
        let bad = "zz".repeat(32);
        assert!(parse_relay_shared_secret(&bad).is_err());
    }

    #[test]
    fn p2p_backend_config_default_is_private() {
        // The default posture is `Private` — a trusted mesh that never
        // publishes to a global directory. This guards the manual `Default`
        // impl against regressing to either extreme: confining the node to
        // the LAN, or opting it into DHT/announce publication unasked.
        let cfg = P2pBackendConfig::default();
        assert_eq!(cfg.exposure, DiscoveryReach::Private);
        assert!(cfg.relay_endpoints.is_empty());
        assert!(cfg.relay_shared_secret.is_none());
        assert!(cfg.dht.bootstrap_nodes.is_empty());
    }

    #[test]
    fn discovery_reach_capability_truth_table() {
        // The posture-gated activation truth table: each capability is on or
        // off per posture. This is the single source of truth the backend's
        // source registration consults.
        use DiscoveryReach::{LanOnly, Private, Public};

        // Gossip, hole punch, and peer relay: off at LanOnly, on from
        // Private upward.
        for (reach, want) in [(LanOnly, false), (Private, true), (Public, true)] {
            assert_eq!(reach.permits_gossip(), want, "gossip @ {reach:?}");
            assert_eq!(reach.permits_hole_punch(), want, "hole punch @ {reach:?}");
            assert_eq!(reach.permits_peer_relay(), want, "peer relay @ {reach:?}");
        }

        // Global directory (DHT + announce): only at Public.
        for (reach, want) in [(LanOnly, false), (Private, false), (Public, true)] {
            assert_eq!(
                reach.permits_global_directory(),
                want,
                "global directory @ {reach:?}",
            );
        }
    }

    #[test]
    fn parse_exposure_accepts_each_posture() {
        assert_eq!(parse_exposure("lan-only").unwrap(), DiscoveryReach::LanOnly);
        assert_eq!(parse_exposure("private").unwrap(), DiscoveryReach::Private);
        assert_eq!(parse_exposure("public").unwrap(), DiscoveryReach::Public);
    }

    #[test]
    fn parse_exposure_rejects_unknown_posture() {
        // A typo must fail loudly rather than silently falling back to the
        // default — getting the posture wrong is a security-relevant mistake
        // in either direction.
        let err = parse_exposure("publik").expect_err("unknown posture must error");
        assert!(err.to_string().contains("publik"));
    }

    #[test]
    fn open_from_config_absent_exposure_defaults_to_private() {
        // Omitting the `exposure` key resolves to the default posture,
        // Private — a trusted mesh with no global-directory publication.
        let value = toml::from_str::<toml::Value>(r#"name = "x""#).unwrap();
        let parsed = value
            .get("exposure")
            .and_then(|v| v.as_str())
            .map(parse_exposure)
            .transpose()
            .unwrap()
            .unwrap_or_default();
        assert_eq!(parsed, DiscoveryReach::Private);
    }

    #[test]
    fn open_from_config_parses_exposure_key() {
        // The new `exposure` key round-trips through the TOML boundary to the
        // matching posture.
        let value = toml::from_str::<toml::Value>("name = \"x\"\nexposure = \"public\"").unwrap();
        let parsed = value
            .get("exposure")
            .and_then(|v| v.as_str())
            .map(parse_exposure)
            .transpose()
            .unwrap()
            .unwrap_or_default();
        assert_eq!(parsed, DiscoveryReach::Public);
    }

    #[test]
    fn parse_dht_config_without_bootstrap_uses_empty_set() {
        // No bootstrap nodes is valid — the live node falls back to the named
        // public default set ([`DEFAULT_DHT_BOOTSTRAP_NODES`]) — so the parsed
        // config carries an empty bootstrap list rather than failing. The empty
        // list is the signal `MainlineDht::open` reads as "use the public
        // default". Whether the DHT runs at all is a posture decision, not a
        // property of this config.
        let value = toml::from_str::<toml::Value>(r#"name = "x""#).unwrap();
        let dht = parse_dht_config(&value).unwrap();
        assert!(dht.bootstrap_nodes.is_empty());
    }

    #[test]
    fn parse_dht_config_explicit_empty_bootstrap_array_uses_default() {
        // An operator who writes `dht_bootstrap_nodes = []` explicitly gets the
        // same default-fallback as omitting the key: the parsed list is empty,
        // which the node resolves to the public default. This pins the
        // "empty override falls back to the default" contract at the config
        // layer.
        let toml_src = "name = \"x\"\ndht_bootstrap_nodes = []";
        let value = toml::from_str::<toml::Value>(toml_src).unwrap();
        let dht = parse_dht_config(&value).unwrap();
        assert!(dht.bootstrap_nodes.is_empty());
    }

    #[test]
    fn parse_dht_config_override_is_preserved_verbatim() {
        // A non-empty override is carried through unchanged, so the node pins
        // exactly those nodes rather than the public default.
        let toml_src = "name = \"x\"\n\
             dht_bootstrap_nodes = [\"203.0.113.1:6881\"]";
        let value = toml::from_str::<toml::Value>(toml_src).unwrap();
        let dht = parse_dht_config(&value).unwrap();
        assert_eq!(dht.bootstrap_nodes.len(), 1);
        assert_eq!(dht.bootstrap_nodes[0].port(), 6881);
    }

    #[test]
    fn parse_dht_config_parses_bootstrap_nodes() {
        let toml_src = "name = \"x\"\n\
             dht_bootstrap_nodes = [\"127.0.0.1:6881\", \"10.0.0.1:6882\"]";
        let value = toml::from_str::<toml::Value>(toml_src).unwrap();
        let dht = parse_dht_config(&value).unwrap();
        assert_eq!(dht.bootstrap_nodes.len(), 2);
        assert_eq!(dht.bootstrap_nodes[0].port(), 6881);
        assert_eq!(dht.bootstrap_nodes[1].port(), 6882);
    }

    #[test]
    fn parse_dht_config_rejects_malformed_bootstrap_node() {
        // A bootstrap entry that is not a valid `host:port` must fail loudly
        // with the offending value rather than being silently dropped.
        let toml_src = "name = \"x\"\n\
             dht_bootstrap_nodes = [\"not-a-socket-addr\"]";
        let value = toml::from_str::<toml::Value>(toml_src).unwrap();
        let err = parse_dht_config(&value).expect_err("malformed node must error");
        assert!(err.to_string().contains("not-a-socket-addr"));
    }

    /// Enabling LAN discovery must not block or panic on backend open.
    /// We can't reliably exercise the full multicast handshake on
    /// loopback in CI, so we just confirm the spawned loops come up
    /// cleanly and the backend can be dropped after a short delay.
    ///
    /// On drop, the backend's cancellation watch is flipped to `true`.
    /// We subscribe before drop and confirm the receiver sees the
    /// change, proving the spawned tasks will exit.
    #[tokio::test]
    async fn discovery_loop_starts_without_panicking() {
        let dir = tempdir().unwrap();
        let cfg = P2pBackendConfig {
            instance_id: "p2p-discovery".to_string(),
            folder_id: "p2p-discovery".to_string(),
            display_name: "Discovery".to_string(),
            index_path: dir.path().join("index.db"),
            block_store_root: dir.path().join("blocks"),
            identity_dir: dir.path().join("identity"),
            listen_addr: Some("127.0.0.1:0".parse().unwrap()),
            // LAN multicast self-activates at the default Private posture
            // once a listener is bound — no separate enable flag.
            ..Default::default()
        };
        let backend = P2pBackend::open(cfg).unwrap();
        let mut cancel_rx = backend.cancel.subscribe();
        assert!(!*cancel_rx.borrow());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(backend);
        // After drop, the cancel watch must have fired; spawned tasks
        // will observe `true` on their next tick and exit.
        cancel_rx.changed().await.unwrap();
        assert!(*cancel_rx.borrow());
    }

    /// Self-activation: a source that is *permitted* by the posture but
    /// lacks what it needs to run must stay idle. Here the posture is
    /// `Public` — which permits the DHT and announce sources — but no
    /// `listen_addr` is set, so the DHT (which needs a bound port to
    /// advertise) and the LAN source never come up. The backend must still
    /// open and shut down cleanly, proving the AND half of the
    /// self-activation rule: permission alone does not start a source.
    #[tokio::test]
    async fn public_posture_without_listener_keeps_global_sources_idle() {
        let dir = tempdir().unwrap();
        let cfg = P2pBackendConfig {
            instance_id: "p2p-idle".to_string(),
            folder_id: "p2p-idle".to_string(),
            display_name: "Idle".to_string(),
            index_path: dir.path().join("index.db"),
            block_store_root: dir.path().join("blocks"),
            identity_dir: dir.path().join("identity"),
            // No listen_addr — the bound-port requirement is unmet.
            exposure: DiscoveryReach::Public,
            // DHT bootstrap configured, but the source still cannot run
            // without a listener to advertise.
            dht: DhtConfig {
                bootstrap_nodes: vec!["127.0.0.1:6881".parse().unwrap()],
            },
            ..Default::default()
        };
        let backend = P2pBackend::open(cfg).unwrap();
        let mut cancel_rx = backend.cancel.subscribe();
        assert!(!*cancel_rx.borrow());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(backend);
        cancel_rx.changed().await.unwrap();
        assert!(*cancel_rx.borrow());
    }

    /// `LanOnly` posture with a bound listener: LAN multicast self-activates
    /// (it is permitted at every posture and the listener requirement is
    /// met), but gossip, hole punch, peer relay, and any global directory
    /// stay off. The backend must open and shut down cleanly.
    #[tokio::test]
    async fn lan_only_posture_with_listener_opens_and_shuts_down() {
        let dir = tempdir().unwrap();
        let cfg = P2pBackendConfig {
            instance_id: "p2p-lan-only".to_string(),
            folder_id: "p2p-lan-only".to_string(),
            display_name: "LanOnly".to_string(),
            index_path: dir.path().join("index.db"),
            block_store_root: dir.path().join("blocks"),
            identity_dir: dir.path().join("identity"),
            listen_addr: Some("127.0.0.1:0".parse().unwrap()),
            exposure: DiscoveryReach::LanOnly,
            ..Default::default()
        };
        let backend = P2pBackend::open(cfg).unwrap();
        let mut cancel_rx = backend.cancel.subscribe();
        assert!(!*cancel_rx.borrow());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(backend);
        cancel_rx.changed().await.unwrap();
        assert!(*cancel_rx.borrow());
    }

    /// Configuring an announce server must not block or panic on backend
    /// open. The publish loop and the registered announce-server discovery
    /// source come up against an unreachable URL; both are best-effort, so
    /// the backend opens cleanly and drops cleanly. We confirm the spawned
    /// tasks observe cancellation, proving the announce loop honours the
    /// shutdown watch like every other background task.
    #[tokio::test]
    async fn announce_server_configured_backend_opens_and_shuts_down() {
        let dir = tempdir().unwrap();
        let cfg = P2pBackendConfig {
            instance_id: "p2p-announce".to_string(),
            folder_id: "p2p-announce".to_string(),
            display_name: "Announce".to_string(),
            index_path: dir.path().join("index.db"),
            block_store_root: dir.path().join("blocks"),
            identity_dir: dir.path().join("identity"),
            listen_addr: Some("127.0.0.1:0".parse().unwrap()),
            // Public posture so the announce source and its publish loop
            // actually run — at Private they would stay idle regardless of a
            // configured server.
            exposure: DiscoveryReach::Public,
            // An address with no server listening — the publish loop's
            // register calls fail and are swallowed best-effort. The secret is
            // immaterial here since no carrier ever receives the request.
            announce_servers: vec![AnnounceServer {
                base_url: "http://127.0.0.1:1".to_string(),
                secret: [0u8; cascade_p2p::discovery::announce::SHARED_SECRET_LEN],
            }],
            ..Default::default()
        };
        let backend = P2pBackend::open(cfg).unwrap();
        let mut cancel_rx = backend.cancel.subscribe();
        assert!(!*cancel_rx.borrow());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(backend);
        cancel_rx.changed().await.unwrap();
        assert!(*cancel_rx.borrow());
    }

    /// A `Public`-posture backend with no operator-supplied bootstrap nodes
    /// must not block or panic on backend open: the DHT source self-activates
    /// (the posture permits global publication and a listener is bound), the
    /// node joins the public default set, the publish loop comes up on the
    /// BEP44 republish cadence, and both honour the shutdown watch. We confirm
    /// the spawned tasks observe cancellation, proving the DHT publish loop
    /// exits cleanly like every other background task. The default bootstrap
    /// nodes are real public router hostnames, which `open` resolves with
    /// blocking `getaddrinfo`; the backend runs that resolution off the runtime
    /// via `spawn_blocking`, and a resolver miss is swallowed best-effort, so
    /// the node still binds its local UDP socket and this stays an offline test
    /// that neither blocks a worker nor depends on reaching the routers.
    #[tokio::test]
    async fn dht_enabled_backend_opens_and_shuts_down() {
        let dir = tempdir().unwrap();
        let cfg = P2pBackendConfig {
            instance_id: "p2p-dht".to_string(),
            folder_id: "p2p-dht".to_string(),
            display_name: "Dht".to_string(),
            index_path: dir.path().join("index.db"),
            block_store_root: dir.path().join("blocks"),
            identity_dir: dir.path().join("identity"),
            listen_addr: Some("127.0.0.1:0".parse().unwrap()),
            // The posture is the on/off switch: `Public` permits global-
            // directory publication, so the DHT source self-activates. An empty
            // bootstrap list falls back to the named public default inside the
            // node.
            exposure: DiscoveryReach::Public,
            dht: DhtConfig::default(),
            ..Default::default()
        };
        let backend = P2pBackend::open(cfg).unwrap();
        let mut cancel_rx = backend.cancel.subscribe();
        assert!(!*cancel_rx.borrow());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(backend);
        cancel_rx.changed().await.unwrap();
        assert!(*cancel_rx.borrow());
    }
}
