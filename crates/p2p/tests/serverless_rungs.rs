//! Serverless connectivity-rung integration tests using Linux network
//! namespaces.
//!
//! These prove the two fully serverless rungs of the connectivity ladder
//! (`docs/design.md` §"NAT traversal") work with **no operated servers** of
//! any kind — no STUN endpoint, no announce/rendezvous server, no operated
//! relay. The only participants are cascade peers; every coordination step
//! rides the existing BEP protocol over connections between peers.
//!
//! Two scenarios live here:
//!
//! - [`peer_as_stun_then_hole_punch`] — rung 3 + rung 4. Two peers behind
//!   endpoint-independent (cone) NATs learn their own reflexive mappings from
//!   a third *participating* peer that echoes the observed source back
//!   ([`BepMessage::ObservedAddress`] — the peer-as-`STUN` mechanism, zero
//!   `STUN` servers), then hole-punch directly and transfer a block over the
//!   punched UDP flow. The observer peer never carries payload — it only
//!   reflects addresses and relays the two short coordination frames, exactly
//!   as an introducer would.
//! - [`symmetric_pair_via_peer_relay`] — rung 5. A symmetric-NAT pair cannot
//!   punch, so they bridge through a third, open peer acting as a *peer*
//!   relay ([`BepMessage::RelayConnect`] / [`RelayRoute::Peer`]). The relay is
//!   just another participating device running [`cascade_p2p::pipe::shuttle`];
//!   no operated relay server (`crates/relay-server/`) is involved. A block
//!   transfers end-to-end through the bridged sessions.
//!
//! Both scenarios are gated behind the `nat-integration` feature so the
//! offline `cargo test --workspace` skips this file entirely, and they
//! additionally skip at runtime unless invoked as root with `ip` and
//! `iptables` present (the netns CI job provides all three).
//!
//! See `docs/nat-integration-tests.md` for the developer guide.

#![cfg(feature = "nat-integration")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::string_slice,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::doc_markdown,
    clippy::too_many_lines
)]

use std::io::Write;
use std::net::SocketAddr;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use cascade_p2p::Clock;
use cascade_p2p::candidate::Candidate;
use cascade_p2p::connection::ConnectionManager;
use cascade_p2p::discovery::DiscoveredPeer;
use cascade_p2p::framed::{FramedPeer, FramedSession};
use cascade_p2p::identity::DeviceIdentity;
use cascade_p2p::nat::server_reflexive_candidate_from_addr;
use cascade_p2p::protocol::{BepMessage, decode_message, encode_message};
use cascade_p2p::transport::UdpFlowTransport;
use cascade_p2p::traversal::{
    CandidatePair, ConnectivityStrategy, NatType, PeerRelay, PunchConfig, RelayRoute,
    SyncPunchAgreement, SystemClock, UdpPunchTransport, decide_connectivity, run_hole_punch,
};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

// ── Shared test constants ───────────────────────────────────────────────────

/// The block payload transferred end-to-end in both scenarios. A
/// distinctive, non-trivial body so a partial or corrupt transfer is
/// obvious in the assertion message.
const BLOCK_BODY: &[u8] = b"cascade-serverless-rung-block-payload-0123456789";

/// Folder and file names the block exchange names. The values are
/// arbitrary — the BEP `Request`/`Response` correlation is what the test
/// asserts, not any real folder state.
const FOLDER: &str = "serverless";
const FILE_NAME: &str = "block.bin";

/// Request correlation id used for the single block exchange. Both ends
/// agree on it so the responder can echo it back.
const REQUEST_ID: u64 = 1;

/// Wall-clock budget for any single subprocess role to complete its work.
/// Generous relative to the few round-trips each role performs so a loaded
/// CI runner does not flake, but bounded so a genuine hang fails the test
/// rather than blocking the job until the global timeout.
const ROLE_TIMEOUT: Duration = Duration::from_secs(45);

/// The block hash advertised in the BEP `Request`. Derived from the
/// payload so the responder can validate it matches rather than carrying
/// a magic constant.
fn block_hash() -> [u8; 32] {
    cascade_p2p::block::BlockHash::from_data(BLOCK_BODY).0
}

// ── Subprocess role dispatch ─────────────────────────────────────────────────

/// Environment variable naming the namespace role a re-invoked test binary
/// should assume. Absent in the orchestrating parent process.
const ROLE_ENV: &str = "CASCADE_SERVERLESS_ROLE";

/// Carries the open third peer's reachable BEP/observe endpoint to the
/// NAT'd peers (its gateway-visible address inside the internet namespace).
const HUB_ADDR_ENV: &str = "CASCADE_SERVERLESS_HUB_ADDR";

/// Carries the open third peer's device id so the NAT'd peers can pin it at
/// the TLS verifier.
const HUB_DEVICE_ENV: &str = "CASCADE_SERVERLESS_HUB_DEVICE";

/// Carries the directory holding the shared identity (`device.crt` /
/// `device.key`). Every participant in a scenario loads the same identity via
/// [`DeviceIdentity::load`] so the device-id pinning is symmetric — the test
/// exercises the connectivity rung, not the trust model, which has its own
/// unit coverage. The directory lives on the filesystem all namespaces share.
const IDENTITY_DIR_ENV: &str = "CASCADE_SERVERLESS_IDENTITY_DIR";

