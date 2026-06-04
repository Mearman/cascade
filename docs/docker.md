# Cascade daemon Docker image

The `ghcr.io/mearman/cascade` image runs the Cascade daemon on any Linux host
— a NAS, an UnRAID server, a VPS, or a plain Docker host. It is built for both
`linux/amd64` and `linux/arm64` and published as a multi-arch manifest on every
release alongside the relay image.

This guide covers everything an operator needs to run the daemon in a container:
volume layout, environment variables, the two operating modes, backend-specific
setup, and how to pair the daemon with a `cascade-relay` instance for WAN NAT
traversal.

## Two modes: seed and mount

The daemon can run in two ways inside a container. The right choice depends on
whether you want the filesystem to appear as a browsable mount point on the
host.

### Seed mode (default, unprivileged)

When `CASCADE_MOUNT` is unset or `0`, the entrypoint runs:

```
cascade --config /config start --no-mount
```

and forces `CASCADE_PRESENTER=webdav` so the daemon never touches `/dev/fuse`.
The daemon brings up its configured backends, the state database, sync, and
(if enabled) the P2P block layer. It then binds an in-process WebDAV server at
the address given by `CASCADE_WEBDAV_BIND`. Nothing is mounted inside the
container; other hosts on the network can browse the tree over WebDAV, and the
node seeds blocks to P2P peers regardless of whether anyone is reading over
WebDAV.

This mode requires no extra capabilities and no special devices. It is the
right default for an UnRAID NAS that simply needs to keep a cloud folder or a
P2P node alive without a desktop client.

### Browsable mount mode (`CASCADE_MOUNT=1`)

When `CASCADE_MOUNT=1`, the entrypoint runs:

```
cascade --config /config start
```

without overriding the presenter. On Linux the daemon attempts a FUSE mount at
`CASCADE_MOUNT_POINT` (defaulting to `/mnt/cascade`). To make that mount
visible outside the container's mount namespace — on the host and in sibling
containers — the operator must:

