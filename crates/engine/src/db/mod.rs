#[cfg(feature = "native")]
pub mod schema;

#[cfg(feature = "native")]
use crate::db::schema::SchemaVersion;
#[cfg(feature = "p2p")]
use crate::manage::token::CapabilityToken;
use crate::manage::{Capability, DeviceId, Grant, Scope};
use crate::types::ItemId;
#[cfg(feature = "native")]
use crate::types::{CacheState, Cursor, FileEntry};
#[cfg(feature = "native")]
use anyhow::Result;
use chrono::{DateTime, Utc};
#[cfg(feature = "native")]
use rusqlite::{Connection, OptionalExtension};
#[cfg(feature = "native")]
use std::path::Path;
#[cfg(feature = "native")]
use std::sync::Mutex;

// ───────────── Native-only: StateDb (rusqlite-backed) ─────────────

#[cfg(feature = "native")]
/// Cap a logical `u64` size into the `i64` range `SQLite` stores integers in,
/// for binding via `Option::map`. A real file never exceeds `i64::MAX` bytes
/// (8 EiB), so the saturation point is unreachable; this exists only because
/// rusqlite 0.40 dropped the `u64` `ToSql` impl to prevent silent truncation.
fn size_to_sql(size: u64) -> i64 {
    i64::try_from(size).unwrap_or(i64::MAX)
}

#[cfg(feature = "native")]
/// Read an optional `size` column back as `u64`. The column is stored as
/// `i64` (see [`size_to_sql`]); a negative value is never written, so it
/// clamps to 0. Mirrors the inline `Option<i64>` round-trip the
/// `mod_time` column already uses.
fn size_from_row(row: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<Option<u64>> {
    Ok(row
        .get::<_, Option<i64>>(idx)?
        .map(|s| u64::try_from(s).unwrap_or(0)))
}

#[cfg(feature = "native")]
/// `SQLite` state database. Stores file metadata, backend config,
/// pin rules, lifecycle policies, config cache, sync cursors, and P2P state.
pub struct StateDb {
    conn: Mutex<Connection>,
}

#[cfg(feature = "native")]
impl std::fmt::Debug for StateDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateDb").finish_non_exhaustive()
    }
}

#[cfg(feature = "native")]
impl StateDb {
    /// Open (or create) the state database at the given path.
    /// Creates the directory and initialises the schema if needed.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;

        let db = Self {
            conn: Mutex::new(conn),
        };
        db.migrate()?;

