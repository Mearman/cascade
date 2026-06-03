//! Shared write-path helpers for the NFS presenters (v3 and v4).
//!
//! These funnel every NFS write into the exact engine operations the other
//! presenters use — `Backend::upload`, `Backend::update`, `Backend::create_dir`,
//! `Backend::delete`, and `VfsTree::rename` — resolved through `VfsTree::resolve`
//! just as the `WebDAV` presenter does. There is no parallel write path in the
//! engine; the NFS layer only translates RFC 1813 / RFC 5661 procedure arguments
//! into those calls and maps the resulting errors back to NFS status codes.
//!
//! Writes are unauthenticated at the NFS layer by protocol — the trust model is
//! a localhost mount. A write must nonetheless not escape the export root, so
//! every path is validated by [`validate_vfs_path`] before it reaches a backend.

use std::path::Path;
use std::sync::Arc;

use cascade_engine::backend::{Backend, BackendError};
use cascade_engine::types::FileId;

use super::context::NfsContext;

/// A write rejected before it reached a backend, or a categorised backend error.
///
/// Presenters translate each variant into the protocol-appropriate status code
/// (`NFS3ERR_*` / `NFS4ERR_*`).
#[derive(Debug, Clone, Copy)]
pub enum WriteError {
    /// The path attempted to escape the export root (`..`, absolute escape).
    Traversal,
    /// The target resource does not exist.
    NotFound,
    /// The operation was refused for permission reasons.
    Forbidden,
    /// The operation conflicts with current state (name taken, dir not empty).
    Conflict,
    /// The argument was malformed (empty name, bad path component).
    Invalid,
    /// The backend ran out of space.
    NoSpace,
    /// The underlying backend reported an I/O failure.
    Io,
}

impl WriteError {
    /// Categorise an `anyhow::Error` from a backend call, downcasting to
    /// [`BackendError`] where the backend tagged the failure, otherwise
    /// inspecting the error chain for a disk-full `io::Error`, and finally
    /// falling back to a generic I/O failure.
    fn from_backend(err: &anyhow::Error) -> Self {
        if let Some(backend_err) = err.downcast_ref::<BackendError>() {
            return match backend_err {
                BackendError::NotFound(_) => Self::NotFound,
                BackendError::Forbidden(_) | BackendError::ReadOnly(_) => Self::Forbidden,
                BackendError::Conflict(_) => Self::Conflict,
            };
        }
        for cause in err.chain() {
            if let Some(io_err) = cause.downcast_ref::<std::io::Error>()
                && io_err.kind() == std::io::ErrorKind::StorageFull
            {
                return Self::NoSpace;
            }
        }
        Self::Io
    }
}

/// Reject any VFS path that could escape the export root.
///
/// VFS paths are server-absolute (`/a/b`). A path is rejected if it is empty,
/// not rooted at `/`, or contains a `.` or `..` component. Plain `/` is the
/// export root and is valid.
///
/// # Errors
///
/// Returns [`WriteError::Traversal`] for any path that is not safely contained
/// within the export.
pub fn validate_vfs_path(path: &str) -> Result<(), WriteError> {
    if path.is_empty() || !path.starts_with('/') {
        return Err(WriteError::Traversal);
    }
    for component in path.split('/') {
        if component.is_empty() {
            // Leading slash and any doubled slash produce empty components;
            // a trailing slash likewise. These are benign for a rooted path.
            continue;
        }
        if component == "." || component == ".." {
            return Err(WriteError::Traversal);
        }
    }
    Ok(())
}

/// Reject a single path component (a file or directory name) that is empty or
/// would alter the path's depth.
///
/// # Errors
///
/// Returns [`WriteError::Invalid`] for an empty name and [`WriteError::Traversal`]
/// for `.`, `..`, or a name containing a path separator.
pub fn validate_name(name: &str) -> Result<(), WriteError> {
    if name.is_empty() {
        return Err(WriteError::Invalid);
    }
    if name == "." || name == ".." {
        return Err(WriteError::Traversal);
    }
    if name.contains('/') {
        return Err(WriteError::Traversal);
    }
    Ok(())
}

