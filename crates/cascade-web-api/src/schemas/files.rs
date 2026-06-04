//! File and folder schemas.

use serde::{Deserialize, Serialize};

/// Whether an entry is a file or a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
}

/// A single directory entry or file's metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FileEntry {
    /// The entry's leaf name.
    pub name: String,
    /// The entry's path within the folder.
    pub path: String,
    /// Whether it is a file or a directory.
    pub kind: EntryKind,
    /// The size in bytes; `null` for directories.
    pub size: Option<u64>,
    /// The modification time, when known.
    pub mtime: Option<chrono::DateTime<chrono::Utc>>,
    /// The content etag, when known.
    pub etag: Option<String>,
}

/// `GET /v1/folders/{folder}/children` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FolderChildren {
    /// The canonical BEP folder id.
    pub folder: String,
    /// The directory path listed.
    pub path: String,
    /// The entries in the directory.
    pub entries: Vec<FileEntry>,
    /// The cursor for the next page, or `null` when exhausted.
    pub next_cursor: Option<String>,
}

/// `GET /v1/folders/{folder}/search` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SearchResponse {
    /// The canonical BEP folder id.
    pub folder: String,
    /// The substring query.
    pub query: String,
    /// The matching entries.
    pub entries: Vec<FileEntry>,
    /// The cursor for the next page, or `null` when exhausted.
    pub next_cursor: Option<String>,
}
