# cascade-relay deployment

`cascade-relay` is a stateless byte-pipe relay for Cascade peers that cannot reach each other directly. Peers connect to the relay and authenticate with a shared HMAC secret; the relay pairs them and shuttles already-encrypted bytes between them without inspecting the payload.

Three distribution paths are supported: building from source with `cargo install`, installing a pre-built binary via Homebrew, and running the official Docker image from `ghcr.io`.

## Prerequisites

- A shared HMAC secret shared between the relay and every peer that will use it. Generate one with:

  ```sh
  openssl rand -hex 32
  ```

  The output is a 64-character hexadecimal string (32 bytes). Keep it out of shell history — pass it via a file, environment variable, or secrets manager.

- The relay needs a publicly-reachable TCP address so peers can connect. The default port is `9999`. Open it in your firewall or security group.

## Install paths

### From source (cargo install)

```sh
cargo install --git https://github.com/Mearman/cascade --bin cascade-relay
```

This builds and installs the `cascade-relay` binary from the latest `main` branch. Use a pinned tag for reproducibility:

```sh
cargo install --git https://github.com/Mearman/cascade --tag cascade-relay-v0.1.37 --bin cascade-relay
```

### Homebrew

```sh
brew tap Mearman/cascade
brew install Mearman/cascade/cascade-relay
```

This installs a pre-built binary from the latest `cascade-relay-v*` release. The formula covers macOS (aarch64 and x86_64) and Linux (x86_64 and aarch64) via the same tap used by the main `cascade` formula.

### Docker (ghcr.io)

```sh
docker pull ghcr.io/mearman/cascade-relay:latest
```

Versioned tags match the relay release version (e.g. `ghcr.io/mearman/cascade-relay:0.1.37`). The image is multi-platform (`linux/amd64` and `linux/arm64`).

## Running the relay

### Direct invocation

```sh
cascade-relay \
  --bind 0.0.0.0:9999 \
  --shared-secret "$(cat /etc/cascade/relay.secret)" \
  --metrics-bind 0.0.0.0:9998
```

All flags:

| Flag | Default | Description |
|------|---------|-------------|
| `--bind` | `0.0.0.0:9999` | Address the byte-pipe listener binds to. |
| `--shared-secret` | (required) | 64-char hex HMAC secret. |
| `--session-timeout-seconds` | `60` | How long the first peer may wait for its partner before the session is reaped. |
| `--max-sessions` | `1024` | Maximum simultaneous in-flight sessions. New sessions are rejected when this limit is reached. |
| `--metrics-bind` | (disabled) | Address for the Prometheus `/metrics` HTTP endpoint. |

### Docker

Pass configuration via environment variables:

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

Environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `CASCADE_RELAY_SHARED_SECRET` | (required) | 64-char hex HMAC secret. |
| `CASCADE_RELAY_LISTEN` | `0.0.0.0:9999` | Bind address for the peer listener. |
| `CASCADE_RELAY_METRICS` | (disabled) | Bind address for the `/metrics` endpoint. |

Additional CLI flags (e.g. `--max-sessions`) can be passed as `CMD` arguments after the image name:

```sh
docker run ... ghcr.io/mearman/cascade-relay:latest \
  --session-timeout-seconds 120 \
  --max-sessions 512
```

### Docker Compose

```yaml
services:
  relay:
    image: ghcr.io/mearman/cascade-relay:latest
    restart: unless-stopped
    ports:
      - "9999:9999"
      - "9998:9998"
    environment:
      CASCADE_RELAY_SHARED_SECRET: "${CASCADE_RELAY_SHARED_SECRET}"
      CASCADE_RELAY_LISTEN: "0.0.0.0:9999"
      CASCADE_RELAY_METRICS: "0.0.0.0:9998"
```

Store the secret in a `.env` file (not committed to version control) or inject it via your orchestrator's secrets mechanism.

## Pointing peers at the relay

In the peer's `.cascade` config or backend configuration, set the relay endpoint and the same shared secret:

```toml
[p2p]
relay_endpoints = ["relay.example.com:9999"]
relay_shared_secret = "your-64-char-hex-secret"
```

Both peers must share the same secret as the relay. The relay validates the HMAC handshake before pairing sessions; a mismatched secret causes authentication to fail.

## Observability

When `--metrics-bind` (or `CASCADE_RELAY_METRICS`) is set, the relay serves Prometheus metrics at `http://<metrics-bind>/metrics`. Exposed counters include:

- `relay_sessions_total` — cumulative sessions created.
- `relay_sessions_active` — sessions currently in the paired or waiting state.
- `relay_bytes_relayed_total` — bytes forwarded across all sessions.

## Security notes

- The relay never decrypts peer traffic. Peers establish end-to-end TLS over the byte pipe; the relay sees only opaque bytes after the initial HMAC handshake.
- Keep the shared secret private. Anyone with it can open relay sessions against your server. Use a unique secret per relay deployment.
- Run the relay behind a firewall that allows only `9999/tcp` inbound (and optionally `9998/tcp` for metrics, restricted to your monitoring network).