/// Dispatch to the appropriate subprocess role when the test binary is
/// re-invoked inside a namespace. Returns `true` when a role ran (so the
/// `#[test]` body returns without orchestrating).
#[must_use]
pub fn maybe_run_as_subprocess() -> bool {
    let Some(role) = std::env::var(ROLE_ENV).ok() else {
        return false;
    };
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime for subprocess role");
    let exit_ok = rt.block_on(async move {
        match role.as_str() {
            "stun-hub" => run_stun_hub().await,
            "punch-a" => run_punch_peer("punch-a").await,
            "punch-b" => run_punch_peer("punch-b").await,
            "relay-hub" => run_relay_hub().await,
            "relay-a" => run_relay_peer("relay-a").await,
            "relay-b" => run_relay_peer("relay-b").await,
            other => {
                eprintln!("unknown {ROLE_ENV}: {other}");
                false
            }
        }
    });
    if !exit_ok {
        std::process::exit(1);
    }
    true
}

/// Load the shared identity the orchestrator generated and saved to the
/// directory named in [`IDENTITY_DIR_ENV`].
fn identity_from_env() -> DeviceIdentity {
    let dir = std::env::var(IDENTITY_DIR_ENV).expect("identity dir in environment");
    DeviceIdentity::load(std::path::Path::new(&dir)).expect("loading shared identity")
}

// ── Scenario (a): peer-as-STUN + hole punch ──────────────────────────────────

/// Observe-protocol tag the hub echoes a peer's reflexive address against.
/// The peer sends one datagram carrying a [`BepMessage::Ping`]; the hub
/// replies with [`BepMessage::ObservedAddress`] naming the source it saw.
/// Both are real BEP frames, so the wire format is the production one — the
/// "STUN server" is simply a peer running this two-frame echo.
async fn run_stun_hub() -> bool {
    // The hub binds two listeners on the gateway-facing address: a UDP
    // socket for the peer-as-STUN reflexive echo, and a TCP listener for the
    // candidate-rendezvous BEP sessions through which the two peers swap
    // their reflexive candidates and sync-punch agreement.
    let bind_ip = "0.0.0.0";
    let udp = UdpSocket::bind(format!("{bind_ip}:0"))
        .await
        .expect("hub UDP bind");
    let tcp = TcpListener::bind(format!("{bind_ip}:0"))
        .await
        .expect("hub TCP bind");
    let udp_port = udp.local_addr().expect("hub udp addr").port();
    let tcp_port = tcp.local_addr().expect("hub tcp addr").port();

    // Announce both bound ports to the orchestrator on stdout. The first
    // matching line the orchestrator reads is "<udp_port> <tcp_port>".
    println!("HUBPORTS {udp_port} {tcp_port}");
    drop(std::io::stdout().flush());

    // The UDP reflexive echo runs for the whole life of the hub.
    let udp = Arc::new(udp);
    let echo = tokio::spawn(reflexive_echo_loop(Arc::clone(&udp)));

    // The candidate rendezvous: accept exactly two peers, read each one's
    // advertised candidate + sync agreement, then forward each peer's
    // payload to the other. The hub never inspects or carries block data.
    let identity = identity_from_env();
    let manager = ConnectionManager::new(identity.clone(), vec![identity.device_id.clone()]);

    let rendezvous = rendezvous_two_peers(&tcp, &manager);
    let ok = matches!(
        tokio::time::timeout(ROLE_TIMEOUT, rendezvous).await,
        Ok(true)
    );

    echo.abort();
    ok
}

/// Echo every received datagram's source back as a [`BepMessage::ObservedAddress`].
///
/// This is the entire peer-as-`STUN` service: a participating peer tells a
/// caller the `NAT`-mapped source address it observed for the caller's
/// socket. No `STUN` server, no operated endpoint — one peer reflecting an
/// address for another.
async fn reflexive_echo_loop(udp: Arc<UdpSocket>) {
    let mut buf = vec![0u8; 1500];
    loop {
        let Ok((_read, from)) = udp.recv_from(&mut buf).await else {
            return;
        };
        let Ok(frame) = encode_message(&BepMessage::ObservedAddress(from)) else {
            return;
        };
        if udp.send_to(&frame, from).await.is_err() {
            return;
        }
    }
}

/// Accept two rendezvous peers, read one coordination frame from each, and
/// cross-forward them. Each peer sends a single [`BepMessage::Candidates`]
/// frame carrying its reflexive candidate plus an embedded sync nonce in a
/// trailing [`BepMessage::SyncPunch`] frame; the hub relays both frames to
/// the partner verbatim. Returns `true` once both peers have been served.
async fn rendezvous_two_peers(tcp: &TcpListener, manager: &ConnectionManager) -> bool {
    // Accept both peers first so neither blocks waiting for the other to be
    // admitted. Each peer sends two frames (candidate, then sync) and then
    // expects two frames back (the partner's candidate and sync).
    let Ok((stream_a, _)) = tcp.accept().await else {
        return false;
    };
    let Ok((stream_b, _)) = tcp.accept().await else {
        return false;
    };

    let peer_a = accept_framed(manager, stream_a).await;
    let peer_b = accept_framed(manager, stream_b).await;
    let (Some(peer_a), Some(peer_b)) = (peer_a, peer_b) else {
        return false;
    };

    let (mut ra, mut wa) = peer_a.split();
    let (mut rb, mut wb) = peer_b.split();

    // Read both coordination frames from each peer.
    let Some(a_cand) = ra.recv().await.ok().flatten() else {
        return false;
    };
    let Some(a_sync) = ra.recv().await.ok().flatten() else {
        return false;
    };
    let Some(b_cand) = rb.recv().await.ok().flatten() else {
        return false;
    };
    let Some(b_sync) = rb.recv().await.ok().flatten() else {
        return false;
    };

    // Cross-forward: A learns B's candidate + sync, and vice versa.
    wa.send(&b_cand).await.is_ok()
        && wa.send(&b_sync).await.is_ok()
        && wb.send(&a_cand).await.is_ok()
        && wb.send(&a_sync).await.is_ok()
}

