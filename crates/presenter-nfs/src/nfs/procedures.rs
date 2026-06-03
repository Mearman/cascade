//! NFS procedure handlers.
//!
//! Read procedures: NULL, GETATTR, LOOKUP, READDIR, READ, FSSTAT.
//! Write procedures: WRITE, CREATE, SETATTR, MKDIR, REMOVE, RMDIR, RENAME,
//! COMMIT — gated on the export's [`NfsCacheMode`]. A read-only export
//! ([`NfsCacheMode::Off`]) refuses every write with `NFS3ERR_ROFS`; a
//! write-capable export funnels the write into the same engine operations the
//! `WebDAV` and FUSE presenters use (see [`crate::nfs::write`]).

use super::context::NfsContext;
use super::server::NfsCacheMode;
use super::write::{self, CreateMode, WriteError};
use super::xdr::{
    Fattr3, NF3DIR, NF3REG, NFS3_CREATE_EXCLUSIVE, NFS3_CREATE_GUARDED, NFS3_FILE_SYNC, NFS3_OK,
    NFS3ERR_ACCES, NFS3ERR_EXIST, NFS3ERR_INVAL, NFS3ERR_IO, NFS3ERR_NOSPC, NFS3ERR_NOTEMPTY,
    NFS3ERR_ROFS, NFS3ERR_STALE, NFS3PROC_COMMIT, NFS3PROC_CREATE, NFS3PROC_FSSTAT,
    NFS3PROC_GETATTR, NFS3PROC_LOOKUP, NFS3PROC_MKDIR, NFS3PROC_NULL, NFS3PROC_READ,
    NFS3PROC_READDIR, NFS3PROC_REMOVE, NFS3PROC_RENAME, NFS3PROC_RMDIR, NFS3PROC_SETATTR,
    NFS3PROC_WRITE, NfsFh3, NfsTime, PostOpAttr, Sattr3, Specdata3, decode_fh, decode_opaque,
    decode_sattr3, decode_string, decode_u32, decode_u64, encode_bool, encode_fh,
    encode_post_op_attr, encode_post_op_fh3, encode_string, encode_u32, encode_u64,
    encode_wcc_data,
};
use std::sync::Arc;

/// Handle an NFS procedure call with VFS context.
/// Returns XDR-encoded reply.
///
/// Write procedures are gated on `cache_mode`: a read-only export
/// ([`NfsCacheMode::Off`]) refuses them with `NFS3ERR_ROFS`, while a
/// write-capable export performs the write.
pub fn handle_nfs_call(
    proc: u32,
    args: &[u8],
    ctx: &Arc<NfsContext>,
    cache_mode: NfsCacheMode,
) -> Vec<u8> {
    match proc {
        NFS3PROC_NULL => vec![],
        NFS3PROC_GETATTR => handle_getattr(args, ctx),
        NFS3PROC_LOOKUP => handle_lookup(args, ctx),
        NFS3PROC_READDIR => handle_readdir(args, ctx),
        NFS3PROC_READ => handle_read(args, ctx),
        NFS3PROC_FSSTAT => handle_fsstat(args, ctx),
        NFS3PROC_WRITE => guard_write(cache_mode, || handle_write(args, ctx)),
        NFS3PROC_CREATE => guard_write(cache_mode, || handle_create(args, ctx)),
        NFS3PROC_SETATTR => guard_write(cache_mode, || handle_setattr(args, ctx)),
        NFS3PROC_MKDIR => guard_write(cache_mode, || handle_mkdir(args, ctx)),
        NFS3PROC_REMOVE => guard_write(cache_mode, || handle_remove(args, ctx)),
        NFS3PROC_RMDIR => guard_write(cache_mode, || handle_rmdir(args, ctx)),
        NFS3PROC_RENAME => guard_write(cache_mode, || handle_rename(args, ctx)),
        NFS3PROC_COMMIT => guard_write(cache_mode, handle_commit),
        _ => handle_unimplemented(),
    }
}

/// Refuse a write on a read-only export, otherwise run the handler.
///
/// A read-only export ([`NfsCacheMode::Off`]) returns a bare `NFS3ERR_ROFS`
/// status. Write-capable modes ([`NfsCacheMode::Minimal`], [`NfsCacheMode::Full`])
/// run `handler`.
fn guard_write(cache_mode: NfsCacheMode, handler: impl FnOnce() -> Vec<u8>) -> Vec<u8> {
    if cache_mode.writes_permitted() {
        handler()
    } else {
        let mut reply = Vec::new();
        encode_u32(&mut reply, NFS3ERR_ROFS);
        reply
    }
}

/// Map a [`WriteError`] to the appropriate `NFS3ERR_*` status code.
const fn write_error_status(err: WriteError) -> u32 {
    match err {
        WriteError::Traversal | WriteError::Forbidden => NFS3ERR_ACCES,
        WriteError::NotFound => NFS3ERR_STALE,
        WriteError::Conflict => NFS3ERR_EXIST,
        WriteError::Invalid => NFS3ERR_INVAL,
        WriteError::NoSpace => NFS3ERR_NOSPC,
        WriteError::Io => NFS3ERR_IO,
    }
}

/// Resolve the VFS path for a directory file handle, defaulting to the export
/// root when the handle is unknown (mirrors the read procedures).
fn dir_path_for_fh(ctx: &NfsContext, fh: &NfsFh3) -> String {
    let key = fh
        .to_item_id()
        .and_then(|id| id.parse::<u64>().ok())
        .unwrap_or(0);
    ctx.lookup_path(key).unwrap_or_else(|| "/".to_string())
}

/// NULL procedure — do nothing.
const fn _handle_null() -> Vec<u8> {
    vec![]
}