        Ok(db)
    }

    /// Open an in-memory database (for testing).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;

        let db = Self {
            conn: Mutex::new(conn),
        };
        db.migrate()?;

        Ok(db)
    }

    /// Run schema migrations.
    fn migrate(&self) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;

        let current_version = Self::get_version(&conn);
        let target_version = SchemaVersion::current();

        if current_version < target_version {
            schema::migrate(&conn, current_version, target_version)?;
            Self::set_version(&conn, target_version)?;
        }

        Ok(())
    }

    fn get_version(conn: &Connection) -> SchemaVersion {
        let version: i32 = conn
            .query_row(
                "SELECT value FROM schema_meta WHERE key = 'version'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        SchemaVersion(version)
    }

    fn set_version(conn: &Connection, version: SchemaVersion) -> Result<()> {
        conn.execute(
            "CREATE TABLE IF NOT EXISTS schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            [],
        )?;
        conn.execute(
            "INSERT OR REPLACE INTO schema_meta (key, value) VALUES ('version', ?1)",
            [&version.0.to_string()],
        )?;
        Ok(())
    }

    // ── File operations ──

    /// Upsert a file entry into the database.
    ///
    /// The `files.path` column is set to `entry.path` if non-empty, or falls
    /// back to `entry.name`. At this phase the two are always equal; future
    /// phases assemble a mount-prefixed VFS path and store it here.
    pub fn upsert_file(&self, entry: &FileEntry) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        // Use the explicit VFS path when available; fall back to the bare name
        // for entries whose path has not yet been resolved (backwards-compatible
        // during the transition phases).
        let vfs_path = if entry.path.is_empty() {
            &entry.name
        } else {
            &entry.path
        };
        conn.execute(
            "INSERT OR REPLACE INTO files (
                id, backend_id, path, parent_id, name, is_dir, size,
                mime_type, mod_time, remote_hash, cache_state, provenance
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            (
                &entry.id.0,
                entry.id.backend_id(),
                vfs_path,
                &entry.parent_id.0,
                &entry.name,
                entry.is_dir,
                entry.size.map(size_to_sql),
                &entry.mime_type,
                entry.mod_time.map(|t| t.timestamp()),
                &entry.hash,
                "online",
                "cloud",
            ),
        )?;
        Ok(())
    }

    /// Get a file entry by ID.
    pub fn get_file(&self, id: &ItemId) -> Result<Option<FileEntry>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, parent_id, name, path, is_dir, size, mime_type, mod_time, remote_hash
             FROM files WHERE id = ?1",
        )?;

        let result = stmt.query_row([&id.0], |row| {
            Ok(FileEntry {
                id: ItemId(row.get(0)?),
                parent_id: ItemId(row.get(1)?),
                name: row.get(2)?,
                path: row.get(3)?,
                is_dir: row.get(4)?,
                size: size_from_row(row, 5)?,
                mime_type: row.get(6)?,
                mod_time: row
                    .get::<_, Option<i64>>(7)?
                    .map(|ts| chrono::DateTime::from_timestamp(ts, 0).unwrap_or_default()),
                hash: row.get(8)?,
            })
        });

        match result {
            Ok(entry) => Ok(Some(entry)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Look up the stored VFS path for a file by its [`ItemId`].
    ///
    /// Returns `None` when the file is not in the database. The returned string
    /// is the `files.path` column value — the full VFS-absolute, mount-prefixed
    /// path assembled by the sync runner.
    pub fn get_file_path(&self, id: &ItemId) -> Result<Option<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let result = conn
            .query_row("SELECT path FROM files WHERE id = ?1", [&id.0], |row| {
                row.get::<_, String>(0)
            })
            .ok();
        Ok(result)
    }

    /// Delete a file entry by ID.
    pub fn delete_file(&self, id: &ItemId) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        conn.execute("DELETE FROM files WHERE id = ?1", [&id.0])?;
        Ok(())
    }

    /// Delete a file or directory and every descendant in one statement.
    pub fn delete_subtree(&self, root_id: &ItemId) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        conn.execute(
            "WITH RECURSIVE subtree(id) AS (
                SELECT id FROM files WHERE id = ?1
                UNION ALL
                SELECT f.id FROM files f INNER JOIN subtree s ON f.parent_id = s.id
             )
             DELETE FROM files WHERE id IN (SELECT id FROM subtree)",
            [&root_id.0],
        )?;
        Ok(())
    }

    /// Update the cache state of a file.
    pub fn update_cache_state(&self, id: &ItemId, state: CacheState) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let now = chrono::Utc::now().timestamp();
        conn.execute(
            "UPDATE files SET cache_state = ?1, last_access = ?2 WHERE id = ?3",
            (state.as_str(), now, &id.0),
        )?;
        Ok(())
    }

    /// Get the cache state of a file.
    pub fn get_cache_state(&self, id: &ItemId) -> Result<Option<CacheState>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let result: Option<String> = conn
            .query_row(
                "SELECT cache_state FROM files WHERE id = ?1",
                [&id.0],
                |row| row.get(0),
            )
            .ok();

        match result {
            Some(s) => Ok(Some(s.parse()?)),
            None => Ok(None),
        }
    }

    // ── Sync cursor operations ──

    /// Store a sync cursor for a backend.
    pub fn set_cursor(&self, backend_id: &str, cursor: &Cursor) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        conn.execute(
            "INSERT OR REPLACE INTO sync_cursors (backend_id, cursor) VALUES (?1, ?2)",
            (backend_id, &cursor.0),
        )?;
        Ok(())
    }

    /// Get the sync cursor for a backend.
    pub fn get_cursor(&self, backend_id: &str) -> Result<Option<Cursor>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let result = conn
            .query_row(
                "SELECT cursor FROM sync_cursors WHERE backend_id = ?1",
                [backend_id],
                |row| row.get::<_, String>(0),
            )
            .ok();
        Ok(result.map(Cursor))
    }

    // ── Backend registration ──

    /// Register a backend in the database.
    pub fn register_backend(
        &self,
        id: &str,
        backend_type: &str,
        display_name: &str,
        mount_path: Option<&str>,
        config: Option<&str>,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        conn.execute(
            "INSERT OR REPLACE INTO backends (id, backend_type, display_name, mount_path, config)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (id, backend_type, display_name, mount_path, config),
        )?;
        Ok(())
    }

    /// Remove a registered backend by ID. Returns `true` if a row was deleted.
    pub fn remove_backend(&self, id: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let rows = conn.execute("DELETE FROM backends WHERE id = ?1", [id])?;
        Ok(rows > 0)
    }

    /// List all registered backends.
    pub fn list_backends(&self) -> Result<Vec<BackendRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let mut stmt = conn
            .prepare("SELECT id, backend_type, display_name, mount_path, config FROM backends")?;

        let records = stmt
            .query_map([], |row| {
                Ok(BackendRecord {
                    id: row.get(0)?,
                    backend_type: row.get(1)?,
                    display_name: row.get(2)?,
                    mount_path: row.get(3)?,
                    config: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(records)
    }

    // ── Pin rule operations ──

    /// Add a pin rule.
    pub fn add_pin_rule(
        &self,
        path_glob: &str,
        recursive: bool,
        conditions: Option<&str>,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        conn.execute(
            "INSERT OR REPLACE INTO pin_rules (path_glob, recursive, conditions) VALUES (?1, ?2, ?3)",
            (path_glob, recursive, conditions),
        )?;
        Ok(())
    }

    /// Remove a pin rule by path glob.
    pub fn remove_pin_rule(&self, path_glob: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let rows = conn.execute("DELETE FROM pin_rules WHERE path_glob = ?1", [path_glob])?;
        Ok(rows > 0)
    }

    /// List all pin rules.
    pub fn list_pin_rules(&self) -> Result<Vec<PinRuleRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let mut stmt =
            conn.prepare("SELECT id, path_glob, recursive, conditions FROM pin_rules")?;
        let rules = stmt
            .query_map([], |row| {
                Ok(PinRuleRecord {
                    id: row.get(0)?,
                    path_glob: row.get(1)?,
                    recursive: row.get(2)?,
                    conditions: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rules)
    }

    // ── Lifecycle policy operations ──

    /// Add a lifecycle policy.
    pub fn add_lifecycle_policy(
        &self,
        path_glob: &str,
        max_age: Option<i64>,
        max_file_size: Option<i64>,
        priority: i32,
        conditions: Option<&str>,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        conn.execute(
            "INSERT INTO lifecycle_policies (path_glob, max_age, max_file_size, priority, conditions)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (path_glob, max_age, max_file_size, priority, conditions),
        )?;
        Ok(())
    }

    /// List all lifecycle policies ordered by priority descending.
    pub fn list_lifecycle_policies(&self) -> Result<Vec<LifecyclePolicyRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, path_glob, max_age, max_file_size, priority, conditions
             FROM lifecycle_policies ORDER BY priority DESC",
        )?;
        let policies = stmt
            .query_map([], |row| {
                Ok(LifecyclePolicyRecord {
                    id: row.get(0)?,
                    path_glob: row.get(1)?,
                    max_age: row.get(2)?,
                    max_file_size: row.get(3)?,
                    priority: row.get(4)?,
                    conditions: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(policies)
    }

    /// Remove a lifecycle policy by ID.
    pub fn remove_lifecycle_policy(&self, id: i64) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let rows = conn.execute("DELETE FROM lifecycle_policies WHERE id = ?1", [id])?;
        Ok(rows > 0)
    }

    // ── Cache queries ──

    /// List all files in a given cache state.
    pub fn list_files_by_cache_state(&self, state: CacheState) -> Result<Vec<FileEntry>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, parent_id, name, path, is_dir, size, mime_type, mod_time, remote_hash
             FROM files WHERE cache_state = ?1",
        )?;
        let entries = stmt
            .query_map([state.as_str()], |row| {
                Ok(FileEntry {
                    id: ItemId(row.get(0)?),
                    parent_id: ItemId(row.get(1)?),
                    name: row.get(2)?,
                    path: row.get(3)?,
                    is_dir: row.get(4)?,
                    size: size_from_row(row, 5)?,
                    mime_type: row.get(6)?,
                    mod_time: row
                        .get::<_, Option<i64>>(7)?
                        .map(|ts| chrono::DateTime::from_timestamp(ts, 0).unwrap_or_default()),
                    hash: row.get(8)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(entries)
    }

    /// List all files in the state database.
    pub fn list_all_files(&self) -> Result<Vec<FileEntry>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, parent_id, name, path, is_dir, size, mime_type, mod_time, remote_hash
             FROM files",
        )?;
        let entries = stmt
            .query_map([], |row| {
                Ok(FileEntry {
                    id: ItemId(row.get(0)?),
                    parent_id: ItemId(row.get(1)?),
                    name: row.get(2)?,
                    path: row.get(3)?,
                    is_dir: row.get(4)?,
                    size: size_from_row(row, 5)?,
                    mime_type: row.get(6)?,
                    mod_time: row
                        .get::<_, Option<i64>>(7)?
                        .map(|ts| chrono::DateTime::from_timestamp(ts, 0).unwrap_or_default()),
                    hash: row.get(8)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(entries)
    }

    /// List all file entries whose parent matches the given [`ItemId`] string.
    pub fn list_children(&self, parent_id: &str) -> Result<Vec<FileEntry>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, parent_id, name, path, is_dir, size, mime_type, mod_time, remote_hash
             FROM files WHERE parent_id = ?1",
        )?;
        let entries = stmt
            .query_map([&parent_id], |row| {
                Ok(FileEntry {
                    id: ItemId(row.get(0)?),
                    parent_id: ItemId(row.get(1)?),
                    name: row.get(2)?,
                    path: row.get(3)?,
                    is_dir: row.get(4)?,
                    size: size_from_row(row, 5)?,
                    mime_type: row.get(6)?,
                    mod_time: row
                        .get::<_, Option<i64>>(7)?
                        .map(|ts| chrono::DateTime::from_timestamp(ts, 0).unwrap_or_default()),
                    hash: row.get(8)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(entries)
    }

    /// Get total cache size (sum of sizes of cached/pinned files).
    pub fn cache_size(&self) -> Result<i64> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let size: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(size), 0) FROM files WHERE cache_state IN ('cached', 'pinned')",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        Ok(size)
    }

    // ── Dirty file operations ──

    /// Mark a file as dirty (locally modified, pending upload).
    pub fn mark_dirty(&self, id: &ItemId) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        conn.execute("UPDATE files SET dirty = TRUE WHERE id = ?1", [&id.0])?;
        Ok(())
    }

    /// Clear the dirty flag for a file (upload succeeded).
    pub fn clear_dirty(&self, id: &ItemId) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        conn.execute("UPDATE files SET dirty = FALSE WHERE id = ?1", [&id.0])?;
        Ok(())
    }

    /// Set the local (on-disk) path and VFS path for a file.
    /// Used by the cache layer when a file is materialised to disk.
    pub fn set_file_paths(&self, id: &ItemId, path: &str, local_path: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        conn.execute(
            "UPDATE files SET path = ?1, local_path = ?2 WHERE id = ?3",
            (path, local_path, &id.0),
        )?;
        Ok(())
    }

    /// Check whether a file is marked dirty.
    pub fn is_dirty(&self, id: &ItemId) -> Result<Option<bool>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let result = conn
            .query_row("SELECT dirty FROM files WHERE id = ?1", [&id.0], |row| {
                row.get::<_, bool>(0)
            })
            .ok();
        Ok(result)
    }

    /// List all files with `dirty = TRUE`, ordered by path for deterministic processing.
    pub fn list_dirty_files(&self) -> Result<Vec<DirtyFileRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, backend_id, path, parent_id, name, is_dir, size,
                    mime_type, mod_time, remote_hash, local_path
             FROM files WHERE dirty = TRUE
             ORDER BY path ASC",
        )?;
        let records = stmt
            .query_map([], |row| {
                Ok(DirtyFileRecord {
                    id: ItemId(row.get(0)?),
                    backend_id: row.get(1)?,
                    path: row.get(2)?,
                    parent_id: ItemId(row.get(3)?),
                    name: row.get(4)?,
                    is_dir: row.get(5)?,
                    size: size_from_row(row, 6)?,
                    mime_type: row.get(7)?,
                    mod_time: row
                        .get::<_, Option<i64>>(8)?
                        .map(|ts| chrono::DateTime::from_timestamp(ts, 0).unwrap_or_default()),
                    remote_hash: row.get(9)?,
                    local_path: row.get(10)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(records)
    }

    /// Find eviction candidates: cached (not pinned) files ordered by `last_access` ascending (LRU).
    pub fn eviction_candidates(&self, limit: usize) -> Result<Vec<FileEntry>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, parent_id, name, path, is_dir, size, mime_type, mod_time, remote_hash
             FROM files
             WHERE cache_state = 'cached' AND dirty = FALSE
             ORDER BY last_access ASC
             LIMIT ?1",
        )?;
        let entries = stmt
            .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
                Ok(FileEntry {
                    id: ItemId(row.get(0)?),
                    parent_id: ItemId(row.get(1)?),
                    name: row.get(2)?,
                    path: row.get(3)?,
                    is_dir: row.get(4)?,
                    size: size_from_row(row, 5)?,
                    mime_type: row.get(6)?,
                    mod_time: row
                        .get::<_, Option<i64>>(7)?
                        .map(|ts| chrono::DateTime::from_timestamp(ts, 0).unwrap_or_default()),
                    hash: row.get(8)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(entries)
    }

    // ── P2P operations ──

    /// Store block index for a file. Each block hash is recorded with its
    /// ordinal position so blocks can be looked up in order.
    pub fn index_p2p_blocks(&self, file_id: &ItemId, block_hashes: &[[u8; 32]]) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        // Remove any previous index for this file.
        conn.execute(
            "DELETE FROM p2p_block_index WHERE file_id = ?1",
            [&file_id.0],
        )?;
        for (index, hash) in block_hashes.iter().enumerate() {
            let block_index =
                i64::try_from(index).map_err(|e| anyhow::anyhow!("block index overflow: {e}"))?;
            conn.execute(
                "INSERT INTO p2p_block_index (file_id, block_index, block_hash) VALUES (?1, ?2, ?3)",
                (&file_id.0, block_index, hash.as_slice()),
            )?;
        }
        Ok(())
    }

    /// Get block hashes for a file, in ordinal order.
    pub fn get_p2p_blocks(&self, file_id: &ItemId) -> Result<Vec<[u8; 32]>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT block_hash FROM p2p_block_index WHERE file_id = ?1 ORDER BY block_index ASC",
        )?;
        let hashes = stmt
            .query_map([&file_id.0], |row| {
                let hash_blob: Vec<u8> = row.get(0)?;
                let arr: [u8; 32] = hash_blob.try_into().unwrap_or([0u8; 32]);
                Ok(arr)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(hashes)
    }

    /// Store or update a known peer.
    pub fn upsert_peer(
        &self,
        device_id: &str,
        address: &str,
        last_seen: DateTime<Utc>,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        conn.execute(
            "INSERT OR REPLACE INTO p2p_peers (device_id, addresses, last_seen, online) VALUES (?1, ?2, ?3, TRUE)",
            (device_id, address, last_seen.timestamp()),
        )?;
        Ok(())
    }

    /// List all known peers.
    pub fn list_peers(&self) -> Result<Vec<PeerRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let mut stmt =
            conn.prepare("SELECT device_id, name, addresses, last_seen, online FROM p2p_peers")?;
        let peers = stmt
            .query_map([], |row| {
                let last_seen_ts: Option<i64> = row.get(3)?;
                Ok(PeerRecord {
                    device_id: row.get(0)?,
                    name: row.get(1)?,
                    addresses: row.get(2)?,
                    last_seen: last_seen_ts.and_then(|ts| DateTime::from_timestamp(ts, 0)),
                    online: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(peers)
    }

    // ── Management-plane grant operations ──

    /// Insert a capability grant. Returns the row id assigned by `SQLite`.
    pub fn insert_grant(&self, grant: &Grant) -> Result<i64> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let (scope_kind, scope_path) = grant.scope.to_columns();
        conn.execute(
            "INSERT INTO grants (grantee, capability, scope_kind, scope_path, granted_by, expires)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            (
                grant.grantee.as_str(),
                grant.capability.as_wire(),
                scope_kind,
                scope_path,
                grant.granted_by.as_str(),
                grant.expires.map(|e| e.timestamp()),
            ),
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// List every grant currently held, in insertion order.
    pub fn list_grants(&self) -> Result<Vec<GrantRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, grantee, capability, scope_kind, scope_path, granted_by, expires
             FROM grants ORDER BY id ASC",
        )?;
        let rows = stmt
            .query_map([], Self::grant_record_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(GrantRecord::try_from_raw).collect()
    }

    /// The stored [`Scope`] of the grant with row id `id`, or `None` when no
    /// such grant exists.
    ///
    /// Used by the management-plane dispatcher to authorise a `GrantRevoke`
    /// against the scope of the row it will actually delete, rather than a
    /// caller-advertised wire scope.
    pub fn grant_scope(&self, id: i64) -> Result<Option<Scope>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let row = conn.query_row(
            "SELECT scope_kind, scope_path FROM grants WHERE id = ?1",
            [id],
            |row| {
                let kind: String = row.get(0)?;
                let path: Option<String> = row.get(1)?;
                Ok((kind, path))
            },
        );
        match row {
            Ok((kind, path)) => {
                let scope = Scope::from_columns(&kind, path)
                    .ok_or_else(|| anyhow::anyhow!("invalid scope kind in grant {id}: {kind}"))?;
                Ok(Some(scope))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Revoke a grant by its row id. Returns `true` if a row was deleted.
    pub fn revoke_grant(&self, id: i64) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let rows = conn.execute("DELETE FROM grants WHERE id = ?1", [id])?;
        Ok(rows > 0)
    }

    /// Read a raw grant row. The capability and scope are validated by
    /// [`GrantRecord::try_from_raw`] after the SQL boundary.
    fn grant_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawGrantRow> {
        let expires_ts: Option<i64> = row.get(6)?;
        Ok(RawGrantRow {
            id: row.get(0)?,
            grantee: row.get(1)?,
            capability: row.get(2)?,
            scope_kind: row.get(3)?,
            scope_path: row.get(4)?,
            granted_by: row.get(5)?,
            expires: expires_ts.and_then(|ts| DateTime::from_timestamp(ts, 0)),
        })
    }

    // ── Management-plane audit operations ──

    /// Append an audit row. The audit log is append-only — there is
    /// deliberately no update or delete path.
    pub fn append_audit(&self, entry: &AuditEntry) -> Result<i64> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let (scope_kind, scope_path) = entry.scope.to_columns();
        conn.execute(
            "INSERT INTO manage_audit
                 (timestamp, actor_device, capability, scope_kind, scope_path, command, outcome)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            (
                entry.timestamp.timestamp(),
                entry.actor_device.as_str(),
                entry.capability.as_wire(),
                scope_kind,
                scope_path,
                &entry.command,
                &entry.outcome,
            ),
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// List audit rows in append order (ascending row id, which is also
    /// chronological for a monotonic clock).
    pub fn list_audit(&self) -> Result<Vec<AuditRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, timestamp, actor_device, capability, scope_kind, scope_path, command, outcome
             FROM manage_audit ORDER BY id ASC",
        )?;
        let rows = stmt
            .query_map([], Self::audit_record_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(AuditRecord::try_from_raw).collect()
    }

    /// Read a raw audit row. The capability and scope are validated by
    /// [`AuditRecord::try_from_raw`] after the SQL boundary.
    fn audit_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawAuditRow> {
        let timestamp_ts: i64 = row.get(1)?;
        Ok(RawAuditRow {
            id: row.get(0)?,
            timestamp: timestamp_ts,
            actor_device: row.get(2)?,
            capability: row.get(3)?,
            scope_kind: row.get(4)?,
            scope_path: row.get(5)?,
            command: row.get(6)?,
            outcome: row.get(7)?,
        })
    }

    // ── Capability-token operations ──

    /// Record an issued [`CapabilityToken`].
    ///
    /// The full signed token is stored as JSON so the owner can list and reprint
    /// it; the indexed columns mirror the claims for querying. `issued_at` is the
    /// wall clock at issuance. A token id is unique, so re-issuing the same id is
    /// a hard error rather than a silent overwrite.
    #[cfg(feature = "p2p")]
    pub fn insert_token(&self, token: &CapabilityToken, issued_at: DateTime<Utc>) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let claims = &token.claims;
        let (scope_kind, scope_path) = claims.scope.to_columns();
        let token_json = serde_json::to_string(token)?;
        conn.execute(
            "INSERT INTO capability_tokens
                 (token_id, issuer, bearer, capability, scope_kind, scope_path,
                  expires, issued_at, token_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            (
                &claims.token_id,
                claims.issuer.as_str(),
                claims.bearer.as_str(),
                claims.capability.as_wire(),
                scope_kind,
                scope_path,
                claims.expires.timestamp(),
                issued_at.timestamp(),
                &token_json,
            ),
        )?;
        Ok(())
    }

    /// List every token this node has issued, in issuance order, validating the
    /// stored JSON back into a typed [`CapabilityToken`].
    #[cfg(feature = "p2p")]
    pub fn list_tokens(&self) -> Result<Vec<TokenRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT token_id, issued_at, token_json
             FROM capability_tokens ORDER BY issued_at ASC, token_id ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let token_id: String = row.get(0)?;
                let issued_at_ts: i64 = row.get(1)?;
                let token_json: String = row.get(2)?;
                Ok((token_id, issued_at_ts, token_json))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(token_id, issued_at_ts, token_json)| {
                let issued_at = DateTime::from_timestamp(issued_at_ts, 0).ok_or_else(|| {
                    anyhow::anyhow!("invalid issued_at timestamp for token {token_id}")
                })?;
                let token: CapabilityToken = serde_json::from_str(&token_json)
                    .map_err(|e| anyhow::anyhow!("invalid stored token {token_id}: {e}"))?;
                Ok(TokenRecord { issued_at, token })
            })
            .collect()
    }

    /// Add a token id to the append-only revocation list. Returns `true` if the
    /// id was newly revoked, `false` if it was already on the list. `revoked_at`
    /// is the wall clock at revocation.
    #[cfg(feature = "p2p")]
    pub fn revoke_token(&self, token_id: &str, revoked_at: DateTime<Utc>) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let rows = conn.execute(
            "INSERT OR IGNORE INTO token_revocations (token_id, revoked_at)
             VALUES (?1, ?2)",
            (token_id, revoked_at.timestamp()),
        )?;
        Ok(rows > 0)
    }

    /// Whether `token_id` is on the revocation list. Consulted by the verify
    /// path for every token in a presented chain.
    #[cfg(feature = "p2p")]
    pub fn is_token_revoked(&self, token_id: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM token_revocations WHERE token_id = ?1",
            [token_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// The full set of revoked token ids, for building an in-memory predicate
    /// that does not touch the database per token in a chain.
    #[cfg(feature = "p2p")]
    pub fn revoked_token_ids(&self) -> Result<std::collections::HashSet<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let mut stmt = conn.prepare("SELECT token_id FROM token_revocations")?;
        let ids = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<std::collections::HashSet<_>, _>>()?;
        Ok(ids)
    }

    // ── Data-plane grant helpers ──

    /// List every grant whose capability is a data verb (`data:read` or
    /// `data:write`). Used by the `DataAuthority` implementation to evaluate
    /// per-peer, per-folder data-plane access.
    ///
    /// Returns the same [`GrantRecord`] type as [`Self::list_grants`] so callers
    /// share the same validation and conversion path.
    pub fn list_data_grants(&self) -> Result<Vec<GrantRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, grantee, capability, scope_kind, scope_path, granted_by, expires
             FROM grants
             WHERE capability IN ('data:read', 'data:write')
             ORDER BY id ASC",
        )?;
        let rows = stmt
            .query_map([], Self::grant_record_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(GrantRecord::try_from_raw).collect()
    }

    // ── Max file length rule operations ──

    /// Add a max file length rule.
    pub fn add_max_file_length_rule(
        &self,
        path_glob: &str,
        max_bytes: u64,
        priority: i32,
        conditions: Option<&str>,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        conn.execute(
            "INSERT INTO max_file_length_rules (path_glob, max_bytes, priority, conditions)
             VALUES (?1, ?2, ?3, ?4)",
            (path_glob, size_to_sql(max_bytes), priority, conditions),
        )?;
        Ok(())
    }

    /// List all max file length rules ordered by priority descending.
    pub fn list_max_file_length_rules(&self) -> Result<Vec<MaxFileLengthRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, path_glob, max_bytes, priority, conditions
             FROM max_file_length_rules ORDER BY priority DESC",
        )?;
        let rules = stmt
            .query_map([], |row| {
                Ok(MaxFileLengthRecord {
                    id: row.get(0)?,
                    path_glob: row.get(1)?,
                    max_bytes: size_from_row(row, 2)?.unwrap_or(0),
                    priority: row.get(3)?,
                    conditions: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rules)
    }

    /// Remove a max file length rule by ID. Returns `true` if a row was removed.
    pub fn remove_max_file_length_rule(&self, id: i64) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let rows = conn.execute("DELETE FROM max_file_length_rules WHERE id = ?1", [id])?;
        Ok(rows > 0)
    }

    // ── Auth code operations (pairing + device flow) ──

    /// Insert a new pending auth code.
    pub fn insert_auth_code(
        &self,
        code: &str,
        kind: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        conn.execute(
            "INSERT INTO auth_codes (code, kind, status, created_at, expires_at)
             VALUES (?1, ?2, 'pending', ?3, ?4)",
            (code, kind, Utc::now().to_rfc3339(), expires_at.to_rfc3339()),
        )?;
        Ok(())
    }

    /// Look up an auth code by its code string.
    pub fn get_auth_code(&self, code: &str) -> Result<Option<AuthCodeRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT code, kind, status, token_id, token_json, created_at, expires_at
             FROM auth_codes WHERE code = ?1",
        )?;
        let row = stmt
            .query_row([code], |row| {
                Ok(AuthCodeRecord {
                    code: row.get("code")?,
                    kind: row.get("kind")?,
                    status: row.get("status")?,
                    token_id: row.get("token_id")?,
                    token_json: row.get("token_json")?,
                    created_at: row.get("created_at")?,
                    expires_at: row.get("expires_at")?,
                })
            })
            .optional()?;
        Ok(row)
    }

    /// Update an auth code's status, optionally attaching the issued token.
    pub fn update_auth_code(
        &self,
        code: &str,
        status: &str,
        token_id: Option<&str>,
        token_json: Option<&str>,
    ) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let rows = conn.execute(
            "UPDATE auth_codes SET status = ?2, token_id = ?3, token_json = ?4
             WHERE code = ?1",
            (code, status, token_id, token_json),
        )?;
        Ok(rows > 0)
    }

    /// Delete expired auth codes. Returns the number of rows removed.
    pub fn delete_expired_auth_codes(&self) -> Result<u64> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let now = Utc::now().to_rfc3339();
        let rows = conn.execute("DELETE FROM auth_codes WHERE expires_at < ?1", [&now])?;
        Ok(u64::try_from(rows).unwrap_or(0))
    }

    // ── Daemon shared secret ──

    /// Get the daemon shared secret, if one has been generated.
    pub fn get_daemon_secret(&self) -> Result<Option<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let mut stmt = conn.prepare("SELECT secret FROM daemon_secret WHERE id = 1")?;
        let row = stmt
            .query_row([], |row| row.get::<_, String>(0))
            .optional()?;
        Ok(row)
    }

    /// Set (or replace) the daemon shared secret.
    pub fn set_daemon_secret(&self, secret: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        conn.execute(
            "INSERT OR REPLACE INTO daemon_secret (id, secret, created_at) VALUES (1, ?1, ?2)",
            (secret, Utc::now().to_rfc3339()),
        )?;
        Ok(())
    }

    /// Insert or replace a quarantine record. A newer proposal for the same
    /// `(folder_id, peer_device, path)` primary key silently replaces the
    /// older one, keeping the table bounded by the number of distinct paths
    /// per peer per folder.
    pub fn upsert_quarantine(&self, record: &QuarantineRecord) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        conn.execute(
            "INSERT OR REPLACE INTO data_receive_quarantine
                 (folder_id, peer_device, path, file_json, observed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (
                &record.folder_id,
                &record.peer_device,
                &record.path,
                &record.file_json,
                record.observed_at.timestamp(),
            ),
        )?;
        Ok(())
    }

    /// List all quarantined rows for a given `(folder_id, peer_device)` pair,
    /// ordered by path for deterministic iteration.
    pub fn list_quarantine(
        &self,
        folder_id: &str,
        peer_device: &str,
    ) -> Result<Vec<QuarantineRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT folder_id, peer_device, path, file_json, observed_at
             FROM data_receive_quarantine
             WHERE folder_id = ?1 AND peer_device = ?2
             ORDER BY path ASC",
        )?;
        let rows = stmt
            .query_map([folder_id, peer_device], |row| {
                let observed_ts: i64 = row.get(4)?;
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    observed_ts,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        rows.into_iter()
            .map(|(folder_id, peer_device, path, file_json, observed_ts)| {
                let observed_at = DateTime::from_timestamp(observed_ts, 0).ok_or_else(|| {
                    anyhow::anyhow!("invalid observed_at timestamp in quarantine row for {path}")
                })?;
                Ok(QuarantineRecord {
                    folder_id,
                    peer_device,
                    path,
                    file_json,
                    observed_at,
                })
            })
            .collect()
    }

    /// Count quarantined rows for a given `(folder_id, peer_device)` pair.
    /// Useful for the operator-facing surface ("N rejected local additions
    /// from `<peer>`") without loading all the rows.
    pub fn quarantine_count(&self, folder_id: &str, peer_device: &str) -> Result<u64> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM data_receive_quarantine
             WHERE folder_id = ?1 AND peer_device = ?2",
            [folder_id, peer_device],
            |row| row.get(0),
        )?;
        u64::try_from(count).map_err(|e| anyhow::anyhow!("quarantine count overflow: {e}"))
    }

    /// Prune all quarantined rows for a given `(folder_id, peer_device)` pair.
    /// Called by the operator to clear the quarantine after reviewing or
    /// granting `data:write`, or to discard stale proposals.
    ///
    /// Returns the number of rows deleted.
    pub fn prune_quarantine(&self, folder_id: &str, peer_device: &str) -> Result<u64> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let rows = conn.execute(
            "DELETE FROM data_receive_quarantine
             WHERE folder_id = ?1 AND peer_device = ?2",
            [folder_id, peer_device],
        )?;
        u64::try_from(rows).map_err(|e| anyhow::anyhow!("prune count overflow: {e}"))
    }

    // ── Data-plane explicit-control bit operations ──
    //
    // The F2 invariant: a peer who has ever presented a verified data-verb
    // token for a folder stays in explicit-control mode for that folder
    // for as long as the bit persists, so a token revocation or expiry
    // does not widen the absent direction back to the trusted-peer
    // default. The runtime data-plane gate consults `list_data_explicit_control`
    // on every frame; an operator who wants to return a peer to the
    // trusted-peer default for a folder calls `clear_data_explicit_control`
    // after removing the underlying grant.

    /// Record (or update) the explicit-control bit for `(peer, folder)`.
    /// The `data_read` and `data_write` columns are OR-merged with any
    /// existing row: the bit is sticky across multiple verifies, so a
    /// later `data:read` verify must not clear a previous `data:write`
    /// observation. A new row is created on the first verify.
    pub fn record_data_explicit_control(
        &self,
        peer_device: &str,
        folder_id: &str,
        data_read: bool,
        data_write: bool,
        observed_at: DateTime<Utc>,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        conn.execute(
            "INSERT INTO data_explicit_control
                 (peer_device, folder_id, data_read, data_write, observed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(peer_device, folder_id) DO UPDATE SET
                 data_read  = data_explicit_control.data_read  OR excluded.data_read,
                 data_write = data_explicit_control.data_write OR excluded.data_write,
                 observed_at = MAX(data_explicit_control.observed_at, excluded.observed_at)",
            (
                peer_device,
                folder_id,
                data_read,
                data_write,
                observed_at.timestamp(),
            ),
        )?;
        Ok(())
    }

    /// List every explicit-control row. The runtime gate calls this on
    /// every frame, so the read path is the hot one — the result is held
    /// in the engine's in-memory mirror and refreshed from this on
    /// startup and on `clear_data_explicit_control`.
    pub fn list_data_explicit_control(&self) -> Result<Vec<ExplicitControlRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT peer_device, folder_id, data_read, data_write, observed_at
             FROM data_explicit_control
             ORDER BY peer_device ASC, folder_id ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let observed_ts: i64 = row.get(4)?;
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, bool>(3)?,
                    observed_ts,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        rows.into_iter()
            .map(
                |(peer_device, folder_id, data_read, data_write, observed_ts)| {
                    let observed_at =
                        DateTime::from_timestamp(observed_ts, 0).ok_or_else(|| {
                            anyhow::anyhow!(
                                "invalid observed_at timestamp in data_explicit_control row for \
                         {peer_device}/{folder_id}"
                            )
                        })?;
                    Ok(ExplicitControlRecord {
                        peer_device,
                        folder_id,
                        data_read,
                        data_write,
                        observed_at,
                    })
                },
            )
            .collect()
    }

    /// Explicitly clear the bit for `(peer, folder)`. Returns `true` if a
    /// row was deleted, `false` if there was no row. This is the only path
    /// that removes a row; token revocation and token expiry never do.
    pub fn clear_data_explicit_control(&self, peer_device: &str, folder_id: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let rows = conn.execute(
            "DELETE FROM data_explicit_control
             WHERE peer_device = ?1 AND folder_id = ?2",
            [peer_device, folder_id],
        )?;
        Ok(rows > 0)
    }
}

