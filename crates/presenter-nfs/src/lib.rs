//! NFS presenter — NFSv3 server on loopback.
//!
//! Implements the engine's `VfsPresenter` trait, serving files via NFSv3
//! to the OS NFS client.

pub mod nfs;

use std::path::Path;

use async_trait::async_trait;
use cascade_engine::presenter::VfsPresenter;
use cascade_engine::types::{CacheState, ItemId, VfsItem};

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
        // The NFS server is started separately in the mount command.
        // This method is for the presenter trait interface.
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        tracing::info!("stopping NFS presenter");
        // The NFS server is stopped separately in the mount command.
        Ok(())
    }
}
