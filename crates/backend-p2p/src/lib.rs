//! P2P-only storage backend.
//!
//! Unlike `cascade-backend-{gdrive,s3,local}`, this backend has no cloud
//! source of truth. Files live as content-addressed blocks in the local
//! P2P block store; folder metadata lives in a `SQLite` index. The full
//! mesh is reconstituted from peers — Syncthing-style — when peer
//! synchronisation is enabled (Phase 3).
//!
//! For now (Phase 2) the backend is functional as a local-only,
//! deduplicating content-addressed store. The same file uploaded twice
//! costs blocks once. Peer sync is a follow-up that wires the existing
//! `cascade_p2p::BepMessage` machinery onto this index.

pub mod index;
pub mod sync;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use cascade_engine::backend::Backend;
use cascade_engine::types::{Change, Cursor, FileEntry, FileId, ItemId, Quota};
use cascade_p2p::block::{BlockHash, split_data};
use cascade_p2p::identity::DeviceIdentity;
use cascade_p2p::store::BlockStore;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::index::{FolderIndex, IndexEntry};
use crate::sync::SyncEngine;

/// Poll interval reported to the engine when no peer push has arrived
/// recently. Long enough to avoid wasted work but short enough that
/// queued peer changes surface quickly through `changes()`.
#[allow(clippy::duration_suboptimal_units)]
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Configuration for a P2P backend instance.
#[derive(Debug)]
pub struct P2pBackendConfig {
    pub instance_id: String,
    pub display_name: String,
    pub index_path: PathBuf,
    pub block_store_root: PathBuf,
    /// Directory used for the device identity certificate.
    pub identity_dir: PathBuf,
    /// Folder ID exchanged with peers. Defaults to `instance_id`.
    pub folder_id: String,
}

/// A P2P backend instance.
pub struct P2pBackend {
    cfg: P2pBackendConfig,
    index: Arc<FolderIndex>,
    blocks: Arc<BlockStore>,
    sync: SyncEngine,
}

impl std::fmt::Debug for P2pBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("P2pBackend")
            .field("id", &self.cfg.instance_id)
            .field("display_name", &self.cfg.display_name)
            .finish_non_exhaustive()
    }
}

impl P2pBackend {
    /// Open or create a P2P backend at the given index + block store paths.
    pub async fn open(cfg: P2pBackendConfig) -> Result<Self> {
        let index = Arc::new(FolderIndex::open(&cfg.index_path)?);
        let blocks = Arc::new(
            BlockStore::new(&cfg.block_store_root)
                .await
                .context("open block store")?,
        );
        let identity = DeviceIdentity::load_or_generate(&cfg.identity_dir)
            .context("loading P2P backend identity")?;
        let sync = SyncEngine::new(
            cfg.folder_id.clone(),
            index.clone(),
            blocks.clone(),
            identity,
        );
        Ok(Self {
            cfg,
            index,
            blocks,
            sync,
        })
    }

    /// Access the sync engine — used to start a listener and add peers.
    #[must_use]
    pub const fn sync(&self) -> &SyncEngine {
        &self.sync
    }

    /// Convert a `FolderIndex` row into a `FileEntry` keyed under this
    /// backend's instance ID. Root entries (those with no `/` in `path`)
    /// have `parent_id = "root"`; nested entries point at their parent
    /// path as the native ID.
    fn entry_to_file(&self, entry: &IndexEntry) -> FileEntry {
        let (parent_native, name) = match entry.path.rsplit_once('/') {
            Some((parent, name)) => (parent.to_string(), name.to_string()),
            None => ("root".to_string(), entry.path.clone()),
        };
        let modified = chrono::DateTime::from_timestamp(entry.modified, 0);
        FileEntry {
            id: ItemId::new(&self.cfg.instance_id, &entry.path),
            parent_id: ItemId::new(&self.cfg.instance_id, &parent_native),
            name,
            is_dir: entry.is_dir,
            size: if entry.is_dir { None } else { Some(entry.size) },
            mod_time: modified,
            mime_type: None,
            hash: None,
        }
    }

    /// Synthetic root entry (no real index row).
    fn root_entry(&self) -> FileEntry {
        FileEntry::dir(
            ItemId::new(&self.cfg.instance_id, "root"),
            ItemId::new(&self.cfg.instance_id, "root"),
            "P2P".to_string(),
        )
    }
}

#[async_trait]
impl Backend for P2pBackend {
    fn id(&self) -> &str {
        &self.cfg.instance_id
    }

    fn display_name(&self) -> &str {
        &self.cfg.display_name
    }

    async fn quota(&self) -> Result<Option<Quota>> {
        // No accounting yet — peer storage is opaque.
        Ok(None)
    }

