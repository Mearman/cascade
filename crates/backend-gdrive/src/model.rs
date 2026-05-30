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
    /// Set on files that live inside a shared drive.
    pub drive_id: Option<String>,
}

impl DriveFile {
    /// Convert to a Cascade `FileEntry`, skipping trashed files.
    #[must_use]
    pub fn to_file_entry(&self, backend_id: &str) -> Option<FileEntry> {
        if self.trashed {
            return None;
        }
        Some(self.to_file_entry_with_parent(backend_id, None))
    }

    /// Convert to a `FileEntry` for the Bin view.
    ///
    /// Trashed items are included, and the parent is unconditionally set to
    /// the `__trash` synthetic virtual directory so PROPFIND filtering works.
    #[must_use]
    pub fn to_trash_entry(&self, backend_id: &str) -> FileEntry {
        self.to_file_entry_with_parent(backend_id, Some("__trash"))
    }

    /// Internal conversion helper.
    ///
    /// `override_parent` forces a specific native parent ID instead of using
    /// the value from the Drive API. Used when items must be reparented to a
    /// synthetic virtual directory (e.g. `__trash`, `__mydrive`).
    fn to_file_entry_with_parent(
        &self,
        backend_id: &str,
        override_parent: Option<&str>,
    ) -> FileEntry {
        // Default to the My Drive virtual directory when no explicit override
        // and no parent is supplied. Items with empty parents from the
        // Changes stream (e.g. orphans) would otherwise leak to the mount
        // root via the `root` alias.
        let parent_id = override_parent.map_or_else(
            || {
                self.parents.first().map_or_else(
                    || ItemId::new(backend_id, "__mydrive"),
                    |p| ItemId::new(backend_id, p),
                )
            },
            |p| ItemId::new(backend_id, p),
        );

        let mod_time = self
            .modified_time
            .as_ref()
            .and_then(|t| DateTime::<FixedOffset>::parse_from_rfc3339(t).ok())
            .map(|dt| dt.to_utc());

        FileEntry {
            id: ItemId::new(backend_id, &self.id),
            parent_id,
            name: self.name.clone(),
            is_dir: self.mime_type == "application/vnd.google-apps.folder",
            size: self.size.as_ref().and_then(|s| s.parse::<u64>().ok()),
            mod_time,
            mime_type: Some(self.mime_type.clone()),
            hash: self.md5_checksum.clone(),
        }
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

/// A shared drive (formerly "Team Drive") from the drives.list endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct SharedDrive {
    pub id: String,
    pub name: String,
}

impl SharedDrive {
    /// Convert to a `FileEntry` directory parented under the synthetic
    /// `__shared_drives` virtual directory.
    #[must_use]
    pub fn to_file_entry(&self, backend_id: &str) -> FileEntry {
        FileEntry {
            id: ItemId::new(backend_id, &self.id),
            parent_id: ItemId::new(backend_id, "__shared_drives"),
            name: self.name.clone(),
            is_dir: true,
            size: None,
            mod_time: None,
            mime_type: Some("application/vnd.google-apps.folder".to_string()),
            hash: None,
        }
    }
}

/// Response from the drives.list endpoint.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedDriveListResponse {
    pub drives: Vec<SharedDrive>,
    pub next_page_token: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_file(id: &str, name: &str, mime: &str, parents: &[&str], trashed: bool) -> DriveFile {
        DriveFile {
            id: id.to_string(),
            name: name.to_string(),
            mime_type: mime.to_string(),
            parents: parents.iter().map(|s| s.to_string()).collect(),
            size: None,
            modified_time: None,
            md5_checksum: None,
            trashed,
            drive_id: None,
        }
    }

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
            drive_id: None,
        };
        let entry = df.to_file_entry("gdrive").unwrap();
        assert_eq!(entry.name, "test.txt");
        assert!(!entry.is_dir);
        assert_eq!(entry.size, Some(1024));
    }

    #[test]
    fn trashed_file_skipped_by_to_file_entry() {
        let df = make_file("abc", "trash.txt", "text/plain", &[], true);
        assert!(df.to_file_entry("gdrive").is_none());
    }

    #[test]
    fn trashed_file_included_by_to_trash_entry() {
        let df = make_file("abc", "trash.txt", "text/plain", &["real_parent"], true);
        let entry = df.to_trash_entry("gdrive");
        assert_eq!(entry.name, "trash.txt");
        // Parent must be the synthetic __trash virtual directory.
        assert_eq!(entry.parent_id.native_id(), "__trash");
    }

    #[test]
    fn folder_detected() {
        let df = make_file(
            "folder1",
            "Documents",
            "application/vnd.google-apps.folder",
            &["root"],
            false,
        );
        let entry = df.to_file_entry("gdrive").unwrap();
        assert!(entry.is_dir);
    }

    #[test]
    fn shared_drive_to_file_entry() {
        let sd = SharedDrive {
            id: "0AB123".to_string(),
            name: "Engineering".to_string(),
        };
        let entry = sd.to_file_entry("gdrive-personal");
        assert_eq!(entry.name, "Engineering");
        assert!(entry.is_dir);
        assert_eq!(entry.id.native_id(), "0AB123");
        assert_eq!(entry.parent_id.native_id(), "__shared_drives");
    }
}
