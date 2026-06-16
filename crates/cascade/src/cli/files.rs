//! Engine-backed file verbs: `ls`, `cat`, `mkdir`, `cp`, `mv`, `rm`.
//!
//! Each verb constructs a fresh [`NativeEngine`] from the on-disk config
//! (exactly as the daemon does at startup), then drives the VFS tree and the
//! underlying [`Backend`] trait directly. No mount is involved and no daemon
//! needs to be running; the verbs operate on local engine state and the
//! configured backends.
//!
//! The verbs are thin: they resolve the backend for a path through the
//! [`VfsTree`] and forward to the matching [`Backend`] operation
//! (`read_dir`, `metadata`/`download`, `create_dir`, `upload`, `delete`,
//! `move_entry`). Cross-backend copies and moves download from the source
//! backend then upload to the destination, mirroring the cross-backend move
//! semantics the VFS tree already implements.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use cascade_engine::backend::Backend;
use cascade_engine::engine::{EngineConfig, NativeEngine};
use cascade_engine::types::{DirEntry, FileId};
use cascade_engine::vfs::resolve_listing_native_id;

use super::CliContext;
use super::mount::{load_main_config, rebuild_backends};

/// Build a fresh native engine from the on-disk config, exactly as the daemon
/// does at startup. The engine owns its own state database handle and VFS tree;
/// no mount is performed and no presenter is registered.
fn build_engine(ctx: &CliContext) -> Result<NativeEngine> {
    let main_config = load_main_config(&ctx.config_dir)?;
    let shared_http: Arc<dyn cascade_engine::portable::HttpClient> =
        Arc::new(cascade_engine::portable::native::ReqwestClient::new());
    let backends = rebuild_backends(&main_config, &ctx.config_dir, shared_http)?;
    let engine_config = EngineConfig {
        db_path: ctx.db_path.clone(),
        mount_point: PathBuf::from("/"),
        backends,
        cache_dir: None,
        enable_p2p: false,
        p2p_data_dir: None,
        p2p_posture: None,
        p2p_relay_endpoints: Vec::new(),
        p2p_relay_shared_secret: None,
        backend_factory: None,
    };
    NativeEngine::new(engine_config).context("building engine from on-disk config")
}

/// Resolve `path` to its backend and backend-relative sub-path under the
/// engine's VFS read lock, then return clones that outlive the lock.
fn resolve_in_vfs(engine: &NativeEngine, path: &Path) -> (Arc<dyn Backend>, PathBuf) {
    let vfs = engine.vfs();
    let guard = vfs
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (backend, backend_path) = guard.resolve(path);
    (backend.clone(), backend_path)
}

