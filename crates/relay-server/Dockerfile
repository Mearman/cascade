# Multi-stage build for the cascade-relay binary.
#
# Stage 1: compile cascade-relay in release mode against the pinned
# toolchain (rust-toolchain.toml declares 1.96.0; rustup's automatic
# toolchain selection picks it up from the file when the workspace root
# is present in the build context).
#
# Stage 2: minimal Debian slim runtime — a shell is needed so that the
# entrypoint script can expand CASCADE_RELAY_* environment variables
# into cascade-relay CLI flags.  The relay is the only process in the
# container so the attack surface from shipping a shell is minimal.

FROM rust:1.96-bookworm AS builder

WORKDIR /src

# Copy the full workspace so cargo can resolve all inter-crate
# dependencies (p2p, engine, etc. are referenced by relay-server's
# dev-dependencies and the workspace Cargo.toml).
COPY . .

RUN cargo build --release -p cascade-relay-server && \
    cp /src/target/release/cascade-relay /usr/local/bin/cascade-relay

# ---- runtime ----
FROM debian:bookworm-slim

COPY --from=builder /usr/local/bin/cascade-relay /usr/local/bin/cascade-relay

# Byte-pipe listener (protocol traffic from peers).
EXPOSE 9999
# Optional /metrics and /health HTTP endpoint.
# /health is an unauthenticated liveness probe; /metrics serves Prometheus text.
EXPOSE 9998
# Optional announce/lookup directory endpoint (requires --features announce at build time).
EXPOSE 9997

# Operators configure the relay via these environment variables.
#
# CASCADE_RELAY_SHARED_SECRET  — 64-char hex shared HMAC secret (required)
#                                Generate with: openssl rand -hex 32
# CASCADE_RELAY_LISTEN         — bind address for the peer listener
#                                Default: 0.0.0.0:9999
# CASCADE_RELAY_METRICS        — bind address for /metrics and /health (optional)
#                                Set to 0.0.0.0:9998 to enable.
# CASCADE_RELAY_ANNOUNCE       — bind address for the announce directory (optional)
#                                Requires the announce build feature.
#                                Set to 0.0.0.0:9997 to enable.
ENV CASCADE_RELAY_LISTEN="0.0.0.0:9999"

# The entrypoint shell script maps env vars to CLI flags so that
# operators only need to set environment variables in docker-compose or
# helm values — no need to override the command line.
COPY --from=builder /src/crates/relay-server/docker-entrypoint.sh /docker-entrypoint.sh
RUN chmod +x /docker-entrypoint.sh

# Liveness healthcheck against the /health endpoint on the metrics port.
#
# The /health route is served by the same HTTP listener as /metrics, so the
# check works only when CASCADE_RELAY_METRICS is set. When it is unset the
# check exits 0 immediately (healthy by assumption) so the container does not
# cycle through unhealthy states just because the operator chose not to expose
# metrics. In a production deployment set CASCADE_RELAY_METRICS so the
# orchestrator can actually distinguish a crashed process from a healthy one.
#
# wget is present in debian:bookworm-slim; curl is not installed to keep the
# image minimal.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD if [ -n "${CASCADE_RELAY_METRICS:-}" ]; then \
          wget -qO- "http://${CASCADE_RELAY_METRICS}/health" | grep -q ok; \
        else \
          exit 0; \
        fi

ENTRYPOINT ["/docker-entrypoint.sh"]