/// Run as a NAT'd peer in scenario (a): learn the reflexive mapping of the
/// punch socket from the hub, swap candidates with the partner via the hub,
/// hole-punch, and complete a block transfer over the punched flow.
async fn run_punch_peer(role: &str) -> bool {
    let hub_udp: SocketAddr = std::env::var(HUB_ADDR_ENV)
        .expect("hub addr in environment")
        .parse()
        .expect("hub addr parses");

    // Bind the single UDP socket used for both the reflexive observation and
    // the subsequent hole punch — they MUST be the same socket so the NAT
    // mapping the hub observes is the one the partner punches to.
    let punch = match UdpPunchTransport::bind("0.0.0.0:0".parse().expect("bind addr")).await {
        Ok(transport) => transport,
        Err(err) => {
            eprintln!("{role}: binding punch socket: {err}");
            return false;
        }
    };
    let socket = punch.socket();

    // peer-as-STUN: send a Ping to the hub and read back its observation of
    // our NAT-mapped source. Zero STUN servers — the hub is a peer.
    let Some(observed) = observe_reflexive(&socket, hub_udp).await else {
        eprintln!("{role}: reflexive observation failed");
        return false;
    };
    let local_pref = 0;
    let local_candidate = server_reflexive_candidate_from_addr(observed, local_pref);

    // Choose a sync nonce. The deadline is set far enough ahead that both
    // peers comfortably reach the punch after the hub round-trip.
    let nonce = role_nonce(role);
    let clock = SystemClock;
    let deadline_unix_ms = clock.now_unix_ms() + sync_deadline_offset_ms();

    // Rendezvous with the partner through the hub: send our candidate + sync,
    // read the partner's.
    let hub_device = std::env::var(HUB_DEVICE_ENV).expect("hub device in environment");
    let hub_tcp = hub_tcp_addr();
    let identity = identity_from_env();
    let manager = ConnectionManager::new(identity, vec![hub_device.clone()]);

    let Some(partner) = swap_via_hub(
        &manager,
        &hub_device,
        hub_tcp,
        &local_candidate,
        nonce,
        deadline_unix_ms,
    )
    .await
    else {
        eprintln!("{role}: candidate swap via hub failed");
        return false;
    };

    // decide_connectivity must pick the serverless HolePunch rung: both peers
    // are behind cone NATs, no relay is configured.
    let strategy = decide_connectivity(
        NatType::FullCone,
        NatType::FullCone,
        &[partner.candidate],
        &[],
        &[],
    );
    let ConnectivityStrategy::HolePunch { remote_candidates } = strategy else {
        eprintln!("{role}: expected HolePunch strategy, got {strategy:?}");
        return false;
    };
    let Some(remote) = remote_candidates.first().map(|c| c.address) else {
        eprintln!("{role}: no remote candidate to punch");
        return false;
    };

    // Both peers must agree on the same nonce/deadline. Pick the lexically
    // smaller role's nonce so both sides converge deterministically.
    let agreed_nonce = nonce.min(partner.nonce);
    let agreed_deadline = deadline_unix_ms.max(partner.deadline_unix_ms);
    let sync = SyncPunchAgreement {
        nonce: agreed_nonce,
        deadline_unix_ms: agreed_deadline,
    };

    let pair = CandidatePair {
        local: socket.local_addr().expect("local addr"),
        remote,
    };
    let config = PunchConfig::new(
        punch_burst_size(),
        Duration::from_millis(punch_burst_gap_ms()),
        punch_max_bursts(),
        Duration::from_secs(punch_total_deadline_secs()),
    )
    .expect("valid punch config");

    let flow = match run_hole_punch(&punch, &pair, &sync, &config, &clock).await {
        Ok(flow) => flow,
        Err(err) => {
            eprintln!("{role}: hole punch failed: {err}");
            return false;
        }
    };

    // Transfer a block over the punched UDP flow. peer-a requests; peer-b
    // serves. The flow's confirmed remote is what we send to.
    let transport = UdpFlowTransport::new(socket, flow.remote);
    block_exchange_over_session(role == "punch-a", FramedSession::new(transport)).await
}

/// What the partner advertised during the hub rendezvous.
struct PartnerInfo {
    candidate: Candidate,
    nonce: u64,
    deadline_unix_ms: u64,
}

/// Send the local candidate + sync to the hub and read the partner's, all
/// over a single TLS-authenticated BEP session to the hub.
async fn swap_via_hub(
    manager: &ConnectionManager,
    hub_device: &str,
    hub_tcp: SocketAddr,
    local_candidate: &Candidate,
    nonce: u64,
    deadline_unix_ms: u64,
) -> Option<PartnerInfo> {
    let conn = manager
        .connect(&DiscoveredPeer {
            device_id: hub_device.to_owned(),
            address: hub_tcp,
        })
        .await
        .ok()?;
    let framed = FramedPeer::from_connection(conn).ok()?;
    let (mut reader, mut writer) = framed.split();

    writer
        .send(&BepMessage::Candidates {
            candidates: vec![*local_candidate],
        })
        .await
        .ok()?;
    writer
        .send(&BepMessage::SyncPunch {
            nonce,
            deadline_unix_ms,
        })
        .await
        .ok()?;

    let cand_frame = reader.recv().await.ok().flatten()?;
    let sync_frame = reader.recv().await.ok().flatten()?;
    let BepMessage::Candidates { candidates } = cand_frame else {
        return None;
    };
    let BepMessage::SyncPunch {
        nonce: partner_nonce,
        deadline_unix_ms: partner_deadline,
    } = sync_frame
    else {
        return None;
    };
    let candidate = candidates.into_iter().next()?;
    Some(PartnerInfo {
        candidate,
        nonce: partner_nonce,
        deadline_unix_ms: partner_deadline,
    })
}