/// GETATTR — return file/directory metadata.
fn handle_getattr(args: &[u8], ctx: &Arc<NfsContext>) -> Vec<u8> {
    let mut reply = Vec::new();

    if let Ok((fh, _)) = decode_fh(args) {
        let key = fh
            .to_item_id()
            .and_then(|id| id.parse::<u64>().ok())
            .unwrap_or(0);
        if let Some(path) = ctx.lookup_path(key) {
            let is_dir = path == "/";
            let size = if is_dir {
                0
            } else {
                file_size_sync(ctx, &path)
            };
            let attr = make_attributes_with_size(&path, is_dir, size);
            encode_u32(&mut reply, NFS3_OK);
            encode_post_op_attr(&mut reply, &PostOpAttr::some(attr));
        } else {
            encode_u32(&mut reply, NFS3ERR_STALE);
            encode_post_op_attr(&mut reply, &PostOpAttr::none());
        }
    } else {
        encode_u32(&mut reply, NFS3ERR_INVAL);
        encode_post_op_attr(&mut reply, &PostOpAttr::none());
    }

    reply
}

/// LOOKUP — resolve a name in a directory.
fn handle_lookup(args: &[u8], ctx: &Arc<NfsContext>) -> Vec<u8> {
    let mut reply = Vec::new();

    // Args: dir_fh + name
    if let Ok((fh, rest)) = decode_fh(args) {
        if let Ok((name, _)) = decode_string(rest) {
            // Resolve parent path from context.
            let parent_key = fh
                .to_item_id()
                .and_then(|id| id.parse::<u64>().ok())
                .unwrap_or(0);
            let parent_path = ctx
                .lookup_path(parent_key)
                .unwrap_or_else(|| "/".to_string());
            let child_path = if parent_path == "/" {
                format!("/{name}")
            } else {
                format!("{parent_path}/{name}")
            };

            // Register the child path.
            let child_key = ctx.register_path(&child_path);
            let child_fh = NfsFh3::from_item_id(&child_key.to_string());

            let dir_attr = make_attributes(&parent_path, parent_path == "/");
            let child_attr = make_attributes(&child_path, false);

            encode_u32(&mut reply, NFS3_OK);
            encode_fh(&mut reply, &child_fh);
            encode_post_op_attr(&mut reply, &PostOpAttr::some(dir_attr));
            encode_post_op_attr(&mut reply, &PostOpAttr::some(child_attr));
        } else {
            encode_u32(&mut reply, NFS3ERR_INVAL);
            encode_post_op_attr(&mut reply, &PostOpAttr::none());
            encode_post_op_attr(&mut reply, &PostOpAttr::none());
        }
    } else {
        encode_u32(&mut reply, NFS3ERR_STALE);
        encode_post_op_attr(&mut reply, &PostOpAttr::none());
        encode_post_op_attr(&mut reply, &PostOpAttr::none());
    }

    reply
}

/// READDIR — list directory contents.
fn handle_readdir(args: &[u8], ctx: &Arc<NfsContext>) -> Vec<u8> {
    let mut reply = Vec::new();

    // Args: dir_fh + cookie + cookieverf + dircount + maxcount
    if let Ok((fh, rest)) = decode_fh(args) {
        let dir_key = fh
            .to_item_id()
            .and_then(|id| id.parse::<u64>().ok())
            .unwrap_or(0);
        let dir_path = ctx.lookup_path(dir_key).unwrap_or_else(|| "/".to_string());
        let dir_attr = make_attributes(&dir_path, true);

        // Decode cookie (skip entries before this offset) and
        // cookieverf + counts.
        let cookie = decode_u64(rest).map_or(0, |(c, _)| c);
        // cookieverf is the next 8 bytes — we ignore it for now.

        // Fetch directory listing from the VFS tree synchronously.
        let entries = match ctx.list_dir_sync(&dir_path) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(path = %dir_path, error = %e, "READDIR: failed to list directory");
                encode_u32(&mut reply, NFS3ERR_IO);
                encode_post_op_attr(&mut reply, &PostOpAttr::some(dir_attr));
                return reply;
            }
        };

        encode_u32(&mut reply, NFS3_OK);
        encode_post_op_attr(&mut reply, &PostOpAttr::some(dir_attr));
        // cookieverf (8 bytes of zero).
        encode_u64(&mut reply, 0);

        // Encode directory entries. NFS cookies are 1-based indices
        // into the entry list. Skip entries with cookie <= the
        // requested cookie (client is resuming a previous listing).
        for (i, entry) in entries.iter().enumerate() {
            let entry_cookie = u64::try_from(i).unwrap_or(u64::MAX) + 3; // cookies start at 3 (. and .. take 1,2)
            if entry_cookie <= cookie {
                continue;
            }
            // Register the child path so LOOKUP can find it.
            let child_path = if dir_path == "/" {
                format!("/{}", entry.name)
            } else {
                format!("{}/{}", dir_path, entry.name)
            };
            ctx.register_path(&child_path);

            let fileid = id_hash(&child_path);
            // value_follow = true (more entries coming)
            encode_bool(&mut reply, true);
            encode_u64(&mut reply, fileid);
            encode_string(&mut reply, &entry.name);
            encode_u64(&mut reply, entry_cookie);
        }

        // No more entries.
        encode_bool(&mut reply, false);
        encode_bool(&mut reply, true); // EOF
    } else {
        encode_u32(&mut reply, NFS3ERR_STALE);
        encode_post_op_attr(&mut reply, &PostOpAttr::none());
    }

    reply
}

/// READ — read file data.
fn handle_read(args: &[u8], ctx: &Arc<NfsContext>) -> Vec<u8> {
    let mut reply: Vec<u8> = Vec::new();

    // Args: file_fh + offset + count
    if let Ok((fh, rest)) = decode_fh(args) {
        let offset = decode_u64(rest).ok();
        let count = offset.and_then(|(_, r)| decode_u32(r).ok()).map(|(c, _)| c);

        let file_key = fh
            .to_item_id()
            .and_then(|id| id.parse::<u64>().ok())
            .unwrap_or(0);
        let file_path = ctx.lookup_path(file_key);

        let file_attr = make_attributes(file_path.as_deref().unwrap_or("unknown"), false);

        // Try to fetch file data synchronously via the VFS.
        let data = match &file_path {
            Some(path) => match fetch_file_data_sync(ctx, path, offset.map(|(o, _)| o), count) {
                Ok(d) => d,
                Err(e) => {
                    tracing::debug!(path = %path, error = %e, "READ: failed to fetch data");
                    encode_u32(&mut reply, NFS3ERR_IO);
                    encode_post_op_attr(&mut reply, &PostOpAttr::some(file_attr));
                    return reply;
                }
            },
            None => Vec::new(),
        };

        let is_eof = data.is_empty();
        encode_u32(&mut reply, NFS3_OK);
        encode_post_op_attr(&mut reply, &PostOpAttr::some(file_attr));
        let data_len = u32::try_from(data.len()).unwrap_or(u32::MAX);
        encode_u32(&mut reply, data_len); // count of bytes returned
        encode_bool(&mut reply, is_eof); // EOF if no data
        reply.extend_from_slice(&data);
    } else {
        encode_u32(&mut reply, NFS3ERR_STALE);
        encode_post_op_attr(&mut reply, &PostOpAttr::none());
    }

    reply
}

