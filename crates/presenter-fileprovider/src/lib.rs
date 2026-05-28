//! macOS File Provider presenter bridge.
//!
//! The presenter sends VFS change notifications to the Swift File Provider
//! extension over Cascade's length-prefixed JSON protocol. The crate compiles
//! on every platform; operations fail clearly outside macOS.

pub mod bridge;
pub mod items;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use cascade_engine::presenter::VfsPresenter;
use cascade_engine::types::{CacheState, ItemId, VfsItem};
use serde::Deserialize;
use serde_json::json;

use crate::bridge::FileProviderBridge;
use crate::items::FileProviderItem;

/// Presenter that bridges the Cascade engine to macOS File Provider.
#[derive(Debug)]
pub struct FileProviderPresenter {
    bridge: FileProviderBridge,
    mount_point: PathBuf,
}

impl FileProviderPresenter {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            bridge: FileProviderBridge::new(socket_path),
            mount_point: PathBuf::from("/Volumes/Cascade"),
        }
    }

    pub fn from_default_socket() -> Result<Self> {
        Ok(Self {
            bridge: FileProviderBridge::from_default_socket()?,
            mount_point: PathBuf::from("/Volumes/Cascade"),
        })
    }

    pub fn with_mount_point(mut self, mount_point: impl Into<PathBuf>) -> Self {
        self.mount_point = mount_point.into();
        self
    }

    pub fn mount_point(&self) -> &Path {
        &self.mount_point
    }

    pub fn bridge(&self) -> &FileProviderBridge {
        &self.bridge
    }
}

#[derive(Debug, Deserialize)]
struct FetchContentsResponse {
    path: PathBuf,
}

#[async_trait]
impl VfsPresenter for FileProviderPresenter {
    async fn upsert_item(&self, item: VfsItem) -> Result<()> {
        ensure_file_provider_available()?;
        let item = FileProviderItem::from(item);
        self.bridge
            .request_empty("upsertItem", json!({ "item": item }))
            .await
    }

    async fn delete_item(&self, id: &ItemId) -> Result<()> {
        ensure_file_provider_available()?;
        self.bridge
            .request_empty("deleteItem", json!({ "id": id.to_string() }))
            .await
    }

    async fn update_state(&self, id: &ItemId, state: CacheState) -> Result<()> {
        ensure_file_provider_available()?;
        self.bridge
            .request_empty(
                "updateState",
                json!({
                    "id": id.to_string(),
                    "state": state,
                }),
            )
            .await
    }

    async fn fetch_contents(&self, id: &ItemId) -> Result<PathBuf> {
        ensure_file_provider_available()?;
        let response: FetchContentsResponse = self
            .bridge
            .request("fetchContents", json!({ "id": id.to_string() }))
            .await?;
        Ok(response.path)
    }

    async fn evict_item(&self, id: &ItemId) -> Result<()> {
        ensure_file_provider_available()?;
        self.bridge
            .request_empty("evictItem", json!({ "id": id.to_string() }))
            .await
    }

    async fn start(&self, mount_point: &Path) -> Result<()> {
        ensure_file_provider_available()?;
        tracing::info!(mount_point = %mount_point.display(), "starting File Provider presenter");
        self.bridge
            .request_empty(
                "startPresenter",
                json!({ "mount_point": mount_point.to_string_lossy() }),
            )
            .await
            .with_context(|| format!("start File Provider presenter at {}", mount_point.display()))
    }

    async fn stop(&self) -> Result<()> {
        ensure_file_provider_available()?;
        tracing::info!("stopping File Provider presenter");
        self.bridge.request_empty("stopPresenter", json!({})).await
    }
}

fn ensure_file_provider_available() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    {
        anyhow::bail!("File Provider presenter requires macOS")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presenter_uses_default_mount_point() {
        let presenter = FileProviderPresenter::new("/tmp/cascade-fileprovider-test.sock");
        assert_eq!(presenter.mount_point(), Path::new("/Volumes/Cascade"));
    }

    #[test]
    fn presenter_accepts_custom_mount_point() {
        let presenter = FileProviderPresenter::new("/tmp/cascade-fileprovider-test.sock")
            .with_mount_point("/Users/example/Library/CloudStorage/Cascade");

        assert_eq!(
            presenter.mount_point(),
            Path::new("/Users/example/Library/CloudStorage/Cascade")
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn presenter_fails_outside_macos() {
        let presenter = FileProviderPresenter::new("/tmp/cascade-fileprovider-test.sock");
        let id = ItemId::new("gdrive", "file1");

        let err = presenter.delete_item(&id).await.unwrap_err().to_string();
        assert!(err.contains("macOS"));
    }
}
