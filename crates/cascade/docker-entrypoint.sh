#!/bin/sh
# Entrypoint for the cascade daemon Docker image.
#
# On first boot this script renders a valid config.toml and a per-backend
# <name>.toml under CASCADE_CONFIG_DIR from the CASCADE_* environment
# variables, then execs the daemon as PID 1.  The rendering is idempotent:
# if config.toml already exists it is left untouched, allowing operators
# who manage the config volume directly to bypass the auto-generation by
# placing their own config.toml before the container starts.
#
# Required:
#   CASCADE_BACKEND_TYPE — gdrive | s3 | local | p2p
#
# See the Dockerfile for the full environment-variable reference.

set -eu

# ---------------------------------------------------------------------------
# Defaults
# ---------------------------------------------------------------------------

CASCADE_CONFIG_DIR="${CASCADE_CONFIG_DIR:-/config}"
CASCADE_DATA_DIR="${CASCADE_DATA_DIR:-/data}"
CASCADE_MOUNT="${CASCADE_MOUNT:-0}"
CASCADE_MOUNT_POINT="${CASCADE_MOUNT_POINT:-/mnt/cascade}"
CASCADE_P2P="${CASCADE_P2P:-0}"
CASCADE_P2P_POSTURE="${CASCADE_P2P_POSTURE:-private}"
CASCADE_P2P_LISTEN="${CASCADE_P2P_LISTEN:-0.0.0.0:0}"
CASCADE_S3_REGION="${CASCADE_S3_REGION:-us-east-1}"
RUST_LOG="${RUST_LOG:-info}"
export RUST_LOG

# ---------------------------------------------------------------------------
# Validate required env
# ---------------------------------------------------------------------------

if [ -z "${CASCADE_BACKEND_TYPE:-}" ]; then
    echo "ERROR: CASCADE_BACKEND_TYPE is required." >&2
    echo "       Set it to one of: gdrive, s3, local, p2p" >&2
    exit 1
fi

case "${CASCADE_BACKEND_TYPE}" in
    gdrive | s3 | local | p2p) ;;
    *)
        echo "ERROR: unsupported CASCADE_BACKEND_TYPE '${CASCADE_BACKEND_TYPE}'." >&2
        echo "       Expected one of: gdrive, s3, local, p2p" >&2
        exit 1
        ;;
esac

# Backend name defaults to the type string when not supplied.
CASCADE_BACKEND_NAME="${CASCADE_BACKEND_NAME:-${CASCADE_BACKEND_TYPE}}"

# ---------------------------------------------------------------------------
# Ensure config and data directories exist
# ---------------------------------------------------------------------------

mkdir -p "${CASCADE_CONFIG_DIR}"
mkdir -p "${CASCADE_DATA_DIR}"
mkdir -p "${CASCADE_MOUNT_POINT}"

# Restrict config dir to the running user: it holds OAuth tokens and S3 keys.
chmod 700 "${CASCADE_CONFIG_DIR}"

# ---------------------------------------------------------------------------
# Render config.toml (idempotent — skip if already present)
# ---------------------------------------------------------------------------

CONFIG_TOML="${CASCADE_CONFIG_DIR}/config.toml"

if [ ! -f "${CONFIG_TOML}" ]; then
    P2P_ENABLED="false"
    if [ "${CASCADE_P2P}" = "1" ]; then
        P2P_ENABLED="true"
    fi

    cat > "${CONFIG_TOML}" <<TOML
[backends.${CASCADE_BACKEND_NAME}]
type = "${CASCADE_BACKEND_TYPE}"

[mount]
point = "${CASCADE_MOUNT_POINT}"

