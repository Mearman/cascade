# NAT integration tests — developer guide

The `nat-integration` feature in `crates/p2p` contains end-to-end tests that
simulate two peers behind separate NAT gateways, mediated by a relay server in
a shared "internet" namespace. The tests use Linux network namespaces and veth
pairs, so they only run on Linux and require `CAP_NET_ADMIN` (root).

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
# Build and run — cargo must be able to write to the target directory.
sudo -E env "PATH=$PATH" "HOME=$HOME" \
  cargo test -p cascade-p2p \
    --features nat-integration \
    --test nat_integration \
    -- --test-threads=1 --nocapture
```

`--test-threads=1` is mandatory. The harness manipulates global kernel state
(network namespaces, routing tables, iptables rules) and concurrent test
instances would collide on the fixed namespace names.

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
for ns in cascade-internet cascade-peer-a cascade-peer-b; do
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
builds the test binary without sudo, then invokes `cargo test` under
`sudo -E` to grant `CAP_NET_ADMIN`. The job uses `--test-threads=1` for the
same reason as local runs.
