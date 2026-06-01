//! Folder index — persistent file metadata for a P2P backend instance.
//!
//! Stores per-file: path, type (file/dir), size, modified time, the list of
//! content-addressed block hashes that reassemble the file, a row-level
//! monotonically-increasing counter used for `changes()` cursors, and a
//! per-file version vector (one `(device_short_id, counter)` entry per
//! device that has ever modified the row) used to resolve concurrent
//! edits with peer Index updates.
//!
//! The index lives in its own `SQLite` database file (one per backend instance)
//! so it can be rebuilt or wiped without touching the main cascade state DB.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

/// One row in the folder index.
#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: i64,
    /// Concatenated 32-byte block hashes, in order.
    pub block_hashes: Vec<u8>,
    pub deleted: bool,
    /// Monotonic row sequence number used by the `changes()` cursor.
    /// This is *not* a version vector — see `version`. Bumped on every
    /// upsert and tombstone.
    pub row_version: i64,
    /// Per-file version vector — one `(device_short_id, counter)`
    /// entry per device that has ever modified the row, sorted ascending
    /// by `device_short_id`. Empty for rows that pre-date the version
    /// vector schema migration; new writes always carry at least one
    /// entry (the local device's counter).
    pub version: Vec<(u64, u64)>,
}

/// SQLite-backed folder index.
pub struct FolderIndex {
    conn: Mutex<Connection>,
    db_path: PathBuf,
}

impl std::fmt::Debug for FolderIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FolderIndex")
            .field("db_path", &self.db_path)
            .finish_non_exhaustive()
    }
}