[p2p]
enabled = ${P2P_ENABLED}
TOML

    # When the engine's P2P optimisation layer is on, surface the posture and
    # WAN relay so a cloud-backed node is reachable through NAT. These map to
    # the [p2p] keys cascade reads (singular relay_endpoint), distinct from the
    # plural relay_endpoints inside a p2p-type backend's own TOML. relay_endpoint
    # accepts a host:port — a DNS or Docker service name is resolved at startup.
    if [ "${CASCADE_P2P}" = "1" ]; then
        printf 'posture = "%s"\n' "${CASCADE_P2P_POSTURE}" >> "${CONFIG_TOML}"
        if [ -n "${CASCADE_P2P_RELAY_ENDPOINT:-}" ]; then
            printf 'relay_endpoint = "%s"\n' "${CASCADE_P2P_RELAY_ENDPOINT}" >> "${CONFIG_TOML}"
        fi
        if [ -n "${CASCADE_P2P_RELAY_SECRET:-}" ]; then
            printf 'relay_shared_secret = "%s"\n' "${CASCADE_P2P_RELAY_SECRET}" >> "${CONFIG_TOML}"
        fi
    fi

    chmod 600 "${CONFIG_TOML}"
fi

# ---------------------------------------------------------------------------
# Render per-backend <name>.toml (idempotent — skip if already present)
#
# Secrets (S3 keys, OAuth secrets, relay/announce secrets) are written
# only to the 0600 config files, never placed on argv, so they do not
# appear in /proc/<pid>/cmdline or process listings.
# ---------------------------------------------------------------------------

BACKEND_TOML="${CASCADE_CONFIG_DIR}/${CASCADE_BACKEND_NAME}.toml"

if [ ! -f "${BACKEND_TOML}" ]; then
    case "${CASCADE_BACKEND_TYPE}" in
        gdrive)
            if [ -z "${CASCADE_GDRIVE_CLIENT_ID:-}" ]; then
                echo "ERROR: CASCADE_GDRIVE_CLIENT_ID is required for CASCADE_BACKEND_TYPE=gdrive" >&2
                exit 1
            fi
            if [ -z "${CASCADE_GDRIVE_CLIENT_SECRET:-}" ]; then
                echo "ERROR: CASCADE_GDRIVE_CLIENT_SECRET is required for CASCADE_BACKEND_TYPE=gdrive" >&2
                exit 1
            fi

            # Account identifier for token persistence defaults to the backend name.
            GDRIVE_ACCOUNT="${CASCADE_GDRIVE_ACCOUNT:-${CASCADE_BACKEND_NAME}}"

            # Write 0600 before any content to prevent a window where the secret
            # is readable by other processes.
            : > "${BACKEND_TOML}"
            chmod 600 "${BACKEND_TOML}"

            cat > "${BACKEND_TOML}" <<TOML
type = "gdrive"
account = "${GDRIVE_ACCOUNT}"
client_id = "${CASCADE_GDRIVE_CLIENT_ID}"
client_secret = "${CASCADE_GDRIVE_CLIENT_SECRET}"
TOML
            ;;

        s3)
            if [ -z "${CASCADE_S3_ENDPOINT:-}" ]; then
                echo "ERROR: CASCADE_S3_ENDPOINT is required for CASCADE_BACKEND_TYPE=s3" >&2
                exit 1
            fi
            if [ -z "${CASCADE_S3_BUCKET:-}" ]; then
                echo "ERROR: CASCADE_S3_BUCKET is required for CASCADE_BACKEND_TYPE=s3" >&2
                exit 1
            fi
            if [ -z "${CASCADE_S3_ACCESS_KEY_ID:-}" ]; then
                echo "ERROR: CASCADE_S3_ACCESS_KEY_ID is required for CASCADE_BACKEND_TYPE=s3" >&2
                exit 1
            fi
            if [ -z "${CASCADE_S3_SECRET_ACCESS_KEY:-}" ]; then
                echo "ERROR: CASCADE_S3_SECRET_ACCESS_KEY is required for CASCADE_BACKEND_TYPE=s3" >&2
                exit 1
            fi

            : > "${BACKEND_TOML}"
            chmod 600 "${BACKEND_TOML}"

            cat > "${BACKEND_TOML}" <<TOML
