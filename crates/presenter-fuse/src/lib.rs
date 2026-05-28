//! FUSE presenter — exposes the VFS tree as a mounted Linux FUSE filesystem.
//!
//! Implements the engine's `VfsPresenter` trait. On Linux, the FUSE mount is
//! served via the `fuser` crate. On other platforms, all operations return
//! errors — the crate compiles but does not mount.

pub mod inode;
pub mod ops;

use std::path::Path;

use async_trait::async_trait;
use cascade_engine::presenter::VfsPresenter;
use cascade_engine::types::{CacheState, ItemId, VfsItem};

#[cfg(target_os = "linux")]
use crate::ops::FuseOps;

/// FUSE presenter wrapping an inode map and VFS tree reference.
pub struct FusePresenter {
    /// The root ItemId for the VFS tree.
    #[allow(dead_code)] // Used on Linux in start()
    root_id: ItemId,
    /// Mount point path.
    mount_point: std::path::PathBuf,
}

impl FusePresenter {
    /// Create a new FUSE presenter for the given root ItemId.
    pub fn new(root_id: ItemId) -> Self {
        Self {
            root_id,
            mount_point: std::path::PathBuf::from("/mnt/cascade"),
        }
    }

    /// Set a custom mount point.
    pub fn with_mount_point(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.mount_point = path.into();
        self
    }
}

#[async_trait]
impl VfsPresenter for FusePresenter {
    async fn upsert_item(&self, item: VfsItem) -> anyhow::Result<()> {
        tracing::debug!(id = %item.id, name = %item.name, "upsert_item");
        // The inode map is managed inside FuseOps during FUSE runtime.
        // When the engine pushes an upsert, the inode will be allocated on
        // the next lookup/getattr if not already present.
        Ok(())
    }

    async fn delete_item(&self, id: &ItemId) -> anyhow::Result<()> {
        tracing::debug!(id = %id, "delete_item");
        // TODO: Remove from inode map and invalidate FUSE cache entry.
        Ok(())
    }

    async fn update_state(&self, id: &ItemId, state: CacheState) -> anyhow::Result<()> {
        tracing::debug!(id = %id, state = %state, "update_state");
        Ok(())
    }

    async fn fetch_contents(&self, id: &ItemId) -> anyhow::Result<std::path::PathBuf> {
        tracing::debug!(id = %id, "fetch_contents");
        // TODO: Download file to cache and return local path.
        anyhow::bail!("fetch_contents not yet implemented")
    }

    async fn evict_item(&self, id: &ItemId) -> anyhow::Result<()> {
        tracing::debug!(id = %id, "evict_item");
        // TODO: Remove cached file, mark as Online.
        Ok(())
    }

    async fn start(&self, mount_point: &Path) -> anyhow::Result<()> {
        let mount_display = mount_point.display();

        #[cfg(target_os = "linux")]
        {
            tracing::info!(mount_point = %mount_display, "starting FUSE presenter");

            let ops = FuseOps::new(self.root_id.clone());
            let mp = mount_point.to_path_buf();

            // Spawn the FUSE session in a background task.
            // fuser::spawn_mount2 blocks, so we run it in a dedicated thread.
            std::thread::spawn(move || {
                let config = fuser::Config {
                    mount_options: vec![
                        fuser::MountOption::RO,
                        fuser::MountOption::FSName("cascade".to_string()),
                        fuser::MountOption::DefaultPermissions,
                    ],
                    ..fuser::Config::default()
                };
                match fuser::spawn_mount2(ops, &mp, &config) {
                    Ok(_session) => {
                        tracing::info!("FUSE session ended");
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "FUSE mount failed");
                    }
                }
            });

            Ok(())
        }

        #[cfg(not(target_os = "linux"))]
        {
            tracing::warn!(
                mount_point = %mount_display,
                "FUSE presenter is not available on this platform (Linux only)"
            );
            anyhow::bail!(
                "FUSE presenter requires Linux. Current platform does not support FUSE mounting."
            )
        }
    }

    async fn stop(&self) -> anyhow::Result<()> {
        tracing::info!("stopping FUSE presenter");

        #[cfg(target_os = "linux")]
        {
            // TODO: Signal the FUSE session to unmount and wait for the thread.
            // fuser sessions unmount when dropped, or via fuser::unmount().
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuse_presenter_new() {
        let root = ItemId::new("gdrive", "root");
        let presenter = FusePresenter::new(root.clone());
        assert_eq!(presenter.root_id, root);
        assert_eq!(
            presenter.mount_point,
            std::path::PathBuf::from("/mnt/cascade")
        );
    }

    #[test]
    fn fuse_presenter_custom_mount() {
        let root = ItemId::new("gdrive", "root");
        let presenter = FusePresenter::new(root).with_mount_point("/media/cascade");
        assert_eq!(
            presenter.mount_point,
            std::path::PathBuf::from("/media/cascade")
        );
    }

    #[tokio::test]
    async fn start_fails_on_non_linux() {
        let root = ItemId::new("gdrive", "root");
        let presenter = FusePresenter::new(root);

        #[cfg(not(target_os = "linux"))]
        {
            let result = presenter.start(Path::new("/mnt/test")).await;
            assert!(result.is_err());
            let err = result.unwrap_err().to_string();
            assert!(
                err.contains("Linux"),
                "expected Linux-specific error, got: {err}"
            );
        }

        #[cfg(target_os = "linux")]
        {
            // On Linux, start would attempt a real mount — skip in unit tests.
        }
    }

    #[tokio::test]
    async fn upsert_item_is_ok() {
        let root = ItemId::new("gdrive", "root");
        let presenter = FusePresenter::new(root);

        let item = VfsItem {
            id: ItemId::new("gdrive", "file1"),
            parent_id: ItemId::new("gdrive", "root"),
            name: "test.txt".to_string(),
            is_dir: false,
            size: Some(100),
            mod_time: None,
            cache_state: CacheState::Online,
            mime_type: None,
        };

        let result = presenter.upsert_item(item).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn delete_item_is_ok() {
        let root = ItemId::new("gdrive", "root");
        let presenter = FusePresenter::new(root);
        let id = ItemId::new("gdrive", "file1");

        let result = presenter.delete_item(&id).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn fetch_contents_not_implemented() {
        let root = ItemId::new("gdrive", "root");
        let presenter = FusePresenter::new(root);
        let id = ItemId::new("gdrive", "file1");

        let result = presenter.fetch_contents(&id).await;
        assert!(result.is_err());
    }
}