/// Synchronously fetch file data from the VFS backend.
fn fetch_file_data_sync(
    ctx: &NfsContext,
    path: &str,
    offset: Option<u64>,
    max_count: Option<u32>,
) -> anyhow::Result<Vec<u8>> {
    let rt = tokio::runtime::Handle::current();
    rt.block_on(async {
        let (backend, relative) = {
            let vfs = ctx
                .vfs()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (backend, relative) = vfs.resolve(std::path::Path::new(path));
            let result = (Arc::clone(backend), relative);
            drop(vfs);
            result
        };
        let entry = backend.metadata(&relative).await?;
        let mut buf = Vec::new();
        backend.download(&entry, &mut buf).await?;

        // Apply offset and count bounds.
        let off = usize::try_from(offset.unwrap_or(0)).unwrap_or(usize::MAX);
        if off >= buf.len() {
            return Ok(Vec::new());
        }
        let remaining = buf
            .get(off..)
            .ok_or_else(|| anyhow::anyhow!("offset {off} out of bounds"))?;
        let max = max_count.map_or(remaining.len(), |c| {
            usize::try_from(c).unwrap_or(usize::MAX)
        });
        let end = max.min(remaining.len());
        Ok(remaining
            .get(..end)
            .ok_or_else(|| anyhow::anyhow!("slice end {end} out of bounds"))?
            .to_vec())
    })
}

/// FSSTAT — return filesystem statistics.
fn handle_fsstat(args: &[u8], _ctx: &Arc<NfsContext>) -> Vec<u8> {
    let mut reply: Vec<u8> = Vec::new();

    if let Ok((_fh, _)) = decode_fh(args) {
        let root_attr = make_attributes("root", true);
        encode_u32(&mut reply, NFS3_OK);
        encode_post_op_attr(&mut reply, &PostOpAttr::some(root_attr));
        encode_u64(&mut reply, 0); // total bytes
        encode_u64(&mut reply, 0); // free bytes
        encode_u64(&mut reply, 0); // available bytes
        encode_u64(&mut reply, 0); // total file slots
        encode_u64(&mut reply, 0); // free file slots
        encode_u64(&mut reply, 0); // available file slots
        encode_u32(&mut reply, 0); // invarsec (no consistency)
    } else {
        encode_u32(&mut reply, NFS3ERR_STALE);
        encode_post_op_attr(&mut reply, &PostOpAttr::none());
    }

    reply
}

/// WRITE — write `data` at `offset` into a file (RFC 1813 §3.3.7).
///
/// Args: `file (nfs_fh3)` + `offset (uint64)` + `count (uint32)` +
/// `stable (uint32)` + `data (opaque<>)`. The reply on success is
/// `file_wcc (wcc_data)` + `count` + `committed (uint32)` + `verf (8 bytes)`.
fn handle_write(args: &[u8], ctx: &Arc<NfsContext>) -> Vec<u8> {
    let mut reply = Vec::new();

    let Ok((fh, rest)) = decode_fh(args) else {
        encode_u32(&mut reply, NFS3ERR_STALE);
        encode_wcc_data(&mut reply, &PostOpAttr::none());
        return reply;
    };
    let Some(path) = lookup_existing_path(ctx, &fh) else {
        encode_u32(&mut reply, NFS3ERR_STALE);
        encode_wcc_data(&mut reply, &PostOpAttr::none());
        return reply;
    };
    let Ok((offset, rest)) = decode_u64(rest) else {
        encode_u32(&mut reply, NFS3ERR_INVAL);
        encode_wcc_data(&mut reply, &PostOpAttr::none());
        return reply;
    };
    // count + stable precede the data; the opaque length is authoritative.
    let Ok((_count, rest)) = decode_u32(rest) else {
        encode_u32(&mut reply, NFS3ERR_INVAL);
        encode_wcc_data(&mut reply, &PostOpAttr::none());
        return reply;
    };
    let Ok((_stable, rest)) = decode_u32(rest) else {
        encode_u32(&mut reply, NFS3ERR_INVAL);
        encode_wcc_data(&mut reply, &PostOpAttr::none());
        return reply;
    };
    let Ok((data, _)) = decode_opaque(rest) else {
        encode_u32(&mut reply, NFS3ERR_INVAL);
        encode_wcc_data(&mut reply, &PostOpAttr::none());
        return reply;
    };

    match write::write_file(ctx, &path, offset, data) {
        Ok(new_size) => {
            let attr = make_attributes_with_size(&path, false, new_size);
            let written = u32::try_from(data.len()).unwrap_or(u32::MAX);
            encode_u32(&mut reply, NFS3_OK);
            encode_wcc_data(&mut reply, &PostOpAttr::some(attr));
            encode_u32(&mut reply, written); // count
            encode_u32(&mut reply, NFS3_FILE_SYNC); // committed
            encode_u64(&mut reply, 0); // write verifier
        }
        Err(e) => {
            encode_u32(&mut reply, write_error_status(e));
            encode_wcc_data(&mut reply, &PostOpAttr::none());
        }
    }

    reply
}

