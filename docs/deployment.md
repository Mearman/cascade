# Cascade deployment guide

This document covers deploying the two pieces of public infrastructure that enable Cascade peers behind NATs to find and reach each other across the internet: the **announce Worker** and the **relay server**.

Neither component stores file content or metadata. They are coordination infrastructure only. An operator who does not need WAN connectivity between devices can skip both and rely on LAN discovery and gossip alone.

## What runs where

| Component | Suggested host | Cost | Persistent state |
|-----------|---------------|------|-----------------|
| Announce Worker | Cloudflare Workers (free tier) | Zero | Soft state in Workers KV; expires automatically |
| Relay server | Any always-on host (VPS, home server, cloud VM) | Bandwidth only | None — the relay is stateless |

The two components are independent. You can run either or both:

- **Announce only** — peers can find each other's current addresses without the relay, then connect directly or via NAT hole-punching.
- **Relay only** — peers can connect through the relay without the announce directory, as long as they already know each other's relay session IDs through some other means (manual config, gossip).
- **Both** — the typical setup for a WAN mesh. Peers announce their candidates to the Worker; when they want to reach an unknown peer they look up its candidates, try direct connection and hole-punching, and fall back to the relay if both fail.

## Security posture

**The relay is a blind byte-pipe.** After the HMAC handshake it forwards already-encrypted bytes between peers and never inspects the payload. End-to-end TLS is established by the peers; the relay operator sees only ciphertext.

**The announce directory is a blind carrier of self-certifying blobs.** The Worker stores and serves signed candidate sets verbatim. It never validates the signature — the *looking-up peer* does that on read. The Worker gates *writers* with an HMAC write-auth tag so only peers with the shared secret may register candidates. The stored blob being self-certifying means a compromised Worker cannot forge reachability — it can only serve stale blobs, which the verifying peer's signature check will reject.