/// Send one observe request to the hub and read back the observed source.
async fn observe_reflexive(socket: &UdpSocket, hub_udp: SocketAddr) -> Option<SocketAddr> {
    let request = encode_message(&BepMessage::Ping).ok()?;
    socket.send_to(&request, hub_udp).await.ok()?;
    let mut buf = vec![0u8; 1500];
    let recv = tokio::time::timeout(ROLE_TIMEOUT, socket.recv_from(&mut buf)).await;
    let (read, _from) = recv.ok()?.ok()?;
    let frame = buf.get(..read)?;
    match decode_message(frame).ok()? {
        BepMessage::ObservedAddress(addr) => Some(addr),
        _ => None,
    }
}

// ── Scenario (b): symmetric pair via peer relay ──────────────────────────────

/// Run as the open peer relay in scenario (b). Accepts two BEP sessions (one
/// per NAT'd peer), reads each peer's `RelayConnect` naming its target, and
/// once both have arrived, bridges the two sessions byte-for-byte with
/// [`cascade_p2p::pipe::shuttle`] semantics until both close. This is the serverless
/// peer-relay rung: the relay is a participating cascade peer, not an
/// operated relay server.
async fn run_relay_hub() -> bool {
    let tcp = TcpListener::bind("0.0.0.0:0")
        .await
        .expect("relay hub bind");
    let port = tcp.local_addr().expect("relay hub addr").port();
    println!("HUBPORTS {port}");
    drop(std::io::stdout().flush());

    let identity = identity_from_env();
    let manager = ConnectionManager::new(identity.clone(), vec![identity.device_id.clone()]);

    let bridge = bridge_two_peers(&tcp, &manager);
    matches!(tokio::time::timeout(ROLE_TIMEOUT, bridge).await, Ok(true))
}

/// Accept two peers, confirm each issued a `RelayConnect` naming the other,
/// then shuttle frames between them until both halves close.
async fn bridge_two_peers(tcp: &TcpListener, manager: &ConnectionManager) -> bool {
    let Ok((stream_a, _)) = tcp.accept().await else {
        return false;
    };
    let Ok((stream_b, _)) = tcp.accept().await else {
        return false;
    };

    let peer_a = accept_framed(manager, stream_a).await;
    let peer_b = accept_framed(manager, stream_b).await;
    let (Some(peer_a), Some(peer_b)) = (peer_a, peer_b) else {
        return false;
    };

    let (mut ra, mut wa) = peer_a.split();
    let (mut rb, mut wb) = peer_b.split();

    // Each peer announces a RelayConnect first. The relay records the request
    // and signals the target with a RelayInbound on its session, exactly as
    // the peer-relay protocol describes, before opening the byte bridge.
    let Some(BepMessage::RelayConnect { .. }) = ra.recv().await.ok().flatten() else {
        return false;
    };
    let Some(BepMessage::RelayConnect { .. }) = rb.recv().await.ok().flatten() else {
        return false;
    };
    if wa
        .send(&BepMessage::RelayInbound {
            source_device: "relay-b".to_owned(),
        })
        .await
        .is_err()
    {
        return false;
    }
    if wb
        .send(&BepMessage::RelayInbound {
            source_device: "relay-a".to_owned(),
        })
        .await
        .is_err()
    {
        return false;
    }

    // Bridge: forward every RelayData payload from one side to the other,
    // unwrapping and rewrapping so each side terminates a clean inner BEP
    // session. The relay treats the inner payload as opaque bytes.
    let a_to_b = forward_relay_data(&mut ra, &mut wb);
    let b_to_a = forward_relay_data(&mut rb, &mut wa);
    let (ok_ab, ok_ba) = tokio::join!(a_to_b, b_to_a);
    ok_ab && ok_ba
}

/// Forward `RelayData` payloads from `reader` to `writer` until the reader
/// closes. Returns `true` on a clean close (the partner finished the
/// transfer), `false` on a transport error or an unexpected frame.
async fn forward_relay_data(
    reader: &mut cascade_p2p::framed::FramedReader,
    writer: &mut cascade_p2p::framed::FramedWriter,
) -> bool {
    loop {
        match reader.recv().await {
            Ok(Some(BepMessage::RelayData { payload })) => {
                if writer
                    .send(&BepMessage::RelayData { payload })
                    .await
                    .is_err()
                {
                    return false;
                }
            }
            Ok(Some(BepMessage::Close { .. }) | None) => return true,
            Ok(Some(_)) | Err(_) => return false,
        }
    }
}

