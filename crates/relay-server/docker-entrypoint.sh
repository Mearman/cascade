#!/bin/sh
# Entrypoint for the cascade-relay Docker image.
#
# Maps CASCADE_RELAY_* environment variables to cascade-relay CLI flags
# so operators can configure the relay purely through docker-compose
# environment: blocks or helm values, without overriding the CMD.
#
# Required:
#   CASCADE_RELAY_SHARED_SECRET  — 64-character hex shared HMAC secret.
#                                  Generate with: openssl rand -hex 32
#                                  Passed via environment variable only, never
#                                  on the command line, to prevent the secret
#                                  appearing in /proc/<pid>/cmdline.
#
# Optional:
#   CASCADE_RELAY_LISTEN    — bind address for the peer listener.
#                             Default: 0.0.0.0:9999
#   CASCADE_RELAY_METRICS   — bind address for the /metrics and /health HTTP
#                             endpoint. When unset both endpoints are disabled.
#   CASCADE_RELAY_ANNOUNCE  — bind address for the /announce/<device_id>
#                             directory endpoint (requires the `announce` build
#                             feature). When unset the announce endpoint is
#                             disabled.
#
# Any additional arguments passed to the container (CMD overrides) are
# forwarded as-is after the flags derived from env vars, allowing
# advanced options such as --session-timeout-seconds or --max-sessions
# to be set via docker run arguments.

set -eu

if [ -z "${CASCADE_RELAY_SHARED_SECRET:-}" ]; then
    echo "ERROR: CASCADE_RELAY_SHARED_SECRET is not set." >&2
    echo "       Generate a secret with: openssl rand -hex 32" >&2
    exit 1
fi

# Export so clap picks it up via env = "CASCADE_RELAY_SHARED_SECRET".
# The secret is never passed on the command line, preventing it from
# appearing in /proc/<pid>/cmdline or process listings.
export CASCADE_RELAY_SHARED_SECRET

ARGS="--bind ${CASCADE_RELAY_LISTEN:-0.0.0.0:9999}"

if [ -n "${CASCADE_RELAY_METRICS:-}" ]; then
    ARGS="$ARGS --metrics-bind $CASCADE_RELAY_METRICS"
fi

if [ -n "${CASCADE_RELAY_ANNOUNCE:-}" ]; then
    ARGS="$ARGS --announce-bind $CASCADE_RELAY_ANNOUNCE"
fi

# shellcheck disable=SC2086
exec /usr/local/bin/cascade-relay $ARGS "$@"