/// Join a validated parent path and child name into a server-absolute VFS path.
fn join_child(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{parent}/{name}")
    }
}

/// Resolve a VFS path to its owning backend and the path relative to that
/// backend, cloning the `Arc` so the `VfsTree` read lock is released before any
/// await — the same pattern the read path uses.
///
/// The resolved path is made backend-relative by stripping any leading
/// separator. A leading `/` would otherwise cause `Path::join` inside a
/// path-based backend (e.g. the local backend's `upload`/`create_dir`) to
/// discard the configured root and escape the export — the very traversal the
/// write path must prevent. Stripping it here keeps every write contained.
fn resolve(ctx: &NfsContext, path: &str) -> (Arc<dyn Backend>, std::path::PathBuf) {
    let vfs = ctx
        .vfs()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (backend, relative) = vfs.resolve(Path::new(path));
    let relative = relative
        .strip_prefix("/")
        .map_or_else(|_| relative.clone(), Path::to_path_buf);
    (Arc::clone(backend), relative)
}

/// Run an async backend operation to completion on the current Tokio runtime.
///
/// All NFS procedure handlers are synchronous (they return `Vec<u8>`), so —
/// exactly as the read path does — the write path bridges into async via
/// `block_on` on the ambient runtime handle.
fn block_on<F, T>(future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tokio::runtime::Handle::current().block_on(future)
}

/// Write `data` at `offset` into the file at `path`, returning the file's new
/// total size.
///
/// Mirrors a POSIX `pwrite`: the existing content is fetched, `data` is spliced
/// in at `offset` (extending with zero bytes if `offset` is past the current
/// end), and the whole content is written back through `Backend::update` when
/// the file already exists or `Backend::upload` when it does not. The download
/// is required because the engine's write contract replaces whole-file content;
/// `Minimal` cache mode keeps nothing on local disk beyond this in-flight
/// buffer.
///
/// # Errors
///
/// Returns a [`WriteError`] if the path escapes the export, the parent cannot be
/// resolved, or the backend rejects the write.
pub fn write_file(
    ctx: &NfsContext,
    path: &str,
    offset: u64,
    data: &[u8],
) -> Result<u64, WriteError> {
    validate_vfs_path(path)?;
    let (backend, relative) = resolve(ctx, path);
    let off = usize::try_from(offset).map_err(|_| WriteError::Invalid)?;

    block_on(async move {
        let existing = backend.metadata(&relative).await.ok();

        let mut buf = if let Some(entry) = &existing {
            let mut current = Vec::new();
            backend
                .download(entry, &mut current)
                .await
                .map_err(|e| WriteError::from_backend(&e))?;
            current
        } else {
            Vec::new()
        };

        if buf.len() < off {
            buf.resize(off, 0);
        }
        let end = off.saturating_add(data.len());
        if buf.len() < end {
            buf.resize(end, 0);
        }
        if let Some(slot) = buf.get_mut(off..end) {
            slot.copy_from_slice(data);
        } else {
            return Err(WriteError::Io);
        }

        let new_size = u64::try_from(buf.len()).map_err(|_| WriteError::Io)?;

        if let Some(entry) = existing {
            let file_id = FileId(entry.id.0.clone());
            let mut cursor = std::io::Cursor::new(buf);
            backend
                .update(&file_id, &mut cursor)
                .await
                .map_err(|e| WriteError::from_backend(&e))?;
        } else {
            let parent_id = parent_file_id(backend.as_ref(), &relative).await;
            let mut cursor = std::io::Cursor::new(buf);
            backend
                .upload(&relative, &mut cursor, &parent_id)
                .await
                .map_err(|e| WriteError::from_backend(&e))?;
        }

        Ok(new_size)
    })
}