/// CREATE — create a regular file in a directory (RFC 1813 §3.3.8).
///
/// Args: `where (dir_fh + name)` + `how (createhow3)`. The reply on success is
/// `obj (post_op_fh3)` + `obj_attributes (post_op_attr)` + `dir_wcc (wcc_data)`.
fn handle_create(args: &[u8], ctx: &Arc<NfsContext>) -> Vec<u8> {
    let mut reply = Vec::new();

    let Ok((dir_fh, rest)) = decode_fh(args) else {
        encode_u32(&mut reply, NFS3ERR_STALE);
        encode_wcc_data(&mut reply, &PostOpAttr::none());
        return reply;
    };
    let Ok((name, rest)) = decode_string(rest) else {
        encode_u32(&mut reply, NFS3ERR_INVAL);
        encode_wcc_data(&mut reply, &PostOpAttr::none());
        return reply;
    };
    // Decode the createhow3 mode discriminant. GUARDED/EXCLUSIVE must refuse an
    // existing target with NFS3ERR_EXIST; UNCHECKED must not truncate one. The
    // trailing sattr3 / verifier carries no content effect for Cascade and is
    // not consumed further — CREATE is the last meaningful decode of its args.
    let Ok((mode, _rest)) = decode_u32(rest) else {
        encode_u32(&mut reply, NFS3ERR_INVAL);
        encode_wcc_data(&mut reply, &PostOpAttr::none());
        return reply;
    };
    let create_mode = match mode {
        NFS3_CREATE_GUARDED => CreateMode::Guarded,
        NFS3_CREATE_EXCLUSIVE => CreateMode::Exclusive,
        _ => CreateMode::Unchecked,
    };
    let parent_path = dir_path_for_fh(ctx, &dir_fh);

    match write::create_file(ctx, &parent_path, &name, create_mode) {
        Ok(child_path) => {
            let key = ctx.register_path(&child_path);
            let child_fh = NfsFh3::from_item_id(&key.to_string());
            let attr = make_attributes_with_size(&child_path, false, 0);
            encode_u32(&mut reply, NFS3_OK);
            encode_post_op_fh3(&mut reply, Some(&child_fh));
            encode_post_op_attr(&mut reply, &PostOpAttr::some(attr));
            encode_wcc_data(&mut reply, &PostOpAttr::none());
        }
        Err(e) => {
            encode_u32(&mut reply, write_error_status(e));
            encode_wcc_data(&mut reply, &PostOpAttr::none());
        }
    }

    reply
}

/// SETATTR — change file attributes; the only mutation honoured is a size
/// change (truncate / extend) (RFC 1813 §3.3.2).
///
/// Args: `object (nfs_fh3)` + `new_attributes (sattr3)` + `guard`. The reply on
/// success is `obj_wcc (wcc_data)`.
fn handle_setattr(args: &[u8], ctx: &Arc<NfsContext>) -> Vec<u8> {
    let mut reply = Vec::new();

    let Ok((fh, rest)) = decode_fh(args) else {
        encode_u32(&mut reply, NFS3ERR_STALE);
        encode_wcc_data(&mut reply, &PostOpAttr::none());
        return reply;
    };
    let Some(path) = lookup_existing_path(ctx, &fh) else {
        encode_u32(&mut reply, NFS3ERR_STALE);
        encode_wcc_data(&mut reply, &PostOpAttr::none());
        return reply;
    };
    let Ok((attrs, _rest)) = decode_sattr3(rest) else {
        encode_u32(&mut reply, NFS3ERR_INVAL);
        encode_wcc_data(&mut reply, &PostOpAttr::none());
        return reply;
    };

    // Only a size change has a content effect. Mode/uid/gid/time changes are
    // accepted as no-ops — the backends model neither — so a client `chmod`
    // does not fail the whole SETATTR.
    let Sattr3 { size, .. } = attrs;
    if let Some(new_size) = size {
        match write::truncate_file(ctx, &path, new_size) {
            Ok(()) => {
                let attr = make_attributes_with_size(&path, false, new_size);
                encode_u32(&mut reply, NFS3_OK);
                encode_wcc_data(&mut reply, &PostOpAttr::some(attr));
            }
            Err(e) => {
                encode_u32(&mut reply, write_error_status(e));
                encode_wcc_data(&mut reply, &PostOpAttr::none());
            }
        }
    } else {
        let current = file_size_sync(ctx, &path);
        let attr = make_attributes_with_size(&path, false, current);
        encode_u32(&mut reply, NFS3_OK);
        encode_wcc_data(&mut reply, &PostOpAttr::some(attr));
    }

    reply
}

/// MKDIR — create a directory (RFC 1813 §3.3.9).
fn handle_mkdir(args: &[u8], ctx: &Arc<NfsContext>) -> Vec<u8> {
    let mut reply = Vec::new();

    let Ok((dir_fh, rest)) = decode_fh(args) else {
        encode_u32(&mut reply, NFS3ERR_STALE);
        encode_wcc_data(&mut reply, &PostOpAttr::none());
        return reply;
    };
    let Ok((name, _rest)) = decode_string(rest) else {
        encode_u32(&mut reply, NFS3ERR_INVAL);
        encode_wcc_data(&mut reply, &PostOpAttr::none());
        return reply;
    };
    let parent_path = dir_path_for_fh(ctx, &dir_fh);

    match write::make_dir(ctx, &parent_path, &name) {
        Ok(child_path) => {
            let key = ctx.register_path(&child_path);
            let child_fh = NfsFh3::from_item_id(&key.to_string());
            let attr = make_attributes_with_size(&child_path, true, 0);
            encode_u32(&mut reply, NFS3_OK);
            encode_post_op_fh3(&mut reply, Some(&child_fh));
            encode_post_op_attr(&mut reply, &PostOpAttr::some(attr));
            encode_wcc_data(&mut reply, &PostOpAttr::none());
        }
        Err(e) => {
            encode_u32(&mut reply, write_error_status(e));
            encode_wcc_data(&mut reply, &PostOpAttr::none());
        }
    }

    reply
}

/// REMOVE — delete a file (RFC 1813 §3.3.12).
fn handle_remove(args: &[u8], ctx: &Arc<NfsContext>) -> Vec<u8> {
    handle_unlink(args, ctx, false)
}

/// RMDIR — delete a directory (RFC 1813 §3.3.13).
fn handle_rmdir(args: &[u8], ctx: &Arc<NfsContext>) -> Vec<u8> {
    handle_unlink(args, ctx, true)
}