impl FolderIndex {
    /// Open or create an index at the given path.
    ///
    /// New databases get every column from the start. Older databases —
    /// created before the version-vector schema — are migrated in
    /// place: the `version_blob` column is added with an empty default.
    /// A `peer_sequences` table is also created (or added on migration)
    /// so the sync engine can record the highest `FileInfo::sequence`
    /// it has seen from each peer and request only the delta on
    /// reconnect.
    ///
    /// Schema initialisation runs inside a single `BEGIN EXCLUSIVE`
    /// transaction so that two processes opening the same `SQLite` file
    /// concurrently serialise on the migration rather than racing on
    /// `PRAGMA table_info` and `ALTER TABLE`. `PRAGMA user_version`
    /// records the highest applied migration so already-migrated
    /// databases skip the column check on subsequent opens.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let mut conn =
            Connection::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Exclusive)
            .context("begin schema transaction")?;

        // Step 1: base schema. Idempotent — `IF NOT EXISTS` everywhere,
        // so this is a no-op on a database that already has the schema.
        tx.execute_batch(
            r"
            CREATE TABLE IF NOT EXISTS files (
                path TEXT PRIMARY KEY NOT NULL,
                is_dir INTEGER NOT NULL,
                size INTEGER NOT NULL DEFAULT 0,
                modified INTEGER NOT NULL DEFAULT 0,
                block_hashes BLOB NOT NULL DEFAULT (x''),
                deleted INTEGER NOT NULL DEFAULT 0,
                version INTEGER NOT NULL DEFAULT 1,
                version_blob BLOB NOT NULL DEFAULT (x'')
            );
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS peer_sequences (
                peer_device_id TEXT PRIMARY KEY NOT NULL,
                max_sequence INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS files_version ON files(version);
            ",
        )
        .context("init schema")?;

        // Step 2: migrations. `PRAGMA user_version` is the schema
        // version sentinel — each bump represents a one-time migration.
        let current_version: i64 = tx
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .context("read user_version")?;

        if current_version < 1 {
            // Migration 1: add `version_blob` to pre-v0.1.20 databases
            // that were created before the column existed. New
            // databases get the column from the `CREATE TABLE` above
            // and skip the `ALTER`. We still have to check, because
            // `user_version` is 0 in both cases.
            let has_version_blob: bool = tx
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('files') WHERE name = 'version_blob'",
                    [],
                    |r| r.get::<_, i64>(0).map(|n| n > 0),
                )
                .context("probe files.version_blob")?;
            if !has_version_blob {
                tx.execute(
                    "ALTER TABLE files ADD COLUMN version_blob BLOB NOT NULL DEFAULT (x'')",
                    [],
                )
                .context("add files.version_blob")?;
            }
            tx.execute_batch("PRAGMA user_version = 1")
                .context("bump user_version")?;
        }

        if current_version < 2 {
            // Migration 2: add `peer_sequences` for delta-sync tracking.
            // New databases already created the table via the base
            // schema above; the `IF NOT EXISTS` makes the re-create a
            // no-op there. Existing v1 databases pick the table up here.
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS peer_sequences (
                    peer_device_id TEXT PRIMARY KEY NOT NULL,
                    max_sequence INTEGER NOT NULL
                )",
            )
            .context("create peer_sequences table")?;
            tx.execute_batch("PRAGMA user_version = 2")
                .context("bump user_version to 2")?;
        }

        tx.commit().context("commit schema transaction")?;
        Ok(Self {
            conn: Mutex::new(conn),
            db_path: path.to_path_buf(),
        })
    }

    /// Insert or update an entry. Always bumps the row sequence number.
    ///
    /// The per-file `version` vector is persisted verbatim — callers
    /// are responsible for bumping the local device's counter before
    /// calling this when the change originates locally.
    pub fn upsert(&self, entry: &IndexEntry) -> Result<i64> {
        let next_row_version = self.next_row_version()?;
        let version_blob = encode_version_blob(&entry.version);
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        conn.execute(
            "INSERT INTO files (path, is_dir, size, modified, block_hashes, deleted, version, version_blob)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(path) DO UPDATE SET
               is_dir = excluded.is_dir,
               size = excluded.size,
               modified = excluded.modified,
               block_hashes = excluded.block_hashes,
               deleted = excluded.deleted,
               version = excluded.version,
               version_blob = excluded.version_blob",
            params![
                entry.path,
                i64::from(entry.is_dir),
                i64::try_from(entry.size).unwrap_or(i64::MAX),
                entry.modified,
                entry.block_hashes,
                i64::from(entry.deleted),
                next_row_version,
                version_blob,
            ],
        )?;
        Ok(next_row_version)
    }

    /// Mark an entry as deleted (tombstone). Returns the new row
    /// sequence number.
    ///
    /// The row's `modified` column is bumped to the current wall-clock
    /// timestamp. The version vector is *not* mutated here — callers
    /// that originate the delete locally must bump the local device's
    /// counter and call `upsert` instead. This helper exists for
    /// remote-driven deletes where the peer's version vector is
    /// supplied directly through `upsert`.
    pub fn mark_deleted(&self, path: &str) -> Result<i64> {
        let next_row_version = self.next_row_version()?;
        let now = chrono::Utc::now().timestamp();
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        conn.execute(
            "UPDATE files SET deleted = 1, modified = ?2, version = ?3 WHERE path = ?1",
            params![path, now, next_row_version],
        )?;
        Ok(next_row_version)
    }

    /// Get a single entry by path.
    pub fn get(&self, path: &str) -> Result<Option<IndexEntry>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        let row = conn
            .query_row(
                "SELECT path, is_dir, size, modified, block_hashes, deleted, version, version_blob
                 FROM files WHERE path = ?1",
                params![path],
                Self::map_row,
            )
            .optional()?;
        Ok(row)
    }

    /// List direct children of a parent path (no recursion).
    ///
    /// Pass `""` for the root.
    pub fn list_children(&self, parent: &str) -> Result<Vec<IndexEntry>> {
        let prefix = if parent.is_empty() {
            String::new()
        } else {
            format!("{parent}/")
        };
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT path, is_dir, size, modified, block_hashes, deleted, version, version_blob
             FROM files
             WHERE path LIKE ?1 || '%'
               AND deleted = 0
               AND instr(substr(path, length(?1) + 1), '/') = 0
               AND length(path) > length(?1)",
        )?;
        let rows = stmt
            .query_map(params![prefix], Self::map_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// All entries (including tombstones) with row sequence greater
    /// than `since`. Used to generate Change events from a cursor.
    pub fn entries_since(&self, since: i64) -> Result<Vec<IndexEntry>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT path, is_dir, size, modified, block_hashes, deleted, version, version_blob
             FROM files WHERE version > ?1 ORDER BY version ASC",
        )?;
        let rows = stmt
            .query_map(params![since], Self::map_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Current max row sequence number (cursor value to report after a
    /// `changes()` poll).
    pub fn max_version(&self) -> Result<i64> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        let v: Option<i64> = conn
            .query_row("SELECT MAX(version) FROM files", [], |r| r.get(0))
            .optional()?
            .flatten();
        Ok(v.unwrap_or(0))
    }

    /// Return the highest [`FileInfo::sequence`](cascade_p2p::protocol::FileInfo::sequence)
    /// we have ever received from `peer_device_id`, or `0` if we have
    /// never recorded anything for that peer.
    ///
    /// The sync engine consults this on reconnect to send only entries
    /// whose `row_version` exceeds the value, avoiding a full Index
    /// re-enumeration on every session.
    pub fn get_peer_max_sequence(&self, peer_device_id: &str) -> Result<u64> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        let value: Option<i64> = conn
            .query_row(
                "SELECT max_sequence FROM peer_sequences WHERE peer_device_id = ?1",
                params![peer_device_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(value.map_or(0, |v| u64::try_from(v).unwrap_or(0)))
    }

    /// Record the highest [`FileInfo::sequence`](cascade_p2p::protocol::FileInfo::sequence)
    /// we have received from `peer_device_id`.
    ///
    /// Always overwrites the stored value with `max_sequence`; the
    /// caller is responsible for combining successive observations with
    /// the previous value (e.g. by taking `max(prev, observed)`) before
    /// calling.
    pub fn set_peer_max_sequence(&self, peer_device_id: &str, max_sequence: u64) -> Result<()> {
        let stored = i64::try_from(max_sequence).unwrap_or(i64::MAX);
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        conn.execute(
            "INSERT INTO peer_sequences (peer_device_id, max_sequence)
             VALUES (?1, ?2)
             ON CONFLICT(peer_device_id) DO UPDATE SET max_sequence = excluded.max_sequence",
            params![peer_device_id, stored],
        )?;
        Ok(())
    }

    fn next_row_version(&self) -> Result<i64> {
        Ok(self.max_version()? + 1)
    }

    fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexEntry> {
        let blob: Vec<u8> = row.get(7)?;
        let version = decode_version_blob(&blob).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Blob, e.into())
        })?;
        Ok(IndexEntry {
            path: row.get(0)?,
            is_dir: row.get::<_, i64>(1)? != 0,
            size: row.get::<_, i64>(2)?.try_into().unwrap_or(0),
            modified: row.get(3)?,
            block_hashes: row.get(4)?,
            deleted: row.get::<_, i64>(5)? != 0,
            row_version: row.get(6)?,
            version,
        })
    }
}