/// Create an empty regular file named `name` in the directory at `parent_path`,
/// returning the new file's server-absolute VFS path.
///
/// # Errors
///
/// Returns a [`WriteError`] if the parent path escapes the export, the name is
/// invalid, or the backend rejects the upload.
pub fn create_file(ctx: &NfsContext, parent_path: &str, name: &str) -> Result<String, WriteError> {
    validate_vfs_path(parent_path)?;
    validate_name(name)?;
    let child_path = join_child(parent_path, name);
    let (backend, relative) = resolve(ctx, &child_path);

    block_on(async move {
        let parent_id = parent_file_id(backend.as_ref(), &relative).await;
        let mut cursor = std::io::Cursor::new(Vec::new());
        backend
            .upload(&relative, &mut cursor, &parent_id)
            .await
            .map_err(|e| WriteError::from_backend(&e))?;
        Ok(child_path)
    })
}

/// Truncate or extend the file at `path` to exactly `size` bytes.
///
/// # Errors
///
/// Returns a [`WriteError`] if the path escapes the export, the file does not
/// exist, or the backend rejects the write.
pub fn truncate_file(ctx: &NfsContext, path: &str, size: u64) -> Result<(), WriteError> {
    validate_vfs_path(path)?;
    let (backend, relative) = resolve(ctx, path);
    let target = usize::try_from(size).map_err(|_| WriteError::Invalid)?;

    block_on(async move {
        let entry = backend
            .metadata(&relative)
            .await
            .map_err(|e| WriteError::from_backend(&e))?;

        let mut buf = Vec::new();
        backend
            .download(&entry, &mut buf)
            .await
            .map_err(|e| WriteError::from_backend(&e))?;
        buf.resize(target, 0);

        let file_id = FileId(entry.id.0.clone());
        let mut cursor = std::io::Cursor::new(buf);
        backend
            .update(&file_id, &mut cursor)
            .await
            .map_err(|e| WriteError::from_backend(&e))?;
        Ok(())
    })
}

/// Create a directory named `name` under `parent_path`, returning the new
/// directory's server-absolute VFS path.
///
/// # Errors
///
/// Returns a [`WriteError`] if the parent path escapes the export, the name is
/// invalid, or the backend rejects the operation.
pub fn make_dir(ctx: &NfsContext, parent_path: &str, name: &str) -> Result<String, WriteError> {
    validate_vfs_path(parent_path)?;
    validate_name(name)?;
    let child_path = join_child(parent_path, name);
    let (backend, relative) = resolve(ctx, &child_path);

    block_on(async move {
        backend
            .create_dir(&relative)
            .await
            .map_err(|e| WriteError::from_backend(&e))?;
        Ok(child_path)
    })
}

/// Remove the file or directory named `name` under `parent_path`.
///
/// `expect_dir` selects the kind being removed (`REMOVE` vs `RMDIR`): a
/// mismatch between the request and the on-disk kind is reported as
/// [`WriteError::Invalid`] so the caller can map it to the correct status.
///
/// # Errors
///
/// Returns a [`WriteError`] if the path escapes the export, the entry does not
/// exist, the kind does not match, or the backend rejects the delete.
pub fn remove_entry(
    ctx: &NfsContext,
    parent_path: &str,
    name: &str,
    expect_dir: bool,
) -> Result<String, WriteError> {
    validate_vfs_path(parent_path)?;
    validate_name(name)?;
    let child_path = join_child(parent_path, name);
    let (backend, relative) = resolve(ctx, &child_path);

    block_on(async move {
        let entry = backend
            .metadata(&relative)
            .await
            .map_err(|e| WriteError::from_backend(&e))?;
        if entry.is_dir != expect_dir {
            return Err(WriteError::Invalid);
        }
        backend
            .delete(&entry)
            .await
            .map_err(|e| WriteError::from_backend(&e))?;
        Ok(child_path)
    })
}

