//! NFS procedure handlers.
//!
//! Phase 1 read-only procedures: NULL, GETATTR, LOOKUP, READDIR, READ, FSSTAT.
//! Write procedures return `NFS3ERR_ROFS`.

use super::context::NfsContext;
use super::xdr::{
    decode_fh, decode_string, decode_u32, decode_u64, encode_bool, encode_fh, encode_post_op_attr,
    encode_string, encode_u32, encode_u64, Fattr3, NfsFh3, NfsTime, PostOpAttr, Specdata3,
    NF3DIR, NF3REG, NFS3ERR_INVAL, NFS3ERR_IO, NFS3ERR_ROFS, NFS3ERR_STALE, NFS3_OK,
    NFS3PROC_COMMIT, NFS3PROC_CREATE, NFS3PROC_FSSTAT, NFS3PROC_GETATTR, NFS3PROC_LOOKUP,
    NFS3PROC_MKDIR, NFS3PROC_NULL, NFS3PROC_READ, NFS3PROC_READDIR, NFS3PROC_REMOVE,
    NFS3PROC_RENAME, NFS3PROC_RMDIR, NFS3PROC_SETATTR, NFS3PROC_WRITE,
};
use std::sync::Arc;

/// Handle an NFS procedure call with VFS context.
/// Returns XDR-encoded reply.
pub fn handle_nfs_call(proc: u32, args: &[u8], ctx: &Arc<NfsContext>) -> Vec<u8> {
    match proc {
        NFS3PROC_NULL => vec![],
        NFS3PROC_GETATTR => handle_getattr(args, ctx),
        NFS3PROC_LOOKUP => handle_lookup(args, ctx),
        NFS3PROC_READDIR => handle_readdir(args, ctx),
        NFS3PROC_READ => handle_read(args, ctx),
        NFS3PROC_FSSTAT => handle_fsstat(args, ctx),
        // Phase 1: all write operations return read-only error.
        NFS3PROC_SETATTR | NFS3PROC_WRITE | NFS3PROC_CREATE | NFS3PROC_MKDIR | NFS3PROC_REMOVE
        | NFS3PROC_RMDIR | NFS3PROC_RENAME | NFS3PROC_COMMIT => handle_readonly(),
        _ => handle_unimplemented(),
    }
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
            let attr = make_attributes(&path, path == "/");
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
            Some(path) => {
                match fetch_file_data_sync(ctx, path, offset.map(|(o, _)| o), count) {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::debug!(path = %path, error = %e, "READ: failed to fetch data");
                        encode_u32(&mut reply, NFS3ERR_IO);
                        encode_post_op_attr(&mut reply, &PostOpAttr::some(file_attr));
                        return reply;
                    }
                }
            }
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
        let off = usize::try_from(offset.unwrap_or(0))
            .unwrap_or(usize::MAX);
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

/// Write operations on a read-only filesystem.
fn handle_readonly() -> Vec<u8> {
    let mut reply = Vec::new();
    encode_u32(&mut reply, NFS3ERR_ROFS);
    // Most write replies include post_op_attr for the dir — but for an
    // error reply, wcc_data is sufficient with no pre/post attrs.
    reply
}

/// Unimplemented procedure.
fn handle_unimplemented() -> Vec<u8> {
    let mut reply = Vec::new();
    encode_u32(&mut reply, NFS3ERR_INVAL);
    reply
}

/// Build synthetic attributes for a file/directory.
fn make_attributes(id: &str, is_dir: bool) -> Fattr3 {
    Fattr3 {
        ftype: if is_dir { NF3DIR } else { NF3REG },
        mode: if is_dir { 0o755 } else { 0o644 },
        nlink: if is_dir { 2 } else { 1 },
        uid: 501,
        gid: 20,
        size: 0,
        used: 0,
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

        let reply = handle_nfs_call(NFS3PROC_GETATTR, &args, &ctx);
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

        let reply = handle_nfs_call(NFS3PROC_GETATTR, &args, &ctx);
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

        let reply = handle_nfs_call(NFS3PROC_LOOKUP, &args, &ctx);
        let (status, _) = decode_u32(&reply).unwrap();
        assert_eq!(status, NFS3_OK);

        // Verify child path was registered.
        let child_key = NfsContext::path_to_key("/Documents");
        assert_eq!(ctx.lookup_path(child_key), Some("/Documents".to_string()));
    }

    #[test]
    fn write_returns_rofs() {
        let ctx = test_ctx();
        let reply = handle_nfs_call(NFS3PROC_CREATE, &[], &ctx);
        let (status, _) = decode_u32(&reply).unwrap();
        assert_eq!(status, NFS3ERR_ROFS);
    }

    #[test]
    fn fsstat_returns_ok() {
        let ctx = test_ctx();
        let root_key = ctx.root_key();
        let fh = NfsFh3::from_item_id(&root_key.to_string());
        let mut args = Vec::new();
        encode_fh(&mut args, &fh);

        let reply = handle_nfs_call(NFS3PROC_FSSTAT, &args, &ctx);
        let (status, _) = decode_u32(&reply).unwrap();
        assert_eq!(status, NFS3_OK);
    }
}