/// An issued capability-token row read back from the database.
#[cfg(feature = "p2p")]
#[derive(Debug, Clone)]
pub struct TokenRecord {
    /// When the token was issued.
    pub issued_at: DateTime<Utc>,
    /// The full signed token.
    pub token: CapabilityToken,
}

/// Raw column values for a `grants` row, before validation.
#[cfg(feature = "native")]
struct RawGrantRow {
    id: i64,
    grantee: String,
    capability: String,
    scope_kind: String,
    scope_path: Option<String>,
    granted_by: String,
    expires: Option<DateTime<Utc>>,
}

/// A capability grant row from the database, with its row id.
#[derive(Debug, Clone)]
pub struct GrantRecord {
    /// The `SQLite` row id, used to revoke the grant.
    pub id: i64,
    /// The grant itself.
    pub grant: Grant,
}

impl GrantRecord {
    /// Validate a raw grant row into a typed record. Fails loudly if the
    /// stored capability or scope cannot be parsed, rather than silently
    /// dropping an unrecognised grant.
    #[cfg(feature = "native")]
    fn try_from_raw(raw: RawGrantRow) -> Result<Self> {
        let capability = Capability::from_wire(&raw.capability).ok_or_else(|| {
            anyhow::anyhow!("unknown capability in grant {}: {}", raw.id, raw.capability)
        })?;
        let scope = Scope::from_columns(&raw.scope_kind, raw.scope_path).ok_or_else(|| {
            anyhow::anyhow!("invalid scope kind in grant {}: {}", raw.id, raw.scope_kind)
        })?;
        Ok(Self {
            id: raw.id,
            grant: Grant {
                grantee: DeviceId::new(raw.grantee),
                capability,
                scope,
                granted_by: DeviceId::new(raw.granted_by),
                expires: raw.expires,
            },
        })
    }
}

