//! Google Drive backend (Phase 1: read-only).
//!
//! Uses the Drive API v3 with OAuth2 device code flow.
//! Change detection via the Changes API (cursor-based).

pub mod auth;
pub mod client;
pub mod model;


use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use cascade_engine::backend::Backend;
use cascade_engine::types::{Change, Cursor, FileEntry, FileId, Quota};

/// Create a Google Drive backend from config.
pub fn create_backend(_config: &toml::Value) -> anyhow::Result<Box<dyn Backend>> {
    Ok(Box::new(NullGdriveBackend))
}

/// Google Drive backend implementation.
struct NullGdriveBackend;

#[async_trait]
impl Backend for NullGdriveBackend {
    fn id(&self) -> &str {
        "gdrive"
    }

    fn display_name(&self) -> &str {
        "Google Drive"
    }

    async fn quota(&self) -> anyhow::Result<Option<Quota>> {
        Ok(None)
    }

    async fn changes(&self, _cursor: Option<&Cursor>) -> anyhow::Result<(Vec<Change>, Cursor)> {
        Ok((vec![], Cursor("start".to_string())))
    }

    async fn metadata(&self, path: &Path) -> anyhow::Result<FileEntry> {
        anyhow::bail!("gdrive metadata not yet implemented for {:?}", path)
    }

    async fn download(
        &self,
        _file: &FileEntry,
        _writer: &mut (dyn tokio::io::AsyncWrite + Unpin),
    ) -> anyhow::Result<()> {
        anyhow::bail!("gdrive download not yet implemented")
    }

    async fn upload(
        &self,
        _path: &Path,
        _reader: &mut (dyn tokio::io::AsyncRead + Unpin),
        _parent_id: &FileId,
    ) -> anyhow::Result<FileEntry> {
        anyhow::bail!("gdrive upload not yet implemented")
    }

    async fn create_dir(&self, _path: &Path) -> anyhow::Result<FileEntry> {
        anyhow::bail!("gdrive create_dir not yet implemented")
    }

    async fn delete(&self, _file: &FileEntry) -> anyhow::Result<()> {
        anyhow::bail!("gdrive delete not yet implemented")
    }

    async fn move_entry(&self, _src: &Path, _dst: &Path) -> anyhow::Result<FileEntry> {
        anyhow::bail!("gdrive move not yet implemented")
    }

    async fn poll_interval(&self) -> Option<Duration> {
        Some(Duration::from_secs(60))
    }
}
