//! Folder index — persistent file metadata for a P2P backend instance.
//!
//! Stores per-file: path, type (file/dir), size, modified time, the list of
//! content-addressed block hashes that reassemble the file, and a per-file
//! monotonically-increasing version used for change detection and Last-Write-
//! Wins merge with peer Index updates.
//!
//! The index lives in its own `SQLite` database file (one per backend instance)
//! so it can be rebuilt or wiped without touching the main cascade state DB.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

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
    pub version: i64,
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
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let conn =
            Connection::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        conn.execute_batch(
            r"
            CREATE TABLE IF NOT EXISTS files (
                path TEXT PRIMARY KEY NOT NULL,
                is_dir INTEGER NOT NULL,
                size INTEGER NOT NULL DEFAULT 0,
                modified INTEGER NOT NULL DEFAULT 0,
                block_hashes BLOB NOT NULL DEFAULT (x''),
                deleted INTEGER NOT NULL DEFAULT 0,
                version INTEGER NOT NULL DEFAULT 1
            );
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS files_version ON files(version);
            ",
        )
        .context("init schema")?;
        Ok(Self {
            conn: Mutex::new(conn),
            db_path: path.to_path_buf(),
        })
    }

    /// Insert or update an entry. Always bumps the version.
    pub fn upsert(&self, entry: &IndexEntry) -> Result<i64> {
        let next_version = self.next_version()?;
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        conn.execute(
            "INSERT INTO files (path, is_dir, size, modified, block_hashes, deleted, version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(path) DO UPDATE SET
               is_dir = excluded.is_dir,
               size = excluded.size,
               modified = excluded.modified,
               block_hashes = excluded.block_hashes,
               deleted = excluded.deleted,
               version = excluded.version",
            params![
                entry.path,
                i64::from(entry.is_dir),
                i64::try_from(entry.size).unwrap_or(i64::MAX),
                entry.modified,
                entry.block_hashes,
                i64::from(entry.deleted),
                next_version,
            ],
        )?;
        Ok(next_version)
    }

    /// Mark an entry as deleted (tombstone). Returns the new version.
    pub fn mark_deleted(&self, path: &str) -> Result<i64> {
        let next_version = self.next_version()?;
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        conn.execute(
            "UPDATE files SET deleted = 1, version = ?2 WHERE path = ?1",
            params![path, next_version],
        )?;
        Ok(next_version)
    }

    /// Get a single entry by path.
    pub fn get(&self, path: &str) -> Result<Option<IndexEntry>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        let row = conn
            .query_row(
                "SELECT path, is_dir, size, modified, block_hashes, deleted, version
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
            "SELECT path, is_dir, size, modified, block_hashes, deleted, version
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

    /// All entries (including tombstones) with version greater than `since`.
    /// Used to generate Change events from a cursor.
    pub fn entries_since(&self, since: i64) -> Result<Vec<IndexEntry>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT path, is_dir, size, modified, block_hashes, deleted, version
             FROM files WHERE version > ?1 ORDER BY version ASC",
        )?;
        let rows = stmt
            .query_map(params![since], Self::map_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Current max version (cursor value to report after a `changes()` poll).
    pub fn max_version(&self) -> Result<i64> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        let v: Option<i64> = conn
            .query_row("SELECT MAX(version) FROM files", [], |r| r.get(0))
            .optional()?
            .flatten();
        Ok(v.unwrap_or(0))
    }

    fn next_version(&self) -> Result<i64> {
        Ok(self.max_version()? + 1)
    }

    fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexEntry> {
        Ok(IndexEntry {
            path: row.get(0)?,
            is_dir: row.get::<_, i64>(1)? != 0,
            size: row.get::<_, i64>(2)?.try_into().unwrap_or(0),
            modified: row.get(3)?,
            block_hashes: row.get(4)?,
            deleted: row.get::<_, i64>(5)? != 0,
            version: row.get(6)?,
        })
    }
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
            version: 0,
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
        assert_eq!(got.version, 1);
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
}