/// Remove the file or directory named `name` under `parent_path` regardless of
/// kind. Used by `NFSv4` REMOVE, which does not distinguish files from
/// directories at the protocol level.
///
/// # Errors
///
/// Returns a [`WriteError`] if the path escapes the export, the entry does not
/// exist, or the backend rejects the delete.
pub fn remove_any(ctx: &NfsContext, parent_path: &str, name: &str) -> Result<String, WriteError> {
    validate_vfs_path(parent_path)?;
    validate_name(name)?;
    let child_path = join_child(parent_path, name);
    let (backend, relative) = resolve(ctx, &child_path);

    block_on(async move {
        let entry = backend
            .metadata(&relative)
            .await
            .map_err(|e| WriteError::from_backend(&e))?;
        backend
            .delete(&entry)
            .await
            .map_err(|e| WriteError::from_backend(&e))?;
        Ok(child_path)
    })
}

/// Rename `from_name` under `from_parent` to `to_name` under `to_parent`.
///
/// Routes through [`cascade_engine::vfs::VfsTree::rename`], which handles both
/// same-backend moves and cross-backend download/upload/delete.
///
/// # Errors
///
/// Returns a [`WriteError`] if either path escapes the export, a name is
/// invalid, or the backend rejects the move.
pub fn rename_entry(
    ctx: &NfsContext,
    from_parent: &str,
    from_name: &str,
    to_parent: &str,
    to_name: &str,
) -> Result<(String, String), WriteError> {
    validate_vfs_path(from_parent)?;
    validate_vfs_path(to_parent)?;
    validate_name(from_name)?;
    validate_name(to_name)?;
    let src_path = join_child(from_parent, from_name);
    let dst_path = join_child(to_parent, to_name);

    block_on(async {
        ctx.rename(&src_path, &dst_path)
            .await
            .map_err(|e| WriteError::from_backend(&e))
    })?;
    Ok((src_path, dst_path))
}

/// Resolve the `FileId` of the parent directory of a backend-relative path.
///
/// Mirrors the `WebDAV` presenter: the parent is resolved by `Backend::metadata`
/// when present, otherwise the backend's conventional root id (`{backend}:root`)
/// is used so a top-level create still lands in the right place.
async fn parent_file_id(backend: &dyn Backend, relative: &Path) -> FileId {
    let root_id = FileId(format!("{}:root", backend.id()));
    let Some(parent) = relative.parent() else {
        return root_id;
    };
    if parent.as_os_str().is_empty() {
        return root_id;
    }
    match backend.metadata(parent).await {
        Ok(entry) => FileId(entry.id.0),
        Err(_) => root_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_vfs_path_accepts_root_and_nested() {
        assert!(validate_vfs_path("/").is_ok());
        assert!(validate_vfs_path("/a").is_ok());
        assert!(validate_vfs_path("/a/b/c").is_ok());
    }

    #[test]
    fn validate_vfs_path_rejects_traversal() {
        assert!(matches!(
            validate_vfs_path("/a/../b"),
            Err(WriteError::Traversal)
        ));
        assert!(matches!(
            validate_vfs_path("/.."),
            Err(WriteError::Traversal)
        ));
        assert!(matches!(
            validate_vfs_path("a/b"),
            Err(WriteError::Traversal)
        ));
        assert!(matches!(validate_vfs_path(""), Err(WriteError::Traversal)));
    }

    #[test]
    fn validate_name_rejects_separators_and_dots() {
        assert!(validate_name("file.txt").is_ok());
        assert!(matches!(validate_name(""), Err(WriteError::Invalid)));
        assert!(matches!(validate_name(".."), Err(WriteError::Traversal)));
        assert!(matches!(validate_name("."), Err(WriteError::Traversal)));
        assert!(matches!(validate_name("a/b"), Err(WriteError::Traversal)));
    }

    #[test]
    fn join_child_handles_root_and_nested() {
        assert_eq!(join_child("/", "a"), "/a");
        assert_eq!(join_child("/a", "b"), "/a/b");
    }
}
