//! Content-addressed block store.
//!
//! Blocks are stored under `~/.config/cascade/blocks/` using a sharded
//! directory layout: `blocks/{hash_prefix}/{hash}.blk` where `hash_prefix`
//! is the first two hex characters of the SHA-256 digest.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;

use crate::block::{BlockHash, FileBlocks, split_data};

/// Default root directory for block storage.
const BLOCKS_DIR_NAME: &str = "blocks";

/// Content-addressed block store.
///
/// Stores blocks on disk in a sharded directory layout and maintains an
/// in-memory index mapping file IDs to their block descriptions.
#[derive(Debug)]
pub struct BlockStore {
    root: PathBuf,
}

impl BlockStore {
    /// Create a new block store rooted at the given directory.
    ///
    /// The directory is created if it does not exist. This is a
    /// synchronous one-off — the per-block read/write paths remain
    /// async and unaffected.
    pub fn new(root: &Path) -> Result<Self> {
        let blocks_dir = root.join(BLOCKS_DIR_NAME);
        std::fs::create_dir_all(&blocks_dir)
            .with_context(|| format!("creating blocks directory {}", blocks_dir.display()))?;
        Ok(Self { root: blocks_dir })
    }

    /// Return the default root path (`~/.config/cascade/`).
    pub fn default_root() -> Result<PathBuf> {
        let config_dir = dirs_sys::config_dir()?;
        Ok(config_dir.join("cascade"))
    }

    /// Store a block. Data is written to disk keyed by its SHA-256 hash.
    /// If a block with the same hash already exists, this is a no-op.
    pub async fn store_block(&self, hash: &BlockHash, data: &[u8]) -> Result<()> {
        let path = self.block_path(hash);
        if path.exists() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::File::create(&path).await?;
        file.write_all(data).await?;
        file.flush().await?;
        Ok(())
    }

    /// Retrieve a block by hash. Returns `None` if not found.
    pub async fn get_block(&self, hash: &BlockHash) -> Result<Option<Vec<u8>>> {
        let path = self.block_path(hash);
        if !path.exists() {
            return Ok(None);
        }
        let data = tokio::fs::read(&path).await?;
        Ok(Some(data))
    }

    /// Check whether a block exists in the store.
    #[must_use]
    pub fn has_block(&self, hash: &BlockHash) -> bool {
        self.block_path(hash).exists()
    }

    /// Split file data into blocks and store each one. Returns the
    /// [`FileBlocks`] describing the file's block structure.
    pub async fn index_data(&self, data: &[u8]) -> Result<FileBlocks> {
        let file_blocks = split_data(data);
        for block_data in data.chunks(file_blocks.block_size as usize) {
            let hash = BlockHash::from_data(block_data);
            self.store_block(&hash, block_data).await?;
        }
        Ok(file_blocks)
    }

    /// Reassemble a file from its blocks. Reads each block from disk and
    /// concatenates them in order. Returns the reassembled bytes.
    pub async fn reassemble(&self, blocks: &FileBlocks) -> Result<Vec<u8>> {
        let capacity = usize::try_from(blocks.size)
            .map_err(|_| anyhow::anyhow!("file size too large for allocation on this platform"))?;
        let mut output = Vec::with_capacity(capacity);
        for hash in &blocks.blocks {
            let data = self
                .get_block(hash)
                .await?
                .ok_or_else(|| anyhow::anyhow!("missing block {hash}"))?;
            output.extend_from_slice(&data);
        }
        Ok(output)
    }

    /// Delete a block from the store. Used for cleanup/eviction.
    pub async fn remove_block(&self, hash: &BlockHash) -> Result<()> {
        let path = self.block_path(hash);
        if path.exists() {
            tokio::fs::remove_file(&path).await?;
        }
        Ok(())
    }

    /// Return the on-disk path for a block hash.
    fn block_path(&self, hash: &BlockHash) -> PathBuf {
        let hex = hash.to_string();
        // The hex string is always 64 ASCII characters (32 bytes × 2 hex chars).
        // Taking the first 2 bytes is always valid.
        let prefix = hex.get(..2).unwrap_or("xx");
        self.root.join(prefix).join(format!("{hex}.blk"))
    }
}

/// Platform-agnostic config directory resolution. Avoids pulling in the
/// `dirs` crate for a single function.
mod dirs_sys {
    use std::path::PathBuf;

    pub fn config_dir() -> Result<PathBuf, anyhow::Error> {
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        {
            let home = std::env::var("HOME").map_err(|_| anyhow::anyhow!("HOME not set"))?;
            Ok(PathBuf::from(home).join("Library/Application Support"))
        }

        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            std::env::var("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|_| {
                    let home =
                        std::env::var("HOME").map_err(|_| anyhow::anyhow!("HOME not set"))?;
                    Ok(PathBuf::from(home).join(".config"))
                })
        }

        #[cfg(target_os = "windows")]
        {
            std::env::var("APPDATA")
                .map(PathBuf::from)
                .map_err(|_| anyhow::anyhow!("APPDATA not set"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn store_and_retrieve_block() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlockStore::new(dir.path()).unwrap();

        let data = b"hello block store";
        let hash = BlockHash::from_data(data);

        store.store_block(&hash, data).await.unwrap();
        let retrieved = store.get_block(&hash).await.unwrap().unwrap();
        assert_eq!(retrieved, data);
    }

    #[tokio::test]
    async fn duplicate_store_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlockStore::new(dir.path()).unwrap();

        let data = b"duplicate";
        let hash = BlockHash::from_data(data);

        store.store_block(&hash, data).await.unwrap();
        store.store_block(&hash, data).await.unwrap();

        let retrieved = store.get_block(&hash).await.unwrap().unwrap();
        assert_eq!(retrieved, data);
    }

    #[tokio::test]
    async fn missing_block_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlockStore::new(dir.path()).unwrap();

        let hash = BlockHash::from_data(b"nonexistent");
        let result = store.get_block(&hash).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn has_block_check() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlockStore::new(dir.path()).unwrap();

        let data = b"check me";
        let hash = BlockHash::from_data(data);

        assert!(!store.has_block(&hash));
        store.store_block(&hash, data).await.unwrap();
        assert!(store.has_block(&hash));
    }

    #[tokio::test]
    async fn index_and_reassemble_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlockStore::new(dir.path()).unwrap();

        // Create data spanning multiple blocks.
        let original = vec![0xAA; 128 * 1024 * 3 + 512];
        let file_blocks = store.index_data(&original).await.unwrap();

        assert_eq!(file_blocks.size, original.len() as u64);
        assert_eq!(file_blocks.block_count(), 4);

        let reassembled = store.reassemble(&file_blocks).await.unwrap();
        assert_eq!(reassembled, original);
    }

    #[tokio::test]
    async fn block_path_sharding() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlockStore::new(dir.path()).unwrap();

        let data = b"shard test";
        let hash = BlockHash::from_data(data);
        let path = store.block_path(&hash);

        let hex = hash.to_string();
        assert_eq!(
            path,
            dir.path()
                .join("blocks")
                .join(&hex[..2])
                .join(format!("{hex}.blk"))
        );
    }

    #[tokio::test]
    async fn remove_block() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlockStore::new(dir.path()).unwrap();

        let data = b"to be removed";
        let hash = BlockHash::from_data(data);

        store.store_block(&hash, data).await.unwrap();
        assert!(store.has_block(&hash));

        store.remove_block(&hash).await.unwrap();
        assert!(!store.has_block(&hash));
    }
}