    async fn changes(&self, cursor: Option<&Cursor>) -> Result<(Vec<Change>, Cursor)> {
        let since: i64 = cursor.and_then(|c| c.0.parse().ok()).unwrap_or(0);
        let entries = self.index.entries_since(since)?;
        let mut changes = Vec::with_capacity(entries.len());
        for entry in &entries {
            let file = self.entry_to_file(entry);
            if entry.deleted {
                changes.push(Change::Deleted(file));
            } else {
                changes.push(Change::Created(file));
            }
        }
        let new_cursor = self.index.max_version()?;
        Ok((changes, Cursor(new_cursor.to_string())))
    }

    async fn metadata(&self, path: &Path) -> Result<FileEntry> {
        let path_str = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("non-utf8 path"))?
            .trim_start_matches('/')
            .to_string();
        if path_str.is_empty() {
            return Ok(self.root_entry());
        }
        let entry = self
            .index
            .get(&path_str)?
            .ok_or_else(|| anyhow::anyhow!("not found: {path_str}"))?;
        if entry.deleted {
            anyhow::bail!("not found (deleted): {path_str}");
        }
        Ok(self.entry_to_file(&entry))
    }

    async fn download(
        &self,
        file: &FileEntry,
        writer: &mut (dyn tokio::io::AsyncWrite + Unpin + Send),
    ) -> Result<()> {
        let native = file.id.native_id();
        let entry = self
            .index
            .get(native)?
            .ok_or_else(|| anyhow::anyhow!("not in index: {native}"))?;
        let block_size = cascade_p2p::block::block_size_for_file(entry.size);
        // Block hashes are stored as concatenated 32-byte values.
        for (idx, chunk) in entry.block_hashes.chunks(32).enumerate() {
            let mut h = [0u8; 32];
            h.copy_from_slice(chunk);
            let hash = BlockHash(h);
            let data = if let Some(data) = self.blocks.get_block(&hash).await? {
                data
            } else {
                let fetched = self
                    .sync
                    .fetch_block(native, idx, block_size, h)
                    .await
                    .ok_or_else(|| anyhow::anyhow!("block {hash} missing and no peer had it"))?;
                // Cache the fetched block locally so future reads hit
                // the store without round-tripping the network.
                self.blocks.store_block(&hash, &fetched).await?;
                fetched
            };
            writer.write_all(&data).await?;
        }
        writer.flush().await?;
        Ok(())
    }

    async fn upload(
        &self,
        path: &Path,
        reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
        parent_id: &FileId,
    ) -> Result<FileEntry> {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow::anyhow!("invalid filename"))?;
        let parent_native = parent_id.native_id();
        let path_str = if parent_native == "root" || parent_native.is_empty() {
            name.to_string()
        } else {
            format!("{parent_native}/{name}")
        };

        let mut data = Vec::new();
        reader.read_to_end(&mut data).await?;
        let size = data.len() as u64;

        let blocks_info = split_data(&data);
        let block_size = blocks_info.block_size as usize;
        let mut hash_blob = Vec::with_capacity(blocks_info.blocks.len() * 32);
        for (idx, hash) in blocks_info.blocks.iter().enumerate() {
            let start = idx * block_size;
            let end = (start + block_size).min(data.len());
            #[allow(clippy::indexing_slicing)] // bounds derived from split_data
            let slice = &data[start..end];
            self.blocks.store_block(hash, slice).await?;
            hash_blob.extend_from_slice(&hash.0);
        }

        let entry = IndexEntry {
            path: path_str.clone(),
            is_dir: false,
            size,
            modified: chrono::Utc::now().timestamp(),
            block_hashes: hash_blob,
            deleted: false,
            version: 0,
        };
        self.index.upsert(&entry)?;
        self.sync.broadcast_update(&entry).await;
        Ok(self.entry_to_file(&entry))
    }

    async fn update(
        &self,
        file_id: &FileId,
        reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
    ) -> Result<FileEntry> {
        let native = file_id.native_id();
        let existing = self
            .index
            .get(native)?
            .ok_or_else(|| anyhow::anyhow!("not in index: {native}"))?;
        let parent = match native.rsplit_once('/') {
            Some((parent, _)) => format!("{}:{parent}", self.cfg.instance_id),
            None => format!("{}:root", self.cfg.instance_id),
        };
        // Re-upload using the same path.
        self.upload(Path::new(&existing.path), reader, &FileId(parent))
            .await
    }

    async fn create_dir(&self, path: &Path) -> Result<FileEntry> {
        let path_str = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("non-utf8 path"))?
            .trim_start_matches('/')
            .to_string();
        let entry = IndexEntry {
            path: path_str,
            is_dir: true,
            size: 0,
            modified: chrono::Utc::now().timestamp(),
            block_hashes: Vec::new(),
            deleted: false,
            version: 0,
        };
        self.index.upsert(&entry)?;
        Ok(self.entry_to_file(&entry))
    }

    async fn delete(&self, file: &FileEntry) -> Result<()> {
        let native = file.id.native_id();
        self.index.mark_deleted(native)?;
        Ok(())
    }

    async fn move_entry(&self, src: &Path, dst: &Path) -> Result<FileEntry> {
        let src_str = src
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("non-utf8 src path"))?
            .trim_start_matches('/');
        let dst_str = dst
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("non-utf8 dst path"))?
            .trim_start_matches('/');
        let existing = self
            .index
            .get(src_str)?
            .ok_or_else(|| anyhow::anyhow!("not in index: {src_str}"))?;
        // Insert at destination, mark source as deleted.
        let mut new_entry = existing;
        new_entry.path = dst_str.to_string();
        self.index.upsert(&new_entry)?;
        self.index.mark_deleted(src_str)?;
        Ok(self.entry_to_file(&new_entry))
    }

    async fn list_children(&self, parent_native_id: &str) -> Result<Vec<FileEntry>> {
        let parent = if parent_native_id == "root" {
            ""
        } else {
            parent_native_id
        };
        let rows = self.index.list_children(parent)?;
        Ok(rows.iter().map(|e| self.entry_to_file(e)).collect())
    }

    async fn poll_interval(&self) -> Option<std::time::Duration> {
        // The local index is updated synchronously and there is no remote
        // source to poll (peer sync pushes IndexUpdate messages when
        // wired). 60s is a sensible default so changes() is still called
        // periodically to flush queued peer changes.
        Some(POLL_INTERVAL)
    }
}