/// `cascade ls <path>` — list the immediate children of a directory.
pub async fn ls(ctx: &CliContext, path: &str) -> Result<()> {
    let engine = build_engine(ctx)?;
    let (backend, backend_path) = resolve_in_vfs(&engine, Path::new(path));
    let backend_path_str = path_to_str(&backend_path);
    let native_id = resolve_listing_native_id(backend.as_ref(), &backend_path_str).await?;
    let children = backend.list_children(&native_id).await?;
    let mut entries: Vec<DirEntry> = children
        .into_iter()
        .map(|child| DirEntry {
            name: child.name,
            is_dir: child.is_dir,
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    if entries.is_empty() {
        println!("(empty)");
    } else {
        for entry in entries {
            let kind = if entry.is_dir { "d" } else { "-" };
            println!("{kind} {}", entry.name);
        }
    }
    Ok(())
}

/// `cascade cat <path>` — print a file's contents to stdout.
pub async fn cat(ctx: &CliContext, path: &str) -> Result<()> {
    use std::io::Write as _;
    let engine = build_engine(ctx)?;
    let (backend, backend_path) = resolve_in_vfs(&engine, Path::new(path));
    let entry = backend
        .metadata(&backend_path)
        .await
        .with_context(|| format!("reading metadata for {path}"))?;
    let data = backend
        .download(&entry)
        .await
        .with_context(|| format!("downloading {path}"))?;
    // Write raw bytes so binary content is not corrupted.
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    handle
        .write_all(&data)
        .context("writing file contents to stdout")?;
    Ok(())
}

/// `cascade mkdir <path>` — create a directory.
pub async fn mkdir(ctx: &CliContext, path: &str) -> Result<()> {
    let engine = build_engine(ctx)?;
    let (backend, backend_path) = resolve_in_vfs(&engine, Path::new(path));
    backend
        .create_dir(&backend_path)
        .await
        .with_context(|| format!("creating directory {path}"))?;
    println!("created directory {path}");
    Ok(())
}

/// `cascade rm <path>` — delete a file or directory.
pub async fn rm(ctx: &CliContext, path: &str) -> Result<()> {
    let engine = build_engine(ctx)?;
    let (backend, backend_path) = resolve_in_vfs(&engine, Path::new(path));
    let entry = backend
        .metadata(&backend_path)
        .await
        .with_context(|| format!("reading metadata for {path}"))?;
    backend
        .delete(&entry)
        .await
        .with_context(|| format!("deleting {path}"))?;
    println!("removed {path}");
    Ok(())
}

/// `cascade mv <src> <dst>` — move or rename an entry.
///
/// Same-backend moves delegate to [`Backend::move_entry`]; cross-backend moves
/// download from the source, upload to the destination, then delete the
/// original, mirroring [`cascade_engine::vfs::VfsTree::rename`].
pub async fn mv(ctx: &CliContext, src: &str, dst: &str) -> Result<()> {
    let engine = build_engine(ctx)?;
    let (src_backend, src_path, dst_backend, dst_path, same_backend) = {
        let vfs = engine.vfs();
        let guard = vfs
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (src_backend, src_path) = guard.resolve(Path::new(src));
        let (dst_backend, dst_path) = guard.resolve(Path::new(dst));
        (
            src_backend.clone(),
            src_path,
            dst_backend.clone(),
            dst_path,
            Arc::ptr_eq(src_backend, dst_backend),
        )
    };
    if same_backend {
        src_backend
            .move_entry(&src_path, &dst_path)
            .await
            .with_context(|| format!("moving {src} to {dst}"))?;
    } else {
        cross_backend_copy(&src_backend, &src_path, &dst_backend, &dst_path)
            .await
            .with_context(|| format!("cross-backend move {src} to {dst}"))?;
        let entry = src_backend
            .metadata(&src_path)
            .await
            .with_context(|| format!("re-reading source metadata for {src}"))?;
        src_backend
            .delete(&entry)
            .await
            .with_context(|| format!("deleting source after cross-backend move {src}"))?;
    }
    println!("moved {src} to {dst}");
    Ok(())
}

/// `cascade cp <src> <dst>` — copy a file.
///
/// Copies always download from the source and upload to the destination, even
/// within the same backend, because the `Backend` trait has no copy primitive.
pub async fn cp(ctx: &CliContext, src: &str, dst: &str) -> Result<()> {
    let engine = build_engine(ctx)?;
    let (src_backend, src_path, dst_backend, dst_path) = {
        let vfs = engine.vfs();
        let guard = vfs
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (src_backend, src_path) = guard.resolve(Path::new(src));
        let (dst_backend, dst_path) = guard.resolve(Path::new(dst));
        (src_backend.clone(), src_path, dst_backend.clone(), dst_path)
    };
    cross_backend_copy(&src_backend, &src_path, &dst_backend, &dst_path)
        .await
        .with_context(|| format!("copying {src} to {dst}"))?;
    println!("copied {src} to {dst}");
    Ok(())
}

/// Download an entry from `src_backend` and upload it to `dst_backend`,
/// resolving the destination parent id.
async fn cross_backend_copy(
    src_backend: &Arc<dyn Backend>,
    src_path: &Path,
    dst_backend: &Arc<dyn Backend>,
    dst_path: &Path,
) -> Result<()> {
    let entry = src_backend
        .metadata(src_path)
        .await
        .with_context(|| format!("reading source metadata {}", src_path.display()))?;
    let data = src_backend
        .download(&entry)
        .await
        .with_context(|| format!("downloading source {}", src_path.display()))?;
    let parent_id = resolve_parent_id(dst_backend, dst_path).await?;
    dst_backend
        .upload(dst_path, &data, &parent_id)
        .await
        .with_context(|| format!("uploading to destination {}", dst_path.display()))?;
    Ok(())
}

/// Resolve the parent directory's [`FileId`] for a destination path.
///
/// The destination's parent is the path minus its final component. An empty
/// parent (the backend root) resolves to the backend's root native id via
/// [`Backend::metadata`] on `/`.
async fn resolve_parent_id(backend: &Arc<dyn Backend>, path: &Path) -> Result<FileId> {
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    if parent.as_os_str().is_empty() {
        // The backend root: resolve its native id through metadata on the root
        // path, which every backend recognises.
        let root_entry = backend.metadata(Path::new("/")).await?;
        Ok(FileId(root_entry.id.0))
    } else {
        let entry = backend.metadata(parent).await?;
        Ok(FileId(entry.id.0))
    }
}

/// Convert a backend-relative path to the string form
/// `resolve_listing_native_id` expects: empty for the root, otherwise the
/// path with no leading separator.
fn path_to_str(path: &Path) -> String {
    let s = path.to_string_lossy();
    if s.is_empty() {
        String::new()
    } else {
        s.trim_start_matches('/').to_owned()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use std::path::Path;

    use super::*;
    use crate::cli::CliContext;

    /// Write a minimal `config.toml` + local backend config into `dir`,
    /// pointing the local backend at `root`. The backend mounts at the neutral
    /// root (`mount = "/"`) so paths like `/notes` resolve into it rather than
    /// under a `local/` prefix.
    fn seed_local_config(dir: &Path, root: &Path) -> CliContext {
        std::fs::write(
            dir.join("config.toml"),
            "[backends]\n[backends.local]\ntype = \"local\"\nmount = \"/\"\n",
        )
        .unwrap();
        let backend_toml = format!("root_path = \"{}\"\n", root.display());
        std::fs::write(dir.join("local.toml"), backend_toml).unwrap();
        CliContext {
            config_dir: dir.to_path_buf(),
            db_path: dir.join("state.db"),
            pid_path: dir.join("cascade.pid"),
        }
    }

    #[tokio::test]
    async fn mkdir_ls_cat_rm_round_trip() {
        let config_dir = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let ctx = seed_local_config(config_dir.path(), root.path());

        // mkdir a directory under the backend root.
        mkdir(&ctx, "/notes").await.expect("mkdir should succeed");

        // ls the root — the new directory should appear.
        let entries = collect_ls(&ctx, "/").await;
        assert!(
            entries
                .iter()
                .any(|(name, is_dir)| name == "notes" && *is_dir),
            "ls / should list the notes directory, got {entries:?}",
        );

        // Seed a file inside the new directory via the backend, then cat and
        // rm it.
        seed_file(&ctx, "/notes/hello.txt", b"hello world").await;
        cat(&ctx, "/notes/hello.txt")
            .await
            .expect("cat should succeed");

        // rm the file.
        rm(&ctx, "/notes/hello.txt")
            .await
            .expect("rm should succeed");
        let entries = collect_ls(&ctx, "/notes").await;
        assert!(
            entries.iter().all(|(name, _)| name != "hello.txt"),
            "hello.txt should be gone after rm, got {entries:?}",
        );

        // rm the directory.
        rm(&ctx, "/notes").await.expect("rm dir should succeed");
        let entries = collect_ls(&ctx, "/").await;
        assert!(
            entries.iter().all(|(name, _)| name != "notes"),
            "notes should be gone after rm, got {entries:?}",
        );
    }

    /// Collect an ls listing as `(name, is_dir)` pairs by building the engine
    /// and reading the backend directly. This mirrors what the `ls` handler
    /// does, without capturing stdout.
    async fn collect_ls(ctx: &CliContext, path: &str) -> Vec<(String, bool)> {
        let engine = build_engine(ctx).unwrap();
        let (backend, backend_path) = resolve_in_vfs(&engine, Path::new(path));
        let backend_path_str = path_to_str(&backend_path);
        let native_id = resolve_listing_native_id(backend.as_ref(), &backend_path_str)
            .await
            .unwrap();
        let children = backend.list_children(&native_id).await.unwrap();
        children
            .into_iter()
            .map(|child| (child.name, child.is_dir))
            .collect()
    }

    /// Seed a file with the given contents by building the engine and calling
    /// the backend's upload directly.
    async fn seed_file(ctx: &CliContext, path: &str, contents: &[u8]) {
        let engine = build_engine(ctx).unwrap();
        let (backend, backend_path) = resolve_in_vfs(&engine, Path::new(path));
        let parent_id = resolve_parent_id(&backend, &backend_path).await.unwrap();
        backend
            .upload(&backend_path, contents, &parent_id)
            .await
            .unwrap();
    }
}
