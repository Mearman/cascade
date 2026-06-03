# cascade-relay

Opaque byte-pipe relay server for Cascade peers behind NATs.

Peers connect over WebSocket, authenticate with a shared HMAC-SHA256 secret, and are paired by session ID. The relay shuttles already-encrypted bytes between them without inspecting the payload. End-to-end TLS is the peers' responsibility; the relay sees only ciphertext after the initial handshake.

Full deployment instructions are in [`docs/deployment.md`](../../docs/deployment.md) at the repository root.

## Quick start

### Generate a shared secret

```sh
openssl rand -hex 32
```

This produces a 64-character hexadecimal string (32 bytes). Both the relay and every peer that connects through it must hold the same secret. Keep it out of shell history.

### Run directly

```sh
cascade-relay \
  --bind 0.0.0.0:9999 \
  --shared-secret "$(cat /etc/cascade/relay.secret)" \
  --metrics-bind 0.0.0.0:9998
```

Pass the secret via `--shared-secret` or the `CASCADE_RELAY_SHARED_SECRET` environment variable; the env-var form is preferred because it never appears in `/proc/<pid>/cmdline`.

### Run with Docker

```sh
docker run -d \
  --name cascade-relay \
  --restart unless-stopped \
  -p 9999:9999 \
  -p 9998:9998 \
  -e CASCADE_RELAY_SHARED_SECRET="$(cat /etc/cascade/relay.secret)" \
  -e CASCADE_RELAY_METRICS="0.0.0.0:9998" \
  ghcr.io/mearman/cascade-relay:latest
```

### Run with systemd

Copy `cascade-relay.service` to `/etc/systemd/system/`, copy `relay.env.example` to `/etc/cascade/relay.env`, fill in the secret, then:

```sh
systemctl daemon-reload
systemctl enable --now cascade-relay
```

Create a dedicated user before enabling:

```sh
useradd --system --no-create-home --shell /usr/sbin/nologin cascade-relay
```

## CLI flags

| Flag | Default | Description |
|------|---------|-------------|
| `--bind` | `0.0.0.0:9999` | Byte-pipe listener address. Open this port in your firewall. |
| `--shared-secret` | (required) | 64-char hex HMAC secret; prefer `CASCADE_RELAY_SHARED_SECRET`. |
| `--session-timeout-seconds` | `60` | How long the first peer waits for its partner before the session is reaped. |
| `--max-sessions` | `1024` | Maximum simultaneous sessions. New sessions are rejected past this limit. |
| `--metrics-bind` | (disabled) | Address for the `/metrics` (Prometheus) and `/health` (liveness) endpoint. |
| `--announce-bind` | (disabled) | Address for the `/announce/<device_id>` directory endpoint. Requires `--features announce`. |

## Endpoints

When `--metrics-bind` is set:

- `GET /health` — unauthenticated liveness probe; returns `200 OK` with body `ok`. Use this for container healthchecks and load-balancer probes.
- `GET /metrics` — Prometheus text exposition of the relay counters.

When `--announce-bind` is set (and the binary was built with `--features announce`):

- `POST /announce/<device_id>` — register a signed candidate set for a device. Requires the HMAC write-auth header.
- `GET /announce/<device_id>` — look up a device's signed candidate set.

## Environment variables

| Variable | CLI equivalent | Notes |
|----------|----------------|-------|
| `CASCADE_RELAY_SHARED_SECRET` | `--shared-secret` | Never pass the secret on the command line in production. |
| `CASCADE_RELAY_LISTEN` | `--bind` | Used by `docker-entrypoint.sh`; not read by `cascade-relay` directly. |
| `CASCADE_RELAY_METRICS` | `--metrics-bind` | Used by `docker-entrypoint.sh`. |
| `CASCADE_RELAY_ANNOUNCE` | `--announce-bind` | Used by `docker-entrypoint.sh`. |

## Security

The relay is a blind byte-pipe. It does not decrypt peer traffic, read filenames, or store anything on disk. The only authority it has is pairing sessions whose HMAC handshakes verify against the shared secret.

- Keep the shared secret private. Anyone with it can open relay sessions.
- One secret per deployment is a reasonable default for a personal mesh. Rotate it by updating the relay and every peer's config simultaneously.
- Restrict the metrics port to your monitoring network; it carries counter data that reveals traffic volumes.
- The announce directory (when enabled) stores self-certifying signed blobs. The relay stores and serves them verbatim; it never validates the signature. The looking-up peer is the verifier.

## Build features

| Feature | Default | Description |
|---------|---------|-------------|
| `metrics` | enabled | Enables the `/metrics` and `/health` HTTP endpoint (axum). |
| `announce` | disabled | Enables the `/announce/<device_id>` directory endpoint. |

Build without metrics to reduce the binary size:

```sh
cargo build --release -p cascade-relay-server --no-default-features
```

Build with the announce directory:

```sh
cargo build --release -p cascade-relay-server --features announce
```
