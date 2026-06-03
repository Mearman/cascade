#!/bin/sh
# Launcher script for the cascade-relay systemd unit.
#
# systemd does not support bash-style ${VAR:-default} parameter expansion in
# ExecStart; it only expands plain ${VAR} (empty string when unset).  This
# script is exec'd by ExecStart so that optional flags — --metrics-bind and
# --announce-bind — are only passed when the corresponding environment
# variables are non-empty, matching the docker-entrypoint.sh behaviour
# exactly.  The script is installed alongside the unit file.
#
# Environment variables (set in /etc/cascade/relay.env via EnvironmentFile=):
#
#   CASCADE_RELAY_SHARED_SECRET  — 64-char hex HMAC secret (required).
#                                  Never passed on the command line; clap
#                                  picks it up via env = "CASCADE_RELAY_SHARED_SECRET".
#   CASCADE_RELAY_LISTEN         — bind address for the byte-pipe listener.
#                                  Default (from clap): 0.0.0.0:9999
#   CASCADE_RELAY_METRICS        — bind address for /metrics and /health.
#                                  When unset, neither endpoint is started.
#   CASCADE_RELAY_ANNOUNCE       — bind address for the announce directory.
#                                  Requires the announce build feature.
#                                  When unset, the announce endpoint is disabled.

set -eu

if [ -z "${CASCADE_RELAY_SHARED_SECRET:-}" ]; then
    echo "ERROR: CASCADE_RELAY_SHARED_SECRET is not set." >&2
    echo "       Set it in /etc/cascade/relay.env." >&2
    echo "       Generate a secret with: openssl rand -hex 32" >&2
    exit 1
fi

# --bind is optional here: when CASCADE_RELAY_LISTEN is unset the binary's
# own clap default (0.0.0.0:9999) applies.
ARGS=""
if [ -n "${CASCADE_RELAY_LISTEN:-}" ]; then
    ARGS="--bind ${CASCADE_RELAY_LISTEN}"
fi

if [ -n "${CASCADE_RELAY_METRICS:-}" ]; then
    ARGS="$ARGS --metrics-bind ${CASCADE_RELAY_METRICS}"
fi

if [ -n "${CASCADE_RELAY_ANNOUNCE:-}" ]; then
    ARGS="$ARGS --announce-bind ${CASCADE_RELAY_ANNOUNCE}"
fi

# CASCADE_RELAY_SHARED_SECRET remains in the environment; clap reads it via
# env = "CASCADE_RELAY_SHARED_SECRET" without it appearing in the command line.
# shellcheck disable=SC2086
exec /usr/local/bin/cascade-relay $ARGS "$@"
