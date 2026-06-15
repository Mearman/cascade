# Cascade node protocol

> Status: **design**. This document specifies the wire contract by which nodes
> interoperate in a Cascade mesh, so that an independent implementation — in any
> language — can be a first-class peer, not a client of a Cascade binary. The
> protocol is the contract; no single codebase is. Parts marked **draft
> (co-design)** are not yet implemented and are being designed jointly with the
> first external implementation.

## Why a specified protocol

Cascade's mesh is valuable beyond the Cascade binary: its device identity,
authenticated transport, capability-token authorisation, content exchange, and
(forthcoming) exec and op-log frames are exactly what another tool would
otherwise reimplement to mesh with it. Rather than have such tools depend on the
Cascade binary at runtime, they should be able to implement this protocol and
join as peers.

The hazard of two implementations is drift: an under-specified protocol degrades
"interoperable" to "interoperable in theory" the first time one side relies on
behaviour the other never promised. So three rules are load-bearing:

1. **The wire is specified here, not by reference to the Rust code.** Behaviour
   that matters for interop is written down; the implementation conforms to the
   document, not the reverse.
2. **Versioned, with a capability-negotiation handshake.** Nodes advertise a
   protocol version and the capability domains they support, and degrade
   gracefully — heterogeneous peers (one with capabilities the other lacks) are
   the normal case, not an error.
3. **Conformance vectors are the forcing function.** A shared, language-neutral
   set of test vectors — handshake transcripts, token-verification cases, frame
   encode/decode fixtures — is executed by every implementation's CI. Two
   implementations that both pass the vectors stay compatible; documentation
   alone does not guarantee that.

## Identity

A node's identity is its **device ID**: the base32 encoding of the SHA-256 of its
self-generated TLS certificate. All peer connections are TLS-encrypted and
authenticated by device ID — a peer is exactly the holder of the private key for
the certificate whose hash is its ID. This is the existing Cascade scheme; an
interoperating implementation adopts it verbatim so identities are mutually
verifiable.

## Transport and handshake

Peer connections are mutually-authenticated TLS. Reachability is governed by a
single `DiscoveryReach` posture (`lan-only` / `private` / `public`); WAN peers
traverse NAT via the opaque byte-pipe relay (HMAC-gated, payload-blind) and the
rendezvous-by-presence path under the `public` posture. On connect, peers
exchange a **handshake** carrying:

- the protocol **version**,
- the set of **capability domains** the node supports (see below),
- identity proof (implicit in the TLS layer).

A peer must not send frames for a capability domain the other did not advertise,
and must reject (or quarantine) frames it does not understand rather than guess.

## Capability domains

A node advertises which of these it implements; the mesh is heterogeneous by
design.

| Domain          | Frames                                  | Status            |
| --------------- | --------------------------------------- | ----------------- |
| `content`       | block exchange (BEP-derived)            | implemented       |
| `management`    | `ManageRequest` / `ManageResponse`      | implemented       |
| `exec`          | process/PTY control + streams           | draft (co-design) |
| `oplog`         | per-peer append-only log sync           | draft (co-design) |

A node that implements only `content` + `management` is a normal Cascade file
node; one that adds `exec` can broker terminals and processes; one that adds
`oplog` participates in a replicated operation log. None is required of all
peers.

## Authorisation

Authority is a **capability grant** — a verb over a scope — held on a node and
carried, between nodes, as a **signed capability token**. Tokens are signed by
the issuing node's device key; a bearer presents one and the verifier checks
signature, expiry, and revocation before authorising the carried grant through
the same path an on-node grant takes. Delegation forms **bounded chains**: each
hop can only narrow authority, never widen it, and a token's expiry is clamped to
its parent's. Every authorised command is written to an append-only audit log.

The dangerous verb classes — backend administration, node lifecycle, grant
administration, and (when implemented) **exec** — are never satisfied by a
node-wide grant; they require an explicit scope and a deliberate grant. See
[`exec-capability.md`](exec-capability.md) for why exec sits in this tier.

## Frame categories

- **`content` — block exchange.** Content-addressed, immutable blocks. The
  substrate for file bytes; adaptive block sizes; last-write-wins per block for
  P2P-only folders. (Implemented.)
- **`management` — control.** `ManageRequest` / `ManageResponse`: a verb command
  set (status, pin, cache, config, policy, backend, lifecycle, grant
  administration) dispatched into the same handlers the local CLI drives, gated
  by per-command authorisation and audited. (Implemented.)
- **`oplog` — per-peer log sync (draft).** An operation log replicated as
  content: each peer's log is a **single-writer, append-only file**, so two
  peers' logs can never block-conflict, and distributing them is replication —
  consumers merge all peers' logs by a deterministic reduce. The unit is a
  per-peer log file, never one shared log file (whose per-block LWW would corrupt
  concurrent appends). The op shape, signing, and the reduce contract are the
  co-design items.
- **`exec` — process/PTY control and streams (draft).** Control verbs travel as
  `management` frames; live stdin/stdout/stderr travel as **stream channels over
  the transport**, never through the content-addressed block store (a live stream
  is not immutable content). See [`exec-capability.md`](exec-capability.md).

## Versioning and compatibility

Every connection negotiates a protocol version and capability set at handshake.
A version bump that changes a frame's meaning is gated by the conformance
vectors: an implementation claiming a version must pass that version's vectors.
Unknown capabilities and unknown frame types are ignored or quarantined per
posture, never assumed.

## Status

Implemented today: identity, TLS transport, discovery/relay, capability tokens
and delegation, `content`, and `management`. The `exec` and `oplog` domains are
draft and co-designed with the first external peer implementation; this document
is the place their wire shapes land before either side builds them.
