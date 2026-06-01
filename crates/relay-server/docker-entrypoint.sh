#!/bin/sh
# Entrypoint for the cascade-relay Docker image.
#
# Maps CASCADE_RELAY_* environment variables to cascade-relay CLI flags
# so operators can configure the relay purely through docker-compose
# environment: blocks or helm values, without overriding the CMD.
#
# Required:
#   CASCADE_RELAY_SECRET  — 64-character hex shared HMAC secret.
#                           Generate with: openssl rand -hex 32
#
# Optional:
#   CASCADE_RELAY_LISTEN  — bind address for the peer listener.
#                           Default: 0.0.0.0:9999
#   CASCADE_RELAY_METRICS — bind address for the /metrics HTTP endpoint.
#                           When unset the metrics endpoint is disabled.
#
# Any additional arguments passed to the container (CMD overrides) are
# forwarded as-is after the flags derived from env vars, allowing
# advanced options such as --session-timeout-seconds or --max-sessions
# to be set via docker run arguments.

set -eu

if [ -z "${CASCADE_RELAY_SECRET:-}" ]; then
    echo "ERROR: CASCADE_RELAY_SECRET is not set." >&2
    echo "       Generate a secret with: openssl rand -hex 32" >&2
    exit 1
fi

ARGS="--bind ${CASCADE_RELAY_LISTEN:-0.0.0.0:9999}"
ARGS="$ARGS --shared-secret $CASCADE_RELAY_SECRET"

if [ -n "${CASCADE_RELAY_METRICS:-}" ]; then
    ARGS="$ARGS --metrics-bind $CASCADE_RELAY_METRICS"
fi

# shellcheck disable=SC2086
exec /usr/local/bin/cascade-relay $ARGS "$@"