/// An audit entry to append — the input side of the append-only audit log.
#[derive(Debug, Clone)]
pub struct AuditEntry {
    /// When the command was processed.
    pub timestamp: DateTime<Utc>,
    /// The device that issued the command.
    pub actor_device: DeviceId,
    /// The capability the command exercised.
    pub capability: Capability,
    /// The scope the command targeted.
    pub scope: Scope,
    /// A short human-readable summary of the command.
    pub command: String,
    /// The outcome (for example `allowed`, `denied`, or an error summary).
    pub outcome: String,
}

/// Raw column values for a `manage_audit` row, before validation.
#[cfg(feature = "native")]
struct RawAuditRow {
    id: i64,
    timestamp: i64,
    actor_device: String,
    capability: String,
    scope_kind: String,
    scope_path: Option<String>,
    command: String,
    outcome: String,
}

/// An audit row read back from the database, with its row id.
#[derive(Debug, Clone)]
pub struct AuditRecord {
    /// The `SQLite` row id (monotonic, hence the append order).
    pub id: i64,
    /// The audit entry.
    pub entry: AuditEntry,
}

impl AuditRecord {
    /// Validate a raw audit row into a typed record. Fails loudly if the
    /// stored timestamp, capability, or scope cannot be parsed.
    #[cfg(feature = "native")]
    fn try_from_raw(raw: RawAuditRow) -> Result<Self> {
        let timestamp = DateTime::from_timestamp(raw.timestamp, 0)
            .ok_or_else(|| anyhow::anyhow!("invalid audit timestamp in row {}", raw.id))?;
        let capability = Capability::from_wire(&raw.capability).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown capability in audit row {}: {}",
                raw.id,
                raw.capability
            )
        })?;
        let scope = Scope::from_columns(&raw.scope_kind, raw.scope_path).ok_or_else(|| {
            anyhow::anyhow!(
                "invalid scope kind in audit row {}: {}",
                raw.id,
                raw.scope_kind
            )
        })?;
        Ok(Self {
            id: raw.id,
            entry: AuditEntry {
                timestamp,
                actor_device: DeviceId::new(raw.actor_device),
                capability,
                scope,
                command: raw.command,
                outcome: raw.outcome,
            },
        })
    }
}

