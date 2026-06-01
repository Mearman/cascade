//! Block splitting, hashing, and reassembly.
//!
//! Files are split into fixed-size blocks with adaptive sizing based on file
//! size. Each block is hashed with SHA-256 for content-addressed storage.

use std::path::Path;

use anyhow::Result;
use sha2::{Digest, Sha256};

/// SHA-256 digest — 32 bytes.
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub struct BlockHash(pub [u8; 32]);

impl BlockHash {
    /// Compute SHA-256 of the given data.
    #[must_use]
    pub fn from_data(data: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(data);
        let hash = hasher.finalize().into();
        Self(hash)
    }

    /// Return the raw bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Display for BlockHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Block-level description of a file's content.
#[derive(Debug, Clone)]
pub struct FileBlocks {
    /// File size in bytes.
    pub size: u64,
    /// Block size used for splitting.
    pub block_size: u32,
    /// SHA-256 hashes of each block, in order.
    pub blocks: Vec<BlockHash>,
}

impl FileBlocks {
    /// Number of blocks.
    #[must_use]
    pub const fn block_count(&self) -> usize {
        self.blocks.len()
    }
}

/// Block size thresholds.
const SIZE_250MB: u64 = 250 * 1024 * 1024;
const SIZE_1GB: u64 = 1024 * 1024 * 1024;

/// Block sizes.
pub(crate) const BLOCK_128KB: u32 = 128 * 1024;
const BLOCK_512KB: u32 = 512 * 1024;
const BLOCK_1MB: u32 = 1024 * 1024;

/// Determine block size for a file based on its total size.
///
/// - Under 250 MB → 128 KB blocks
/// - 250 MB to 1 GB → 512 KB blocks
/// - Over 1 GB → 1 MB blocks
#[must_use]
pub const fn block_size_for_file(file_size: u64) -> u32 {
    if file_size <= SIZE_250MB {
        BLOCK_128KB
    } else if file_size <= SIZE_1GB {
        BLOCK_512KB
    } else {
        BLOCK_1MB
    }
}

/// Split file data into blocks and hash each one.
///
/// Returns a [`FileBlocks`] describing the file's block structure.
/// The final block may be shorter than `block_size`.
pub fn split_data(data: &[u8]) -> FileBlocks {
    let file_size = data.len() as u64;
    let block_size = block_size_for_file(file_size);
    let blocks: Vec<BlockHash> = data
        .chunks(block_size as usize)
        .map(BlockHash::from_data)
        .collect();

    FileBlocks {
        size: file_size,
        block_size,
        blocks,
    }
}

/// Read a file, split into blocks, and hash each.
///
/// Convenience wrapper around [`split_data`] that reads from disk.
pub async fn split_file(path: &Path) -> Result<FileBlocks> {
    let data = tokio::fs::read(path).await?;
    Ok(split_data(&data))
}

/// Reassemble file content from an ordered sequence of block data slices.
///
/// `block_data` must be in block order and each slice must match the expected
/// block size (except the last, which may be shorter). The caller is
/// responsible for verifying block hashes before calling this.
#[must_use]
pub fn reassemble_blocks(block_data: &[Vec<u8>]) -> Vec<u8> {
    let total_size: usize = block_data.iter().map(std::vec::Vec::len).sum();
    let mut output = Vec::with_capacity(total_size);
    for block in block_data {
        output.extend_from_slice(block);
    }
    output
}

/// Reassemble blocks and write to a file.
pub async fn reassemble_to_file(block_data: &[Vec<u8>], output: &Path) -> Result<()> {
    let content = reassemble_blocks(block_data);
    tokio::fs::write(output, &content).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_size_adaptive() {
        assert_eq!(block_size_for_file(0), BLOCK_128KB);
        assert_eq!(block_size_for_file(SIZE_250MB), BLOCK_128KB);
        assert_eq!(block_size_for_file(SIZE_250MB + 1), BLOCK_512KB);
        assert_eq!(block_size_for_file(SIZE_1GB), BLOCK_512KB);
        assert_eq!(block_size_for_file(SIZE_1GB + 1), BLOCK_1MB);
    }

    #[test]
    fn split_empty() {
        let fb = split_data(&[]);
        assert_eq!(fb.size, 0);
        assert_eq!(fb.block_count(), 0);
        assert_eq!(fb.block_size, BLOCK_128KB);
    }

    #[test]
    fn split_single_block() {
        let data = vec![0xAB; 100];
        let fb = split_data(&data);
        assert_eq!(fb.size, 100);
        assert_eq!(fb.block_count(), 1);
        assert_eq!(fb.block_size, BLOCK_128KB);
    }

    #[test]
    fn split_multiple_blocks() {
        let data = vec![0x42; BLOCK_128KB as usize * 3 + 500];
        let fb = split_data(&data);
        assert_eq!(fb.size, data.len() as u64);
        assert_eq!(fb.block_count(), 4);
        // Last block is the remainder.
        assert_eq!(fb.blocks.len(), 4);
    }

    #[test]
    fn block_hash_deterministic() {
        let data = b"hello cascade";
        let h1 = BlockHash::from_data(data);
        let h2 = BlockHash::from_data(data);
        assert_eq!(h1, h2);
    }

    #[test]
    fn block_hash_different_data() {
        let h1 = BlockHash::from_data(b"foo");
        let h2 = BlockHash::from_data(b"bar");
        assert_ne!(h1, h2);
    }

    #[test]
    fn reassemble_round_trip() {
        let original = vec![0xDD; BLOCK_128KB as usize * 2 + 1024];
        let fb = split_data(&original);
        assert_eq!(fb.block_count(), 3);

        // Simulate storing and retrieving each block.
        let blocks: Vec<Vec<u8>> = original
            .chunks(fb.block_size as usize)
            .map(<[u8]>::to_vec)
            .collect();

        let reassembled = reassemble_blocks(&blocks);
        assert_eq!(reassembled, original);
    }

    #[test]
    fn block_hash_display() {
        let h = BlockHash::from_data(b"test");
        let s = h.to_string();
        assert_eq!(s.len(), 64);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn split_file_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.bin");
        let output = dir.path().join("output.bin");

        let data = vec![0x77; BLOCK_128KB as usize + 2048];
        tokio::fs::write(&input, &data).await.unwrap();

        let fb = split_file(&input).await.unwrap();
        assert_eq!(fb.size, data.len() as u64);

        let blocks: Vec<Vec<u8>> = data
            .chunks(fb.block_size as usize)
            .map(<[u8]>::to_vec)
            .collect();

        reassemble_to_file(&blocks, &output).await.unwrap();
        let result = tokio::fs::read(&output).await.unwrap();
        assert_eq!(result, data);
    }
}
