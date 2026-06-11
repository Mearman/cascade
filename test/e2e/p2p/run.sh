#!/usr/bin/env bash
# End-to-end test for the cascade P2P backend.
#
# Spins up two cascade daemons in Docker on a shared bridge network,
# wires them together as peers, and verifies that a file uploaded via
# node A's WebDAV mount becomes readable through node B's WebDAV mount —
# proving the BEP wire path works across separate processes on a real
# (containerised) network stack.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

COMPOSE=(docker compose -f compose.yml)
NODE_A_URL="http://127.0.0.1:18080"
NODE_B_URL="http://127.0.0.1:18081"

log() { printf '[e2e] %s\n' "$*" >&2; }

cleanup() {
    log "tearing down compose stack"
    "${COMPOSE[@]}" down -v --remove-orphans >/dev/null 2>&1 || true
    rm -rf configs/node-a configs/node-b
}
trap cleanup EXIT

reset_configs() {
    rm -rf configs/node-a configs/node-b
    mkdir -p configs/node-a configs/node-b
}

write_config() {
    local DIR="$1" PEER_DEV="$2" PEER_HOST="$3"
    cat > "$DIR/config.toml" <<EOF
[backends.shared]
type = "p2p"
# Single-backend node: mount the shared folder at the neutral root so its
# content appears directly at "/" (uniform-backend-mounts default would place
# it at "/p2p-shared/"). Serving a single backend at non-root via WebDAV is a
# tracked follow-up; this test exercises P2P sync, not the mount layout.
mount = "/"

[mount]
point = "/data/mount"
EOF
    cat > "$DIR/shared.toml" <<EOF
type = "p2p"
name = "shared"
data_dir = "/data/p2p"
listen_addr = "0.0.0.0:22000"

[[peers]]
device_id = "$PEER_DEV"
address = "$PEER_HOST:22000"
EOF
}

phase1_gen_identities() {
    log "phase 1: generating device identities"
    DEV_A=$("${COMPOSE[@]}" run --rm --no-deps node-a \
        --config /config p2p-identity --data-dir /data/p2p \
        | tail -n 1 | tr -d '\r\n')
    DEV_B=$("${COMPOSE[@]}" run --rm --no-deps node-b \
        --config /config p2p-identity --data-dir /data/p2p \
        | tail -n 1 | tr -d '\r\n')
    log "  node-a: $DEV_A"
    log "  node-b: $DEV_B"
    if [[ -z "$DEV_A" || -z "$DEV_B" ]]; then
        log "FAIL: empty device id"
        exit 1
    fi
}

phase2_write_configs() {
    log "phase 2: writing peer configs"
    write_config configs/node-a "$DEV_B" "node-b"
    write_config configs/node-b "$DEV_A" "node-a"
}

phase3_start() {
    log "phase 3: starting nodes"
    "${COMPOSE[@]}" up -d node-a node-b
}

wait_for_webdav() {
    local URL="$1" LABEL="$2"
    log "waiting for $LABEL at $URL"
    for _ in $(seq 1 60); do
        if curl -sf -o /dev/null -X PROPFIND -H "Depth: 0" "$URL/"; then
            log "  $LABEL ready"
            return 0
        fi
        sleep 1
    done
    log "FAIL: $LABEL never came up; dumping logs"
    "${COMPOSE[@]}" logs --tail=80 >&2
    return 1
}

phase4_wait_ready() {
    wait_for_webdav "$NODE_A_URL" "node-a"
    wait_for_webdav "$NODE_B_URL" "node-b"
    log "waiting for the peer connection to settle"
    sleep 8
}

phase5_test_propagation() {
    local PAYLOAD="cascade-p2p-e2e $RANDOM-$(date +%s)"

    log "exploring VFS root on node-a"
    curl -fsS -X PROPFIND -H "Depth: 1" "$NODE_A_URL/" | head -c 2000
    printf '\n'

    local REMOTE_PATH="/probe.txt"
    log "phase 5: PUT to node-a$REMOTE_PATH"
    curl -fsS -X PUT \
        -H "Content-Type: text/plain" \
        --data-binary "$PAYLOAD" \
        "$NODE_A_URL$REMOTE_PATH"

    log "polling node-b for the same path"
    local GOT=""
    for _ in $(seq 1 30); do
        GOT=$(curl -fsS "$NODE_B_URL$REMOTE_PATH" 2>/dev/null || true)
        if [[ "$GOT" == "$PAYLOAD" ]]; then
            log "PASS: node-b returned the same content (${#PAYLOAD} bytes)"
            return 0
        fi
        sleep 1
    done

    log "FAIL: node-b never returned the expected content"
    log "  expected: $PAYLOAD"
    log "  got:      ${GOT:0:200}"
    "${COMPOSE[@]}" logs --tail=120 >&2
    return 1
}

main() {
    reset_configs
    "${COMPOSE[@]}" build
    phase1_gen_identities
    phase2_write_configs
    phase3_start
    phase4_wait_ready
    phase5_test_propagation
}

main "$@"
