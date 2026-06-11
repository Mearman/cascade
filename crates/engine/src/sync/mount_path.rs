//! Mount-prefix helpers for the VFS path model.
//!
//! Two pure functions implement the only point at which a backend mount prefix
//! is applied to or stripped from a VFS path:
//!
//! - [`apply_mount_prefix`] — called *once*, on the way **in**, when the sync
//!   runner assembles a child's full VFS path from its parent path and its
//!   basename. For a backend mounted at `/` the prefix is empty and the
//!   function is a no-op.
//!
//! - [`strip_mount_prefix`] — called *once*, on the way **out**, in
//!   `flush_dirty_files` just before calling `backend.upload`, so the backend
//!   always receives a native, mount-relative path. Again a no-op when the
//!   prefix is empty.
//!
//! The two functions are strict inverses of each other:
//! `strip_mount_prefix(p, apply_mount_prefix(p, rel)) == rel` for all valid
//! inputs. Nothing else in the engine touches the mount prefix.

use std::path::Path;

/// Prepend `mount_prefix` to `relative_path`, returning the full VFS path.
///
/// The result uses forward-slash separators (VFS paths are always `/`-separated,
/// never OS-path-separated). No leading slash is added; VFS paths are root-
/// relative but do not start with `/`.
///
/// When `mount_prefix` is empty (a backend mounted at `/`), the function
/// returns `relative_path` unchanged — it is a no-op.
///
/// # Examples
///
/// ```
/// # use cascade_engine::sync::mount_path::apply_mount_prefix;
/// use std::path::Path;
///
/// // Ordinary prefix.
/// assert_eq!(
///     apply_mount_prefix(Path::new("personal"), "Documents/report.txt"),
///     "personal/Documents/report.txt"
/// );
///
/// // Empty prefix (backend mounted at "/") — no-op.
/// assert_eq!(
///     apply_mount_prefix(Path::new(""), "Documents/report.txt"),
///     "Documents/report.txt"
/// );
///
/// // Nested prefix (backend mounted at "work/projects").
/// assert_eq!(
///     apply_mount_prefix(Path::new("work/projects"), "repo/README.md"),
///     "work/projects/repo/README.md"
/// );
/// ```
#[must_use]
pub fn apply_mount_prefix(mount_prefix: &Path, relative_path: &str) -> String {
    let prefix_str = mount_prefix.to_string_lossy();
    if prefix_str.is_empty() {
        return relative_path.to_owned();
    }
    // Normalise the prefix to forward slashes (the mount prefix comes from a
    // PathBuf which may use OS separators on Windows).
    let prefix_fwd: String = prefix_str
        .chars()
        .map(|c| if c == '\\' { '/' } else { c })
        .collect();
    if relative_path.is_empty() {
        return prefix_fwd;
    }
    format!("{prefix_fwd}/{relative_path}")
}

