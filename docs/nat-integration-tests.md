# NAT integration tests — developer guide

The `nat-integration` feature in `crates/p2p` contains end-to-end tests that
exercise the connectivity ladder (`docs/design.md` §"NAT traversal") across
two peers behind separate NAT gateways, with a third node in a shared
"internet" namespace. The tests use Linux network namespaces and veth pairs,
so they only run on Linux and require `CAP_NET_ADMIN` (root).

Two test binaries live behind the feature:

- `nat_integration` — the operated-relay rung. A relay server
  (`crates/relay-server/`) bridges two NAT'd peers; a peer sends a frame and
  asserts the echo.
- `serverless_rungs` — the two **serverless** rungs, proven with no operated
  servers of any kind (no STUN endpoint, no announce/rendezvous server, no
  operated relay):
  - `peer_as_stun_then_hole_punch` (rungs 3 + 4) — two cone-NAT peers learn
    their own reflexive mappings from a third *participating* peer that echoes
    the observed source back (`BepMessage::ObservedAddress`, the peer-as-STUN
    mechanism), then hole-punch and transfer a block over the punched UDP flow.
  - `symmetric_pair_via_peer_relay` (rung 5) — a symmetric-NAT pair cannot
    punch, so they bridge through a third, open peer acting as a peer relay
    (`BepMessage::RelayConnect` / `RelayRoute::Peer`) and transfer a block
    end-to-end through it. The relay is a participating cascade peer, not an
    operated relay server.

## Prerequisites

- Linux kernel 3.8+ with user namespace support (standard on any modern
  distribution; Ubuntu 20.04+ and Fedora 34+ are well-tested).
- `iproute2` (`ip netns`, `ip link`, `ip addr`, `ip route`, `ip rule`).
- `iptables` for the MASQUERADE NAT rules.
- A Rust toolchain (the workspace `rust-toolchain.toml` pins the version;
  `rustup` installs it automatically).

On Debian/Ubuntu:

```bash
sudo apt-get install iproute2 iptables
```

## Running locally

```bash
# Operated-relay rung. cargo must be able to write to the target directory.
sudo -E env "PATH=$PATH" "HOME=$HOME" \
  cargo test -p cascade-p2p \
    --features nat-integration \
    --test nat_integration \
    -- --test-threads=1 --nocapture

# Serverless rungs (peer-as-STUN + hole punch, and peer relay).
sudo -E env "PATH=$PATH" "HOME=$HOME" \
  cargo test -p cascade-p2p \
    --features nat-integration \
    --test serverless_rungs \
    -- --test-threads=1 --nocapture
```

`--test-threads=1` is mandatory. The harness manipulates global kernel state
(network namespaces, routing tables, iptables rules) and concurrent test
instances would collide on the fixed namespace names. The two binaries use
distinct namespace names (`cascade-*` vs `cascade-sl-*`) so they may run
back-to-back, but never run them in parallel.

`--nocapture` is optional but useful — it lets the relay subprocess's log
output surface in the terminal so you can see where the relay is bound.

## Network topology

The harness creates three network namespaces:

```
 cascade-peer-a (10.0.1.2/24)
      |
 veth-a-ext (10.0.1.1/24) ── cascade-internet ── veth-b-ext (10.0.2.1/24)
                                    |
                             cascade-peer-b (10.0.2.2/24)
```

Each peer namespace has a default route via its local gateway (`.1`) and a
MASQUERADE NAT rule on its egress veth. The internet namespace has IP
forwarding enabled and sees both peers only at their gateway addresses.

The relay server binds inside `cascade-internet` on an ephemeral port. Both
peer subprocesses connect to the relay at `10.0.1.1:<port>`.

## How the harness works

The test binary re-invokes itself as a subprocess inside each namespace using
`ip netns exec`. The `CASCADE_NETNS_ROLE` environment variable selects the
subprocess role:

| Role | Behaviour |
|------|-----------|
| `relay` | Spawns the relay server, prints the bound address to stdout, then blocks until stdin closes. |
| `peer-a` | Connects to the relay, sends a fixed test block, asserts the echo. |
| `peer-b` | Connects to the relay, echoes the first frame it receives. |

The harness reads the relay's bound port from its stdout, then spawns `peer-b`
and `peer-a` in that order with a 200 ms stagger. A `NetNsHarness` guard
struct handles teardown via `Drop`, so cleanup runs even when the test panics.

## Teardown and cleanup

If a test run is interrupted (e.g. `Ctrl-C`), the namespaces may be left
behind. Clean them up manually:

```bash
for ns in cascade-internet cascade-peer-a cascade-peer-b \
          cascade-sl-internet cascade-sl-peer-a cascade-sl-peer-b; do
  sudo ip netns delete "$ns" 2>/dev/null || true
done
```

## Skipping

The test skips itself gracefully (prints a message and returns) when:

- The process is not running as root (UID 0).
- The `ip` or `iptables` commands are not on PATH.

These conditions are checked at the start of `NetNsHarness::setup`. No
explicit `SKIP` mechanism is needed — the test returns without asserting
anything and `cargo test` reports it as passed.

## Serverless-rung topology (`serverless_rungs`)

This binary uses the same three-namespace shape but renames the namespaces
(`cascade-sl-internet`, `cascade-sl-peer-a`, `cascade-sl-peer-b`) on the
`10.10.x.x` range, and the third node is a participating peer rather than an
operated server.

The egress NAT mode differs per scenario:

| Scenario | Egress NAT | Decision | Third node |
|----------|-----------|----------|------------|
| `peer_as_stun_then_hole_punch` | MASQUERADE (cone) | `HolePunch` | peer-as-STUN observer + candidate rendezvous |
| `symmetric_pair_via_peer_relay` | MASQUERADE `--random` (symmetric) | `Relay { RelayRoute::Peer }` | open peer relay |

`--random` forces a fresh source-port allocation per destination, turning the
endpoint-independent default into an endpoint-dependent (symmetric) mapping the
partner cannot predict — so the punch is impossible and the peer-relay rung is
the only path.

The binary re-invokes itself as a subprocess via `CASCADE_SERVERLESS_ROLE`
(`stun-hub`, `punch-a`, `punch-b`, `relay-hub`, `relay-a`, `relay-b`). The
orchestrator generates one shared device identity, saves it to a temp
directory passed via `CASCADE_SERVERLESS_IDENTITY_DIR`, and every participant
loads it so the TLS device-id pinning is symmetric — the test exercises the
connectivity rung, not the trust model. The hub prints its bound ports on a
`HUBPORTS …` stdout line; the orchestrator reads that line, then spawns the two
NAT'd peers. Each scenario asserts a BEP block `Request`/`Response` completes
over the expected rung.

No STUN, announce, or relay server runs in either serverless scenario.

## Adding new scenarios

1. Add a new `CASCADE_NETNS_ROLE` variant in `run_as_subprocess()`.
2. Implement the corresponding role function.
3. Add a new `#[test]` that calls `maybe_run_as_subprocess()` at the top,
   sets up the harness, spawns subprocesses, and asserts the outcome.
4. Run `cargo test -p cascade-p2p --features nat-integration --test nat_integration -- --test-threads=1`
   to verify.

## CI

The `nat-integration` job in `.github/workflows/ci.yml` runs after the `check`
and `test` jobs on `ubuntu-latest`. It installs `iproute2` and `iptables`,
builds both test binaries without sudo, then invokes `cargo test` under
`sudo -E` to grant `CAP_NET_ADMIN` — once for `nat_integration` and once for
`serverless_rungs`. The job uses `--test-threads=1` for the same reason as
local runs.