/// Shared body for REMOVE and RMDIR. Args: `object (dir_fh + name)`. The reply
/// on success is `dir_wcc (wcc_data)`.
fn handle_unlink(args: &[u8], ctx: &Arc<NfsContext>, expect_dir: bool) -> Vec<u8> {
    let mut reply = Vec::new();

    let Ok((dir_fh, rest)) = decode_fh(args) else {
        encode_u32(&mut reply, NFS3ERR_STALE);
        encode_wcc_data(&mut reply, &PostOpAttr::none());
        return reply;
    };
    let Ok((name, _rest)) = decode_string(rest) else {
        encode_u32(&mut reply, NFS3ERR_INVAL);
        encode_wcc_data(&mut reply, &PostOpAttr::none());
        return reply;
    };
    let parent_path = dir_path_for_fh(ctx, &dir_fh);

    match write::remove_entry(ctx, &parent_path, &name, expect_dir) {
        Ok(child_path) => {
            ctx.remove_path(NfsContext::path_to_key(&child_path));
            encode_u32(&mut reply, NFS3_OK);
            encode_wcc_data(&mut reply, &PostOpAttr::none());
        }
        Err(e) => {
            // A kind mismatch (file vs directory) is reported as `Invalid`. A
            // non-empty directory is refused by the backend's non-recursive
            // delete as `Conflict`, which for RMDIR maps to NFS3ERR_NOTEMPTY.
            let status = match (&e, expect_dir) {
                (WriteError::Conflict, true) => NFS3ERR_NOTEMPTY,
                _ => write_error_status(e),
            };
            encode_u32(&mut reply, status);
            encode_wcc_data(&mut reply, &PostOpAttr::none());
        }
    }

    reply
}

/// RENAME — move/rename an entry (RFC 1813 §3.3.14).
///
/// Args: `from (dir_fh + name)` + `to (dir_fh + name)`. The reply on success is
/// `fromdir_wcc (wcc_data)` + `todir_wcc (wcc_data)`.
fn handle_rename(args: &[u8], ctx: &Arc<NfsContext>) -> Vec<u8> {
    let mut reply = Vec::new();

    let Ok((from_fh, rest)) = decode_fh(args) else {
        encode_u32(&mut reply, NFS3ERR_STALE);
        encode_wcc_data(&mut reply, &PostOpAttr::none());
        encode_wcc_data(&mut reply, &PostOpAttr::none());
        return reply;
    };
    let Ok((from_name, rest)) = decode_string(rest) else {
        encode_u32(&mut reply, NFS3ERR_INVAL);
        encode_wcc_data(&mut reply, &PostOpAttr::none());
        encode_wcc_data(&mut reply, &PostOpAttr::none());
        return reply;
    };
    let Ok((to_fh, rest)) = decode_fh(rest) else {
        encode_u32(&mut reply, NFS3ERR_STALE);
        encode_wcc_data(&mut reply, &PostOpAttr::none());
        encode_wcc_data(&mut reply, &PostOpAttr::none());
        return reply;
    };
    let Ok((to_name, _rest)) = decode_string(rest) else {
        encode_u32(&mut reply, NFS3ERR_INVAL);
        encode_wcc_data(&mut reply, &PostOpAttr::none());
        encode_wcc_data(&mut reply, &PostOpAttr::none());
        return reply;
    };

    let from_parent = dir_path_for_fh(ctx, &from_fh);
    let to_parent = dir_path_for_fh(ctx, &to_fh);

    match write::rename_entry(ctx, &from_parent, &from_name, &to_parent, &to_name) {
        Ok((src_path, _dst_path)) => {
            ctx.remove_path(NfsContext::path_to_key(&src_path));
            encode_u32(&mut reply, NFS3_OK);
            encode_wcc_data(&mut reply, &PostOpAttr::none());
            encode_wcc_data(&mut reply, &PostOpAttr::none());
        }
        Err(e) => {
            encode_u32(&mut reply, write_error_status(e));
            encode_wcc_data(&mut reply, &PostOpAttr::none());
            encode_wcc_data(&mut reply, &PostOpAttr::none());
        }
    }

    reply
}

/// COMMIT — flush buffered writes (RFC 1813 §3.3.21).
///
/// Cascade replies `NFS3_FILE_SYNC` for every WRITE, so there is no buffered
/// data to flush. COMMIT therefore succeeds immediately with empty `wcc_data`
/// and a zero verifier.
fn handle_commit() -> Vec<u8> {
    let mut reply = Vec::new();
    encode_u32(&mut reply, NFS3_OK);
    encode_wcc_data(&mut reply, &PostOpAttr::none());
    encode_u64(&mut reply, 0); // write verifier
    reply
}

/// Unimplemented procedure.
fn handle_unimplemented() -> Vec<u8> {
    let mut reply = Vec::new();
    encode_u32(&mut reply, NFS3ERR_INVAL);
    reply
}

/// Look up the VFS path for a file handle, returning `None` for the root or any
/// unregistered handle — used by procedures that operate on an existing file.
fn lookup_existing_path(ctx: &NfsContext, fh: &NfsFh3) -> Option<String> {
    let key = fh.to_item_id().and_then(|id| id.parse::<u64>().ok())?;
    ctx.lookup_path(key)
}

/// Fetch the current size of a file via the backend, returning 0 when the file
/// is absent or its size is unknown.
fn file_size_sync(ctx: &NfsContext, path: &str) -> u64 {
    ctx.metadata_sync(path)
        .ok()
        .and_then(|entry| entry.size)
        .unwrap_or(0)
}

/// Build synthetic attributes for a file/directory with an unknown (zero)
/// size. Used by procedures that do not report content length (LOOKUP,
/// READDIR, FSSTAT, READ — which carries its own byte count).
fn make_attributes(id: &str, is_dir: bool) -> Fattr3 {
    make_attributes_with_size(id, is_dir, 0)
}

/// Build synthetic attributes for a file/directory with a known size.
fn make_attributes_with_size(id: &str, is_dir: bool, size: u64) -> Fattr3 {
    Fattr3 {
        ftype: if is_dir { NF3DIR } else { NF3REG },
        mode: if is_dir { 0o755 } else { 0o644 },
        nlink: if is_dir { 2 } else { 1 },
        uid: 501,
        gid: 20,
        size,
        used: size,
        rdev: Specdata3::default(),
        fsid: 0,
        fileid: id_hash(id),
        atime: NfsTime::epoch(),
        mtime: NfsTime::epoch(),
        ctime: NfsTime::epoch(),
    }
}