/// A registered backend row from the database.
#[derive(Debug, Clone)]
pub struct BackendRecord {
    pub id: String,
    pub backend_type: String,
    pub display_name: String,
    pub mount_path: Option<String>,
    pub config: Option<String>,
}

/// A pin rule row from the database.
#[derive(Debug, Clone)]
pub struct PinRuleRecord {
    pub id: i64,
    pub path_glob: String,
    pub recursive: bool,
    pub conditions: Option<String>,
}

/// A lifecycle policy row from the database.
#[derive(Debug, Clone)]
pub struct LifecyclePolicyRecord {
    pub id: i64,
    pub path_glob: String,
    pub max_age: Option<i64>,
    pub max_file_size: Option<i64>,
    pub priority: i32,
    pub conditions: Option<String>,
}

/// A dirty file record — a locally modified file pending upload.
#[derive(Debug, Clone)]
pub struct DirtyFileRecord {
    pub id: ItemId,
    pub backend_id: String,
    pub path: String,
    pub parent_id: ItemId,
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub mime_type: Option<String>,
    pub mod_time: Option<chrono::DateTime<chrono::Utc>>,
    pub remote_hash: Option<String>,
    pub local_path: Option<String>,
}

/// A P2P peer row from the database.
#[derive(Debug, Clone)]
pub struct PeerRecord {
    pub device_id: String,
    pub name: Option<String>,
    pub addresses: Option<String>,
    pub last_seen: Option<DateTime<Utc>>,
    pub online: bool,
}

