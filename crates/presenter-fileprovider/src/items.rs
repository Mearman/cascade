use anyhow::{Context, Result};
use cascade_engine::types::{CacheState, ItemId, VfsItem};
use serde::{Deserialize, Serialize};

/// File Provider-facing representation of a VFS item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileProviderItem {
    pub id: String,
    pub parent_id: String,
    pub filename: String,
    pub is_directory: bool,
    pub size: Option<u64>,
    pub content_type: Option<String>,
    pub last_modified: Option<String>,
    pub cache_state: CacheState,
}

impl From<VfsItem> for FileProviderItem {
    fn from(item: VfsItem) -> Self {
        Self {
            id: item.id.0,
            parent_id: item.parent_id.0,
            filename: item.name,
            is_directory: item.is_dir,
            size: item.size,
            content_type: item.mime_type,
            last_modified: item.mod_time.map(|date| date.to_rfc3339()),
            cache_state: item.cache_state,
        }
    }
}

impl TryFrom<FileProviderItem> for VfsItem {
    type Error = anyhow::Error;

    fn try_from(item: FileProviderItem) -> Result<Self> {
        let mod_time = item
            .last_modified
            .map(|value| {
                chrono::DateTime::parse_from_rfc3339(&value)
                    .with_context(|| format!("invalid File Provider modification date: {value}"))
                    .map(|date| date.with_timezone(&chrono::Utc))
            })
            .transpose()?;

        Ok(Self {
            id: ItemId(item.id),
            parent_id: ItemId(item.parent_id),
            name: item.filename,
            is_dir: item.is_directory,
            size: item.size,
            mod_time,
            cache_state: item.cache_state,
            mime_type: item.content_type,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn vfs_item() -> VfsItem {
        VfsItem {
            id: ItemId::new("gdrive", "file1"),
            parent_id: ItemId::new("gdrive", "root"),
            name: "report.pdf".to_string(),
            is_dir: false,
            size: Some(4096),
            mod_time: Some(Utc.with_ymd_and_hms(2026, 5, 28, 6, 20, 0).unwrap()),
            cache_state: CacheState::Cached,
            mime_type: Some("application/pdf".to_string()),
        }
    }

    #[test]
    fn maps_vfs_item_to_file_provider_item() {
        let item = FileProviderItem::from(vfs_item());

        assert_eq!(item.id, "gdrive:file1");
        assert_eq!(item.parent_id, "gdrive:root");
        assert_eq!(item.filename, "report.pdf");
        assert!(!item.is_directory);
        assert_eq!(item.size, Some(4096));
        assert_eq!(item.content_type, Some("application/pdf".to_string()));
        assert_eq!(
            item.last_modified,
            Some("2026-05-28T06:20:00+00:00".to_string())
        );
        assert_eq!(item.cache_state, CacheState::Cached);
    }

    #[test]
    fn maps_file_provider_item_to_vfs_item() {
        let file_provider_item = FileProviderItem::from(vfs_item());
        let item = VfsItem::try_from(file_provider_item).unwrap();

        assert_eq!(item.id, ItemId::new("gdrive", "file1"));
        assert_eq!(item.parent_id, ItemId::new("gdrive", "root"));
        assert_eq!(item.name, "report.pdf");
        assert!(!item.is_dir);
        assert_eq!(item.size, Some(4096));
        assert_eq!(item.cache_state, CacheState::Cached);
        assert_eq!(item.mime_type, Some("application/pdf".to_string()));
        assert_eq!(
            item.mod_time,
            Some(Utc.with_ymd_and_hms(2026, 5, 28, 6, 20, 0).unwrap())
        );
    }

    #[test]
    fn rejects_invalid_file_provider_date() {
        let mut item = FileProviderItem::from(vfs_item());
        item.last_modified = Some("not-a-date".to_string());

        let err = VfsItem::try_from(item).unwrap_err().to_string();
        assert!(err.contains("invalid File Provider modification date"));
    }
}