type = "s3"
endpoint = "${CASCADE_S3_ENDPOINT}"
bucket = "${CASCADE_S3_BUCKET}"
region = "${CASCADE_S3_REGION}"
access_key_id = "${CASCADE_S3_ACCESS_KEY_ID}"
secret_access_key = "${CASCADE_S3_SECRET_ACCESS_KEY}"
TOML
            ;;

        local)
            if [ -z "${CASCADE_LOCAL_ROOT:-}" ]; then
                echo "ERROR: CASCADE_LOCAL_ROOT is required for CASCADE_BACKEND_TYPE=local" >&2
                echo "       Set it to a path under CASCADE_DATA_DIR (${CASCADE_DATA_DIR})" >&2
                exit 1
            fi

            mkdir -p "${CASCADE_LOCAL_ROOT}"

            : > "${BACKEND_TOML}"
            chmod 600 "${BACKEND_TOML}"

            cat > "${BACKEND_TOML}" <<TOML
type = "local"
root = "${CASCADE_LOCAL_ROOT}"
TOML
            ;;

        p2p)
            # The p2p backend requires a `name` key matching the backend name;
            # open_from_config bails without it.
            : > "${BACKEND_TOML}"
            chmod 600 "${BACKEND_TOML}"

            # Write the mandatory fields unconditionally.
            cat > "${BACKEND_TOML}" <<TOML
type = "p2p"
name = "${CASCADE_BACKEND_NAME}"
exposure = "${CASCADE_P2P_POSTURE}"
listen_addr = "${CASCADE_P2P_LISTEN}"
TOML

            # relay_endpoints and relay_shared_secret are optional; omit them
            # when not supplied rather than writing empty strings.
            if [ -n "${CASCADE_P2P_RELAY_ENDPOINT:-}" ]; then
                printf 'relay_endpoints = ["%s"]\n' "${CASCADE_P2P_RELAY_ENDPOINT}" \
                    >> "${BACKEND_TOML}"
            fi
            if [ -n "${CASCADE_P2P_RELAY_SECRET:-}" ]; then
                printf 'relay_shared_secret = "%s"\n' "${CASCADE_P2P_RELAY_SECRET}" \
                    >> "${BACKEND_TOML}"
            fi

            # Announce-server entry: both URL and secret required together.
            if [ -n "${CASCADE_P2P_ANNOUNCE_URL:-}" ]; then
                if [ -z "${CASCADE_P2P_ANNOUNCE_SECRET:-}" ]; then
                    echo "ERROR: CASCADE_P2P_ANNOUNCE_SECRET is required when CASCADE_P2P_ANNOUNCE_URL is set" >&2
                    exit 1
                fi
                cat >> "${BACKEND_TOML}" <<TOML

[[announce_servers]]
url = "${CASCADE_P2P_ANNOUNCE_URL}"
shared_secret = "${CASCADE_P2P_ANNOUNCE_SECRET}"
TOML
            fi
            ;;
    esac
fi

# ---------------------------------------------------------------------------
# Seed mode vs mount mode
#
# Default (CASCADE_MOUNT unset or 0): run headless seed mode.
#   - Force the WebDAV presenter via CASCADE_PRESENTER=webdav so no FUSE
#     mount is attempted regardless of the Linux FUSE presenter's default
#     behaviour.
#   - Pass --no-mount so the daemon does not try to mount the WebDAV
#     filesystem at CASCADE_MOUNT_POINT.
#   - The in-process WebDAV server binds per CASCADE_WEBDAV_BIND and can
#     be published for browse-over-WebDAV access.
#
# Browsable mode (CASCADE_MOUNT=1): leave CASCADE_PRESENTER unset so the
#   Linux FUSE → NFS presenter chain runs normally.  Requires:
#     --cap-add SYS_ADMIN
#     --device /dev/fuse
#     rshared bind-mount of CASCADE_MOUNT_POINT
# ---------------------------------------------------------------------------

if [ "${CASCADE_MOUNT}" = "1" ]; then
    # Browsable mode: presenter selection falls through to the daemon's
    # built-in Linux chain (FUSE first, then NFS fallback).
    exec /usr/local/bin/cascade \
        --config "${CASCADE_CONFIG_DIR}" \
        start \
        "$@"
else
    # Seed mode: force WebDAV presenter and skip the OS-level mount.
    export CASCADE_PRESENTER=webdav
    exec /usr/local/bin/cascade \
        --config "${CASCADE_CONFIG_DIR}" \
        start \
        --no-mount \
        "$@"
fi