/// Simple hash of a string to a u64 fileid.
fn id_hash(s: &str) -> u64 {
    let mut hash: u64 = 5381;
    for byte in s.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(u64::from(byte));
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use cascade_engine::backend::NullBackend;
    use cascade_engine::vfs::VfsTree;
    use std::sync::RwLock;

    fn test_ctx() -> Arc<NfsContext> {
        let vfs = Arc::new(RwLock::new(VfsTree::new(Arc::new(NullBackend::new(
            "test",
        )))));
        let ctx = Arc::new(NfsContext::new(vfs));
        // Register root so GETATTR works.
        ctx.register_path("/");
        ctx
    }

    #[test]
    fn getattr_valid_fh() {
        let ctx = test_ctx();
        let root_key = ctx.root_key();
        let fh = NfsFh3::from_item_id(&root_key.to_string());
        let mut args = Vec::new();
        encode_fh(&mut args, &fh);

        let reply = handle_nfs_call(NFS3PROC_GETATTR, &args, &ctx, NfsCacheMode::Minimal);
        let (status, _) = decode_u32(&reply).unwrap();
        assert_eq!(status, NFS3_OK);
    }

    #[test]
    fn getattr_invalid_fh() {
        let ctx = test_ctx();
        let fh = NfsFh3 {
            data: [0u8; super::super::xdr::NFS3_FHSIZE],
        };
        let mut args = Vec::new();
        encode_fh(&mut args, &fh);

        let reply = handle_nfs_call(NFS3PROC_GETATTR, &args, &ctx, NfsCacheMode::Minimal);
        let (status, _) = decode_u32(&reply).unwrap();
        assert_eq!(status, NFS3ERR_STALE);
    }

    #[test]
    fn lookup_resolves_name() {
        let ctx = test_ctx();
        let root_key = ctx.root_key();
        let fh = NfsFh3::from_item_id(&root_key.to_string());
        let mut args = Vec::new();
        encode_fh(&mut args, &fh);
        encode_string(&mut args, "Documents");

        let reply = handle_nfs_call(NFS3PROC_LOOKUP, &args, &ctx, NfsCacheMode::Minimal);
        let (status, _) = decode_u32(&reply).unwrap();
        assert_eq!(status, NFS3_OK);

        // Verify child path was registered.
        let child_key = NfsContext::path_to_key("/Documents");
        assert_eq!(ctx.lookup_path(child_key), Some("/Documents".to_string()));
    }

    #[test]
    fn fsstat_returns_ok() {
        let ctx = test_ctx();
        let root_key = ctx.root_key();
        let fh = NfsFh3::from_item_id(&root_key.to_string());
        let mut args = Vec::new();
        encode_fh(&mut args, &fh);

        let reply = handle_nfs_call(NFS3PROC_FSSTAT, &args, &ctx, NfsCacheMode::Minimal);
        let (status, _) = decode_u32(&reply).unwrap();
        assert_eq!(status, NFS3_OK);
    }
}

/// Write-path integration tests backed by a real `LocalBackend` over a tempdir,
/// so each procedure exercises the actual engine write operations rather than a
/// stub.
#[cfg(test)]
mod write_tests {
    use super::*;
    use cascade_engine::vfs::VfsTree;
    use std::sync::RwLock;
    use tempfile::TempDir;

    /// Build a write-capable context rooted at a fresh tempdir, returning the
    /// context and the `TempDir` guard (which must outlive the context).
    fn writable_ctx() -> (Arc<NfsContext>, TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = toml::Value::Table({
            let mut t = toml::map::Map::new();
            t.insert(
                "root_path".to_string(),
                toml::Value::String(dir.path().to_string_lossy().into_owned()),
            );
            t.insert("id".to_string(), toml::Value::String("local".to_string()));
            t
        });
        let backend: Arc<dyn cascade_engine::backend::Backend> =
            cascade_backend_local::create_backend(&config)
                .expect("create local backend")
                .into();
        let vfs = Arc::new(RwLock::new(VfsTree::new(backend)));
        let ctx = Arc::new(NfsContext::new(vfs));
        ctx.register_path("/");
        (ctx, dir)
    }

    /// Run a synchronous NFS handler that internally uses `block_on`, from
    /// within a multi-thread Tokio test, by entering a blocking section.
    fn call(ctx: &Arc<NfsContext>, proc: u32, args: &[u8], mode: NfsCacheMode) -> Vec<u8> {
        tokio::task::block_in_place(|| handle_nfs_call(proc, args, ctx, mode))
    }

    /// Encode a `WRITE3args` body: `file_fh` + offset + count + stable + data.
    fn write_args(fh: &NfsFh3, offset: u64, data: &[u8]) -> Vec<u8> {
        let mut args = Vec::new();
        encode_fh(&mut args, fh);
        encode_u64(&mut args, offset);
        encode_u32(&mut args, u32::try_from(data.len()).unwrap_or(u32::MAX));
        encode_u32(&mut args, super::super::xdr::NFS3_UNSTABLE);
        super::super::xdr::encode_opaque(&mut args, data);
        args
    }

    /// Encode a `CREATE3args` body: `dir_fh` + name + `createhow3` (UNCHECKED,
    /// empty `sattr3`).
    fn create_args(dir_fh: &NfsFh3, name: &str) -> Vec<u8> {
        create_args_mode(dir_fh, name, super::super::xdr::NFS3_CREATE_UNCHECKED)
    }

    /// Encode a `CREATE3args` body with an explicit `createmode3` discriminant
    /// and an empty `sattr3`.
    fn create_args_mode(dir_fh: &NfsFh3, name: &str, mode: u32) -> Vec<u8> {
        let mut args = Vec::new();
        encode_fh(&mut args, dir_fh);
        encode_string(&mut args, name);
        encode_u32(&mut args, mode);
        // Empty sattr3: every set_* discriminant false; two set_time DONT_CHANGE.
        for _ in 0..4 {
            encode_bool(&mut args, false);
        }
        encode_u32(&mut args, 0); // atime: DONT_CHANGE
        encode_u32(&mut args, 0); // mtime: DONT_CHANGE
        args
    }