**The shared secret** authenticates the HMAC handshake at the relay (byte-pipe admission) and signs announce writes (write auth). One secret can serve both roles; they are the same 32-byte key. A compromised secret lets an attacker open relay sessions and post fake candidate sets, but not intercept existing sessions (the relay never sees the plaintext) and not forge valid signed candidate envelopes (those are signed with the device's TLS key, not the shared secret).

## Announce Worker

The announce Worker hosts the peer-discovery directory. Devices publish their current reachable candidates here; other devices look them up by device ID.

Full deploy steps are in [`workers/announce/DEPLOY.md`](../workers/announce/DEPLOY.md). The summary:

1. `wrangler kv:namespace create ANNOUNCE` — create the KV namespace; paste the returned ID into `wrangler.toml`.
2. `wrangler secret put ANNOUNCE_SHARED_SECRET` — set the HMAC write key.
3. `wrangler deploy` — build the wasm and deploy.

The Worker URL is `https://cascade-announce.<your-account>.workers.dev`.

## Relay server

The relay server is a long-running process that pairs peers by session ID and shuttles bytes between them.

Full setup steps are in [`crates/relay-server/README.md`](../crates/relay-server/README.md). The summary:

### Install

```sh
cargo install --git https://github.com/Mearman/cascade --bin cascade-relay
```

Or pull the Docker image:

```sh
docker pull ghcr.io/mearman/cascade-relay:latest
```

### Run (systemd — recommended for an always-on host)

```sh
# Create a dedicated unprivileged user.
useradd --system --no-create-home --shell /usr/sbin/nologin cascade-relay

# Install the unit file, launcher script, and the environment file template.
cp crates/relay-server/cascade-relay.service /etc/systemd/system/
install -m 0755 crates/relay-server/cascade-relay-start.sh /usr/local/bin/
cp crates/relay-server/relay.env.example /etc/cascade/relay.env
chmod 600 /etc/cascade/relay.env
chown cascade-relay: /etc/cascade/relay.env

# Edit /etc/cascade/relay.env and fill in CASCADE_RELAY_SHARED_SECRET.
# Then start the service.
systemctl daemon-reload
systemctl enable --now cascade-relay
```

The unit file is at [`crates/relay-server/cascade-relay.service`](../crates/relay-server/cascade-relay.service); the env file template is at [`crates/relay-server/relay.env.example`](../crates/relay-server/relay.env.example).

### Run (Docker)

```sh
docker run -d \
  --name cascade-relay \
  --restart unless-stopped \
  -p 9999:9999 \
  -p 9998:9998 \
  -e CASCADE_RELAY_SHARED_SECRET="$(cat /etc/cascade/relay.secret)" \
  -e CASCADE_RELAY_LISTEN="0.0.0.0:9999" \
  -e CASCADE_RELAY_METRICS="0.0.0.0:9998" \
  ghcr.io/mearman/cascade-relay:latest
```

### Ports

| Port | Purpose |
|------|---------|
| `9999/tcp` | Byte-pipe listener — peers connect here. Open in firewall. |
| `9998/tcp` | `/metrics` (Prometheus) and `/health` (liveness probe). Restrict to monitoring network. |
| `9997/tcp` | Announce directory (optional, requires `--features announce` build). |

### Health check

When `CASCADE_RELAY_METRICS` is set, `GET /health` returns `200 OK` with body `ok`. Use this for container orchestrator probes and load-balancer health checks. It is unauthenticated and carries no sensitive information.

```sh
curl http://relay.example.com:9998/health
```

## Pointing cascade at the infrastructure

Set the exposure posture to `public` and configure the announce server and relay endpoints in each device's P2P backend config. The posture must be `public` for announce-server discovery and relay to activate; the default is `private` (LAN only).

```toml
# ~/.config/cascade/backends/my-p2p-folder.toml

# public: LAN discovery, gossip, hole-punching, announce servers, relay, and DHT.
# private: LAN discovery, gossip, hole-punching, and relay — no global publication.
# lan-only: only UDP multicast LAN discovery.
exposure = "public"

# Announce server.  Do NOT bake in a default hostname — operator-supplied only.
# The key is `url`, not `base_url`.
[[announce_servers]]
url = "https://cascade-announce.<your-account>.workers.dev"
shared_secret = "your-64-char-hex-secret"   # same secret as ANNOUNCE_SHARED_SECRET

# Relay.  relay_shared_secret must match CASCADE_RELAY_SHARED_SECRET.
# These are flat top-level keys — there is no [relay] sub-section.
# relay_endpoints takes IP:port strings (SocketAddr); resolve the hostname
# to its IP first if needed.
relay_endpoints = ["203.0.113.10:9999"]
relay_shared_secret = "your-64-char-hex-secret"   # same secret as CASCADE_RELAY_SHARED_SECRET
```

Both the announce server and the relay use the same HMAC key length (32 bytes / 64 hex chars). You may use the same secret for both if they are operated together by the same person; use separate secrets if the announce Worker and the relay are operated by different parties.

## Generating secrets

```sh
openssl rand -hex 32
```

Keep secrets out of shell history. Prefer reading from a file or a secrets manager rather than passing on the command line.

## What does NOT need deploying

- **LAN discovery** — UDP multicast on the local network; runs without any server.
- **Gossip** — peers exchange peer-book snapshots over established connections; runs without any server.
- **NAT hole-punching** — coordinated by the two peers themselves over existing connections; no server needed beyond the relay for the initial rendezvous.
- **Mainline DHT** — uses the public BitTorrent DHT bootstrap nodes; no operator deployment needed, just set `exposure = "public"`.

---

## Running the daemon on a NAS or UnRAID server

The `ghcr.io/mearman/cascade` image packages the Cascade daemon for container
deployment. It is a separate image from `ghcr.io/mearman/cascade-relay` and
built for both `linux/amd64` and `linux/arm64`. Full operator documentation —
environment variables, volume layout, the two operating modes, backend-specific
setup — is in [`docs/docker.md`](docker.md). This section covers the
relationship between the daemon image and the relay.

### Pairing a NAS node with the relay

A NAS behind NAT that wants WAN reachability needs the relay for the fallback
path. The daemon and relay share a single 64-char hex HMAC secret (`openssl
rand -hex 32`). On the relay side that secret is `CASCADE_RELAY_SHARED_SECRET`;
on the daemon side it is `CASCADE_P2P_RELAY_SECRET`.

The same relay that serves laptop-to-laptop NAT traversal can serve the NAS.
No separate relay instance is needed.

```sh
# On the relay host (if not already running):
docker run -d \
  --name cascade-relay \
  --restart unless-stopped \
  -p 9999:9999 \
  -p 9998:9998 \
  -e CASCADE_RELAY_SHARED_SECRET="<shared-secret>" \
  -e CASCADE_RELAY_LISTEN="0.0.0.0:9999" \
  -e CASCADE_RELAY_METRICS="0.0.0.0:9998" \
  ghcr.io/mearman/cascade-relay:latest

# On the NAS (headless seed mode, P2P backend, public posture):
docker run -d \
  --name cascade \
  --restart unless-stopped \
  -v /mnt/user/appdata/cascade:/config \
  -v /mnt/user/cascade-data:/data \
  -e CASCADE_BACKEND_TYPE=p2p \
  -e CASCADE_BACKEND_NAME=my-mesh \
  -e CASCADE_P2P_POSTURE=public \
  -e CASCADE_P2P_LISTEN=0.0.0.0:22000 \
  -e CASCADE_P2P_RELAY_ENDPOINT=relay.example.com:9999 \
  -e CASCADE_P2P_RELAY_SECRET="<shared-secret>" \
  -p 22000:22000 \
  ghcr.io/mearman/cascade:latest
```

With this setup the NAS node registers its current candidates with the
announce directory, accepts BEP connections from peers that reach it directly,
and falls back through the relay when hole-punching fails. Other devices in the
mesh configure the same relay endpoint and secret in their own P2P backend
TOML (see [Pointing cascade at the infrastructure](#pointing-cascade-at-the-infrastructure)
above).

For a single Compose file that runs both services together on one Docker
network, see [`deploy/docker-compose.full.yml`](../deploy/docker-compose.full.yml).
That template also demonstrates the seed-vs-mount choice (commented inline) so
the operator can pick the mode that suits the NAS without editing anything
beyond environment variables.

### Seed vs mount on a NAS

The default seed mode is the right choice for most NAS deployments: no
elevated privileges, no kernel extension, no `/dev/fuse` on the host. The
daemon keeps cloud folders and P2P block stores alive, and other hosts on the
LAN browse the tree over WebDAV. If a directly mounted filesystem is needed
(for example, to feed a media server that requires a local path), set
`CASCADE_MOUNT=1` and follow the privilege requirements documented in
[`docs/docker.md`](docker.md#browsable-mount-mode-cascade_mount1).

### Cloud-backed node with P2P block sharing

A NAS running a Google Drive or S3 backend can also participate in P2P block
exchange by setting `CASCADE_P2P=1`. This enables the engine's optimisation
layer — nearby peers that hold copies of the same files exchange blocks
directly instead of each fetching from the cloud. The relay path is used when
those peers are behind different NATs.

`CASCADE_P2P_POSTURE` and `CASCADE_P2P_RELAY_*` drive both a `p2p`-type backend
and the cloud-backed optimisation layer: set `CASCADE_P2P=1` with the posture
and relay variables and the daemon threads them into the engine's P2P layer, so
no hand-edited backend TOML is needed. The relay endpoint accepts a DNS hostname
or a Docker service name (such as `relay:9999` on a shared compose network),
resolved when the daemon starts.