/// A quarantined receive row — a proposed file from a peer whose `data:write`
/// grant was absent or expired for the folder at the time the index frame was
/// processed.
#[derive(Debug, Clone)]
pub struct QuarantineRecord {
    /// The folder this proposal belongs to.
    pub folder_id: String,
    /// The peer device that sent the proposal.
    pub peer_device: String,
    /// The path the file would occupy.
    pub path: String,
    /// The serialised `FileInfo` JSON as sent by the peer.
    pub file_json: String,
    /// Unix timestamp (seconds) when the proposal was observed.
    pub observed_at: DateTime<Utc>,
}

/// A row of the F2 explicit-control bit: a peer is in explicit-control
/// mode for a folder, with the per-direction state observed the last
/// time a verified data-verb token granted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplicitControlRecord {
    /// The peer device in explicit-control mode.
    pub peer_device: String,
    /// The BEP folder id the control applies to.
    pub folder_id: String,
    /// Whether the verified token granted `data:read` for this folder.
    pub data_read: bool,
    /// Whether the verified token granted `data:write` for this folder.
    pub data_write: bool,
    /// Unix timestamp (seconds) when the bit was last refreshed.
    pub observed_at: DateTime<Utc>,
}

/// An auth code record from the `auth_codes` table.
#[derive(Debug, Clone)]
pub struct AuthCodeRecord {
    /// The short code the user enters or is shown.
    pub code: String,
    /// Either `pairing` or `device`.
    pub kind: String,
    /// `pending`, `authorised`, or `consumed`.
    pub status: String,
    /// The token id set when the code is authorised.
    pub token_id: Option<String>,
    /// The full `CapabilityToken` JSON set when authorised.
    pub token_json: Option<String>,
    /// When the code was created (RFC 3339).
    pub created_at: String,
    /// When the code expires (RFC 3339).
    pub expires_at: String,
}