    /// Encode a `diropargs3` body: `dir_fh` + name. Shared by MKDIR/REMOVE/RMDIR.
    fn dirop_args(dir_fh: &NfsFh3, name: &str) -> Vec<u8> {
        let mut args = Vec::new();
        encode_fh(&mut args, dir_fh);
        encode_string(&mut args, name);
        args
    }

    /// Encode an `MKDIR3args` body: `dir_fh` + name + empty `sattr3`.
    fn mkdir_args(dir_fh: &NfsFh3, name: &str) -> Vec<u8> {
        let mut args = dirop_args(dir_fh, name);
        for _ in 0..4 {
            encode_bool(&mut args, false);
        }
        encode_u32(&mut args, 0); // atime DONT_CHANGE
        encode_u32(&mut args, 0); // mtime DONT_CHANGE
        args
    }

    /// Encode a `SETATTR3args` body: `object_fh` + `sattr3` (size set) + guard.
    fn setattr_size_args(fh: &NfsFh3, size: u64) -> Vec<u8> {
        let mut args = Vec::new();
        encode_fh(&mut args, fh);
        encode_bool(&mut args, false); // mode unset
        encode_bool(&mut args, false); // uid unset
        encode_bool(&mut args, false); // gid unset
        encode_bool(&mut args, true); // size set
        encode_u64(&mut args, size);
        encode_u32(&mut args, 0); // atime DONT_CHANGE
        encode_u32(&mut args, 0); // mtime DONT_CHANGE
        encode_bool(&mut args, false); // guard: no check
        args
    }

    /// Register a file path and return its file handle.
    fn fh_for(ctx: &NfsContext, path: &str) -> NfsFh3 {
        let key = ctx.register_path(path);
        NfsFh3::from_item_id(&key.to_string())
    }

