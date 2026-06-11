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
#[must_use]
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
#[must_use]
pub fn conflict_name(original_name: &str, device_name: &str) -> String {
    let date = chrono::Utc::now().format("%Y-%m-%d");
    let suffix = format!(" ({device_name} {date}).conflict");

    // Split name and extension.
    original_name.rfind('.').map_or_else(
        || format!("{original_name}{suffix}"),
        |dot_pos| {
            let stem = original_name.get(..dot_pos).unwrap_or(original_name);
            let ext = original_name.get(dot_pos..).unwrap_or("");
            format!("{stem}{suffix}{ext}")
        },
    )
}

/// Derive the full VFS path for the conflict copy of a file.
///
/// The conflict copy lives in the same directory as the original. The parent
/// directory is extracted from `original_vfs_path` by dropping the final
/// `/`-delimited segment, then the [`conflict_name`] is appended.
///
/// When `original_vfs_path` has no `/` separator (the file sits directly at
/// the VFS root, or the backend is mounted at `"/"`), the conflict copy path
/// is just the conflict filename with no parent component.
///
/// # Examples
///
/// ```
/// # use cascade_engine::sync::conflict::conflict_vfs_path;
/// let p = conflict_vfs_path("personal/Documents/report.pdf", "laptop");
/// assert!(p.starts_with("personal/Documents/"));
/// assert!(p.ends_with(".conflict.pdf"));
///
/// // File at root (no parent segment).
/// let q = conflict_vfs_path("README", "server");
/// assert!(q.starts_with("README (server "));
/// assert!(q.ends_with(".conflict"));
/// ```
#[must_use]
pub fn conflict_vfs_path(original_vfs_path: &str, device_name: &str) -> String {
    // Split on the last `/` to obtain the parent directory and the basename.
    original_vfs_path.rfind('/').map_or_else(
        // No parent segment — the file sits directly at the VFS root.
        || conflict_name(original_vfs_path, device_name),
        |slash_pos| {
            let parent = original_vfs_path
                .get(..slash_pos)
                .unwrap_or(original_vfs_path);
            let name = original_vfs_path
                .get(slash_pos + 1..)
                .unwrap_or(original_vfs_path);
            format!("{}/{}", parent, conflict_name(name, device_name))
        },
    )
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

    // ── conflict_vfs_path ──

    #[test]
    fn conflict_vfs_path_preserves_parent_directory() {
        let p = conflict_vfs_path("personal/Documents/report.pdf", "laptop");
        assert!(
            p.starts_with("personal/Documents/"),
            "parent dir must be preserved: {p}"
        );
        assert!(p.ends_with(".conflict.pdf"), "extension preserved: {p}");
        assert!(
            p.contains("report (laptop "),
            "conflict name must embed device: {p}"
        );
    }

    #[test]
    fn conflict_vfs_path_nested_mount() {
        // A backend mounted at a two-segment prefix.
        let p = conflict_vfs_path("work/projects/plan.md", "dev-box");
        assert!(p.starts_with("work/projects/"), "parent preserved: {p}");
        assert!(p.ends_with(".conflict.md"), "extension: {p}");
    }

    #[test]
    fn conflict_vfs_path_no_parent_segment() {
        // File directly at the VFS root (backend mounted at "/").
        let p = conflict_vfs_path("README", "server");
        assert!(!p.contains('/'), "no slash for root-level file: {p}");
        assert!(p.starts_with("README (server "), "conflict name: {p}");
        assert!(p.ends_with(".conflict"), "no extension: {p}");
    }

    #[test]
    fn conflict_vfs_path_is_consistent_with_conflict_name() {
        // `conflict_vfs_path` on a path with a single parent segment must
        // produce exactly `<parent>/<conflict_name(<basename>)>`.
        let vfs_path = "Archive/report.pdf";
        let cp = conflict_vfs_path(vfs_path, "laptop");
        let cn = conflict_name("report.pdf", "laptop");
        assert_eq!(cp, format!("Archive/{cn}"));
    }
}
