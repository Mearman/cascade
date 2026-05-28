//! Drive API data model — File, Folder, Shared Drive types.

use cascade_engine::types::{FileEntry, ItemId};
use chrono::{DateTime, FixedOffset};
use serde::Deserialize;

/// A file or folder from the Drive API.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveFile {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    #[serde(default)]
    pub parents: Vec<String>,
    pub size: Option<String>,
    pub modified_time: Option<String>,
    pub md5_checksum: Option<String>,
    #[serde(default)]
    pub trashed: bool,
}

impl DriveFile {
    /// Convert to a Cascade FileEntry.
    pub fn to_file_entry(&self, backend_id: &str) -> Option<FileEntry> {
        if self.trashed {
            return None;
        }
        let parent_id = self
            .parents
            .first()
            .map(|p| ItemId::new(backend_id, p))
            .unwrap_or(ItemId::new(backend_id, "root"));

        let mod_time = self
            .modified_time
            .as_ref()
            .and_then(|t| DateTime::<FixedOffset>::parse_from_rfc3339(t).ok())
            .map(|dt| dt.to_utc());

        Some(FileEntry {
            id: ItemId::new(backend_id, &self.id),
            parent_id,
            name: self.name.clone(),
            is_dir: self.mime_type == "application/vnd.google-apps.folder",
            size: self.size.as_ref().and_then(|s| s.parse::<u64>().ok()),
            mod_time,
            mime_type: Some(self.mime_type.clone()),
            hash: self.md5_checksum.clone(),
        })
    }
}

/// About (quota) response from the Drive API.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AboutResponse {
    pub storage_quota: Option<StorageQuota>,
}

/// Storage quota from the Drive API.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageQuota {
    pub limit: Option<String>,
    pub usage: Option<String>,
}

/// A change from the Drive API Changes stream.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveChange {
    pub kind: Option<String>,
    pub file_id: Option<String>,
    pub file: Option<DriveFile>,
    pub removed: Option<bool>,
}

/// Changes list response.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangesResponse {
    pub changes: Vec<DriveChange>,
    pub next_page_token: Option<String>,
    pub new_start_page_token: Option<String>,
}

/// File list response from files.list.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileListResponse {
    pub files: Vec<DriveFile>,
    pub next_page_token: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_file_to_file_entry() {
        let df = DriveFile {
            id: "abc123".to_string(),
            name: "test.txt".to_string(),
            mime_type: "text/plain".to_string(),
            parents: vec!["parent1".to_string()],
            size: Some("1024".to_string()),
            modified_time: Some("2026-05-27T10:00:00Z".to_string()),
            md5_checksum: Some("hash123".to_string()),
            trashed: false,
        };
        let entry = df.to_file_entry("gdrive").unwrap();
        assert_eq!(entry.name, "test.txt");
        assert!(!entry.is_dir);
        assert_eq!(entry.size, Some(1024));
    }

    #[test]
    fn trashed_file_skipped() {
        let df = DriveFile {
            id: "abc".to_string(),
            name: "trash.txt".to_string(),
            mime_type: "text/plain".to_string(),
            parents: vec![],
            size: None,
            modified_time: None,
            md5_checksum: None,
            trashed: true,
        };
        assert!(df.to_file_entry("gdrive").is_none());
    }

    #[test]
    fn folder_detected() {
        let df = DriveFile {
            id: "folder1".to_string(),
            name: "Documents".to_string(),
            mime_type: "application/vnd.google-apps.folder".to_string(),
            parents: vec!["root".to_string()],
            size: None,
            modified_time: None,
            md5_checksum: None,
            trashed: false,
        };
        let entry = df.to_file_entry("gdrive").unwrap();
        assert!(entry.is_dir);
    }
}