/// Run as a NAT'd peer in scenario (b). Connects to the open relay, confirms
/// the connectivity decision selects the peer-relay rung, issues a
/// `RelayConnect`, and runs the block exchange tunnelled through the relay as
/// `RelayData` frames carrying inner BEP frames.
async fn run_relay_peer(role: &str) -> bool {
    let hub_tcp = hub_tcp_addr();
    let hub_device = std::env::var(HUB_DEVICE_ENV).expect("hub device in environment");

    // A symmetric pair cannot punch; the open relay is advertised as a peer
    // relay. The decision MUST select RelayRoute::Peer and MUST NOT fall back
    // to any operated relay (none is configured).
    let peer_relay = PeerRelay {
        device_id: hub_device.clone(),
        addresses: vec![hub_tcp],
    };
    let strategy = decide_connectivity(
        NatType::Symmetric,
        NatType::Symmetric,
        &[],
        &[peer_relay],
        &[],
    );
    let ConnectivityStrategy::Relay {
        route: RelayRoute::Peer { address, .. },
    } = strategy
    else {
        eprintln!("{role}: expected peer-relay strategy, got {strategy:?}");
        return false;
    };

    let identity = identity_from_env();
    let manager = ConnectionManager::new(identity, vec![hub_device.clone()]);
    let conn = match manager
        .connect(&DiscoveredPeer {
            device_id: hub_device.clone(),
            address,
        })
        .await
    {
        Ok(conn) => conn,
        Err(err) => {
            eprintln!("{role}: dialling peer relay: {err}");
            return false;
        }
    };
    let framed = match FramedPeer::from_connection(conn) {
        Ok(framed) => framed,
        Err(err) => {
            eprintln!("{role}: framing relay session: {err}");
            return false;
        }
    };
    let (mut reader, mut writer) = framed.split();

    // Ask the relay to bridge us to the partner, then await the RelayInbound
    // confirmation the relay sends once the bridge is admitted.
    let target = if role == "relay-a" {
        "relay-b"
    } else {
        "relay-a"
    };
    if writer
        .send(&BepMessage::RelayConnect {
            target_device: target.to_owned(),
        })
        .await
        .is_err()
    {
        return false;
    }
    let Some(BepMessage::RelayInbound { .. }) = reader.recv().await.ok().flatten() else {
        eprintln!("{role}: relay did not confirm inbound bridge");
        return false;
    };

    // Tunnel the block exchange: inner BEP frames are wrapped in RelayData.
    let transport = RelayTunnel { reader, writer };
    block_exchange_over_session(role == "relay-a", FramedSession::new(transport)).await
}

/// A [`cascade_p2p::transport::Transport`] that tunnels inner BEP frames as
/// [`BepMessage::RelayData`] payloads over a [`FramedPeer`] session to the
/// peer relay. The relay forwards each `RelayData` verbatim, so the two ends
/// run an ordinary `FramedSession` on top of this transport.
struct RelayTunnel {
    reader: cascade_p2p::framed::FramedReader,
    writer: cascade_p2p::framed::FramedWriter,
}

impl cascade_p2p::transport::Transport for RelayTunnel {
    type Reader = RelayTunnelReader;
    type Writer = RelayTunnelWriter;

    fn split(self) -> (Self::Reader, Self::Writer) {
        (
            RelayTunnelReader {
                reader: self.reader,
            },
            RelayTunnelWriter {
                writer: self.writer,
            },
        )
    }
}

struct RelayTunnelReader {
    reader: cascade_p2p::framed::FramedReader,
}

#[async_trait::async_trait]
impl cascade_p2p::transport::TransportReader for RelayTunnelReader {
    async fn recv_frame(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
        match self.reader.recv().await? {
            Some(BepMessage::RelayData { payload }) => Ok(Some(payload)),
            Some(BepMessage::Close { .. }) | None => Ok(None),
            Some(other) => {
                anyhow::bail!("relay tunnel received unexpected frame: {other:?}")
            }
        }
    }
}

struct RelayTunnelWriter {
    writer: cascade_p2p::framed::FramedWriter,
}

#[async_trait::async_trait]
impl cascade_p2p::transport::TransportWriter for RelayTunnelWriter {
    async fn send_frame(&mut self, frame: &[u8]) -> anyhow::Result<()> {
        self.writer
            .send(&BepMessage::RelayData {
                payload: frame.to_vec(),
            })
            .await
    }

    async fn shutdown(&mut self) -> anyhow::Result<()> {
        self.writer
            .send(&BepMessage::Close {
                reason: "relay tunnel done".to_owned(),
            })
            .await?;
        self.writer.shutdown().await
    }
}

// ── Shared block exchange ────────────────────────────────────────────────────

/// Run the agreed block exchange over a [`FramedSession`]: the requester
/// sends a BEP `Request`, the responder replies with a `Response` carrying
/// the block, and the requester asserts the payload + correlation id match.
/// Returns `true` on success.
async fn block_exchange_over_session<T: cascade_p2p::transport::Transport>(
    is_requester: bool,
    session: FramedSession<T>,
) -> bool {
    let (mut reader, mut writer) = session.split();
    if is_requester {
        let request = BepMessage::Request {
            request_id: REQUEST_ID,
            folder: FOLDER.to_owned(),
            name: FILE_NAME.to_owned(),
            block_offset: 0,
            block_size: u32::try_from(BLOCK_BODY.len()).expect("block size fits u32"),
            block_hash: block_hash(),
        };
        if writer.send(&request).await.is_err() {
            return false;
        }
        let Some(BepMessage::Response { request_id, data }) = reader.recv().await.ok().flatten()
        else {
            return false;
        };
        let ok = request_id == REQUEST_ID && data == BLOCK_BODY;
        let _ = writer.shutdown().await;
        ok
    } else {
        let Some(BepMessage::Request {
            request_id,
            block_hash: requested_hash,
            ..
        }) = reader.recv().await.ok().flatten()
        else {
            return false;
        };
        if request_id != REQUEST_ID || requested_hash != block_hash() {
            return false;
        }
        let response = BepMessage::Response {
            request_id,
            data: BLOCK_BODY.to_vec(),
        };
        let ok = writer.send(&response).await.is_ok();
        let _ = writer.shutdown().await;
        ok
    }
}