/// CLI entry point — construct a backend from a TOML config table.
///
/// Expected keys:
/// - `name` (required) — instance name; used to derive `id = "p2p-{name}"`
/// - `display_name` (optional) — human-readable label
/// - `data_dir` (optional) — base dir for index + block store;
///   defaults to `${HOME}/.config/cascade/p2p-{name}`
pub fn create_backend(config: &toml::Value) -> Result<Box<dyn Backend>> {
    let name = config
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("p2p backend requires 'name'"))?
        .to_string();
    let display_name = config
        .get("display_name")
        .and_then(|v| v.as_str())
        .unwrap_or(&name)
        .to_string();
    let data_dir = config
        .get("data_dir")
        .and_then(|v| v.as_str())
        .map_or_else(|| default_data_dir(&name), PathBuf::from);

    let instance_id = format!("p2p-{name}");
    let cfg = P2pBackendConfig {
        folder_id: instance_id.clone(),
        instance_id,
        display_name,
        index_path: data_dir.join("index.db"),
        block_store_root: data_dir.join("blocks"),
        identity_dir: data_dir.join("identity"),
    };

    // Open is async — block on a runtime handle. The CLI is already in a
    // tokio context when this is called.
    let rt = tokio::runtime::Handle::current();
    let backend = rt.block_on(P2pBackend::open(cfg))?;
    Ok(Box::new(backend))
}

