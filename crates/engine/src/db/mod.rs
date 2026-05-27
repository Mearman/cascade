pub mod schema;

use crate::db::schema::SchemaVersion;
use crate::types::*;
use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;

/// SQLite state database. Stores file metadata, backend config,
/// pin rules, lifecycle policies, config cache, sync cursors, and P2P state.
pub struct StateDb {
    conn: Mutex<Connection>,
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
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;

        let current_version = Self::get_version(&conn)?;
        let target_version = SchemaVersion::current();

        if current_version < target_version {
            schema::migrate(&conn, current_version, target_version)?;
            Self::set_version(&conn, target_version)?;
        }

        Ok(())
    }

    fn get_version(conn: &Connection) -> Result<SchemaVersion> {
        let version: i32 = conn
            .query_row(
                "SELECT value FROM schema_meta WHERE key = 'version'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        Ok(SchemaVersion(version))
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
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        conn.execute(
            "INSERT OR REPLACE INTO files (
                id, backend_id, path, parent_id, name, is_dir, size,
                mime_type, mod_time, remote_hash, cache_state, provenance
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            (
                &entry.id.0,
                entry.id.backend_id(),
                "", // path — would need to be set by caller
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
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
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
                mod_time: row.get::<_, Option<i64>>(6)?.map(|ts| {
                    chrono::DateTime::from_timestamp(ts, 0).unwrap_or_default()
                }),
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
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        conn.execute("DELETE FROM files WHERE id = ?1", [&id.0])?;
        Ok(())
    }

    /// Update the cache state of a file.
    pub fn update_cache_state(&self, id: &ItemId, state: CacheState) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        let now = chrono::Utc::now().timestamp();
        conn.execute(
            "UPDATE files SET cache_state = ?1, last_access = ?2 WHERE id = ?3",
            (state.as_str(), now, &id.0),
        )?;
        Ok(())
    }

    /// Get the cache state of a file.
    pub fn get_cache_state(&self, id: &ItemId) -> Result<Option<CacheState>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
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
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        conn.execute(
            "INSERT OR REPLACE INTO sync_cursors (backend_id, cursor) VALUES (?1, ?2)",
            (backend_id, &cursor.0),
        )?;
        Ok(())
    }

    /// Get the sync cursor for a backend.
    pub fn get_cursor(&self, backend_id: &str) -> Result<Option<Cursor>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
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
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        conn.execute(
            "INSERT OR REPLACE INTO backends (id, backend_type, display_name, mount_path, config)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (id, backend_type, display_name, mount_path, config),
        )?;
        Ok(())
    }

    /// List all registered backends.
    pub fn list_backends(&self) -> Result<Vec<BackendRecord>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, backend_type, display_name, mount_path, config FROM backends",
        )?;

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
        db.register_backend("gdrive", "gdrive", "Google Drive", None, None).unwrap();

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

        db.register_backend("gdrive", "gdrive", "Google Drive", None, None).unwrap();

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

        db.register_backend("gdrive-personal", "gdrive", "Google Drive (Personal)", None, None)
            .unwrap();

        let backends = db.list_backends().unwrap();
        assert_eq!(backends.len(), 1);
        assert_eq!(backends[0].backend_type, "gdrive");
    }
}