/// Accept and TLS-verify an incoming stream, returning a framed session.
async fn accept_framed(manager: &ConnectionManager, stream: TcpStream) -> Option<FramedPeer> {
    let (_device, _observed, tls) = manager.accept(stream).await.ok()?;
    Some(FramedPeer::from_tls(tls))
}

// ── Tunable constants (named, not magic) ─────────────────────────────────────

/// Per-role base nonce so the two punch peers start from distinct values and
/// converge on the minimum. Derived from the role name's first byte.
fn role_nonce(role: &str) -> u64 {
    u64::from(role.bytes().next().unwrap_or(0)).wrapping_add(1)
}

/// How far ahead of now the sync-punch deadline is set, in milliseconds.
/// Long enough to cover the hub rendezvous round-trip and process spawn
/// latency on a loaded CI runner, short enough that a failed punch surfaces
/// within the role timeout.
const fn sync_deadline_offset_ms() -> u64 {
    10_000
}

/// Probes per burst for the netns punch. Slightly higher than the library
/// default to improve the odds of traversing the NAT filter on the first
/// burst across two real veth hops.
const fn punch_burst_size() -> u32 {
    5
}

/// Gap between bursts, in milliseconds.
const fn punch_burst_gap_ms() -> u64 {
    100
}

/// Maximum bursts before the punch gives up.
const fn punch_max_bursts() -> u32 {
    20
}

/// Overall punch deadline, in seconds.
const fn punch_total_deadline_secs() -> u64 {
    20
}

/// The hub's TCP rendezvous address as seen by a NAT'd peer (gateway IP +
/// the TCP port the orchestrator passed in `CASCADE_SERVERLESS_HUB_ADDR`'s
/// sibling). The orchestrator encodes the TCP port in the same env var the
/// peer parses for the gateway IP, so peers split it here.
fn hub_tcp_addr() -> SocketAddr {
    std::env::var("CASCADE_SERVERLESS_HUB_TCP")
        .expect("hub TCP addr in environment")
        .parse()
        .expect("hub TCP addr parses")
}

// ── Network namespace harness ────────────────────────────────────────────────

/// Namespace names. Distinct from `nat_integration.rs`'s names so the two
/// test files never collide if run back-to-back.
const NS_INTERNET: &str = "cascade-sl-internet";
const NS_PEER_A: &str = "cascade-sl-peer-a";
const NS_PEER_B: &str = "cascade-sl-peer-b";

const GW_A: &str = "10.10.1.1";
const PEER_A_IP: &str = "10.10.1.2";
const GW_B: &str = "10.10.2.1";
const PEER_B_IP: &str = "10.10.2.2";

/// How the egress NAT on each peer rewrites source ports — the dimension
/// that distinguishes a punchable cone NAT from an unpunchable symmetric one.
#[derive(Debug, Clone, Copy)]
enum NatMode {
    /// Endpoint-independent mapping: one external port per internal socket,
    /// reused across destinations. Linux MASQUERADE's default behaviour;
    /// behaves as a full cone for hole-punch purposes.
    Cone,
    /// Endpoint-dependent mapping: a fresh external port per destination
    /// (`--random` on the SNAT). The partner cannot predict the mapping, so
    /// the pair cannot punch and must relay.
    Symmetric,
}

/// Guard that builds and (on drop) tears down the three-namespace topology.
struct NetNsHarness;

impl NetNsHarness {
    /// Build the topology with the given egress NAT mode on both peers.
    /// Returns `None` (and the caller skips) when prerequisites are missing.
    fn setup(mode: NatMode) -> Option<Self> {
        if !is_root() {
            eprintln!("serverless_rungs: not running as root — skipping");
            return None;
        }
        if !command_exists("ip") || !command_exists("iptables") {
            eprintln!("serverless_rungs: ip or iptables not found — skipping");
            return None;
        }

        Self::teardown_namespaces();

        run_required("ip", &["netns", "add", NS_INTERNET]);
        run_required("ip", &["netns", "add", NS_PEER_A]);
        run_required("ip", &["netns", "add", NS_PEER_B]);

        Self::add_veth("veth-a-ext", "veth-a-int", NS_INTERNET, NS_PEER_A);
        Self::add_veth("veth-b-ext", "veth-b-int", NS_INTERNET, NS_PEER_B);

        for ns in [NS_INTERNET, NS_PEER_A, NS_PEER_B] {
            ns_run(ns, "ip", &["link", "set", "lo", "up"]);
        }

        ns_run(
            NS_INTERNET,
            "ip",
            &["addr", "add", &format!("{GW_A}/24"), "dev", "veth-a-ext"],
        );
        ns_run(NS_INTERNET, "ip", &["link", "set", "veth-a-ext", "up"]);
        ns_run(
            NS_INTERNET,
            "ip",
            &["addr", "add", &format!("{GW_B}/24"), "dev", "veth-b-ext"],
        );
        ns_run(NS_INTERNET, "ip", &["link", "set", "veth-b-ext", "up"]);

        Self::configure_peer(NS_PEER_A, "veth-a-int", PEER_A_IP, GW_A, mode);
        Self::configure_peer(NS_PEER_B, "veth-b-int", PEER_B_IP, GW_B, mode);

        ns_run(NS_INTERNET, "sysctl", &["-w", "net.ipv4.ip_forward=1"]);

        Some(Self)
    }

    /// Create one veth pair and move each end into its target namespace.
    fn add_veth(ext: &str, int: &str, ext_ns: &str, int_ns: &str) {
        run_required(
            "ip",
            &["link", "add", ext, "type", "veth", "peer", "name", int],
        );
        run_required("ip", &["link", "set", ext, "netns", ext_ns]);
        run_required("ip", &["link", "set", int, "netns", int_ns]);
    }

