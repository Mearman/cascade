//! `SyncEngine` methods split out of `sync.rs` to keep that
//! file under the source-length cap. Declared from there via `mod ...;`,
//! so this is a child module of the parent and the methods stay part of
//! the same `impl SyncEngine` surface with full private access.

use super::{
    Arc, AtomicU64, BepMessage, BlockHash, ByteMeter, CallerAuthentication, ConnectionManager,
    Context, DeviceId, DiscoveredPeer, FileInfo, Folder, FramedHalfReader, FramedHalfWriter,
    FramedPeer, FramedSession, HashMap, ManageCommand, ManageErrorKind, ManageResult, ManageScope,
    Mutex, Peer, PeerHandle, RelayTransport, Result, SessionReaderBoxed, SessionWriterBoxed,
    SocketAddr, SyncEngine, SyncPunchAgreement, Transport, debug, entry_to_file_info,
    gather_local_candidates, info, mpsc, oneshot, peer_relay, unix_timestamp_seconds,
};

impl SyncEngine {
    /// Open an HMAC-authenticated relay connection to `peer` via `relay`.
    ///
    /// The relay client drives the full handshake against the
    /// `cascade-relay-server` (see
    /// [`cascade_p2p::relay::RelayClient::connect_with_secret`] and
    /// `crates/relay-server/src/auth.rs`). On success the relay
    /// WebSocket is upgraded to a full BEP session via
    /// [`RelayTransport`] → [`FramedSession`] → [`Self::run_transport_session`].
    ///
    /// The session id used for the rendezvous is the remote peer's
    /// device id: that matches the legacy `RelayClient::connect` API
    /// shape. A future round will agree the session id out of band so
    /// both peers meet at the same URL path.
    pub(crate) async fn attempt_relay(&self, peer: &Peer, relay: SocketAddr) -> Result<()> {
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
    pub(crate) async fn attempt_peer_relay(
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
    pub(crate) async fn run_relay_carry_loop(
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
    pub(crate) async fn forward_relay_data(&self, from_device: &str, payload: Vec<u8>) {
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
    pub(crate) async fn handle_inbound(
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
    /// stamped via [`PeerBook::mark_seen`](cascade_p2p::wan::PeerBook::mark_seen) so subsequent gossip
    /// broadcasts can carry an accurate per-peer `last_seen` instead of
    /// falling back to the broadcast time.
    pub(crate) async fn record_peer(&self, device_id: &str, address: SocketAddr) {
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
    pub(crate) async fn run_framed_session(
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
    pub(crate) fn run_transport_session<T>(
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
    pub(crate) async fn run_session_loop(
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
    pub(crate) fn snapshot_since(&self, since: u64) -> Result<Vec<FileInfo>> {
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
    pub(crate) async fn handle_message(
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
    /// A verified request is dispatched through the injected
    /// [`ManageDispatch`](cascade_engine::manage::ManageDispatch) port, which
    /// resolves the caller's grants, authorises, audits BEFORE
    /// applying any side effect, and runs the same command handlers the local
    /// CLI drives. When no dispatch port is configured the node is not accepting
    /// remote administration, so the request is refused with a typed
    /// [`ManageErrorKind::Unauthorised`] error rather than dropped.
    pub(crate) async fn handle_manage_request(
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
    pub(crate) async fn admit_relay_bridge(
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
    pub(crate) async fn spawn_relay_terminal(&self, carry_device: &str, source_device: String) {
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
    pub(crate) async fn remove_relay_terminal(&self, carry_device: &str) {
        self.relay_terminals.lock().await.remove(carry_device);
    }
}
