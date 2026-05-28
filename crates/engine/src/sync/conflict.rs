//! Conflict detection and resolution.
//!
//! When the same file is modified both locally and remotely, a conflict occurs.
//! Resolution strategy: keep both versions. The losing version (local) is renamed
//! with the device name and date, e.g. `report (work-laptop 2026-05-27).conflict`.

use crate::types::FileEntry;

/// Result of a conflict check between local and remote versions of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictCheck {
    /// No conflict — one or both versions unchanged.
    NoConflict,
    /// Conflict detected — both local and remote changed.
    Conflict {
        local_entry: Box<FileEntry>,
        remote_entry: Box<FileEntry>,
    },
}

/// Determine if a sync update creates a conflict.
///
/// A conflict occurs when:
/// 1. The remote file was modified (hash differs from what we last synced)
/// 2. The local file is dirty (has unpushed changes)
pub fn check_conflict(
    local_entry: &FileEntry,
    remote_entry: &FileEntry,
    local_dirty: bool,
) -> ConflictCheck {
    // If local isn't dirty, there's no conflict — just accept the remote version.
    if !local_dirty {
        return ConflictCheck::NoConflict;
    }

    // If the remote hash matches our local hash, no conflict — same content.
    if let (Some(local_hash), Some(remote_hash)) = (&local_entry.hash, &remote_entry.hash)
        && local_hash == remote_hash
    {
        return ConflictCheck::NoConflict;
    }

    // Both changed — conflict.
    ConflictCheck::Conflict {
        local_entry: Box::new(local_entry.clone()),
        remote_entry: Box::new(remote_entry.clone()),
    }
}

/// Generate a conflict file name for the losing (local) version.
///
/// Format: `{name} ({device} {date}).conflict`
/// Example: `report.pdf` → `report (work-laptop 2026-05-27).conflict.pdf`
pub fn conflict_name(original_name: &str, device_name: &str) -> String {
    let date = chrono::Utc::now().format("%Y-%m-%d");
    let suffix = format!(" ({device_name} {date}).conflict");

    // Split name and extension.
    if let Some(dot_pos) = original_name.rfind('.') {
        let stem = &original_name[..dot_pos];
        let ext = &original_name[dot_pos..];
        format!("{stem}{suffix}{ext}")
    } else {
        format!("{original_name}{suffix}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ItemId;

    fn make_entry(name: &str, hash: Option<&str>) -> FileEntry {
        FileEntry::file(
            ItemId::new("test", "id"),
            ItemId::new("test", "root"),
            name.to_string(),
        )
        .with_size(Some(100))
        .with_hash(hash.map(String::from))
    }

    #[test]
    fn no_conflict_when_local_clean() {
        let local = make_entry("file.txt", Some("abc"));
        let remote = make_entry("file.txt", Some("def"));
        assert_eq!(
            check_conflict(&local, &remote, false),
            ConflictCheck::NoConflict
        );
    }

    #[test]
    fn no_conflict_when_same_hash() {
        let local = make_entry("file.txt", Some("abc"));
        let remote = make_entry("file.txt", Some("abc"));
        assert_eq!(
            check_conflict(&local, &remote, true),
            ConflictCheck::NoConflict
        );
    }

    #[test]
    fn conflict_when_both_changed() {
        let local = make_entry("file.txt", Some("abc"));
        let remote = make_entry("file.txt", Some("def"));
        let result = check_conflict(&local, &remote, true);
        assert!(matches!(result, ConflictCheck::Conflict { .. }));
    }

    #[test]
    fn conflict_name_with_extension() {
        let name = conflict_name("report.pdf", "work-laptop");
        assert!(name.starts_with("report (work-laptop 2"));
        assert!(name.ends_with(".conflict.pdf"));
    }

    #[test]
    fn conflict_name_without_extension() {
        let name = conflict_name("README", "server");
        assert!(name.starts_with("README (server 2"));
        assert!(name.ends_with(".conflict"));
    }

    #[test]
    fn conflict_name_double_extension() {
        let name = conflict_name("archive.tar.gz", "desktop");
        assert!(name.starts_with("archive.tar (desktop 2"));
        assert!(name.ends_with(".conflict.gz"));
    }
}