    /// Address, route, and NAT a peer namespace's internal interface.
    fn configure_peer(ns: &str, dev: &str, ip: &str, gw: &str, mode: NatMode) {
        ns_run(ns, "ip", &["addr", "add", &format!("{ip}/24"), "dev", dev]);
        ns_run(ns, "ip", &["link", "set", dev, "up"]);
        ns_run(ns, "ip", &["route", "add", "default", "via", gw]);

        let mut nat_args = vec![
            "-t",
            "nat",
            "-A",
            "POSTROUTING",
            "-o",
            dev,
            "-j",
            "MASQUERADE",
        ];
        if matches!(mode, NatMode::Symmetric) {
            // `--random` forces a fresh source-port allocation per
            // destination, turning the endpoint-independent default into an
            // endpoint-dependent (symmetric) mapping the partner cannot
            // predict — so the punch is impossible and the relay rung is the
            // only path.
            nat_args.push("--random");
        }
        ns_run(ns, "iptables", &nat_args);
    }

    fn teardown_namespaces() {
        for ns in [NS_INTERNET, NS_PEER_A, NS_PEER_B] {
            let _ = Command::new("ip").args(["netns", "delete", ns]).output();
        }
    }
}

impl Drop for NetNsHarness {
    fn drop(&mut self) {
        Self::teardown_namespaces();
    }
}

// ── Process orchestration helpers ────────────────────────────────────────────

/// Spawn the hub subprocess inside the internet namespace and read the
/// `HUBPORTS …` line it prints. Returns the child and the parsed ports.
///
/// `test_name` is the owning scenario's `#[test]` function name. libtest
/// treats the positional argument as a name filter, so it must be a real
/// test name (passed with `--exact`) for the child to run a `#[test]` body
/// and reach [`maybe_run_as_subprocess`]; an invented marker matches zero
/// tests and the child exits without dispatching any role.
fn spawn_hub(test_name: &str, role: &str, identity_dir: &str) -> (std::process::Child, Vec<u16>) {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;

    let current_exe = std::env::current_exe().expect("current_exe");
    let mut child = Command::new("ip")
        .args(["netns", "exec", NS_INTERNET])
        .arg(&current_exe)
        .args([test_name, "--exact", "--nocapture", "--test-threads=1"])
        .env(ROLE_ENV, role)
        .env(IDENTITY_DIR_ENV, identity_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawning hub subprocess");

    let stdout = child.stdout.take().expect("hub stdout");
    let mut reader = BufReader::new(stdout);
    let ports = loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).expect("reading hub stdout");
        assert!(n != 0, "hub closed stdout without printing HUBPORTS");
        // Under `--nocapture` libtest writes the `test <name> ... ` progress
        // prefix to stdout with no trailing newline, so the role's
        // `println!("HUBPORTS …")` lands on the same physical line. Scan for
        // the marker token anywhere in the line rather than requiring it at
        // the start. Everything after the marker is the space-separated port
        // list; libtest's trailing summary cannot follow it on this line
        // because `println!` terminates it.
        if let Some((_prefix, rest)) = line.split_once("HUBPORTS ") {
            break rest
                .split_whitespace()
                .map(|tok| tok.parse::<u16>().expect("hub port parses"))
                .collect::<Vec<_>>();
        }
    };
    // Keep stdout drained on a background thread so the child never blocks on
    // a full pipe while it serves the rendezvous/bridge.
    std::thread::spawn(move || {
        let mut sink = String::new();
        while reader.read_line(&mut sink).unwrap_or(0) != 0 {
            sink.clear();
        }
    });
    (child, ports)
}

/// Spawn a peer subprocess inside `ns` with the given environment, returning
/// the child handle. The caller joins it to collect the exit status.
///
/// `test_name` is the owning scenario's `#[test]` function name, passed as
/// the libtest filter with `--exact` so the child actually runs that test
/// body and reaches [`maybe_run_as_subprocess`]. See [`spawn_hub`].
fn spawn_peer(
    test_name: &str,
    ns: &str,
    role: &str,
    envs: &[(&str, String)],
) -> std::process::Child {
    use std::process::Stdio;
    let current_exe = std::env::current_exe().expect("current_exe");
    let mut cmd = Command::new("ip");
    cmd.args(["netns", "exec", ns])
        .arg(&current_exe)
        .args([test_name, "--exact", "--nocapture", "--test-threads=1"])
        .env(ROLE_ENV, role)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.spawn()
        .unwrap_or_else(|err| panic!("spawning {role}: {err}"))
}

/// Wait for a peer child up to [`ROLE_TIMEOUT`], killing it on timeout.
/// Returns `true` when it exited zero.
fn wait_peer(mut child: std::process::Child, role: &str) -> bool {
    let deadline = std::time::Instant::now() + ROLE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    eprintln!("{role} timed out after {ROLE_TIMEOUT:?}");
                    return false;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(err) => {
                eprintln!("waiting on {role}: {err}");
                return false;
            }
        }
    }
}

fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .is_some_and(|s| s.trim() == "0")
}

fn command_exists(command: &str) -> bool {
    Command::new("which")
        .arg(command)
        .output()
        .is_ok_and(|o| o.status.success())
}

