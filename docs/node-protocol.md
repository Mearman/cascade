# Cascade node protocol

> Status: **protocol version 1, frozen**. This document specifies the wire
> contract by which nodes interoperate in a Cascade mesh, so that an independent
> implementation — in any language — can be a first-class peer, not a client of a
> Cascade binary. The protocol is the contract; no single codebase is. The
> capability-negotiation handshake, the exec control verbs and live stdio stream
> frames, and the oplog sync frames are all frozen at version 1: their byte
> layouts are fixed and exercised by the language-neutral conformance vectors in
> [`conformance/`](conformance/). The one part still in co-design is the *internal
> byte shape of an oplog entry* — its payload schema, signature, and the
> deterministic reduce/merge — which the oplog frames carry as opaque,
> length-prefixed bytes; the frame envelope around it is frozen.

## Why a specified protocol

Cascade's mesh is valuable beyond the Cascade binary: its device identity,
authenticated transport, capability-token authorisation, content exchange, and
exec and op-log frames are exactly what another tool would otherwise reimplement
to mesh with it. Rather than have such tools depend on the Cascade binary at
runtime, they should be able to implement this protocol and join as peers.

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
rendezvous-by-presence path under the `public` posture. On connect, immediately
after TLS verification and *before* any other post-TLS frame, each peer sends a
single **Handshake** frame (message type 17) carrying:

- the protocol **version** (a `u32`; version 1 is the first negotiated version),
- the set of **capability domains** the node implements (see below), each a
  frozen `u32` discriminant,
- identity proof (implicit in the TLS layer).

The usable capability set for the connection is the **intersection** of what the
local node advertises and what the peer advertised, taken in the frozen domain
discriminant order `content < management < exec < oplog`, so the negotiated set
is independent of the order either side listed its domains. A peer must not send
frames for a capability domain that is not in the negotiated set (one the other
did not advertise), and must reject (or quarantine) frames it does not understand
rather than guess. A node advertises `exec` only when it has an exec provider
wired in, and `oplog` only when the oplog subsystem is built; the present
implementation advertises `content`, `management`, and `exec`.