/// A max file length rule row from the database.
#[derive(Debug, Clone)]
pub struct MaxFileLengthRecord {
    /// The row id.
    pub id: i64,
    /// Glob pattern matched against file paths.
    pub path_glob: String,
    /// Maximum allowed file size in bytes.
    pub max_bytes: u64,
    /// Higher-priority rules take precedence.
    pub priority: i32,
    /// Optional conditional expression.
    pub conditions: Option<String>,
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;

    #[test]
    fn test_open_and_migrate() {
        let db = StateDb::open_in_memory().unwrap();
        // Should have created all tables
        let backends = db.list_backends().unwrap();
        assert!(backends.is_empty());
    }

    #[test]
    fn test_file_crud() {
        let db = StateDb::open_in_memory().unwrap();

        // Register backend first (foreign key constraint)
        db.register_backend("gdrive", "gdrive", "Google Drive", None, None)
            .unwrap();

        let file_id = ItemId::new("gdrive", "file1");
        let parent_id = ItemId::new("gdrive", "root");
        let entry = FileEntry::file(file_id.clone(), parent_id, "test.txt".into());

        db.upsert_file(&entry).unwrap();

        let retrieved = db.get_file(&file_id).unwrap().unwrap();
        assert_eq!(retrieved.name, "test.txt");
        assert!(!retrieved.is_dir);

        db.delete_file(&file_id).unwrap();
        assert!(db.get_file(&file_id).unwrap().is_none());
    }

    #[test]
    fn test_cache_state() {
        let db = StateDb::open_in_memory().unwrap();

        db.register_backend("gdrive", "gdrive", "Google Drive", None, None)
            .unwrap();

        let file_id = ItemId::new("gdrive", "file1");
        let parent_id = ItemId::new("gdrive", "root");
        let entry = FileEntry::file(file_id.clone(), parent_id, "test.txt".into());
        db.upsert_file(&entry).unwrap();

        assert_eq!(
            db.get_cache_state(&file_id).unwrap(),
            Some(CacheState::Online)
        );

        db.update_cache_state(&file_id, CacheState::Cached).unwrap();
        assert_eq!(
            db.get_cache_state(&file_id).unwrap(),
            Some(CacheState::Cached)
        );
    }

    #[test]
    fn test_sync_cursor() {
        let db = StateDb::open_in_memory().unwrap();

        assert!(db.get_cursor("gdrive").unwrap().is_none());

        db.set_cursor("gdrive", &Cursor("token123".into())).unwrap();
        assert_eq!(
            db.get_cursor("gdrive").unwrap().map(|c| c.0),
            Some("token123".into())
        );
    }

    #[test]
    fn test_backend_registration() {
        let db = StateDb::open_in_memory().unwrap();

        db.register_backend(
            "gdrive-personal",
            "gdrive",
            "Google Drive (Personal)",
            None,
            None,
        )
        .unwrap();

        let backends = db.list_backends().unwrap();
        assert_eq!(backends.len(), 1);
        assert_eq!(backends[0].backend_type, "gdrive");
    }