1. Add `--cap-add SYS_ADMIN` (libfuse3's `fusermount3` requires it inside a
   container; without it the mount syscall is blocked by the container
   runtime's seccomp filter).
2. Expose `/dev/fuse` with `--device /dev/fuse` (the kernel FUSE device must
   be present in the container's `/dev`).
3. Bind-mount `CASCADE_MOUNT_POINT` with `rshared` propagation so the mount
   the daemon creates in its namespace propagates back to the host. With
   `docker run` that means `:rshared` at the end of the `-v` flag; with Docker
   Compose use:

   ```yaml
   volumes:
     - type: bind
       source: /mnt/cascade
       target: /mnt/cascade
       bind:
         propagation: rshared
   ```

The runtime image ships `libfuse3-3` and `fusermount3` for this path.
`ca-certificates` is present in both modes for Google Drive and S3 TLS.

On Linux `--no-mount` suppresses the FUSE mount and the daemon serves the tree
over its in-process WebDAV server instead, so seed mode is a genuine
unprivileged path. The entrypoint also sets `CASCADE_PRESENTER=webdav` in seed
mode as a belt-and-braces guard so `/dev/fuse` is never touched without
`CASCADE_MOUNT=1`.

## Volume layout

Two volumes are declared in the image:

| Volume | Purpose |
|--------|---------|
| `/config` | Configuration and small persistent state: `config.toml`, per-backend `<name>.toml` files, `state.db`, `cascade.pid`, and Google Drive token files under `gdrive-tokens/`. Must survive restarts. |
| `/data` | Bulky runtime state: P2P block store and index, local-backend roots, cache. Keep this on a large, fast volume — on UnRAID that is typically an array share or a cache pool disk. |

Separating the two volumes means you can put `/config` on a small, reliable
volume (an SSD, a ZFS pool) and `/data` on whatever large storage the NAS has
available, without the block store polluting the config directory.

## Environment variables

All configuration is injected through environment variables. The entrypoint
renders `config.toml` and the per-backend TOML files from these variables on
first boot; subsequent starts are idempotent (existing files are not
overwritten).

Secrets (S3 keys, Google OAuth secret, relay/announce HMAC secrets) are read
from the environment and written only to `0600`-mode config files. They are
never placed on the command line and will not appear in `ps` output or
`/proc/<pid>/cmdline`.

### Core variables

| Variable | Default | Description |
|----------|---------|-------------|
| `CASCADE_CONFIG_DIR` | `/config` | Config directory passed to `cascade --config`. Holds `config.toml`, per-backend files, `state.db`, `cascade.pid`, and gdrive tokens. Must be a writable bind-mount or named volume. |
| `CASCADE_DATA_DIR` | `/data` | Root for bulky runtime state: P2P block store, local-backend roots, cache. |
| `CASCADE_BACKEND_TYPE` | — **(required)** | Backend type to provision on first boot: `gdrive`, `s3`, `local`, or `p2p`. The daemon exits cleanly when no backends are configured, so this must be set. |
| `CASCADE_BACKEND_NAME` | `$CASCADE_BACKEND_TYPE` | Logical name for the backend; becomes the `[backends.<name>]` table key and the per-backend `<name>.toml` filename. |
| `CASCADE_MOUNT` | `0` | Set to `1` to enable the in-container FUSE mount. See [Browsable mount mode](#browsable-mount-mode-cascade_mount1) above for the required privilege flags. |
| `CASCADE_MOUNT_POINT` | `/mnt/cascade` | Path written to `[mount].point` in `config.toml`. In seed mode the path is recorded but nothing is mounted. |
| `CASCADE_PRESENTER` | *(set by entrypoint)* | Existing daemon override read by `mount.rs`. The entrypoint sets this to `webdav` in seed mode so no FUSE attempt is made. Leave it alone unless you have a specific reason to override. |
| `CASCADE_WEBDAV_BIND` | `127.0.0.1:0` | Bind address for the in-process WebDAV server in seed mode. Set to `0.0.0.0:<port>` and publish the port to expose the tree over WebDAV to other hosts on the network. |
| `RUST_LOG` | `info` | Tracing log filter. Set to `debug` during initial bring-up. |

### P2P optimisation layer

These variables configure the engine's P2P block-sharing layer, which sits in
front of cloud backends and accelerates fetches between peers that hold the
same files. This is independent of a pure-P2P backend.

| Variable | Default | Description |
|----------|---------|-------------|
| `CASCADE_P2P` | `0` | Set to `1` to enable the P2P optimisation layer (`[p2p].enabled` in `config.toml`). |
| `CASCADE_P2P_POSTURE` | `private` | Discovery reach: `lan-only`, `private`, or `public`. `public` activates DHT, announce servers, and relay. |
| `CASCADE_P2P_LISTEN` | `0.0.0.0:0` | BEP listener bind address. Set a fixed port and publish it when the operator port-forwards at the router. |
| `CASCADE_P2P_RELAY_ENDPOINT` | *(unset)* | `host:port` of a `cascade-relay` server for WAN NAT traversal. Required when posture is `public` and the node is behind NAT. |
| `CASCADE_P2P_RELAY_SECRET` | *(unset)* | 64-char hex HMAC secret matching the relay's `CASCADE_RELAY_SHARED_SECRET`. Required alongside `CASCADE_P2P_RELAY_ENDPOINT`. |
| `CASCADE_P2P_ANNOUNCE_URL` | *(unset)* | Base URL of the announce Worker (e.g. `https://cascade-announce.example.workers.dev`). |
| `CASCADE_P2P_ANNOUNCE_SECRET` | *(unset)* | 64-char hex HMAC write key for the announce server. Required when `CASCADE_P2P_ANNOUNCE_URL` is set. |

`CASCADE_P2P_POSTURE` and `CASCADE_P2P_RELAY_*` apply both to a `p2p`-type
backend and to the engine's optimisation-layer P2P (`[p2p].enabled`): for a
cloud-backed node, set `CASCADE_P2P=1` together with the posture and relay
variables and the entrypoint renders them into the `[p2p]` section, which the
daemon threads into the engine. The relay endpoint accepts a DNS hostname or a
Docker service name (resolved at startup), not only a literal IP.

### Google Drive backend

| Variable | Default | Description |
|----------|---------|-------------|
| `CASCADE_GDRIVE_CLIENT_ID` | *(unset)* | Google OAuth2 Desktop application client ID. Required when `CASCADE_BACKEND_TYPE=gdrive`. |
| `CASCADE_GDRIVE_CLIENT_SECRET` | *(unset)* | Google OAuth2 client secret. Required alongside `CASCADE_GDRIVE_CLIENT_ID`. |
| `CASCADE_GDRIVE_ACCOUNT` | `$CASCADE_BACKEND_NAME` | Account identifier used as the token filename (`gdrive-tokens/<account>.json`). |

### S3-compatible backend

| Variable | Default | Description |
|----------|---------|-------------|
| `CASCADE_S3_ENDPOINT` | *(unset)* | S3 endpoint URL. Required when `CASCADE_BACKEND_TYPE=s3`. |
| `CASCADE_S3_BUCKET` | *(unset)* | Bucket name. Required when `CASCADE_BACKEND_TYPE=s3`. |
| `CASCADE_S3_REGION` | `us-east-1` | AWS region or equivalent. |
| `CASCADE_S3_ACCESS_KEY_ID` | *(unset)* | Access key ID. Required when `CASCADE_BACKEND_TYPE=s3`. |
| `CASCADE_S3_SECRET_ACCESS_KEY` | *(unset)* | Secret access key. Required when `CASCADE_BACKEND_TYPE=s3`. |

### Pure-P2P backend

| Variable | Default | Description |
|----------|---------|-------------|
| `CASCADE_P2P_POSTURE` | `private` | Discovery reach for the backend (`exposure` key in `<name>.toml`): `lan-only`, `private`, or `public`. |
| `CASCADE_P2P_LISTEN` | `0.0.0.0:0` | BEP listener bind address for the backend. |
| `CASCADE_P2P_RELAY_ENDPOINT` | *(unset)* | Relay `host:port` written to `relay_endpoints` in the backend TOML. |
| `CASCADE_P2P_RELAY_SECRET` | *(unset)* | Relay HMAC secret written to `relay_shared_secret` in the backend TOML. |
| `CASCADE_P2P_ANNOUNCE_URL` | *(unset)* | Announce server URL for the backend. |
| `CASCADE_P2P_ANNOUNCE_SECRET` | *(unset)* | Announce server write key for the backend. |

### Local backend

| Variable | Default | Description |
|----------|---------|-------------|
| `CASCADE_LOCAL_ROOT` | *(unset)* | Filesystem root for a local (adopt-and-sync) backend. Required when `CASCADE_BACKEND_TYPE=local`. Should be a path under `CASCADE_DATA_DIR`. |

## Quick-start examples

### Seed mode with an S3 bucket

```sh
docker run -d \
  --name cascade \
  --restart unless-stopped \
  -v /mnt/user/appdata/cascade:/config \
  -v /mnt/user/cascade-data:/data \
  -p 8080:8080 \
  -e CASCADE_BACKEND_TYPE=s3 \
  -e CASCADE_BACKEND_NAME=my-bucket \
  -e CASCADE_S3_ENDPOINT=https://s3.amazonaws.com \
  -e CASCADE_S3_BUCKET=my-cascade-bucket \
  -e CASCADE_S3_REGION=eu-west-1 \
  -e CASCADE_S3_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE \
  -e CASCADE_S3_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY \
  -e CASCADE_WEBDAV_BIND=0.0.0.0:8080 \
  ghcr.io/mearman/cascade:latest
```

The tree is then browsable over WebDAV at `http://<host>:8080/`.

### Browsable FUSE mount (requires privileges)

```sh
mkdir -p /mnt/cascade

docker run -d \
  --name cascade \
  --restart unless-stopped \
  --cap-add SYS_ADMIN \
  --device /dev/fuse \
  -v /mnt/user/appdata/cascade:/config \
  -v /mnt/user/cascade-data:/data \
  -v /mnt/cascade:/mnt/cascade:rshared \
  -e CASCADE_MOUNT=1 \
  -e CASCADE_BACKEND_TYPE=s3 \
  -e CASCADE_S3_ENDPOINT=https://s3.amazonaws.com \
  -e CASCADE_S3_BUCKET=my-cascade-bucket \
  -e CASCADE_S3_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE \
  -e CASCADE_S3_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY \
  ghcr.io/mearman/cascade:latest
```

Files appear under `/mnt/cascade` on the host as the daemon fetches them on
demand.

## Google Drive: one-time device-code authentication

Google Drive authentication uses a device-code flow — a short, one-time human
step that cannot be automated. The sequence is:

1. Start the container at least once with `CASCADE_BACKEND_TYPE=gdrive` so the
   entrypoint writes the backend TOML with your client ID and secret.
2. Run the auth command inside the running container:

   ```sh
   docker exec -it cascade \
     cascade --config /config backend auth gdrive
   ```

   The daemon prints a URL and a short code. Open the URL in a browser, enter
   the code, and grant access. The command exits once the token is confirmed.

3. The token is written to `/config/gdrive-tokens/<account>.json`. Because
   `/config` is a persistent volume, the token survives container restarts and
   image upgrades. The daemon refreshes the token silently in the background.

Important: the daemon does **not** block at startup when the token is absent.
It only errors with `Not authenticated. Run cascade backend auth gdrive` on the
first content access. You can start the container, run the auth step, and then
reads will work immediately without restarting the daemon.

## Pairing with cascade-relay for WAN NAT traversal

If the NAS is behind NAT and you want devices on other networks to reach it
over P2P, pair it with a `cascade-relay` deployment. The relay is a blind
byte-pipe that never inspects content; full deployment instructions are in
[`docs/deployment.md`](deployment.md).

Generate a shared secret:

```sh
openssl rand -hex 32
```

Use the same secret as `CASCADE_RELAY_SHARED_SECRET` on the relay and as
`CASCADE_P2P_RELAY_SECRET` on the daemon.

```sh
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
  -e CASCADE_P2P_RELAY_SECRET=<64-char-hex-secret> \
  -e CASCADE_P2P_ANNOUNCE_URL=https://cascade-announce.example.workers.dev \
  -e CASCADE_P2P_ANNOUNCE_SECRET=<64-char-hex-secret> \
  -p 22000:22000 \
  ghcr.io/mearman/cascade:latest
```

Set `CASCADE_P2P_POSTURE=public` to activate DHT, announce-server discovery,
and relay fallback. Without a fixed `CASCADE_P2P_LISTEN` port and a
corresponding port-forward at the router, hole-punching is your only direct
path — the relay handles everything else.

See [`deploy/docker-compose.full.yml`](../deploy/docker-compose.full.yml) for a
Compose template that runs the daemon and relay together on one network.

## Ports

| Port | Purpose | Mode |
|------|---------|------|
| WebDAV port (see `CASCADE_WEBDAV_BIND`) | In-process WebDAV server for seed-mode browsing | Seed |
| `CASCADE_P2P_LISTEN` port | BEP listener for block exchange | Either |

No port is hard-coded in the image. Publish the ports you need with `-p` or in
the Compose `ports:` block.

## Health check

The image includes a `HEALTHCHECK` that probes the WebDAV root path `/` on the
bind address when `CASCADE_WEBDAV_BIND` is set (the WebDAV presenter has no
dedicated `/health` route). When it is unset the check
exits `0` immediately so the container does not cycle through unhealthy states
when seed-mode browsing is not needed. In a production deployment configure
`CASCADE_WEBDAV_BIND` so the orchestrator can distinguish a crashed process
from a healthy one.

## Generating secrets

```sh
openssl rand -hex 32
```

Keep secrets out of shell history. Use Docker secrets, environment files passed
with `--env-file`, or a secrets manager rather than passing them inline on the
command line.

## The cascade-relay image

`ghcr.io/mearman/cascade-relay` runs the relay server — a blind, stateless
byte-pipe that pairs two peers over WebSocket and forwards already-encrypted
frames between them. The relay never inspects payload; it only HMAC-authenticates
the session negotiation and then becomes transparent. It is built for
`linux/amd64` and `linux/arm64` alongside the daemon image and published on
every release.

You only need the relay when daemon nodes are behind NAT and need to reach each
other across the internet. For LAN-only meshes or a mesh where at least one peer
has a public IP, the relay is not required.

Full deployment instructions — including Cloudflare Worker alternatives for the
announce directory — are in [`docs/deployment.md`](deployment.md).

### Relay environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `CASCADE_RELAY_SHARED_SECRET` | — **(required)** | 64-char hex HMAC secret. Generate with `openssl rand -hex 32`. Must match `CASCADE_P2P_RELAY_SECRET` on every daemon that uses this relay. |
| `CASCADE_RELAY_LISTEN` | `0.0.0.0:9999` | Bind address for the peer listener. |
| `CASCADE_RELAY_METRICS` | *(unset)* | Bind address for the `/metrics` and `/health` HTTP endpoints. Set to `0.0.0.0:9998` to enable Prometheus scraping and the liveness probe. |
| `CASCADE_RELAY_ANNOUNCE` | *(unset)* | Bind address for an in-process peer-announce directory endpoint. Requires the `announce` build feature. Set to `0.0.0.0:9997` to activate. |

### Relay ports

| Port | Purpose |
|------|---------|
| `9999` | Peer listener (encrypted BEP byte-pipe). Must be reachable from the internet for WAN NAT traversal. |
| `9998` | `/metrics` (Prometheus) and `/health` HTTP endpoint. Enable with `CASCADE_RELAY_METRICS`. |
| `9997` | Announce directory endpoint. Enable with `CASCADE_RELAY_ANNOUNCE` (requires the announce build feature). |

### Relay health check

The image's `HEALTHCHECK` probes `/health` on the metrics port when
`CASCADE_RELAY_METRICS` is set and expects the response body to contain `ok`.
When the metrics endpoint is disabled the check exits `0` immediately.
In a production deployment set `CASCADE_RELAY_METRICS` so the orchestrator can
distinguish a crashed relay from a healthy one.

### Relay quick start

```sh
# Generate once and record the value — the daemon's CASCADE_P2P_RELAY_SECRET must match.
openssl rand -hex 32

docker run -d \
  --name cascade-relay \
  --restart unless-stopped \
  -e CASCADE_RELAY_SHARED_SECRET=<64-char-hex-secret> \
  -e CASCADE_RELAY_METRICS=0.0.0.0:9998 \
  -p 9999:9999 \
  -p 9998:9998 \
  ghcr.io/mearman/cascade-relay:latest
```

Expose port `9999` in the host firewall and, if the relay host is behind NAT,
port-forward `9999` from the router. The daemon's `CASCADE_P2P_RELAY_ENDPOINT`
should point at the relay's public IP or hostname on port `9999`.

## Compose templates

Three ready-to-use Docker Compose templates live in `deploy/`:

| File | What it runs | When to use |
|------|-------------|-------------|
| [`deploy/docker-compose.seed.yml`](../deploy/docker-compose.seed.yml) | Daemon in headless seed mode | Unprivileged NAS deployment; tree browsable over WebDAV at port 8080. No `SYS_ADMIN`, no `/dev/fuse`. |
| [`deploy/docker-compose.mount.yml`](../deploy/docker-compose.mount.yml) | Daemon with in-container FUSE mount | Host filesystem access at `/mnt/cascade`. Requires `SYS_ADMIN`, `/dev/fuse`, and `rshared` bind propagation. |
| [`deploy/docker-compose.full.yml`](../deploy/docker-compose.full.yml) | Daemon + relay on one Docker network | Daemon in seed mode paired with a relay on the same compose network for WAN NAT traversal. |

Each template has detailed inline comments. Copy or symlink the closest one to
your setup, supply credentials via a `.env` file, and run:

```sh
docker compose -f deploy/docker-compose.seed.yml up -d
```

For the full stack template, generate a shared secret before starting:

```sh
openssl rand -hex 32
# Set CASCADE_RELAY_SHARED_SECRET in the relay service and CASCADE_P2P_RELAY_SECRET
# in the cascade service to the same value in docker-compose.full.yml (or a .env file).
docker compose -f deploy/docker-compose.full.yml up -d
```