/// Encode a version vector as a flat blob: pairs of 8-byte big-endian
/// (`device_short_id`, counter), in stored order.
fn encode_version_blob(version: &[(u64, u64)]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(version.len() * 16);
    for (id, ctr) in version {
        buf.extend_from_slice(&id.to_be_bytes());
        buf.extend_from_slice(&ctr.to_be_bytes());
    }
    buf
}

/// Decode the on-disk version vector blob. An empty blob is a valid
/// empty vector (used for rows that pre-date the schema migration).
fn decode_version_blob(blob: &[u8]) -> Result<Vec<(u64, u64)>> {
    if blob.len() % 16 != 0 {
        anyhow::bail!(
            "version_blob has length {}, not a multiple of 16",
            blob.len()
        );
    }
    let mut out = Vec::with_capacity(blob.len() / 16);
    for chunk in blob.chunks_exact(16) {
        let (id_bytes, ctr_bytes) = chunk.split_at(8);
        let mut id_arr = [0u8; 8];
        let mut ctr_arr = [0u8; 8];
        id_arr.copy_from_slice(id_bytes);
        ctr_arr.copy_from_slice(ctr_bytes);
        out.push((u64::from_be_bytes(id_arr), u64::from_be_bytes(ctr_arr)));
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn entry(path: &str, is_dir: bool, size: u64) -> IndexEntry {
        IndexEntry {
            path: path.to_string(),
            is_dir,
            size,
            modified: 0,
            block_hashes: Vec::new(),
            deleted: false,
            row_version: 0,
            version: Vec::new(),
        }
    }

    #[test]
    fn upsert_and_get() {
        let dir = tempdir().unwrap();
        let idx = FolderIndex::open(&dir.path().join("test.db")).unwrap();
        let v = idx.upsert(&entry("foo.txt", false, 42)).unwrap();
        assert_eq!(v, 1);
        let got = idx.get("foo.txt").unwrap().unwrap();
        assert_eq!(got.size, 42);
        assert!(!got.is_dir);
        assert_eq!(got.row_version, 1);
    }

    #[test]
    fn upsert_preserves_version_vector() {
        let dir = tempdir().unwrap();
        let idx = FolderIndex::open(&dir.path().join("vv.db")).unwrap();
        let mut e = entry("doc.txt", false, 1);
        e.version = vec![(7, 3), (42, 1)];
        idx.upsert(&e).unwrap();
        let got = idx.get("doc.txt").unwrap().unwrap();
        assert_eq!(got.version, vec![(7, 3), (42, 1)]);
    }

    #[test]
    fn open_migrates_pre_version_blob_database() {
        // Build a database with the *old* schema (no version_blob), then
        // reopen via FolderIndex::open and confirm the column is added
        // with an empty default for the existing row.
        let dir = tempdir().unwrap();
        let path = dir.path().join("legacy.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE files (
                    path TEXT PRIMARY KEY NOT NULL,
                    is_dir INTEGER NOT NULL,
                    size INTEGER NOT NULL DEFAULT 0,
                    modified INTEGER NOT NULL DEFAULT 0,
                    block_hashes BLOB NOT NULL DEFAULT (x''),
                    deleted INTEGER NOT NULL DEFAULT 0,
                    version INTEGER NOT NULL DEFAULT 1
                );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO files (path, is_dir, size, modified, deleted, version)
                 VALUES ('legacy.txt', 0, 99, 1700000000, 0, 1)",
                [],
            )
            .unwrap();
        }
        let idx = FolderIndex::open(&path).unwrap();
        let row = idx.get("legacy.txt").unwrap().unwrap();
        assert_eq!(row.size, 99);
        assert!(row.version.is_empty(), "legacy row has empty vector");
    }

    #[test]
    fn migration_is_idempotent_and_bumps_user_version() {
        // Hand-craft a database with the pre-v0.1.20 schema (no
        // `version_blob` column) and confirm:
        //   1. The first `FolderIndex::open` migrates it and sets
        //      `PRAGMA user_version` to 1.
        //   2. A second open is a no-op — it does not attempt the
        //      `ALTER TABLE` again. (If `user_version` weren't being
        //      consulted, the column check still saves us, but tracking
        //      the version is what makes the migration cheap to skip
        //      and safe under concurrent open contention.)
        //
        // This is the regression test for the `BEGIN EXCLUSIVE` +
        // `PRAGMA user_version` change: under the previous code, a
        // second concurrent open could race on the `ALTER TABLE` and
        // fail with `duplicate column name`. We can't easily simulate
        // process-level contention from a unit test, but we can prove
        // the migration leaves the database in a state where subsequent
        // opens skip the alter entirely.
        let dir = tempdir().unwrap();
        let path = dir.path().join("legacy.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE files (
                    path TEXT PRIMARY KEY NOT NULL,
                    is_dir INTEGER NOT NULL,
                    size INTEGER NOT NULL DEFAULT 0,
                    modified INTEGER NOT NULL DEFAULT 0,
                    block_hashes BLOB NOT NULL DEFAULT (x''),
                    deleted INTEGER NOT NULL DEFAULT 0,
                    version INTEGER NOT NULL DEFAULT 1
                );",
            )
            .unwrap();
        }

        // First open — migration runs.
        drop(FolderIndex::open(&path).unwrap());

        // Inspect the database directly: `user_version` must reflect
        // the latest applied migration (currently 2 — peer_sequences)
        // and `version_blob` must exist.
        let probe = Connection::open(&path).unwrap();
        let user_version: i64 = probe
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(user_version, 2, "migration should bump user_version");
        let has_column: i64 = probe
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('files') WHERE name = 'version_blob'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_column, 1, "version_blob column should exist");
        drop(probe);

        // Second open — must succeed without attempting the ALTER again.
        // If `BEGIN EXCLUSIVE` weren't wrapping the migration, two
        // racing processes could both hit `ALTER TABLE ... ADD COLUMN
        // version_blob` and one would error with `duplicate column
        // name`. The user_version guard short-circuits that probe.
        drop(FolderIndex::open(&path).unwrap());
    }

    #[test]
    fn list_children_one_level_only() {
        let dir = tempdir().unwrap();
        let idx = FolderIndex::open(&dir.path().join("t.db")).unwrap();
        idx.upsert(&entry("a", true, 0)).unwrap();
        idx.upsert(&entry("a/b.txt", false, 1)).unwrap();
        idx.upsert(&entry("a/c", true, 0)).unwrap();
        idx.upsert(&entry("a/c/deep.txt", false, 1)).unwrap();
        idx.upsert(&entry("other.txt", false, 1)).unwrap();

        let root = idx.list_children("").unwrap();
        let root_names: Vec<_> = root.iter().map(|e| e.path.clone()).collect();
        assert!(root_names.contains(&"a".to_string()));
        assert!(root_names.contains(&"other.txt".to_string()));
        assert!(!root_names.contains(&"a/b.txt".to_string()));

        let in_a = idx.list_children("a").unwrap();
        let a_names: Vec<_> = in_a.iter().map(|e| e.path.clone()).collect();
        assert!(a_names.contains(&"a/b.txt".to_string()));
        assert!(a_names.contains(&"a/c".to_string()));
        assert!(!a_names.contains(&"a/c/deep.txt".to_string()));
    }

    #[test]
    fn deleted_entries_excluded_from_list() {
        let dir = tempdir().unwrap();
        let idx = FolderIndex::open(&dir.path().join("t.db")).unwrap();
        idx.upsert(&entry("foo.txt", false, 1)).unwrap();
        idx.mark_deleted("foo.txt").unwrap();
        let root = idx.list_children("").unwrap();
        assert!(root.is_empty());
        // But entries_since should still include the tombstone.
        let since = idx.entries_since(0).unwrap();
        assert!(since.iter().any(|e| e.deleted));
    }

    #[test]
    fn entries_since_returns_only_newer() {
        let dir = tempdir().unwrap();
        let idx = FolderIndex::open(&dir.path().join("t.db")).unwrap();
        idx.upsert(&entry("a", false, 1)).unwrap(); // v=1
        idx.upsert(&entry("b", false, 1)).unwrap(); // v=2
        let after = idx.upsert(&entry("c", false, 1)).unwrap(); // v=3
        assert_eq!(after, 3);
        let since_two = idx.entries_since(2).unwrap();
        assert_eq!(since_two.len(), 1);
        assert_eq!(since_two[0].path, "c");
    }

    #[test]
    fn peer_max_sequence_defaults_to_zero_for_unknown_peer() {
        let dir = tempdir().unwrap();
        let idx = FolderIndex::open(&dir.path().join("seq.db")).unwrap();
        assert_eq!(idx.get_peer_max_sequence("unknown-device").unwrap(), 0);
    }

    #[test]
    fn peer_max_sequence_round_trip() {
        let dir = tempdir().unwrap();
        let idx = FolderIndex::open(&dir.path().join("seq.db")).unwrap();
        idx.set_peer_max_sequence("peer-a", 17).unwrap();
        idx.set_peer_max_sequence("peer-b", 42).unwrap();
        assert_eq!(idx.get_peer_max_sequence("peer-a").unwrap(), 17);
        assert_eq!(idx.get_peer_max_sequence("peer-b").unwrap(), 42);
    }

    #[test]
    fn peer_max_sequence_overwrites_previous_value() {
        let dir = tempdir().unwrap();
        let idx = FolderIndex::open(&dir.path().join("seq.db")).unwrap();
        idx.set_peer_max_sequence("peer-a", 5).unwrap();
        idx.set_peer_max_sequence("peer-a", 9).unwrap();
        assert_eq!(idx.get_peer_max_sequence("peer-a").unwrap(), 9);
    }

    #[test]
    fn migration_two_creates_peer_sequences_on_legacy_database() {
        // A database that completed migration 1 (version_blob present,
        // user_version = 1) but predates migration 2 should pick up the
        // `peer_sequences` table on the next open.
        let dir = tempdir().unwrap();
        let path = dir.path().join("legacy_v1.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE files (
                    path TEXT PRIMARY KEY NOT NULL,
                    is_dir INTEGER NOT NULL,
                    size INTEGER NOT NULL DEFAULT 0,
                    modified INTEGER NOT NULL DEFAULT 0,
                    block_hashes BLOB NOT NULL DEFAULT (x''),
                    deleted INTEGER NOT NULL DEFAULT 0,
                    version INTEGER NOT NULL DEFAULT 1,
                    version_blob BLOB NOT NULL DEFAULT (x'')
                );
                PRAGMA user_version = 1;",
            )
            .unwrap();
        }

        let idx = FolderIndex::open(&path).unwrap();
        // The table must now exist and be usable.
        idx.set_peer_max_sequence("peer-a", 11).unwrap();
        assert_eq!(idx.get_peer_max_sequence("peer-a").unwrap(), 11);

        // user_version should have advanced to 2.
        let probe = Connection::open(&path).unwrap();
        let user_version: i64 = probe
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(user_version, 2);
    }
}