fn ns_run(ns: &str, cmd: &str, args: &[&str]) {
    let status = Command::new("ip")
        .arg("netns")
        .arg("exec")
        .arg(ns)
        .arg(cmd)
        .args(args)
        .status()
        .unwrap_or_else(|err| panic!("failed to spawn `ip netns exec {ns} {cmd}`: {err}"));
    assert!(
        status.success(),
        "`ip netns exec {ns} {cmd} {args:?}` exited with {status}"
    );
}

fn run_required(cmd: &str, args: &[&str]) {
    let status = Command::new(cmd)
        .args(args)
        .status()
        .unwrap_or_else(|err| panic!("failed to spawn `{cmd} {args:?}`: {err}"));
    assert!(status.success(), "`{cmd} {args:?}` exited with {status}");
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// Name of the scenario (a) test, reused as the libtest filter when the
/// orchestrator re-invokes this binary for each subprocess role. It must
/// match the `#[test]` function name exactly so `--exact` selects it.
const TEST_PEER_AS_STUN: &str = "peer_as_stun_then_hole_punch";

/// Name of the scenario (b) test, used the same way as [`TEST_PEER_AS_STUN`].
const TEST_SYMMETRIC_RELAY: &str = "symmetric_pair_via_peer_relay";

/// Scenario (a): two NAT'd peers learn their reflexive mappings from a third
/// participating peer (peer-as-STUN — zero STUN servers), hole-punch, and
/// complete a block transfer over the punched flow. No STUN, announce, or
/// relay server is configured anywhere.
#[test]
fn peer_as_stun_then_hole_punch() {
    if maybe_run_as_subprocess() {
        return;
    }
    let Some(harness) = NetNsHarness::setup(NatMode::Cone) else {
        println!("peer_as_stun_then_hole_punch: prerequisites missing — test skipped");
        return;
    };

    let identity = DeviceIdentity::generate().expect("generate identity");
    let id_dir = tempfile::tempdir().expect("identity tempdir");
    identity.save(id_dir.path()).expect("save shared identity");
    let id_dir_str = id_dir.path().to_string_lossy().into_owned();

    let (mut hub, ports) = spawn_hub(TEST_PEER_AS_STUN, "stun-hub", &id_dir_str);
    assert_eq!(ports.len(), 2, "stun hub prints udp and tcp ports");
    let hub_udp = format!("{GW_A}:{}", ports[0]);
    let hub_tcp = format!("{GW_A}:{}", ports[1]);

    let common = |role: &str| -> Vec<(&str, String)> {
        vec![
            (IDENTITY_DIR_ENV, id_dir_str.clone()),
            (HUB_ADDR_ENV, hub_udp.clone()),
            ("CASCADE_SERVERLESS_HUB_TCP", hub_tcp.clone()),
            (HUB_DEVICE_ENV, identity.device_id.clone()),
            (ROLE_ENV, role.to_owned()),
        ]
    };

    let peer_b = spawn_peer(TEST_PEER_AS_STUN, NS_PEER_B, "punch-b", &common("punch-b"));
    std::thread::sleep(Duration::from_millis(200));
    let peer_a = spawn_peer(TEST_PEER_AS_STUN, NS_PEER_A, "punch-a", &common("punch-a"));

    let a_ok = wait_peer(peer_a, "punch-a");
    let b_ok = wait_peer(peer_b, "punch-b");

    let _ = hub.kill();
    let _ = hub.wait();
    drop(harness);

    assert!(a_ok, "punch-a did not complete the block transfer");
    assert!(b_ok, "punch-b did not complete the block transfer");
}

/// Scenario (b): a symmetric-NAT pair cannot punch and bridges through a
/// third, open peer acting as a peer relay (`RelayConnect` / `RelayRoute::Peer`).
/// No operated relay server is involved. A block transfers end-to-end through
/// the bridged sessions.
#[test]
fn symmetric_pair_via_peer_relay() {
    if maybe_run_as_subprocess() {
        return;
    }
    let Some(harness) = NetNsHarness::setup(NatMode::Symmetric) else {
        println!("symmetric_pair_via_peer_relay: prerequisites missing — test skipped");
        return;
    };

    let identity = DeviceIdentity::generate().expect("generate identity");
    let id_dir = tempfile::tempdir().expect("identity tempdir");
    identity.save(id_dir.path()).expect("save shared identity");
    let id_dir_str = id_dir.path().to_string_lossy().into_owned();

    let (mut hub, ports) = spawn_hub(TEST_SYMMETRIC_RELAY, "relay-hub", &id_dir_str);
    assert_eq!(ports.len(), 1, "relay hub prints one port");
    let hub_tcp = format!("{GW_A}:{}", ports[0]);

    let common = |role: &str| -> Vec<(&str, String)> {
        vec![
            (IDENTITY_DIR_ENV, id_dir_str.clone()),
            ("CASCADE_SERVERLESS_HUB_TCP", hub_tcp.clone()),
            (HUB_DEVICE_ENV, identity.device_id.clone()),
            (ROLE_ENV, role.to_owned()),
        ]
    };

    let peer_b = spawn_peer(
        TEST_SYMMETRIC_RELAY,
        NS_PEER_B,
        "relay-b",
        &common("relay-b"),
    );
    std::thread::sleep(Duration::from_millis(200));
    let peer_a = spawn_peer(
        TEST_SYMMETRIC_RELAY,
        NS_PEER_A,
        "relay-a",
        &common("relay-a"),
    );

    let a_ok = wait_peer(peer_a, "relay-a");
    let b_ok = wait_peer(peer_b, "relay-b");

    let _ = hub.kill();
    let _ = hub.wait();
    drop(harness);

    assert!(a_ok, "relay-a did not complete the block transfer");
    assert!(b_ok, "relay-b did not complete the block transfer");
}
