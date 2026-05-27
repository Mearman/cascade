//! NFS presenter — NFSv3 server on loopback.
//!
//! Phase 1 provides the trait implementation and protocol structures.
//! The full NFS server will be completed in the NFS integration phase.

pub mod nfs;

use std::path::Path;

use async_trait::async_trait;
use cascade_engine::types::{CacheState, ItemId, VfsItem};

/// Platform-agnostic presenter trait for presenting the VFS to the OS.
#[async_trait]
pub trait VfsPresenter: Send + Sync {
    /// A file was added or updated in the VFS.
    async fn upsert_item(&self, item: VfsItem) -> anyhow::Result<()>;

    /// A file or directory was deleted from the VFS.
    async fn delete_item(&self, id: &ItemId) -> anyhow::Result<()>;

    /// A file's cache state changed.
    async fn update_state(&self, id: &ItemId, state: CacheState) -> anyhow::Result<()>;

    /// Download a file's contents (on-demand). Returns local path.
    async fn fetch_contents(&self, id: &ItemId) -> anyhow::Result<std::path::PathBuf>;

    /// Evict a file (free up space).
    async fn evict_item(&self, id: &ItemId) -> anyhow::Result<()>;

    /// Start presenting the VFS at the given mount point.
    async fn start(&self, mount_point: &Path) -> anyhow::Result<()>;

    /// Stop presenting.
    async fn stop(&self) -> anyhow::Result<()>;
}

/// NFS presenter using an NFSv3 server on loopback.
pub struct NfsPresenter {
    mount_point: std::path::PathBuf,
    nfs_port: u16,
}

impl NfsPresenter {
    pub fn new(mount_point: impl Into<std::path::PathBuf>) -> Self {
        Self {
            mount_point: mount_point.into(),
            nfs_port: 0, // OS-assigned
        }
    }

    /// Create with default mount point.
    pub fn default_mount() -> Self {
        Self {
            mount_point: std::path::PathBuf::from("/mnt/cascade"),
            nfs_port: 0,
        }
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.nfs_port = port;
        self
    }
}

#[async_trait]
impl VfsPresenter for NfsPresenter {
    async fn upsert_item(&self, _item: VfsItem) -> anyhow::Result<()> {
        // TODO: Invalidate NFS cache entry for this item
        Ok(())
    }

    async fn delete_item(&self, _id: &ItemId) -> anyhow::Result<()> {
        // TODO: Invalidate NFS cache entry
        Ok(())
    }

    async fn update_state(&self, _id: &ItemId, _state: CacheState) -> anyhow::Result<()> {
        Ok(())
    }

    async fn fetch_contents(&self, _id: &ItemId) -> anyhow::Result<std::path::PathBuf> {
        // TODO: Download file to cache and return local path
        anyhow::bail!("fetch_contents not yet implemented")
    }

    async fn evict_item(&self, _id: &ItemId) -> anyhow::Result<()> {
        // TODO: Remove cached file
        Ok(())
    }

    async fn start(&self, mount_point: &Path) -> anyhow::Result<()> {
        tracing::info!(
            mount_point = %mount_point.display(),
            port = self.nfs_port,
            "starting NFS presenter"
        );
        // TODO: Start NFSv3 server on loopback, mount via OS NFS client
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        tracing::info!("stopping NFS presenter");
        // TODO: Unmount NFS, stop server
        Ok(())
    }
}
