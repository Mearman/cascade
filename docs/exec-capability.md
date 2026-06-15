# Exec capability — terminals and processes

> Status: **design, not implemented.** This note proposes a second capability
> domain for Cascade — brokered remote compute — alongside the existing storage
> (file) domain. It reuses Cascade's identity, capability-token, transport, and
> audit spine; the new surface is a node-side exec subsystem and a set of
> grant-gated verbs.

## What and why

Cascade already crossed the line from pure file sync to remote node
administration at v10: the management plane can restart the daemon, push config,
and add backends on another node over the authenticated peer connection. The
exec capability extends that remote-administration surface to **spawning and
controlling terminals and processes**: an authorised peer can open a PTY or run
a process on a node and stream its output back, gated by the same token model and
written to the same audit log.

This makes a Cascade node independently more useful — a grant-gated remote shell
across your own devices is valuable on its own — and gives any peer
implementation (see [`node-protocol.md`](node-protocol.md)) a uniform,
mesh-native way to broker compute without inventing a parallel auth model.

## Placement

A new workspace crate, `crates/exec`, owns the node-side compute:

- **PTYs** via [`portable-pty`](https://crates.io/crates/portable-pty) (the
  wezterm crate) — a native PTY without a Node/node-gyp toolchain, fitting the
  all-Rust workspace.
- **Processes** via `tokio::process` for headless child processes (no TTY).

The engine drives it through the management dispatch; `crates/exec` never reaches
the network or the grant store directly — it is a capability provider the
authorised management path calls into, mirroring how backends are self-contained
behind the `Backend` trait.

## Control plane — new management verbs

Extend the `ManageRequest` / `ManageResponse` command set and the grant verb
vocabulary:

- `pty.spawn` (shell, cwd, env, size) → session id
- `pty.write` (session, bytes)
- `pty.resize` (session, cols, rows)
- `pty.kill` (session, signal)
- `proc.spawn` (argv, cwd, env) → session id
- `proc.signal` (session, signal)
- `proc.kill` (session)

These dispatch into `crates/exec` through the existing per-command authorisation
and append-only audit, exactly as the storage and lifecycle verbs do.

## Data plane — streams over the transport

Control travels as management frames; **live `stdin` / `stdout` / `stderr`
travel as stream channels over the authenticated peer connection (and the relay
for WAN)** — never through the content-addressed block store. A running stream is
ephemeral and mutable; the block store is for immutable, addressable content, and
forcing live output through it would be a category error. Streams carry
backpressure so a slow consumer throttles the producer rather than unbounded
buffering on the node.

## Authorisation — exec is a dangerous verb

This is the part that must be rigorous, because it changes Cascade's blast radius
from "wrong file synced" to **remote code execution gated by a grant**. Exec
verbs join the dangerous tier alongside backend, lifecycle, and grant
administration, with the same discipline Cascade already applies there:

- **Never satisfied by a node-wide grant.** Exec authority is granted to an
  explicit scope, deliberately, never blanket.
- **Bounded delegation.** A delegated exec token can only narrow authority and
  its expiry is clamped to its parent's, like every other token.
- **Audited.** Every spawn, signal, and kill is written to the append-only audit
  log with the bearer, scope, and command.
- **Revocable at the next check.** A revoked token fails the verifier on the next
  command, cutting access promptly.

The exec authorisation path should be treated as the most rigorously designed and
tested code in this feature — a flaw there is the difference between a capability
and an exploit.

## Representation and discovery

Running sessions are tracked in node state (a new `exec_sessions` table, or an
extension of the management state), so a node can enumerate its live sessions for
an authorised peer. The node advertises the `exec` capability in the protocol
handshake ([`node-protocol.md`](node-protocol.md)) so peers know it can broker
compute; a node that omits it simply offers files and management as before.

A later, optional refinement: surface live sessions through a synthetic
`/proc`-style view in the existing presenters, so a terminal or process appears
as a node in the VFS. That is sugar over the mechanism above, not a prerequisite.

## Status

Design only. Nothing here is built. The control verbs and stream framing are the
`exec` half of the draft node protocol and are co-designed with the first peer
implementation before either side writes code.
