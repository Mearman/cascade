#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::string_slice
    )
)]
//! macOS `FSKit` presenter bridge.
//!
//! Connects the Cascade engine to Apple's `FSKit` framework on macOS 15.4+ (Tahoe
//! / macOS 26). `FSKit` is the modern, kext-free replacement for both FUSE and
//! File Provider — it exposes a full POSIX filesystem via Finder, `mount`, and
//! every standard I/O API without requiring a kernel extension.
//!
//! # Architecture
//!
//! The crate mirrors the pattern established by `cascade-presenter-fileprovider`:
//!
//! - **Rust side** (`FSKitPresenter`) implements `VfsPresenter`, communicating
//!   with the Swift `FSKit` extension over Cascade's length-prefixed JSON protocol
//!   on a Unix domain socket.
//! - **Swift side** (`CascadeFSKit/`) is an `FSKit` `FSUnaryFileSystem` extension
//!   that receives filesystem callbacks from the kernel and translates them into
//!   protocol messages to the engine.
//!
//! On non-macOS platforms, every operation fails with a clear error message.
//! The crate compiles everywhere — it does not link `FSKit` directly.

pub mod bridge;
pub mod items;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use cascade_engine::presenter::VfsPresenter;
use cascade_engine::types::{CacheState, ItemId, VfsItem};
use serde::Deserialize;
use serde_json::json;

use crate::bridge::FSKitBridge;
use crate::items::FSKitItem;

/// Presenter that bridges the Cascade engine to macOS `FSKit`.
///
/// `FSKit` is available on macOS 15.4+ and is Apple's recommended replacement for
/// FUSE and File Provider. Unlike File Provider, `FSKit` presents a real POSIX
/// filesystem with full `getattr` / `setattr` / `lookup` / `read` / `write`
/// semantics — not a shallow content-sync surface.
#[derive(Debug)]
pub struct FSKitPresenter {
    bridge: FSKitBridge,
    mount_point: PathBuf,
}

impl FSKitPresenter {
    /// Create a new `FSKit` presenter that talks to the extension over the given socket.
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            bridge: FSKitBridge::new(socket_path),
            mount_point: PathBuf::from("/Volumes/Cascade"),
        }
    }

    /// Create using the default socket path (`~/.config/cascade/fskit.sock`).
    pub fn from_default_socket() -> Result<Self> {
        Ok(Self {
            bridge: FSKitBridge::from_default_socket()?,
            mount_point: PathBuf::from("/Volumes/Cascade"),
        })
    }

    /// Set a custom mount point (the directory where the volume appears in Finder).
    pub fn with_mount_point(mut self, mount_point: impl Into<PathBuf>) -> Self {
        self.mount_point = mount_point.into();
        self
    }

    /// Return the configured mount point.
    pub fn mount_point(&self) -> &Path {
        &self.mount_point
    }

    /// Return a reference to the underlying bridge client.
    pub const fn bridge(&self) -> &FSKitBridge {
        &self.bridge
    }
}

#[derive(Debug, Deserialize)]
struct FetchContentsResponse {
    path: PathBuf,
}

#[async_trait]
impl VfsPresenter for FSKitPresenter {
    async fn upsert_item(&self, item: VfsItem) -> Result<()> {
        #[cfg(not(target_os = "macos"))]
        ensure_not_macos()?;
        let item = FSKitItem::from(item);
        self.bridge
            .request_empty("upsertItem", json!({ "item": item }))
            .await
    }

    async fn delete_item(&self, id: &ItemId) -> Result<()> {
        #[cfg(not(target_os = "macos"))]
        ensure_not_macos()?;
        self.bridge
            .request_empty("deleteItem", json!({ "id": id.to_string() }))
            .await
    }

    async fn update_state(&self, id: &ItemId, state: CacheState) -> Result<()> {
        #[cfg(not(target_os = "macos"))]
        ensure_not_macos()?;
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
        #[cfg(not(target_os = "macos"))]
        ensure_not_macos()?;
        let response: FetchContentsResponse = self
            .bridge
            .request("fetchContents", json!({ "id": id.to_string() }))
            .await?;
        Ok(response.path)
    }

    async fn evict_item(&self, id: &ItemId) -> Result<()> {
        #[cfg(not(target_os = "macos"))]
        ensure_not_macos()?;
        self.bridge
            .request_empty("evictItem", json!({ "id": id.to_string() }))
            .await
    }

    async fn start(&self, mount_point: &Path) -> Result<()> {
        #[cfg(not(target_os = "macos"))]
        ensure_not_macos()?;
        tracing::info!(mount_point = %mount_point.display(), "starting FSKit presenter");
        self.bridge
            .request_empty(
                "startPresenter",
                json!({ "mount_point": mount_point.to_string_lossy() }),
            )
            .await
            .with_context(|| format!("start FSKit presenter at {}", mount_point.display()))
    }

    async fn stop(&self) -> Result<()> {
        #[cfg(not(target_os = "macos"))]
        ensure_not_macos()?;
        tracing::info!("stopping FSKit presenter");
        self.bridge.request_empty("stopPresenter", json!({})).await
    }
}

#[cfg(not(target_os = "macos"))]
fn ensure_not_macos() -> Result<()> {
    anyhow::bail!("FSKit presenter requires macOS 15.4+")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presenter_uses_default_mount_point() {
        let presenter = FSKitPresenter::new("/tmp/cascade-fskit-test.sock");
        assert_eq!(presenter.mount_point(), Path::new("/Volumes/Cascade"));
    }

    #[test]
    fn presenter_accepts_custom_mount_point() {
        let presenter = FSKitPresenter::new("/tmp/cascade-fskit-test.sock")
            .with_mount_point("/tmp/cascade-mount");

        assert_eq!(presenter.mount_point(), Path::new("/tmp/cascade-mount"));
    }

    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn presenter_fails_outside_macos() {
        let presenter = FSKitPresenter::new("/tmp/cascade-fskit-test.sock");
        let id = ItemId::new("gdrive", "file1");

        let err = presenter.delete_item(&id).await.unwrap_err().to_string();
        assert!(err.contains("macOS"));
    }
}