A peer that sends `ClusterConfig` (the content domain's opening frame) without a
preceding Handshake is treated as a **pre-version peer** advertising only
`content` and `management` — the documented baseline before versioned
negotiation. This keeps a node that predates the handshake interoperable as a
plain file-plus-management peer; in a greenfield mesh the Handshake is strictly
required and the absence of one is a protocol error.

## Capability domains

A node advertises which of these it implements; the mesh is heterogeneous by
design.

| Domain          | Frames                                                          | Wire tag | Status                       |
| --------------- | --------------------------------------------------------------- | -------- | ---------------------------- |
| `content`       | block exchange (BEP-derived)                                    | 0        | implemented                  |
| `management`    | `ManageRequest` / `ManageResponse`                              | 1        | implemented                  |
| `exec`          | PTY/process control (as `management`) + `ExecStream` stdio      | 2        | frozen (v1)                  |
| `oplog`         | `OplogHave` / `OplogRequest` / `OplogData` per-peer log sync    | 3        | envelope frozen (v1)         |

The `oplog` envelope is frozen; the *entry payload* it carries is opaque, so a
node may advertise `oplog` only once it has agreed the entry schema with its
peers.

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
administration, and **exec** (both `exec:pty` and `exec:proc`) — are never
satisfied by a node-wide grant; they require an explicit folder scope and a
deliberate grant. The verifier enforces this with a single rule: a dangerous
capability whose grant scope is node-wide (root or empty folder — `/`, ``, `//`,
`/.`) never authorises. An exec token narrows and expiry-clamps through the same
capability-agnostic delegation path as every other token. See
[`exec-capability.md`](exec-capability.md) for why exec sits in this tier.

## Frame categories

- **`content` — block exchange.** Content-addressed, immutable blocks. The
  substrate for file bytes; adaptive block sizes; last-write-wins per block for
  P2P-only folders. (Implemented.)
- **`management` — control.** `ManageRequest` / `ManageResponse`: a verb command
  set (status, pin, cache, config, policy, backend, lifecycle, grant
  administration) dispatched into the same handlers the local CLI drives, gated
  by per-command authorisation and audited. (Implemented.)
- **`oplog` — per-peer log sync.** An operation log replicated as content: each
  peer's log is a **single-writer, append-only file**, so two peers' logs can
  never block-conflict, and distributing them is replication — consumers merge all
  peers' logs by a deterministic reduce. The unit is a per-peer log file, never one
  shared log file (whose per-block LWW would corrupt concurrent appends). The sync
  is three frames — `OplogHave` (advertise the head sequence of a named peer's
  log), `OplogRequest` (ask for entries after a sequence), `OplogData` (carry a
  contiguous range of opaque, signed entries). The frame envelope (owning peer id,
  contiguous sequence range, length-prefixed opaque entries) is frozen at version
  1. The **internal byte shape of an entry, its signature, and the deterministic
  reduce/merge** are the remaining co-design items — the protocol crate treats each
  entry as opaque bytes and never interprets them.
- **`exec` — process/PTY control and streams.** Control verbs travel as
  `management` frames: the seven verbs `pty.spawn`, `pty.write`, `pty.resize`,
  `pty.kill`, `proc.spawn`, `proc.signal`, `proc.kill` are `ManageCommand`
  variants (wire discriminants 13..=19), and a `*.spawn` reply carries the new
  session id in a `ManageResult::ExecSpawned { session }` (result discriminant 2).
  Live `stdin`/`stdout`/`stderr` travel as **`ExecStream` frames multiplexed over
  the single peer connection** (message type 18), never through the
  content-addressed block store (a live stream is not immutable content).
  Backpressure is explicit: a consumer advertises a credit window with
  `ExecStreamAck` (message type 19) and the producer must not send past it, so a
  slow consumer throttles the producer rather than the node buffering unboundedly.
  Process exit is reported through the control plane, never as a stream frame.
  Exec stream frames are governed per-frame by the session's owner, so a revoked
  grant cuts the stream at the next frame. See
  [`exec-capability.md`](exec-capability.md).

## Frame envelope and message types

Every frame is `[4-byte big-endian body length][4-byte big-endian message type][XDR body]`.
Message-type allocations are append-only and never renumbered. Variable-width
fields are length-prefixed XDR opaque/string (4-byte length, then the bytes,
then zero-padding to a 4-byte boundary); fixed fields are big-endian `u32`,
`u64`, `i32`, `i64`. Optional fields are a one-word sentinel (`0` = none, `1` =
some) followed, when present, by the value.

| Type | Frame              | Domain        | Frozen |
| ---- | ------------------ | ------------- | ------ |
| 0    | `ClusterConfig`    | content       | yes    |
| 1    | `Index`            | content       | yes    |
| 2    | `IndexUpdate`      | content       | yes    |
| 3    | `Request`          | content       | yes    |
| 4    | `Response`         | content       | yes    |
| 5    | `Ping`             | transport     | yes    |
| 6    | `Close`            | transport     | yes    |
| 7    | `Gossip`           | transport     | yes    |
| 8    | `Candidates`       | transport     | yes    |
| 9    | `SyncPunch`        | transport     | yes    |
| 10   | `ObservedAddress`  | transport     | yes    |
| 11   | `RelayOffer`       | transport     | yes    |
| 12   | `RelayConnect`     | transport     | yes    |
| 13   | `RelayData`        | transport     | yes    |
| 14   | `RelayInbound`     | transport     | yes    |
| 15   | `ManageRequest`    | management    | yes    |
| 16   | `ManageResponse`   | management    | yes    |
| 17   | `Handshake`        | transport     | yes    |
| 18   | `ExecStream`       | exec          | yes    |
| 19   | `ExecStreamAck`    | exec          | yes    |
| 20   | `OplogHave`        | oplog         | yes    |
| 21   | `OplogRequest`     | oplog         | yes    |
| 22   | `OplogData`        | oplog (envelope) |  yes |
| 23   | `ExecExit`         | exec          | yes    |

Transport frames (the handshake itself, keepalive, NAT-traversal, and relay
frames) are domain-independent: every peer speaks them regardless of the
negotiated capability set. The remaining frames are governed by the domain in the
table; the receiver refuses an inbound frame whose domain is not in the
negotiated set. (Exec *control* travels as `management` frames and is governed by
the management domain plus the exec capability grant, not by the exec domain
mapping; the exec domain governs only the `ExecStream`/`ExecStreamAck`/
`ExecExit` stdio frames.)

### Handshake (type 17)

Body: `u32 protocol_version`, `u32 domain_count`, then `domain_count` × `u32`
domain discriminant (`0` content, `1` management, `2` exec, `3` oplog). An
unknown discriminant is dropped, never assumed. `domain_count` is bounded.

### Exec control verbs (within `ManageRequest`, type 15)

The `ManageRequest` body is `u64 request_id`, the command, the target
`ManageScope`, then an optional token (JSON string). The command is a `u32`
discriminant followed by its fields:

- `pty.spawn` (13): opt-string `shell`, string-list `argv`, opt-string `cwd`,
  env (count then `(name, value)` string pairs), `u32` `cols`, `u32` `rows`.
- `pty.write` (14): `u64 session`, opaque `bytes`.
- `pty.resize` (15): `u64 session`, `u32 cols`, `u32 rows`.
- `pty.kill` (16): `u64 session`, `i32 signal`.
- `proc.spawn` (17): string-list `argv`, opt-string `cwd`, env.
- `proc.signal` (18): `u64 session`, `i32 signal`.
- `proc.kill` (19): `u64 session`.

`cols`/`rows` are carried as `u32` words on the wire and range-checked to `u16`
on decode. A `*.spawn` reply is a `ManageResponse` (type 16) whose
`ManageResult` is `ExecSpawned` (discriminant 2): `u64 session`. The signal is a
signed `i32`; POSIX signal numbers apply on Unix, and a Windows node supports
only terminate/kill — the signal-to-action mapping beyond TERM/KILL is
platform-specific and not part of the frozen wire contract.

The scope a `write`/`resize`/`kill`/`signal` is authorised over is **the scope
its session was spawned under**, resolved from node state, not the scope on the
wire — a caller holding `exec:pty` over `/work` cannot drive a session spawned
under `/personal` by lying in the wire scope.

### Exec stdio (`ExecStream`, type 18)

Body: `u64 session`, `u64 seq` (per-session monotonic, for ordering and ack),
`u32 stream` (`0` stdin, `1` stdout, `2` stderr — frozen; an unknown value
fails to decode), opaque `bytes`. Stdin travels manager → node (only `stream ==
0`); stdout/stderr travel node → manager. The bytes never enter the
content-addressed block store.

### Exec backpressure (`ExecStreamAck`, type 19)

Body: `u64 session`, `u64 ack_seq` (highest contiguous sequence accepted), `u32
window` (credit, in bytes, the consumer will accept past `ack_seq`). The producer
must not send beyond the window.

### Exec exit (`ExecExit`, type 23)

Body: `u64 session`, `Option<i32> code` (presence sentinel `0`=absent / `1`=present
then `i32`), `Option<i32> signal` (same encoding). Sent exactly once by the node's
exec output pump after the last `ExecStream` output frame, on the session's
terminal exit. It is a single control frame, not credit-gated: it carries no
sequence number and the manager routes it to the exec-stream consumer registered
for `(device_id, session)` without acking. Exactly one of `code`/`signal` is
present for a normal Unix exit; both absent means the exit status was
indeterminate (the CLI maps that to exit code `1`). A signal-killed process
carries `signal`; the CLI maps it to `128 + signal` per the shell convention.

### Oplog sync (types 20–22)

- `OplogHave` (20): string `peer`, `u64 head_seq`.
- `OplogRequest` (21): string `peer`, `u64 from_seq` (exclusive lower bound).
- `OplogData` (22): string `peer`, `u64 from_seq`, `u32 op_count`, then
  `op_count` × opaque entry. The i-th entry (zero-based) carries sequence
  `from_seq + 1 + i`; a receiver rejects a frame whose `from_seq` would leave a
  gap. Each entry is opaque to the protocol crate — its schema, signature, and the
  reduce/merge consumers apply are the co-design items, not part of the frozen
  envelope.

## Versioning and compatibility

Every connection negotiates a protocol version and capability set at handshake.
The current version is **1**. A version bump that changes a frame's meaning is
gated by the conformance vectors: an implementation claiming a version must pass
that version's vectors. Unknown capabilities and unknown frame types are ignored
or quarantined per posture, never assumed.

## Conformance vectors

The forcing function against drift is a set of language-neutral, byte-exact test
vectors in [`conformance/`](conformance/), versioned by protocol version and
consumed by every implementation's CI:

- `frames.v1.json` — for each frozen frame, the canonical message described for a
  human and the lowercase hex of its full `[len][type][body]` frame. A conformant
  codec must decode the hex to the message and re-encode the message to exactly
  the same hex. Covers the handshake, the exec `ManageCommand` verbs, the
  `ExecStream`/`ExecStreamAck`/`ExecExit` stdio frames, and the oplog frames
  (with arbitrary opaque entry bytes, since the entry payload is not frozen).
- `handshake.v1.json` — for each `(local domains, peer domains)` pair, the
  expected negotiated set and the domains whose frames the local node must refuse
  from that peer. Drives the heterogeneous-peer and graceful-degradation rules,
  including the pre-version-peer baseline.
- `tokens.v1.json` — fully materialised capability tokens (issuer certificate and
  signature inline, so verification needs no shared private key), each with the
  verifying node, the connected device, the wall clock, and the revocation set,
  plus the verdict the verifier must reach (`ok`, or a named hard rejection).
  Covers the success path, delegation chains, scope containment, expiry,
  revocation, bearer binding, and an `exec:pty` token. The signing bytes are the
  serialiser-independent canonical encoding, so the verdicts reproduce across
  languages.

Cascade runs these same JSON files in its own CI (`conformance_frames`,
`conformance_handshake`, `conformance_tokens`), so a drift in either Cascade's
codec/verifier or an external peer's is caught by the shared vectors rather than
discovered in production.

## Status

Implemented today: identity, TLS transport, discovery/relay, capability tokens
and delegation, `content`, `management`, the version/capability handshake, and
the `exec` control verbs and stdio stream frames, all frozen at version 1 and
pinned by the conformance vectors. The `oplog` frame envelope is frozen; its
entry payload, signature, and deterministic reduce/merge remain co-designed with
the first external peer implementation, and the frames carry the entry as opaque
bytes until that co-design lands.
