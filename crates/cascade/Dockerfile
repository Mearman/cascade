# Multi-stage build for the cascade daemon binary.
#
# Stage 1: compile cascade in release mode against the pinned toolchain
# (rust-toolchain.toml declares 1.96.0; rustup's automatic toolchain
# selection picks it up from the file when the workspace root is present
# in the build context).
#
# Stage 2: minimal Debian slim runtime.  A shell is required so that the
# entrypoint script can expand CASCADE_* environment variables into
# config files and CLI flags.  FUSE userspace packages (fuse3 and
# libfuse3-3) are installed so the optional in-container FUSE mount
# (CASCADE_MOUNT=1) works without a separate layer.  ca-certificates is
# installed for gdrive and S3 TLS — the daemon connects to cloud APIs
# over HTTPS in both seed and mount modes.

FROM rust:1.96-bookworm AS builder

WORKDIR /src

# Copy the full workspace so cargo can resolve all inter-crate
# dependencies.
COPY . .

RUN cargo build --release -p cascade && \
    cp /src/target/release/cascade /usr/local/bin/cascade

# ---- runtime ----
FROM debian:bookworm-slim

# Install:
#   fuse3        — fuse3 userspace (provides fusermount3, required by libfuse3)
#   libfuse3-3   — libfuse3 runtime, required when CASCADE_MOUNT=1
#   ca-certificates — TLS root CAs for gdrive OAuth2 and S3 HTTPS endpoints
#   wget         — used by the HEALTHCHECK (matches relay image; curl not installed)
#
# fuse3 and libfuse3-3 are not used in default seed mode (CASCADE_MOUNT
# unset or 0) but are present so the same image serves both modes without
# a separate variant.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        fuse3 \
        libfuse3-3 \
        ca-certificates \
        wget \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/bin/cascade /usr/local/bin/cascade

# WebDAV server port (in-process, seed mode and browsable mode).
# Set CASCADE_WEBDAV_BIND=0.0.0.0:<port> and publish this port to let
# other hosts browse the seeded tree over WebDAV.
EXPOSE 8080

# BEP P2P listener port for a p2p-type backend or the engine P2P layer.
# Set CASCADE_P2P_LISTEN=0.0.0.0:22000 and publish this port when the
# operator port-forwards or maps the container port.
EXPOSE 22000

# Operators configure the daemon via these environment variables.
#
# Required:
#   CASCADE_BACKEND_TYPE  — gdrive | s3 | local | p2p
#
# Optional (key defaults shown):
#   CASCADE_CONFIG_DIR      /config
#   CASCADE_DATA_DIR        /data
#   CASCADE_MOUNT           0   (1 = enable in-container FUSE mount)
#   CASCADE_MOUNT_POINT     /mnt/cascade
#   CASCADE_BACKEND_NAME    <value of CASCADE_BACKEND_TYPE>
#   CASCADE_P2P             0   (1 = enable engine P2P optimisation layer)
#   CASCADE_P2P_POSTURE     private
#   CASCADE_P2P_LISTEN      0.0.0.0:0
#   CASCADE_P2P_RELAY_ENDPOINT  (host:port of cascade-relay, required for WAN)
#   CASCADE_P2P_RELAY_SECRET    (64-char hex HMAC, required with relay)
#   CASCADE_P2P_ANNOUNCE_URL    (https://... announce-server URL)
#   CASCADE_P2P_ANNOUNCE_SECRET (64-char hex HMAC for announce-server writes)
#   CASCADE_PRESENTER       (unset = auto; webdav = force WebDAV)
#   CASCADE_WEBDAV_BIND     127.0.0.1:0
#   RUST_LOG                info
#
# S3 backend (CASCADE_BACKEND_TYPE=s3):
#   CASCADE_S3_ENDPOINT         (required)
#   CASCADE_S3_BUCKET           (required)
#   CASCADE_S3_REGION           us-east-1
#   CASCADE_S3_ACCESS_KEY_ID    (required)
#   CASCADE_S3_SECRET_ACCESS_KEY (required)
#
# Google Drive backend (CASCADE_BACKEND_TYPE=gdrive):
#   CASCADE_GDRIVE_CLIENT_ID     (required)
#   CASCADE_GDRIVE_CLIENT_SECRET (required)
#   CASCADE_GDRIVE_ACCOUNT       <value of CASCADE_BACKEND_NAME>
#
# Local backend (CASCADE_BACKEND_TYPE=local):
#   CASCADE_LOCAL_ROOT  (required; path under CASCADE_DATA_DIR)

ENV CASCADE_CONFIG_DIR="/config" \
    CASCADE_DATA_DIR="/data" \
    CASCADE_MOUNT_POINT="/mnt/cascade" \
    RUST_LOG="info"

# /config: small config volume — holds config.toml, per-backend <name>.toml,
#          state.db, cascade.pid, and gdrive tokens. Must be writable and
#          persistent across restarts.
# /data:   bulky runtime state — P2P block store and index, local-backend
#          roots, cache. Sized and placed independently (e.g. an UnRAID array
#          share). Must be writable.
VOLUME ["/config", "/data"]

COPY --from=builder /src/crates/cascade/docker-entrypoint.sh /docker-entrypoint.sh
RUN chmod +x /docker-entrypoint.sh

# Liveness healthcheck against the WebDAV server's root path.
#
# The WebDAV server is always running in seed mode (CASCADE_MOUNT unset
# or 0) when CASCADE_WEBDAV_BIND is set to a non-loopback address.  In
# default (127.0.0.1:0) mode the port is ephemeral and unknown at build
# time, so the check exits 0 (healthy by assumption) rather than failing
# the container.  Set CASCADE_WEBDAV_BIND=0.0.0.0:8080 in production
# deployments so the orchestrator can distinguish a healthy daemon from a
# crashed process.
#
# wget is installed explicitly above (debian:bookworm-slim does not
# include it in its base package set).
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD if [ -n "${CASCADE_WEBDAV_BIND:-}" ] && \
           [ "${CASCADE_WEBDAV_BIND}" != "127.0.0.1:0" ]; then \
          PORT="${CASCADE_WEBDAV_BIND##*:}"; \
          wget -qO- "http://127.0.0.1:${PORT}/" > /dev/null 2>&1 || exit 1; \
        else \
          exit 0; \
        fi

ENTRYPOINT ["/docker-entrypoint.sh"]