fn default_data_dir(name: &str) -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("cascade")
        .join(format!("p2p-{name}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::io::Cursor as IoCursor;
    use tempfile::tempdir;

    async fn make_backend() -> (tempfile::TempDir, P2pBackend) {
        let dir = tempdir().unwrap();
        let cfg = P2pBackendConfig {
            instance_id: "p2p-test".to_string(),
            folder_id: "p2p-test".to_string(),
            display_name: "Test".to_string(),
            index_path: dir.path().join("index.db"),
            block_store_root: dir.path().join("blocks"),
            identity_dir: dir.path().join("identity"),
        };
        let backend = P2pBackend::open(cfg).await.unwrap();
        (dir, backend)
    }

    #[tokio::test]
    async fn upload_then_download_round_trips() {
        let (_dir, backend) = make_backend().await;
        let data = b"hello world".repeat(1000);
        let mut reader: IoCursor<Vec<u8>> = IoCursor::new(data.clone());
        let entry = backend
            .upload(
                Path::new("hello.txt"),
                &mut reader,
                &FileId("p2p-test:root".to_string()),
            )
            .await
            .unwrap();
        assert_eq!(entry.name, "hello.txt");
        assert_eq!(entry.size, Some(data.len() as u64));

        let mut out: Vec<u8> = Vec::new();
        backend.download(&entry, &mut out).await.unwrap();
        assert_eq!(out, data);
    }

    #[tokio::test]
    async fn list_children_after_uploads() {
        let (_dir, backend) = make_backend().await;
        let mut reader = IoCursor::new(b"a".to_vec());
        backend
            .upload(
                Path::new("a.txt"),
                &mut reader,
                &FileId("p2p-test:root".to_string()),
            )
            .await
            .unwrap();
        let mut reader2 = IoCursor::new(b"b".to_vec());
        backend
            .upload(
                Path::new("b.txt"),
                &mut reader2,
                &FileId("p2p-test:root".to_string()),
            )
            .await
            .unwrap();
        let kids = backend.list_children("root").await.unwrap();
        let names: Vec<_> = kids.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&"a.txt".to_string()));
        assert!(names.contains(&"b.txt".to_string()));
    }

    #[tokio::test]
    async fn changes_after_upload() {
        let (_dir, backend) = make_backend().await;
        let (initial, c0) = backend.changes(None).await.unwrap();
        assert!(initial.is_empty());

        let mut reader = IoCursor::new(b"data".to_vec());
        backend
            .upload(
                Path::new("x.txt"),
                &mut reader,
                &FileId("p2p-test:root".to_string()),
            )
            .await
            .unwrap();

        let (deltas, _c1) = backend.changes(Some(&c0)).await.unwrap();
        assert_eq!(deltas.len(), 1);
        assert!(matches!(deltas[0], Change::Created(_)));
    }

    #[tokio::test]
    async fn delete_marks_tombstone_excluded_from_listing() {
        let (_dir, backend) = make_backend().await;
        let mut reader = IoCursor::new(b"x".to_vec());
        let entry = backend
            .upload(
                Path::new("x.txt"),
                &mut reader,
                &FileId("p2p-test:root".to_string()),
            )
            .await
            .unwrap();
        backend.delete(&entry).await.unwrap();
        let kids = backend.list_children("root").await.unwrap();
        assert!(kids.is_empty());
    }

    /// End-to-end: A uploads through the Backend trait, B connects, and
    /// B's `download()` succeeds even though B's local block store is
    /// empty — the missing blocks must be fetched from A over the wire.
    #[tokio::test]
    async fn cross_backend_download_via_peer_fetch() {
        async fn open_with_folder(dir: &std::path::Path, name: &str) -> P2pBackend {
            let cfg = P2pBackendConfig {
                instance_id: format!("p2p-{name}"),
                folder_id: "shared".to_string(),
                display_name: name.to_string(),
                index_path: dir.join("index.db"),
                block_store_root: dir.join("blocks"),
                identity_dir: dir.join("identity"),
            };
            P2pBackend::open(cfg).await.unwrap()
        }
        let dir_a = tempdir().unwrap();
        let dir_b = tempdir().unwrap();
        let backend_a = open_with_folder(dir_a.path(), "a").await;
        let backend_b = open_with_folder(dir_b.path(), "b").await;

        backend_a
            .sync()
            .trust(backend_b.sync().device_id().to_string())
            .await;
        backend_b
            .sync()
            .trust(backend_a.sync().device_id().to_string())
            .await;

        let (addr_a, _a_task) = backend_a
            .sync()
            .start_listener("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        backend_b
            .sync()
            .connect_to(crate::sync::Peer {
                device_id: backend_a.sync().device_id().to_string(),
                address: addr_a,
            })
            .await
            .unwrap();

        let payload = b"peer-to-peer round trip".repeat(50);
        let mut reader = IoCursor::new(payload.clone());
        let entry_a = backend_a
            .upload(
                Path::new("shared.bin"),
                &mut reader,
                &FileId(format!("{}:root", backend_a.id())),
            )
            .await
            .unwrap();

        // Let the IndexUpdate broadcast and the handshake Index reach B.
        let mut found = None;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            if let Some(local) = backend_b.index.get("shared.bin").unwrap() {
                found = Some(local);
                break;
            }
        }
        let local_b = found.expect("B never received index update");
        assert_eq!(local_b.size, entry_a.size.unwrap());
        // B's block store is empty — download must hit the peer.
        for chunk in local_b.block_hashes.chunks(32) {
            let mut h = [0u8; 32];
            h.copy_from_slice(chunk);
            assert!(
                backend_b
                    .blocks
                    .get_block(&BlockHash(h))
                    .await
                    .unwrap()
                    .is_none()
            );
        }

        let entry_b = backend_b.metadata(Path::new("shared.bin")).await.unwrap();
        let mut out: Vec<u8> = Vec::new();
        backend_b.download(&entry_b, &mut out).await.unwrap();
        assert_eq!(out, payload);
    }
}