    #[test]
    fn test_mark_dirty_and_clear() {
        let db = StateDb::open_in_memory().unwrap();
        db.register_backend("gdrive", "gdrive", "Google Drive", None, None)
            .unwrap();

        let file_id = ItemId::new("gdrive", "file1");
        let parent_id = ItemId::new("gdrive", "root");
        let entry = FileEntry::file(file_id.clone(), parent_id, "test.txt".into());
        db.upsert_file(&entry).unwrap();

        // Initially not dirty.
        assert_eq!(db.is_dirty(&file_id).unwrap(), Some(false));
        assert!(db.list_dirty_files().unwrap().is_empty());

        // Mark dirty.
        db.mark_dirty(&file_id).unwrap();
        assert_eq!(db.is_dirty(&file_id).unwrap(), Some(true));

        let dirty = db.list_dirty_files().unwrap();
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].id, file_id);
        assert_eq!(dirty[0].name, "test.txt");

        // Clear dirty.
        db.clear_dirty(&file_id).unwrap();
        assert_eq!(db.is_dirty(&file_id).unwrap(), Some(false));
        assert!(db.list_dirty_files().unwrap().is_empty());
    }

    #[test]
    fn test_is_dirty_returns_none_for_missing_file() {
        let db = StateDb::open_in_memory().unwrap();
        let file_id = ItemId::new("gdrive", "nonexistent");
        assert_eq!(db.is_dirty(&file_id).unwrap(), None);
    }

    // ── Management-plane grant tests ──

    fn sample_grant() -> Grant {
        Grant {
            grantee: DeviceId::new("MANAGER"),
            capability: Capability::PinWrite,
            scope: Scope::folder("/work"),
            granted_by: DeviceId::new("OWNER"),
            expires: None,
        }
    }

    #[test]
    fn test_grant_insert_list_revoke_round_trip() {
        let db = StateDb::open_in_memory().unwrap();
        assert!(db.list_grants().unwrap().is_empty());

        let id = db.insert_grant(&sample_grant()).unwrap();

        let listed = db.list_grants().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].grant, sample_grant());

        assert!(db.revoke_grant(id).unwrap());
        assert!(db.list_grants().unwrap().is_empty());
        // Revoking again finds nothing.
        assert!(!db.revoke_grant(id).unwrap());
    }

    #[test]
    fn test_grant_node_scope_and_expiry_round_trip() {
        let db = StateDb::open_in_memory().unwrap();
        let expiry = chrono::DateTime::from_timestamp(1_900_000_000, 0).unwrap();
        let grant = Grant {
            grantee: DeviceId::new("MANAGER"),
            capability: Capability::StatusRead,
            scope: Scope::Node,
            granted_by: DeviceId::new("OWNER"),
            expires: Some(expiry),
        };
        db.insert_grant(&grant).unwrap();

        let listed = db.list_grants().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].grant.scope, Scope::Node);
        assert_eq!(listed[0].grant.expires, Some(expiry));
    }

    #[test]
    fn test_grants_list_in_insertion_order() {
        let db = StateDb::open_in_memory().unwrap();
        let first = db.insert_grant(&sample_grant()).unwrap();
        let second = db
            .insert_grant(&Grant {
                capability: Capability::CacheManage,
                ..sample_grant()
            })
            .unwrap();
        let listed = db.list_grants().unwrap();
        assert_eq!(listed[0].id, first);
        assert_eq!(listed[1].id, second);
    }

    // ── Management-plane audit tests ──

    fn audit_entry(command: &str, secs: i64) -> AuditEntry {
        AuditEntry {
            timestamp: chrono::DateTime::from_timestamp(secs, 0).unwrap(),
            actor_device: DeviceId::new("MANAGER"),
            capability: Capability::PinWrite,
            scope: Scope::folder("/work"),
            command: command.to_string(),
            outcome: "allowed".to_string(),
        }
    }

    #[test]
    fn test_audit_append_and_list_round_trip() {
        let db = StateDb::open_in_memory().unwrap();
        assert!(db.list_audit().unwrap().is_empty());

        let entry = audit_entry("pin /work/report", 1_800_000_000);
        let id = db.append_audit(&entry).unwrap();

        let listed = db.list_audit().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].entry.command, "pin /work/report");
        assert_eq!(listed[0].entry.capability, Capability::PinWrite);
        assert_eq!(listed[0].entry.scope, Scope::folder("/work"));
        assert_eq!(listed[0].entry.outcome, "allowed");
    }

    #[test]
    fn test_audit_is_append_only_and_ordered() {
        let db = StateDb::open_in_memory().unwrap();
        // Append three entries; later entries get larger row ids regardless of
        // timestamp, so the listing preserves append order.
        let a = db.append_audit(&audit_entry("first", 1_000)).unwrap();
        let b = db.append_audit(&audit_entry("second", 999)).unwrap();
        let c = db.append_audit(&audit_entry("third", 1_001)).unwrap();
        assert!(a < b && b < c);

        let listed = db.list_audit().unwrap();
        let commands: Vec<&str> = listed.iter().map(|r| r.entry.command.as_str()).collect();
        assert_eq!(commands, ["first", "second", "third"]);
    }

    // ── Capability-token store tests ──

    #[cfg(feature = "p2p")]
    fn sample_token() -> CapabilityToken {
        // The store only persists and reloads token JSON, so any validly-signed
        // token serves. Sign with a freshly generated node identity.
        let node = cascade_p2p::identity::DeviceIdentity::generate().unwrap();
        CapabilityToken::issue(
            "tok-store-1",
            &node,
            &DeviceId::new("BEARER"),
            Capability::PinWrite,
            Scope::folder("/work"),
            chrono::DateTime::from_timestamp(2_000_000_000, 0).unwrap(),
        )
        .unwrap()
    }

    #[cfg(feature = "p2p")]
    #[test]
    fn token_insert_and_list_round_trip() {
        let db = StateDb::open_in_memory().unwrap();
        assert!(db.list_tokens().unwrap().is_empty());

        let token = sample_token();
        let issued_at = chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        db.insert_token(&token, issued_at).unwrap();

        let listed = db.list_tokens().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].token, token);
        assert_eq!(listed[0].issued_at, issued_at);
    }

    #[cfg(feature = "p2p")]
    #[test]
    fn token_revocation_is_recorded_and_queryable() {
        let db = StateDb::open_in_memory().unwrap();
        let now = chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        assert!(!db.is_token_revoked("tok-store-1").unwrap());

        // First revocation is new; a second is a no-op.
        assert!(db.revoke_token("tok-store-1", now).unwrap());
        assert!(!db.revoke_token("tok-store-1", now).unwrap());

        assert!(db.is_token_revoked("tok-store-1").unwrap());
        assert!(db.revoked_token_ids().unwrap().contains("tok-store-1"));
    }

    #[cfg(feature = "p2p")]
    #[test]
    fn duplicate_token_id_is_rejected() {
        let db = StateDb::open_in_memory().unwrap();
        let token = sample_token();
        let issued_at = chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        db.insert_token(&token, issued_at).unwrap();
        // Re-issuing the same token id must be a hard error, never a silent
        // overwrite.
        assert!(db.insert_token(&token, issued_at).is_err());
    }

    // ── Data-verb grant filter tests ──

    #[test]
    fn list_data_grants_returns_only_data_verbs() {
        let db = StateDb::open_in_memory().unwrap();
        // Insert a mix of data and non-data grants.
        db.insert_grant(&Grant {
            grantee: DeviceId::new("PEER"),
            capability: Capability::DataRead,
            scope: Scope::folder("/work"),
            granted_by: DeviceId::new("OWNER"),
            expires: None,
        })
        .unwrap();
        db.insert_grant(&Grant {
            grantee: DeviceId::new("PEER"),
            capability: Capability::PinWrite,
            scope: Scope::folder("/work"),
            granted_by: DeviceId::new("OWNER"),
            expires: None,
        })
        .unwrap();
        db.insert_grant(&Grant {
            grantee: DeviceId::new("PEER"),
            capability: Capability::DataWrite,
            scope: Scope::folder("/work"),
            granted_by: DeviceId::new("OWNER"),
            expires: None,
        })
        .unwrap();

        let all = db.list_grants().unwrap();
        assert_eq!(all.len(), 3, "three grants total");

        let data = db.list_data_grants().unwrap();
        assert_eq!(data.len(), 2, "two data-verb grants");
        assert!(
            data.iter().all(|r| r.grant.capability.is_data_verb()),
            "every returned grant must be a data verb"
        );
    }

    #[test]
    fn list_data_grants_empty_when_no_data_grants() {
        let db = StateDb::open_in_memory().unwrap();
        db.insert_grant(&sample_grant()).unwrap();
        assert!(
            db.list_data_grants().unwrap().is_empty(),
            "no data grants when only admin grants exist"
        );
    }

    // ── Data-receive quarantine tests ──

    fn quarantine_record(folder_id: &str, peer: &str, path: &str) -> QuarantineRecord {
        QuarantineRecord {
            folder_id: folder_id.to_owned(),
            peer_device: peer.to_owned(),
            path: path.to_owned(),
            file_json: "{\"name\":\"test\"}".to_owned(),
            observed_at: chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap(),
        }
    }

    #[test]
    fn quarantine_upsert_list_count_round_trip() {
        let db = StateDb::open_in_memory().unwrap();
        let rec = quarantine_record("folder1", "PEER", "/work/a.txt");
        db.upsert_quarantine(&rec).unwrap();

        let count = db.quarantine_count("folder1", "PEER").unwrap();
        assert_eq!(count, 1);

        let listed = db.list_quarantine("folder1", "PEER").unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, "/work/a.txt");
        assert_eq!(listed[0].file_json, "{\"name\":\"test\"}");
    }

    #[test]
    fn quarantine_upsert_replaces_same_key() {
        let db = StateDb::open_in_memory().unwrap();
        let rec = quarantine_record("folder1", "PEER", "/work/a.txt");
        db.upsert_quarantine(&rec).unwrap();
        // A newer proposal for the same path replaces the older row.
        let updated = QuarantineRecord {
            file_json: "{\"name\":\"updated\"}".to_owned(),
            ..rec
        };
        db.upsert_quarantine(&updated).unwrap();
        let listed = db.list_quarantine("folder1", "PEER").unwrap();
        assert_eq!(listed.len(), 1, "upsert must replace, not append");
        assert_eq!(listed[0].file_json, "{\"name\":\"updated\"}");
    }

    #[test]
    fn quarantine_prune_removes_all_rows_for_peer_folder() {
        let db = StateDb::open_in_memory().unwrap();
        db.upsert_quarantine(&quarantine_record("folder1", "PEER", "/work/a.txt"))
            .unwrap();
        db.upsert_quarantine(&quarantine_record("folder1", "PEER", "/work/b.txt"))
            .unwrap();
        // A row for a different peer must not be pruned.
        db.upsert_quarantine(&quarantine_record("folder1", "OTHER", "/work/c.txt"))
            .unwrap();

        let pruned = db.prune_quarantine("folder1", "PEER").unwrap();
        assert_eq!(pruned, 2);
        assert_eq!(db.quarantine_count("folder1", "PEER").unwrap(), 0);
        // The OTHER peer's row must still exist.
        assert_eq!(db.quarantine_count("folder1", "OTHER").unwrap(), 1);
    }

    #[test]
    fn quarantine_different_folders_are_isolated() {
        let db = StateDb::open_in_memory().unwrap();
        db.upsert_quarantine(&quarantine_record("folder1", "PEER", "/work/a.txt"))
            .unwrap();
        db.upsert_quarantine(&quarantine_record("folder2", "PEER", "/work/a.txt"))
            .unwrap();

        assert_eq!(db.quarantine_count("folder1", "PEER").unwrap(), 1);
        assert_eq!(db.quarantine_count("folder2", "PEER").unwrap(), 1);

        db.prune_quarantine("folder1", "PEER").unwrap();
        assert_eq!(db.quarantine_count("folder1", "PEER").unwrap(), 0);
        assert_eq!(
            db.quarantine_count("folder2", "PEER").unwrap(),
            1,
            "pruning folder1 must not affect folder2"
        );
    }
}
