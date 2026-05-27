//! NFS procedure handlers.
//!
//! Phase 1 read-only procedures: NULL, GETATTR, LOOKUP, READDIR, READ, FSSTAT.
//! Write procedures return NFS3ERR_ROFS.

use super::context::NfsContext;
use super::xdr::*;
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
        NFS3PROC_SETATTR
        | NFS3PROC_WRITE
        | NFS3PROC_CREATE
        | NFS3PROC_MKDIR
        | NFS3PROC_REMOVE
        | NFS3PROC_RMDIR
        | NFS3PROC_RENAME
        | NFS3PROC_COMMIT => handle_readonly(),
        _ => handle_unimplemented(),
    }
}

/// NULL procedure — do nothing.
fn _handle_null() -> Vec<u8> {
    vec![]
}

/// GETATTR — return file/directory metadata.
fn handle_getattr(args: &[u8], ctx: &Arc<NfsContext>) -> Vec<u8> {
    let mut reply = Vec::new();

    match decode_fh(args) {
        Ok((fh, _)) => {
            let key = fh.to_item_id()
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
        }
        Err(_) => {
            encode_u32(&mut reply, NFS3ERR_INVAL);
            encode_post_op_attr(&mut reply, &PostOpAttr::none());
        }
    }

    reply
}

/// LOOKUP — resolve a name in a directory.
fn handle_lookup(args: &[u8], ctx: &Arc<NfsContext>) -> Vec<u8> {
    let mut reply = Vec::new();

    // Args: dir_fh + name
    let dir_fh = decode_fh(args);
    match dir_fh {
        Ok((fh, rest)) => {
            let name_result = decode_string(rest);
            match name_result {
                Ok((name, _)) => {
                    // Resolve parent path from context.
                    let parent_key = fh.to_item_id()
                        .and_then(|id| id.parse::<u64>().ok())
                        .unwrap_or(0);
                    let parent_path = ctx.lookup_path(parent_key).unwrap_or_else(|| "/".to_string());
                    let child_path = if parent_path == "/" {
                        format!("/{}", name)
                    } else {
                        format!("{}/{}", parent_path, name)
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
                }
                Err(_) => {
                    encode_u32(&mut reply, NFS3ERR_INVAL);
                    encode_post_op_attr(&mut reply, &PostOpAttr::none());
                    encode_post_op_attr(&mut reply, &PostOpAttr::none());
                }
            }
        }
        Err(_) => {
            encode_u32(&mut reply, NFS3ERR_STALE);
            encode_post_op_attr(&mut reply, &PostOpAttr::none());
            encode_post_op_attr(&mut reply, &PostOpAttr::none());
        }
    }

    reply
}

/// READDIR — list directory contents.
fn handle_readdir(args: &[u8], ctx: &Arc<NfsContext>) -> Vec<u8> {
    let mut reply = Vec::new();

    // Args: dir_fh + cookie + cookieverf + dircount + maxcount
    let dir_result = decode_fh(args);
    match dir_result {
        Ok((fh, _rest)) => {
            let dir_key = fh.to_item_id()
                .and_then(|id| id.parse::<u64>().ok())
                .unwrap_or(0);
            let dir_path = ctx.lookup_path(dir_key).unwrap_or_else(|| "/".to_string());
            let dir_attr = make_attributes(&dir_path, true);

            encode_u32(&mut reply, NFS3_OK);
            encode_post_op_attr(&mut reply, &PostOpAttr::some(dir_attr));
            // cookieverf (8 bytes of zero).
            encode_u64(&mut reply, 0);
            // No entries — actual directory listing requires async VFS query
            // which can't be done from a sync procedure handler.
            // Phase 1: return empty directory. Phase 2: pre-populate from sync.
            encode_bool(&mut reply, false); // no more entries
            encode_bool(&mut reply, true); // EOF
        }
        Err(_) => {
            encode_u32(&mut reply, NFS3ERR_STALE);
            encode_post_op_attr(&mut reply, &PostOpAttr::none());
        }
    }

    reply
}

/// READ — read file data.
fn handle_read(args: &[u8], _ctx: &Arc<NfsContext>) -> Vec<u8> {
    let mut reply: Vec<u8> = Vec::new();

    // Args: file_fh + offset + count
    let file_result = decode_fh(args);
    match file_result {
        Ok((fh, rest)) => {
            let offset = decode_u64(rest).ok().map(|(o, r)| (o, r));
            let count = offset.and_then(|(_, r)| decode_u32(r).ok()).map(|(c, _)| c);

            let file_id = fh.to_item_id().unwrap_or("root".to_string());
            let file_attr = make_attributes(&file_id, false);

            // Phase 1: return empty data.
            encode_u32(&mut reply, NFS3_OK);
            encode_post_op_attr(&mut reply, &PostOpAttr::some(file_attr));
            encode_u32(&mut reply, count.unwrap_or(0)); // count of bytes returned
            encode_bool(&mut reply, false); // not EOF (empty read)
            encode_u32(&mut reply, 0); // 0 bytes of data
        }
        Err(_) => {
            encode_u32(&mut reply, NFS3ERR_STALE);
            encode_post_op_attr(&mut reply, &PostOpAttr::none());
        }
    }

    reply
}

/// FSSTAT — return filesystem statistics.
fn handle_fsstat(args: &[u8], _ctx: &Arc<NfsContext>) -> Vec<u8> {
    let mut reply: Vec<u8> = Vec::new();

    match decode_fh(args) {
        Ok((_fh, _)) => {
            let root_attr = make_attributes(&"root".to_string(), true);
            encode_u32(&mut reply, NFS3_OK);
            encode_post_op_attr(&mut reply, &PostOpAttr::some(root_attr));
            encode_u64(&mut reply, 0); // total bytes
            encode_u64(&mut reply, 0); // free bytes
            encode_u64(&mut reply, 0); // available bytes
            encode_u64(&mut reply, 0); // total file slots
            encode_u64(&mut reply, 0); // free file slots
            encode_u64(&mut reply, 0); // available file slots
            encode_u32(&mut reply, 0); // invarsec (no consistency)
        }
        Err(_) => {
            encode_u32(&mut reply, NFS3ERR_STALE);
            encode_post_op_attr(&mut reply, &PostOpAttr::none());
        }
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
        hash = hash.wrapping_mul(33).wrapping_add(byte as u64);
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
        let vfs = Arc::new(RwLock::new(
            VfsTree::new(Arc::new(NullBackend::new("test"))),
        ));
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
        let fh = NfsFh3 { data: [0u8; NFS3_FHSIZE] };
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
