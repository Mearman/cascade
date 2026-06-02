# NAT hole-punching design

Cascade's P2P layer needs to connect peers behind home routers, carrier NATs,
and corporate firewalls. STUN detection already runs at startup but nothing
reacts to its output. This document specifies how to turn the detection signal
into an actual connectivity decision — direct, hole-punched, or relayed — and
the protocol changes required to negotiate it.

## Status quo

The STUN client in [`crates/p2p/src/nat.rs`](../crates/p2p/src/nat.rs) (lines
53–62) issues an RFC 5389 Binding Request and compares the mapped address to
the local socket. With a single server it distinguishes only two outcomes:
`NatType::Open` when the addresses match and `NatType::Symmetric` otherwise.
The cone variants in the enum are unreachable.

`detect_nat_with_logging`
([`crates/backend-p2p/src/lib.rs:722-754`](../crates/backend-p2p/src/lib.rs))
runs once at startup against the configured `stun_servers`
([`lib.rs:112-115`](../crates/backend-p2p/src/lib.rs)) and emits a tracing
line. The comment at `lib.rs:243-248` is explicit: "the current `SyncEngine`
does not expose a hook to influence connection behaviour from this signal."

A relay transport exists too —
[`crates/p2p/src/relay.rs`](../crates/p2p/src/relay.rs) is a WebSocket
byte-pipe that tunnels TLS-wrapped BEP frames between two endpoints, and
`crates/relay-server/` carries the matching server. The `SyncEngine`
drives `RelayClient::connect_with_secret` once `decide_connectivity`
picks the `Relay` strategy, authenticating against the server with the
configured shared HMAC secret; the post-relay BEP transport upgrade
(running a real BEP session over the tunnel) lives with the post-punch
upgrade in a future round.

## NAT taxonomy

RFC 4787 classifies NAT mapping and filtering behaviour along two axes; the
historical four-way split is a coarser shorthand:

| NAT type | Mapping | Filtering | Hole-punchable? |
|---|---|---|---|
| **Full cone** | Endpoint-independent | Endpoint-independent | Yes, any remote can dial in once a mapping exists |
| **Restricted cone** | Endpoint-independent | Address-dependent | Yes — both sides must first send to the remote IP |
| **Port-restricted cone** | Endpoint-independent | Address-and-port-dependent | Yes — both sides must send to the remote IP and port |
| **Symmetric** | Address-and-port-dependent | Address-and-port-dependent | No reliably; mapping changes per destination |

The pairwise traversability matrix (assuming UDP, simultaneous open):

|  | Public | Full | Restricted | Port-restricted | Symmetric |
|---|---|---|---|---|---|
| **Public** | Direct | Direct | Direct (peer dials) | Direct (peer dials) | Direct (peer dials) |
| **Full** | Direct | Punch | Punch | Punch | Punch |
| **Restricted** | Direct | Punch | Punch | Punch | Relay |
| **Port-restricted** | Direct | Punch | Punch | Punch | Relay |
| **Symmetric** | Direct | Punch | Relay | Relay | Relay |

Symmetric ↔ symmetric and symmetric ↔ port-restricted are the common
deployments that cannot punch through with plain STUN. Distinguishing the
cone variants needs a two-server protocol per RFC 5780. Cascade treats
anything-but-symmetric as punchable and escalates to relay only when at least
one side is symmetric — the rule libp2p, WebRTC, and Tailscale converged on.