    /// GETATTR size for a registered path.
    fn getattr_size(ctx: &Arc<NfsContext>, path: &str) -> u64 {
        let fh = fh_for(ctx, path);
        let mut args = Vec::new();
        encode_fh(&mut args, &fh);
        let reply = call(ctx, NFS3PROC_GETATTR, &args, NfsCacheMode::Minimal);
        let (status, rest) = decode_u32(&reply).unwrap();
        assert_eq!(status, NFS3_OK);
        // post_op_attr: attributes_follow (bool) then fattr3. size is the 6th
        // field (ftype, mode, nlink, uid, gid are u32 — 5 words — then size u64).
        let (follows, rest) = super::super::xdr::decode_bool(rest).unwrap();
        assert!(follows);
        let mut cursor = rest;
        for _ in 0..5 {
            let (_, r) = decode_u32(cursor).unwrap();
            cursor = r;
        }
        let (size, _) = decode_u64(cursor).unwrap();
        size
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn write_then_getattr_reflects_new_size() {
        let (ctx, _dir) = writable_ctx();
        let file_fh = fh_for(&ctx, "/note.txt");

        let payload = b"hello world";
        let args = write_args(&file_fh, 0, payload);
        let reply = call(&ctx, NFS3PROC_WRITE, &args, NfsCacheMode::Minimal);
        let (status, _) = decode_u32(&reply).unwrap();
        assert_eq!(status, NFS3_OK);

        let size = getattr_size(&ctx, "/note.txt");
        assert_eq!(size, payload.len() as u64);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn write_at_offset_extends_file() {
        let (ctx, _dir) = writable_ctx();
        let file_fh = fh_for(&ctx, "/sparse.bin");

        // Write 4 bytes starting at offset 8: total length must be 12.
        let args = write_args(&file_fh, 8, b"DATA");
        let reply = call(&ctx, NFS3PROC_WRITE, &args, NfsCacheMode::Minimal);
        let (status, _) = decode_u32(&reply).unwrap();
        assert_eq!(status, NFS3_OK);

        assert_eq!(getattr_size(&ctx, "/sparse.bin"), 12);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_then_lookup_finds_file() {
        let (ctx, _dir) = writable_ctx();
        let root_fh = NfsFh3::from_item_id(&ctx.root_key().to_string());

        let args = create_args(&root_fh, "fresh.txt");
        let reply = call(&ctx, NFS3PROC_CREATE, &args, NfsCacheMode::Minimal);
        let (status, _) = decode_u32(&reply).unwrap();
        assert_eq!(status, NFS3_OK);

        // LOOKUP must now resolve the created name.
        let mut lookup = Vec::new();
        encode_fh(&mut lookup, &root_fh);
        encode_string(&mut lookup, "fresh.txt");
        let reply = call(&ctx, NFS3PROC_LOOKUP, &lookup, NfsCacheMode::Minimal);
        let (status, _) = decode_u32(&reply).unwrap();
        assert_eq!(status, NFS3_OK);

        assert_eq!(getattr_size(&ctx, "/fresh.txt"), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn setattr_truncate_shrinks_file() {
        let (ctx, _dir) = writable_ctx();
        let file_fh = fh_for(&ctx, "/big.txt");

        let args = write_args(&file_fh, 0, b"0123456789");
        call(&ctx, NFS3PROC_WRITE, &args, NfsCacheMode::Minimal);
        assert_eq!(getattr_size(&ctx, "/big.txt"), 10);

        let args = setattr_size_args(&file_fh, 4);
        let reply = call(&ctx, NFS3PROC_SETATTR, &args, NfsCacheMode::Minimal);
        let (status, _) = decode_u32(&reply).unwrap();
        assert_eq!(status, NFS3_OK);

        assert_eq!(getattr_size(&ctx, "/big.txt"), 4);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mkdir_then_remove_and_rmdir() {
        let (ctx, _dir) = writable_ctx();
        let root_fh = NfsFh3::from_item_id(&ctx.root_key().to_string());

        // MKDIR.
        let reply = call(
            &ctx,
            NFS3PROC_MKDIR,
            &mkdir_args(&root_fh, "sub"),
            NfsCacheMode::Minimal,
        );
        assert_eq!(decode_u32(&reply).unwrap().0, NFS3_OK);

        // Create a file inside it, then REMOVE that file.
        let sub_fh = fh_for(&ctx, "/sub");
        let inner_fh = fh_for(&ctx, "/sub/inner.txt");
        call(
            &ctx,
            NFS3PROC_WRITE,
            &write_args(&inner_fh, 0, b"x"),
            NfsCacheMode::Minimal,
        );
        let reply = call(
            &ctx,
            NFS3PROC_REMOVE,
            &dirop_args(&sub_fh, "inner.txt"),
            NfsCacheMode::Minimal,
        );
        assert_eq!(decode_u32(&reply).unwrap().0, NFS3_OK);

        // RMDIR the now-empty directory.
        let reply = call(
            &ctx,
            NFS3PROC_RMDIR,
            &dirop_args(&root_fh, "sub"),
            NfsCacheMode::Minimal,
        );
        assert_eq!(decode_u32(&reply).unwrap().0, NFS3_OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn guarded_create_over_existing_returns_exist() {
        let (ctx, _dir) = writable_ctx();
        let root_fh = NfsFh3::from_item_id(&ctx.root_key().to_string());

        // Populate the file first.
        let file_fh = fh_for(&ctx, "/lock.txt");
        call(
            &ctx,
            NFS3PROC_WRITE,
            &write_args(&file_fh, 0, b"held"),
            NfsCacheMode::Minimal,
        );
        assert_eq!(getattr_size(&ctx, "/lock.txt"), 4);

        // GUARDED create over the existing name must be refused with EXIST and
        // must not truncate the existing content.
        let args = create_args_mode(&root_fh, "lock.txt", super::super::xdr::NFS3_CREATE_GUARDED);
        let reply = call(&ctx, NFS3PROC_CREATE, &args, NfsCacheMode::Minimal);
        assert_eq!(decode_u32(&reply).unwrap().0, NFS3ERR_EXIST);
        assert_eq!(getattr_size(&ctx, "/lock.txt"), 4);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unchecked_create_over_existing_preserves_content() {
        let (ctx, _dir) = writable_ctx();
        let root_fh = NfsFh3::from_item_id(&ctx.root_key().to_string());

        let file_fh = fh_for(&ctx, "/keep.txt");
        call(
            &ctx,
            NFS3PROC_WRITE,
            &write_args(&file_fh, 0, b"0123456789"),
            NfsCacheMode::Minimal,
        );
        assert_eq!(getattr_size(&ctx, "/keep.txt"), 10);

        // UNCHECKED create over the existing name must succeed without
        // truncating: the content (and size) survive.
        let args = create_args(&root_fh, "keep.txt");
        let reply = call(&ctx, NFS3PROC_CREATE, &args, NfsCacheMode::Minimal);
        assert_eq!(decode_u32(&reply).unwrap().0, NFS3_OK);
        assert_eq!(getattr_size(&ctx, "/keep.txt"), 10);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rmdir_on_non_empty_directory_returns_notempty_and_preserves_contents() {
        let (ctx, dir) = writable_ctx();
        let root_fh = NfsFh3::from_item_id(&ctx.root_key().to_string());

        // MKDIR then place a file inside it.
        call(
            &ctx,
            NFS3PROC_MKDIR,
            &mkdir_args(&root_fh, "populated"),
            NfsCacheMode::Minimal,
        );
        let inner_fh = fh_for(&ctx, "/populated/child.txt");
        call(
            &ctx,
            NFS3PROC_WRITE,
            &write_args(&inner_fh, 0, b"alive"),
            NfsCacheMode::Minimal,
        );

        // RMDIR on the populated directory must fail NOTEMPTY, not wipe it.
        let reply = call(
            &ctx,
            NFS3PROC_RMDIR,
            &dirop_args(&root_fh, "populated"),
            NfsCacheMode::Minimal,
        );
        assert_eq!(decode_u32(&reply).unwrap().0, NFS3ERR_NOTEMPTY);

        // Both the directory and its child are still on disk.
        assert!(dir.path().join("populated").is_dir());
        assert_eq!(
            std::fs::read(dir.path().join("populated/child.txt")).unwrap(),
            b"alive"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rename_moves_file() {
        let (ctx, _dir) = writable_ctx();
        let root_fh = NfsFh3::from_item_id(&ctx.root_key().to_string());
        let file_fh = fh_for(&ctx, "/old.txt");
        call(
            &ctx,
            NFS3PROC_WRITE,
            &write_args(&file_fh, 0, b"payload"),
            NfsCacheMode::Minimal,
        );

        // RENAME3args: from(dir_fh+name) + to(dir_fh+name).
        let mut args = Vec::new();
        encode_fh(&mut args, &root_fh);
        encode_string(&mut args, "old.txt");
        encode_fh(&mut args, &root_fh);
        encode_string(&mut args, "new.txt");
        let reply = call(&ctx, NFS3PROC_RENAME, &args, NfsCacheMode::Minimal);
        assert_eq!(decode_u32(&reply).unwrap().0, NFS3_OK);

        assert_eq!(getattr_size(&ctx, "/new.txt"), b"payload".len() as u64);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn write_on_read_only_export_returns_rofs() {
        let (ctx, _dir) = writable_ctx();
        let file_fh = fh_for(&ctx, "/blocked.txt");
        let reply = call(
            &ctx,
            NFS3PROC_WRITE,
            &write_args(&file_fh, 0, b"nope"),
            NfsCacheMode::Off,
        );
        assert_eq!(decode_u32(&reply).unwrap().0, NFS3ERR_ROFS);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_on_read_only_export_returns_rofs() {
        let (ctx, _dir) = writable_ctx();
        let root_fh = NfsFh3::from_item_id(&ctx.root_key().to_string());
        let reply = call(
            &ctx,
            NFS3PROC_CREATE,
            &create_args(&root_fh, "nope.txt"),
            NfsCacheMode::Off,
        );
        assert_eq!(decode_u32(&reply).unwrap().0, NFS3ERR_ROFS);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rmdir_on_missing_directory_returns_stale() {
        let (ctx, _dir) = writable_ctx();
        let root_fh = NfsFh3::from_item_id(&ctx.root_key().to_string());
        let reply = call(
            &ctx,
            NFS3PROC_RMDIR,
            &dirop_args(&root_fh, "ghost"),
            NfsCacheMode::Minimal,
        );
        assert_eq!(decode_u32(&reply).unwrap().0, NFS3ERR_STALE);
    }
}
