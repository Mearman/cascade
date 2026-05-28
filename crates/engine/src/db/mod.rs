pub mod schema;

use crate::db::schema::SchemaVersion;
use crate::types::{CacheState, Cursor, FileEntry, ItemId};
use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;

/// `SQLite` state database. Stores file metadata, backend config,
/// pin rules, lifecycle policies, config cache, sync cursors, and P2P state.
pub struct StateDb {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for StateDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateDb").finish_non_exhaustive()
    }
}

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
    pub fn upsert_file(&self, entry: &FileEntry) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        conn.execute(
            "INSERT OR REPLACE INTO files (
                id, backend_id, path, parent_id, name, is_dir, size,
                mime_type, mod_time, remote_hash, cache_state, provenance
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            (
                &entry.id.0,
                entry.id.backend_id(),
                &entry.name, // path derived from file name
                &entry.parent_id.0,
                &entry.name,
                entry.is_dir,
                entry.size,
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
            "SELECT id, parent_id, name, is_dir, size, mime_type, mod_time, remote_hash
             FROM files WHERE id = ?1",
        )?;

        let result = stmt.query_row([&id.0], |row| {
            Ok(FileEntry {
                id: ItemId(row.get(0)?),
                parent_id: ItemId(row.get(1)?),
                name: row.get(2)?,
                is_dir: row.get(3)?,
                size: row.get(4)?,
                mime_type: row.get(5)?,
                mod_time: row
                    .get::<_, Option<i64>>(6)?
                    .map(|ts| chrono::DateTime::from_timestamp(ts, 0).unwrap_or_default()),
                hash: row.get(7)?,
            })
        });

        match result {
            Ok(entry) => Ok(Some(entry)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
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
            "SELECT id, parent_id, name, is_dir, size, mime_type, mod_time, remote_hash
             FROM files WHERE cache_state = ?1",
        )?;
        let entries = stmt
            .query_map([state.as_str()], |row| {
                Ok(FileEntry {
                    id: ItemId(row.get(0)?),
                    parent_id: ItemId(row.get(1)?),
                    name: row.get(2)?,
                    is_dir: row.get(3)?,
                    size: row.get(4)?,
                    mime_type: row.get(5)?,
                    mod_time: row
                        .get::<_, Option<i64>>(6)?
                        .map(|ts| chrono::DateTime::from_timestamp(ts, 0).unwrap_or_default()),
                    hash: row.get(7)?,
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

    /// Find eviction candidates: cached (not pinned) files ordered by `last_access` ascending (LRU).
    pub fn eviction_candidates(&self, limit: usize) -> Result<Vec<FileEntry>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, parent_id, name, is_dir, size, mime_type, mod_time, remote_hash
             FROM files
             WHERE cache_state = 'cached' AND dirty = FALSE
             ORDER BY last_access ASC
             LIMIT ?1",
        )?;
        let entries = stmt
            .query_map([limit], |row| {
                Ok(FileEntry {
                    id: ItemId(row.get(0)?),
                    parent_id: ItemId(row.get(1)?),
                    name: row.get(2)?,
                    is_dir: row.get(3)?,
                    size: row.get(4)?,
                    mime_type: row.get(5)?,
                    mod_time: row
                        .get::<_, Option<i64>>(6)?
                        .map(|ts| chrono::DateTime::from_timestamp(ts, 0).unwrap_or_default()),
                    hash: row.get(7)?,
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

/// A P2P peer row from the database.
#[derive(Debug, Clone)]
pub struct PeerRecord {
    pub device_id: String,
    pub name: Option<String>,
    pub addresses: Option<String>,
    pub last_seen: Option<DateTime<Utc>>,
    pub online: bool,
}

#[cfg(test)]
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
}
