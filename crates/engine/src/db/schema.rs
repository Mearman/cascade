use anyhow::Result;
use rusqlite::Connection;

/// Schema version tracker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SchemaVersion(pub i32);

impl SchemaVersion {
    /// Current schema version.
    #[must_use]
    pub const fn current() -> Self {
        Self(5)
    }
}

/// Run all migrations from `from` to `to`.
pub fn migrate(conn: &Connection, from: SchemaVersion, _to: SchemaVersion) -> Result<()> {
    if from < SchemaVersion(1) {
        v1_init(conn)?;
    }
    if from < SchemaVersion(2) {
        v2_manage_plane(conn)?;
    }
    if from < SchemaVersion(3) {
        v3_capability_tokens(conn)?;
    }
    if from < SchemaVersion(4) {
        v4_data_receive_quarantine(conn)?;
    }
    if from < SchemaVersion(5) {
        v5_data_explicit_control(conn)?;
    }

    Ok(())
}

/// Initial schema — all tables for Phase 1.
fn v1_init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS files (
            id            TEXT PRIMARY KEY,
            backend_id    TEXT NOT NULL,
            path          TEXT UNIQUE NOT NULL,
            parent_id     TEXT,
            name          TEXT NOT NULL,
            is_dir        BOOLEAN NOT NULL,
            size          INTEGER,
            mime_type     TEXT,
            mod_time      INTEGER,
            remote_hash   TEXT,
            local_hash    TEXT,

            cache_state   TEXT NOT NULL DEFAULT 'online',
            provenance    TEXT NOT NULL DEFAULT 'cloud',
            disk_path     TEXT,
            local_path    TEXT,
            cached_at     INTEGER,
            last_access   INTEGER,
            dirty         BOOLEAN NOT NULL DEFAULT FALSE,
            synced_at     INTEGER,

            FOREIGN KEY (backend_id) REFERENCES backends(id)
        );

        CREATE INDEX IF NOT EXISTS idx_files_path ON files(path);
        CREATE INDEX IF NOT EXISTS idx_files_backend ON files(backend_id);
        CREATE INDEX IF NOT EXISTS idx_files_cache_state ON files(cache_state);
        CREATE INDEX IF NOT EXISTS idx_files_last_access ON files(last_access);

        CREATE TABLE IF NOT EXISTS backends (
            id            TEXT PRIMARY KEY,
            backend_type  TEXT NOT NULL,
            display_name  TEXT NOT NULL,
            mount_path    TEXT,
            config        TEXT
        );

        CREATE TABLE IF NOT EXISTS pin_rules (
            id            INTEGER PRIMARY KEY,
            path_glob     TEXT NOT NULL,
            recursive     BOOLEAN NOT NULL DEFAULT TRUE,
            conditions    TEXT
        );

        CREATE UNIQUE INDEX IF NOT EXISTS idx_pin_rules_path ON pin_rules(path_glob);

        CREATE TABLE IF NOT EXISTS lifecycle_policies (
            id            INTEGER PRIMARY KEY,
            path_glob     TEXT NOT NULL,
            max_age       INTEGER,
            max_file_size INTEGER,
            priority      INTEGER NOT NULL DEFAULT 0,
            conditions    TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_lifecycle_priority ON lifecycle_policies(priority DESC);

        CREATE TABLE IF NOT EXISTS config_cache (
            dir_path      TEXT PRIMARY KEY,
            modified_at   INTEGER,
            config        TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS sync_cursors (
            backend_id    TEXT PRIMARY KEY,
            cursor        TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS p2p_peers (
            device_id     TEXT PRIMARY KEY,
            name          TEXT,
            addresses     TEXT,
            last_seen     INTEGER,
            online        BOOLEAN NOT NULL DEFAULT FALSE
        );

        CREATE TABLE IF NOT EXISTS p2p_block_index (
            file_id       TEXT NOT NULL,
            block_index   INTEGER NOT NULL,
            block_hash    BLOB NOT NULL,
            PRIMARY KEY (file_id, block_index)
        );

        CREATE INDEX IF NOT EXISTS idx_block_hash ON p2p_block_index(block_hash);
        ",
    )?;

    Ok(())
}

/// Schema v2 — the node management plane.
///
/// Two tables back the capability model in [`crate::manage`]: `grants` holds
/// the capability grants resolved at authorisation time, and `manage_audit` is
/// an append-only log of every management command the node processed. The
/// audit table has no `UPDATE` or `DELETE` path in the typed API so a
/// compromised manager cannot erase its tracks.
fn v2_manage_plane(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS grants (
            id            INTEGER PRIMARY KEY,
            grantee       TEXT NOT NULL,
            capability    TEXT NOT NULL,
            scope_kind    TEXT NOT NULL,
            scope_path    TEXT,
            granted_by    TEXT NOT NULL,
            expires       INTEGER
        );

        CREATE INDEX IF NOT EXISTS idx_grants_grantee ON grants(grantee);

        CREATE TABLE IF NOT EXISTS manage_audit (
            id            INTEGER PRIMARY KEY,
            timestamp     INTEGER NOT NULL,
            actor_device  TEXT NOT NULL,
            capability    TEXT NOT NULL,
            scope_kind    TEXT NOT NULL,
            scope_path    TEXT,
            command       TEXT NOT NULL,
            outcome       TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_manage_audit_ts ON manage_audit(timestamp);
        ",
    )?;

    Ok(())
}

/// Schema v3 — signed capability tokens.
///
/// Two tables back the portable-grant model in [`crate::manage::token`].
/// `capability_tokens` records every token this node issued, so the owner can
/// list and reprint them; the full signed token is stored as JSON because a
/// token is a self-contained credential the bearer carries offline.
/// `token_revocations` is the append-only revocation list the verify path
/// consults — a token id appearing here is a hard rejection at verify time.
/// Neither table has a typed `DELETE` path: an issued token is a historical
/// fact and a revocation is permanent, so a compromised manager cannot un-issue
/// or un-revoke to cover its tracks. Issue and revoke events are additionally
/// recorded in the existing `manage_audit` log.
fn v3_capability_tokens(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS capability_tokens (
            token_id      TEXT PRIMARY KEY,
            issuer        TEXT NOT NULL,
            bearer        TEXT NOT NULL,
            capability    TEXT NOT NULL,
            scope_kind    TEXT NOT NULL,
            scope_path    TEXT,
            expires       INTEGER NOT NULL,
            issued_at     INTEGER NOT NULL,
            token_json    TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_capability_tokens_bearer ON capability_tokens(bearer);

        CREATE TABLE IF NOT EXISTS token_revocations (
            token_id      TEXT PRIMARY KEY,
            revoked_at    INTEGER NOT NULL
        );
        ",
    )?;

    Ok(())
}

/// Schema v4 — directional data-sharing receive-only quarantine.
///
/// `data_receive_quarantine` holds proposed index rows that arrived from a
/// peer whose `data:write` grant for the folder was absent or expired. Rather
/// than silently discarding the frame (which would hide what the peer is
/// trying to push) or merging it into the authoritative index (which would
/// violate the grant), rejected rows are parked here, keyed by
/// `(folder_id, peer_device, path)`.
///
/// A newer proposal for the same path replaces the older (INSERT OR REPLACE),
/// so the table is bounded by the number of distinct paths per peer per
/// folder — it does not grow without bound. The rows are surfaced to the
/// operator as "rejected local additions from `<peer>`". If the operator later
/// grants `data:write` for that peer, quarantined rows become eligible to
/// merge on the next index exchange (the peer re-sends; the node does not
/// auto-replay the quarantine, so a stale proposal cannot resurrect deleted
/// content). Rows may also be pruned explicitly by the operator.
fn v4_data_receive_quarantine(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS data_receive_quarantine (
            folder_id     TEXT NOT NULL,
            peer_device   TEXT NOT NULL,
            path          TEXT NOT NULL,
            file_json     TEXT NOT NULL,
            observed_at   INTEGER NOT NULL,
            PRIMARY KEY (folder_id, peer_device, path)
        );

        CREATE INDEX IF NOT EXISTS idx_quarantine_folder_peer
            ON data_receive_quarantine(folder_id, peer_device);
        ",
    )?;

    Ok(())
}

/// Schema v5 — explicit-control bit for directional data sharing.
///
/// `data_explicit_control` is the durable backing for the F2 invariant: a
/// peer who has *ever* presented a verified data-verb token for a folder is
/// in explicit-control mode for that folder, even after the token has been
/// revoked or has expired. The runtime data-plane gate consults the bit so
/// the absent direction stays denied; a token-only restriction cannot be
/// widened back to the trusted-peer default by revoking or letting the
/// token lapse.
///
/// The bit is set on the first successful token verify (folded in by the
/// `DataAuthority` impl) and persists across restarts, so a stale
/// restart cannot re-introduce the F2 widening. An explicit
/// `clear_data_explicit_control` row operation — the only path that
/// removes a row — exists for the operator to return the peer to the
/// trusted-peer default after they have removed the underlying grant.
fn v5_data_explicit_control(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS data_explicit_control (
            peer_device   TEXT NOT NULL,
            folder_id     TEXT NOT NULL,
            data_read     BOOLEAN NOT NULL,
            data_write    BOOLEAN NOT NULL,
            observed_at   INTEGER NOT NULL,
            PRIMARY KEY (peer_device, folder_id)
        );
        ",
    )?;

    Ok(())
}