/// Strip `mount_prefix` from `vfs_path`, returning the backend-relative path.
///
/// This is the inverse of [`apply_mount_prefix`]. The result is the native,
/// mount-relative path that the backend understands.
///
/// When `mount_prefix` is empty (a backend mounted at `/`), the function
/// returns `vfs_path` unchanged — it is a no-op.
///
/// Returns `None` when `vfs_path` does not start with the expected prefix plus
/// a `/` separator, which indicates a routing bug (a path was given to the
/// wrong backend's strip call). Callers treat this as an internal invariant
/// violation and must not silently fall back to the raw path.
///
/// # Examples
///
/// ```
/// # use cascade_engine::sync::mount_path::strip_mount_prefix;
/// use std::path::Path;
///
/// // Ordinary prefix.
/// assert_eq!(
///     strip_mount_prefix(Path::new("personal"), "personal/Documents/report.txt"),
///     Some("Documents/report.txt".to_owned())
/// );
///
/// // Empty prefix (backend mounted at "/") — no-op.
/// assert_eq!(
///     strip_mount_prefix(Path::new(""), "Documents/report.txt"),
///     Some("Documents/report.txt".to_owned())
/// );
///
/// // Path does not start with the prefix — routing bug.
/// assert_eq!(
///     strip_mount_prefix(Path::new("personal"), "work/report.txt"),
///     None
/// );
///
/// // Stripping a prefix that equals the full path (a directory entry for the
/// // mount root itself).
/// assert_eq!(
///     strip_mount_prefix(Path::new("personal"), "personal"),
///     Some(String::new())
/// );
/// ```
#[must_use]
pub fn strip_mount_prefix(mount_prefix: &Path, vfs_path: &str) -> Option<String> {
    let prefix_str = mount_prefix.to_string_lossy();
    if prefix_str.is_empty() {
        return Some(vfs_path.to_owned());
    }
    // Normalise to forward slashes, matching apply_mount_prefix.
    let prefix_fwd: String = prefix_str
        .chars()
        .map(|c| if c == '\\' { '/' } else { c })
        .collect();

    // Case 1: the VFS path is exactly the mount prefix (the directory entry for
    // the mount root itself).
    if vfs_path == prefix_fwd {
        return Some(String::new());
    }
    // Case 2: the VFS path starts with "<prefix>/…".
    let expected_prefix = format!("{prefix_fwd}/");
    vfs_path
        .strip_prefix(expected_prefix.as_str())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic, clippy::nursery)]

    use super::*;
    use std::path::Path;

    // ── apply_mount_prefix ──

    #[test]
    fn apply_empty_prefix_is_identity() {
        let prefix = Path::new("");
        assert_eq!(
            apply_mount_prefix(prefix, "Documents/report.txt"),
            "Documents/report.txt"
        );
    }

    #[test]
    fn apply_empty_prefix_on_empty_relative_stays_empty() {
        let prefix = Path::new("");
        assert_eq!(apply_mount_prefix(prefix, ""), "");
    }

    #[test]
    fn apply_simple_prefix() {
        let prefix = Path::new("personal");
        assert_eq!(
            apply_mount_prefix(prefix, "Documents/report.txt"),
            "personal/Documents/report.txt"
        );
    }

    #[test]
    fn apply_nested_prefix() {
        let prefix = Path::new("work/projects");
        assert_eq!(
            apply_mount_prefix(prefix, "repo/README.md"),
            "work/projects/repo/README.md"
        );
    }

    #[test]
    fn apply_prefix_with_empty_relative_returns_prefix() {
        let prefix = Path::new("personal");
        assert_eq!(apply_mount_prefix(prefix, ""), "personal");
    }

    // ── strip_mount_prefix ──

    #[test]
    fn strip_empty_prefix_is_identity() {
        let prefix = Path::new("");
        assert_eq!(
            strip_mount_prefix(prefix, "Documents/report.txt"),
            Some("Documents/report.txt".to_owned())
        );
    }

    #[test]
    fn strip_empty_prefix_on_empty_path_stays_empty() {
        let prefix = Path::new("");
        assert_eq!(strip_mount_prefix(prefix, ""), Some(String::new()));
    }

    #[test]
    fn strip_simple_prefix() {
        let prefix = Path::new("personal");
        assert_eq!(
            strip_mount_prefix(prefix, "personal/Documents/report.txt"),
            Some("Documents/report.txt".to_owned())
        );
    }

    #[test]
    fn strip_nested_prefix() {
        let prefix = Path::new("work/projects");
        assert_eq!(
            strip_mount_prefix(prefix, "work/projects/repo/README.md"),
            Some("repo/README.md".to_owned())
        );
    }

    #[test]
    fn strip_wrong_prefix_returns_none() {
        let prefix = Path::new("personal");
        assert_eq!(strip_mount_prefix(prefix, "work/report.txt"), None);
    }

    #[test]
    fn strip_prefix_that_equals_full_path_returns_empty_string() {
        let prefix = Path::new("personal");
        assert_eq!(strip_mount_prefix(prefix, "personal"), Some(String::new()));
    }

    #[test]
    fn strip_partial_prefix_match_is_rejected() {
        // "personal2/file.txt" must NOT strip as if the prefix were "personal".
        let prefix = Path::new("personal");
        assert_eq!(strip_mount_prefix(prefix, "personal2/file.txt"), None);
    }

    // ── round-trip ──

    #[test]
    fn round_trip_apply_then_strip_with_prefix() {
        let prefix = Path::new("personal");
        let relative = "Documents/report.txt";
        let vfs_path = apply_mount_prefix(prefix, relative);
        assert_eq!(
            strip_mount_prefix(prefix, &vfs_path),
            Some(relative.to_owned())
        );
    }

    #[test]
    fn round_trip_apply_then_strip_empty_prefix() {
        let prefix = Path::new("");
        let relative = "Documents/report.txt";
        let vfs_path = apply_mount_prefix(prefix, relative);
        assert_eq!(
            strip_mount_prefix(prefix, &vfs_path),
            Some(relative.to_owned())
        );
    }

    #[test]
    fn round_trip_apply_then_strip_nested_prefix() {
        let prefix = Path::new("work/projects");
        let relative = "repo/src/main.rs";
        let vfs_path = apply_mount_prefix(prefix, relative);
        assert_eq!(
            strip_mount_prefix(prefix, &vfs_path),
            Some(relative.to_owned())
        );
    }

    #[test]
    fn round_trip_with_empty_relative_path() {
        let prefix = Path::new("personal");
        let relative = "";
        let vfs_path = apply_mount_prefix(prefix, relative);
        // apply returns just the prefix; strip must give back empty.
        assert_eq!(
            strip_mount_prefix(prefix, &vfs_path),
            Some(relative.to_owned())
        );
    }
}
