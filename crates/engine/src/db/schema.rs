use anyhow::Result;
use rusqlite::Connection;

/// Schema version tracker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SchemaVersion(pub i32);

impl SchemaVersion {
    /// Current schema version.
    #[must_use] pub const fn current() -> Self {
        Self(1)
    }
}

/// Run all migrations from `from` to `to`.
pub fn migrate(conn: &Connection, from: SchemaVersion, _to: SchemaVersion) -> Result<()> {
    if from < SchemaVersion(1) {
        v1_init(conn)?;
    }
    // Future migrations:
    // if to >= SchemaVersion(2) && from < SchemaVersion(2) { v2_xxx(conn)?; }

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