Sources: [RFC 4787](https://datatracker.ietf.org/doc/html/rfc4787),
[RFC 5780](https://datatracker.ietf.org/doc/html/rfc5780).

## Connectivity decision tree

```rust
enum ConnectivityStrategy {
    Direct { addr: SocketAddr },
    HolePunch { coordinator_addr: SocketAddr, candidates: Vec<SocketAddr> },
    Relay { relay_addr: Url },
}

fn decide_connectivity(local: NatType, remote: NatType) -> ConnectivityStrategy {
    // Public-to-anything: the public side accepts inbound. If we're the
    // public side, advertise; if we're the NAT'd side, dial.
    if matches!(local, Public) || matches!(remote, Public) {
        return Direct { addr: public_side_listen_addr() };
    }

    // Two cones never both symmetric: hole-punch with the gossip channel
    // as the rendezvous coordinator.
    let cone = |n| matches!(n, FullCone | RestrictedCone | PortRestrictedCone);
    if cone(local) && cone(remote) {
        return HolePunch { coordinator_addr: gossip_path(), candidates: local_candidates() };
    }

    // Mixed cone ↔ symmetric: cone side can still punch if it's full-cone
    // (mapping survives across destinations on the symmetric side's view).
    // Restricted and port-restricted against symmetric is unreliable.
    match (local, remote) {
        (FullCone, Symmetric) | (Symmetric, FullCone) => {
            HolePunch { coordinator_addr: gossip_path(), candidates: local_candidates() }
        }
        _ => Relay { relay_addr: nearest_relay() },
    }
}
```

The strategy is computed once per peer after both sides exchange candidates,
and re-evaluated when a previously direct connection drops or when STUN is
re-run on a network change.

## Signalling channel

ICE-style hole punching needs both peers to learn each other's candidate sets
before they start sending UDP probes. Three options:

**(a) Piggy-back on the existing gossip channel.** Add
`BepMessage::Candidates` to
[`crates/p2p/src/protocol.rs`](../crates/p2p/src/protocol.rs), exchanged once
the TLS handshake completes — over the existing relay or LAN connection if one
exists, or via an introducer peer otherwise.

**(b) Separate rendezvous server.** A central TCP service brokering candidate
exchange. Reintroduces the central server the gossip design explicitly avoids
([`wan.rs:1-10`](../crates/p2p/src/wan.rs)).

**(c) DHT-based discovery.** Syncthing's Global Discovery model. Heavyweight
to operate for a feature most users only need at bootstrap.

**Recommendation: (a).** Introducer-mediated candidate exchange fits the
existing trust model: when A and B share introducer I, the flow A → I → B
carries A's candidates and B replies symmetrically. No new infrastructure, no
new trust roots, same fail-mode as gossip itself.

Wire format (XDR-encoded alongside the existing BEP messages):

```text
BepMessage::Candidates {
    request_id:   u64,             // correlates with hole-punch attempts
    target_device: [u8; 32],       // routing: who this is for
    candidates:    Vec<Candidate>,
}
Candidate {
    kind:          u8,             // 0 = host, 1 = server-reflexive (STUN), 2 = relay
    addr:          SocketAddr,
    priority:      u32,            // ICE-style: prefer host > srflx > relay
}
```

The `target_device` field lets an intermediary route the frame without
inspecting payloads — the intermediary forwards if the target is in its peer
book and discards otherwise.

## Hole-punching protocol

Adapted from libp2p DCUtR. Assume A and B have exchanged candidate sets and
both NAT types permit a punch.

1. **t = 0**: both sides receive each other's candidates and each issues a
   STUN Binding Request to refresh its server-reflexive mapping (3 s timeout,
   reusing `stun_binding_request`).
2. **t = +RTT/2**: the side with the lexicographically lower device ID sends
   `BepMessage::SyncPunch { ts }` over the signalling channel; the other
   echoes it. Both sides record the RTT and schedule the punch.
3. **t = sync_ts**: each side sends three UDP packets 10 ms apart from its
   listening socket to *every* remote candidate. Payload is
   `MAGIC || device_id || nonce`; receipt opens a hole if the NATs cooperate.
4. **t = sync_ts + 50 ms**: each side listens for inbound probes. The first
   valid `MAGIC` packet is echoed back; the originator marks that candidate
   pair `Reachable`.
5. **Retry**: if no pair reaches `Reachable` within 2 s, repeat with 5 s
   backoff, up to three rounds. After three failures, fall through to relay.

Same shape as libp2p DCUtR (synchronised dial, multi-probe, retry with
backoff) and Tailscale's `magicsock`. The 3 × 3-probe-then-relay budget caps
worst-case handshake latency around 10 s.

## Relay fallback

When hole punching fails, traffic flows through a relay. Two operational
models:

**(a) Self-hosted relays on user devices.** Any device detected as
`NatType::Open` can opt in to relay for its peers. The
[`relay.rs`](../crates/p2p/src/relay.rs) byte-pipe model means the relay sees
only ciphertext; cost is bandwidth on the operator's own hardware. Discovery
follows the gossip channel: a relay-capable device advertises a
`relay_endpoint` alongside its peer book entry.

**(b) Volunteer relay pool.** A Syncthing-style network of community-operated
relays.

**Recommendation: (a), with (b) as an opt-in supplement later.** Cascade's
threat model already trusts the device list (TLS certificate fingerprint per
[CLAUDE.md](../CLAUDE.md)), so using your own public-internet device as a
relay extends no trust. A volunteer pool changes the funding model, raises a
governance question, and should wait until users ask for it. For users with
no public-IP device, the recommended path is a $5 VPS.

Security is straightforward because the relay never holds plaintext: TLS
terminates between the two endpoints, and the relay framing
([`relay.rs:82-114`](../crates/p2p/src/relay.rs)) carries opaque bytes.
Operators see traffic shape but not content. Rate-limiting and per-target
quotas live on the relay server.

## Integration points

| Item | Path | Complexity |
|---|---|---|
| New: traversal coordinator (`decide_connectivity`, candidate gathering, probe loop) | `crates/p2p/src/traversal.rs` | M |
| New: relay server binary (Axum + tokio-tungstenite, pairs with existing `RelayClient`) | `crates/relay-server/` | M |
| New: candidate-pair scorer (ICE-style priority, RFC 8445 §5.1.2) | `crates/p2p/src/candidate.rs` | S |
| Modify: add `Candidates` and `SyncPunch` BEP variants | `crates/p2p/src/protocol.rs` | S |
| Modify: full RFC 5780 detection (two-server) with single-server fallback | `crates/p2p/src/nat.rs` | M |
| Modify: hook `decide_connectivity` between peer-lookup and dial | `crates/backend-p2p/src/sync.rs` | L |
| Modify: reconnect loop requests strategy from coordinator on failure | `crates/backend-p2p/src/lib.rs` | S |
| Modify: add `relay_endpoints` to config; gate hole punch on the `exposure` posture (`DiscoveryReach`) | `crates/backend-p2p/src/lib.rs` | S |
| Modify: `PeerBook` carries `last_known_nat_type` and `relay_endpoint` | `crates/p2p/src/wan.rs` | S |
| Modify: cross-link P2P traversal section | `docs/design.md` | S |
| Test: mock NAT topology harness | `crates/p2p/tests/nat_traversal.rs` | M |
| Test: NAT'd containers behind iptables MASQUERADE | `test/e2e/p2p/compose.yml` | M |

## Testing strategy

Real NAT hardware is unnecessary if the contract is faithfully modelled.
Three layers:

**Unit tests against a mock NAT.** A `MockNat` wraps a `UdpSocket` and
rewrites outbound `(src, dst)` pairs per a configurable policy (full,
restricted, port-restricted, symmetric). The traversal coordinator drives
candidate gathering and probe exchange against two `MockNat` instances on a
virtual link. Every (local, remote) pair from the taxonomy gets a
deterministic test asserting the chosen `ConnectivityStrategy`.

**Integration tests with Linux netns.** A harness builds two network
namespaces with `ip netns`, applies `iptables -t nat -A POSTROUTING` rules
per NAT class, and runs the real `traversal.rs` against a mock STUN server
and mock relay. Gated behind `#[ignore]` and `CASCADE_NETNS_TESTS=1` because
it needs `CAP_NET_ADMIN`; runs in CI via a privileged job.

**Docker Compose extension to `test/e2e/p2p/`.** Add three containers — two
peers, one relay — behind separate `MASQUERADE` networks with different
filtering rules. Assert that the block-exchange e2e suite still completes
with both peers behind symmetric NATs (forces relay) and again behind
restricted-cone NATs (forces punch). The existing
[`test/e2e/p2p/compose.yml`](../test/e2e/p2p/compose.yml) is the natural home.

## Open questions

- **Relay binary distribution.** Ship as a separate `cascade-relay` artefact
  in the existing release matrix, or fold it into the main binary behind a
  `cascade relay serve` subcommand?
- **NAT re-detection cadence.** Should detection re-run on network change
  events (link up/down, IP change)? Implementing this needs OS hooks
  per-platform; an interval-based fallback might be acceptable for v1.
- **IPv6 dual-stack policy.** When a peer is reachable on both IPv4 and IPv6,
  should candidates be tried in parallel (Happy Eyeballs) or strictly
  preferred (IPv6 first, IPv4 fallback)?
- **DCUtR upgrade path on libp2p.** libp2p ships a Rust implementation in
  `rust-libp2p`. Worth evaluating whether to vendor that crate or implement
  natively given the rest of the BEP stack is already homegrown.
- **Relay billing model.** If the volunteer pool path is taken later, how is
  abuse prevented and how are operators reimbursed? Out of scope for the
  recommended self-hosted-only v1.

## Sources

- RFC 4787 — Network Address Translation (NAT) Behavioral Requirements for
  Unicast UDP. <https://datatracker.ietf.org/doc/html/rfc4787>
  ([Wayback](https://web.archive.org/web/2026/https://datatracker.ietf.org/doc/html/rfc4787))
- RFC 5780 — NAT Behavior Discovery Using STUN.
  <https://datatracker.ietf.org/doc/html/rfc5780>
  ([Wayback](https://web.archive.org/web/2026/https://datatracker.ietf.org/doc/html/rfc5780))
- RFC 5389 — Session Traversal Utilities for NAT (STUN).
  <https://datatracker.ietf.org/doc/html/rfc5389>
  ([Wayback](https://web.archive.org/web/2026/https://datatracker.ietf.org/doc/html/rfc5389))
- RFC 8445 — Interactive Connectivity Establishment (ICE).
  <https://datatracker.ietf.org/doc/html/rfc8445>
  ([Wayback](https://web.archive.org/web/2026/https://datatracker.ietf.org/doc/html/rfc8445))
- libp2p Direct Connection Upgrade through Relay (DCUtR) specification.
  <https://github.com/libp2p/specs/blob/master/relay/DCUtR.md>
  ([Wayback](https://web.archive.org/web/2026/https://github.com/libp2p/specs/blob/master/relay/DCUtR.md))
- libp2p Circuit Relay v2 specification.
  <https://github.com/libp2p/specs/blob/master/relay/circuit-v2.md>
  ([Wayback](https://web.archive.org/web/2026/https://github.com/libp2p/specs/blob/master/relay/circuit-v2.md))
- libp2p AutoNAT specification.
  <https://github.com/libp2p/specs/blob/master/autonat/README.md>
  ([Wayback](https://web.archive.org/web/2026/https://github.com/libp2p/specs/blob/master/autonat/README.md))
- Tailscale "How NAT traversal works" — David Crawshaw, 2020.
  <https://tailscale.com/blog/how-nat-traversal-works>
  ([Wayback](https://web.archive.org/web/2026/https://tailscale.com/blog/how-nat-traversal-works))
- Tailscale DERP design notes.
  <https://tailscale.com/kb/1232/derp-servers>
  ([Wayback](https://web.archive.org/web/2026/https://tailscale.com/kb/1232/derp-servers))
- Syncthing Block Exchange Protocol v1 specification.
  <https://docs.syncthing.net/specs/bep-v1.html>
  ([Wayback](https://web.archive.org/web/2026/https://docs.syncthing.net/specs/bep-v1.html))
- Syncthing Relay Protocol.
  <https://docs.syncthing.net/specs/relay-v1.html>
  ([Wayback](https://web.archive.org/web/2026/https://docs.syncthing.net/specs/relay-v1.html))
- Syncthing Global Discovery Protocol.
  <https://docs.syncthing.net/specs/globaldisco-v3.html>
  ([Wayback](https://web.archive.org/web/2026/https://docs.syncthing.net/specs/globaldisco-v3.html))
- WebRTC "Anatomy of a WebRTC SDP" / Trickle ICE — Mozilla MDN.
  <https://developer.mozilla.org/en-US/docs/Web/API/WebRTC_API/Connectivity>
  ([Wayback](https://web.archive.org/web/2026/https://developer.mozilla.org/en-US/docs/Web/API/WebRTC_API/Connectivity))
