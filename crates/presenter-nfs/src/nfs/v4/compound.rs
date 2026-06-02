//! `NFSv4` COMPOUND procedure handler.
//!
//! Implements the COMPOUND procedure which chains multiple `NFSv4`
//! operations into a single request. This is the core of `NFSv4` —
//! unlike `NFSv3`'s individual procedures, `NFSv4` uses COMPOUND for
//! everything.

use super::state::StateManager;
use super::xdr::{
    self, ACCESS4_DELETE, ACCESS4_EXECUTE, ACCESS4_LOOKUP, ACCESS4_MODIFY, ACCESS4_READ,
    FATTR4_FILEID, FATTR4_MODE, FATTR4_SIZE, FATTR4_TYPE, Fattr4, NFS4_OK, NFS4ERR_BADHANDLE,
    NFS4ERR_INVAL, NFS4ERR_IO, NFS4ERR_NOTSUPP, NFS4ERR_ROFS, NfsFh4, OP_ACCESS, OP_CLOSE,
    OP_COMMIT, OP_CREATE, OP_GETATTR, OP_GETFH, OP_LINK, OP_LOCK, OP_LOCKT, OP_LOCKU, OP_LOOKUP,
    OP_LOOKUPP, OP_OPEN, OP_PUTFH, OP_PUTROOTFH, OP_READ, OP_READDIR, OP_READLINK, OP_REMOVE,
    OP_RENAME, OP_RESTOREFH, OP_SAVEFH, OP_SETATTR, OP_WRITE,
};
use crate::nfs::context::NfsContext;
use crate::nfs::server::NfsCacheMode;
use std::sync::Arc;

/// The status an unimplemented write operation reports for a given cache mode.
///
/// A read-only export ([`NfsCacheMode::Off`]) refuses writes with
/// `NFS4ERR_ROFS`. A write-capable export reports `NFS4ERR_NOTSUPP` because the
/// write path is not yet implemented — distinct from a deliberately read-only
/// mount. This mirrors the `NFSv3` gating in [`crate::nfs::procedures`].
const fn write_unsupported_status(cache_mode: NfsCacheMode) -> u32 {
    if cache_mode.writes_permitted() {
        NFS4ERR_NOTSUPP
    } else {
        NFS4ERR_ROFS
    }
}

/// Handle an `NFSv4` COMPOUND request.
///
/// The COMPOUND args are: tag (`utf8str_cs`) + minorversion + `argarray<nl_item>`.
/// Each `nl_item` is: (opnum, `op_args`).
///
/// Returns the COMPOUND reply: status + tag + `data<nl_item>`.
pub fn handle_compound(
    args: &[u8],
    ctx: &Arc<NfsContext>,
    state_mgr: &Arc<StateManager>,
    cache_mode: NfsCacheMode,
) -> Vec<u8> {
    let write_unsupported = write_unsupported_status(cache_mode);
    let mut reply = Vec::new();

    // Decode tag.
    let Ok((tag_str, cursor)) = xdr::decode_string(args) else {
        xdr::encode_u32(&mut reply, NFS4ERR_INVAL);
        xdr::encode_u32(&mut reply, 0);
        return reply;
    };
    let _ = tag_str;

    // Decode minorversion.
    let Ok((minor_version, cursor)) = xdr::decode_u32(cursor) else {
        xdr::encode_u32(&mut reply, NFS4ERR_INVAL);
        xdr::encode_u32(&mut reply, 0);
        return reply;
    };
    let _ = minor_version;

    // Decode number of operations.
    let Ok((num_ops, mut cursor)) = xdr::decode_u32(cursor) else {
        xdr::encode_u32(&mut reply, NFS4ERR_INVAL);
        xdr::encode_u32(&mut reply, 0);
        return reply;
    };

    // Current file handle state for this compound.
    let mut current_fh: Option<NfsFh4> = None;
    let mut saved_fh: Option<NfsFh4> = None;

    // Reply header: status (placeholder), tag.
    let status_offset = reply.len();
    xdr::encode_u32(&mut reply, NFS4_OK);
    xdr::encode_string(&mut reply, "");
    xdr::encode_u32(&mut reply, num_ops);

    let mut overall_status = NFS4_OK;

    for _ in 0..num_ops {
        let Ok((opnum, rest)) = xdr::decode_u32(cursor) else {
            overall_status = NFS4ERR_INVAL;
            break;
        };
        cursor = rest;

        let op_reply = match opnum {
            OP_PUTROOTFH => {
                current_fh = Some(NfsFh4::root());
                op_status_reply(NFS4_OK)
            }
            OP_PUTFH => {
                let Ok((fh, rest)) = xdr::decode_fh4(cursor) else {
                    overall_status = NFS4ERR_INVAL;
                    break;
                };
                cursor = rest;
                fh.to_path().map_or_else(
                    || op_status_reply(NFS4ERR_BADHANDLE),
                    |path| {
                        let key = NfsContext::path_to_key(&path);
                        if ctx.lookup_path(key).is_some() || path == "/" {
                            current_fh = Some(fh);
                        } else {
                            ctx.register_path(&path);
                            current_fh = Some(NfsFh4::from_path(&path));
                        }
                        op_status_reply(NFS4_OK)
                    },
                )
            }
            OP_GETFH => current_fh.as_ref().map_or_else(
                || op_status_reply(NFS4ERR_BADHANDLE),
                |fh| {
                    let mut r = op_status_reply(NFS4_OK);
                    xdr::encode_fh4(&mut r, fh);
                    r
                },
            ),
            OP_LOOKUP => {
                let Ok((name_str, rest)) = xdr::decode_string(cursor) else {
                    overall_status = NFS4ERR_INVAL;
                    break;
                };
                cursor = rest;
                match &current_fh {
                    Some(dir_fh) => {
                        let dir_path = dir_fh.to_path().unwrap_or_default();
                        let child_path = if dir_path == "/" {
                            format!("/{name_str}")
                        } else {
                            format!("{dir_path}/{name_str}")
                        };
                        ctx.register_path(&child_path);
                        current_fh = Some(NfsFh4::from_path(&child_path));
                        op_status_reply(NFS4_OK)
                    }
                    None => op_status_reply(NFS4ERR_BADHANDLE),
                }
            }
            OP_LOOKUPP => match &current_fh {
                Some(fh) => {
                    let path = fh.to_path().unwrap_or_default();
                    let parent = parent_path(&path);
                    if parent != path {
                        ctx.register_path(&parent);
                        current_fh = Some(NfsFh4::from_path(&parent));
                    }
                    op_status_reply(NFS4_OK)
                }
                None => op_status_reply(NFS4ERR_BADHANDLE),
            },
            OP_GETATTR => {
                let Ok((bitmap, rest)) = xdr::decode_attr_bitmap(cursor) else {
                    overall_status = NFS4ERR_INVAL;
                    break;
                };
                cursor = rest;
                current_fh.as_ref().map_or_else(
                    || op_status_reply(NFS4ERR_BADHANDLE),
                    |fh| {
                        let path = fh.to_path().unwrap_or_default();
                        let is_dir = path.ends_with('/') || path == "/" || path.is_empty();
                        let attr = make_attributes(&path, is_dir);
                        let mut r = op_status_reply(NFS4_OK);
                        xdr::encode_fattr4(&mut r, &bitmap, &attr);
                        r
                    },
                )
            }
            OP_ACCESS => {
                let Ok((_requested, rest)) = xdr::decode_u32(cursor) else {
                    overall_status = NFS4ERR_INVAL;
                    break;
                };
                cursor = rest;
                current_fh.as_ref().map_or_else(
                    || op_status_reply(NFS4ERR_BADHANDLE),
                    |fh| {
                        let path = fh.to_path().unwrap_or_default();
                        let is_dir = path == "/" || path.ends_with('/');
                        let supported = if is_dir {
                            ACCESS4_READ | ACCESS4_LOOKUP | ACCESS4_DELETE
                        } else {
                            ACCESS4_READ | ACCESS4_MODIFY | ACCESS4_EXECUTE
                        };
                        let mut r = op_status_reply(NFS4_OK);
                        xdr::encode_u32(&mut r, supported);
                        xdr::encode_u32(&mut r, supported);
                        r
                    },
                )
            }
            OP_READDIR => handle_readdir(&mut cursor, ctx, current_fh.as_ref()),
            OP_READ => handle_read(&mut cursor, ctx, current_fh.as_ref()),
            OP_WRITE => handle_write(&mut cursor, write_unsupported),
            OP_OPEN => handle_open(&mut cursor, ctx, state_mgr, current_fh.as_ref()),
            OP_CLOSE => handle_close(&mut cursor, state_mgr),
            OP_CREATE => handle_create(&mut cursor, current_fh.as_ref(), write_unsupported),
            OP_REMOVE => handle_remove(&mut cursor, current_fh.as_ref(), write_unsupported),
            OP_RENAME => handle_rename(
                &mut cursor,
                current_fh.as_ref(),
                saved_fh.as_ref(),
                write_unsupported,
            ),
            OP_SAVEFH => {
                saved_fh.clone_from(&current_fh);
                op_status_reply(NFS4_OK)
            }
            OP_RESTOREFH => {
                current_fh.clone_from(&saved_fh);
                op_status_reply(NFS4_OK)
            }
            OP_COMMIT => op_status_reply(NFS4_OK),
            OP_SETATTR => {
                if let Ok((_, rest)) = xdr::decode_stateid(cursor)
                    && let Ok((_, rest)) = xdr::decode_attr_bitmap(rest)
                {
                    cursor = rest;
                }
                op_status_reply(write_unsupported)
            }
            OP_READLINK | OP_LOCK | OP_LOCKT | OP_LOCKU => op_status_reply(NFS4ERR_INVAL),
            OP_LINK => op_status_reply(write_unsupported),
            _ => {
                overall_status = NFS4ERR_INVAL;
                break;
            }
        };

        reply.extend_from_slice(&op_reply);

        // Check the op status (first 4 bytes of op_reply).
        let op_status: [u8; 4] = op_reply
            .get(0..4)
            .map_or([0, 0, 0, 0], |s| s.try_into().unwrap_or([0u8; 4]));
        let op_status = u32::from_be_bytes(op_status);

        if op_status != NFS4_OK {
            overall_status = op_status;
            break;
        }
    }

    // Patch the overall status in the reply.
    let status_bytes = overall_status.to_be_bytes();
    if let Some(slot) = reply.get_mut(status_offset..status_offset + 4) {
        slot.copy_from_slice(&status_bytes);
    }

    reply
}

/// Handle READDIR operation.
fn handle_readdir(cursor: &mut &[u8], ctx: &NfsContext, current_fh: Option<&NfsFh4>) -> Vec<u8> {
    let Ok((cookie, rest)) = xdr::decode_u64(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    let Ok((_verifier, rest)) = xdr::decode_u64(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    let Ok((_dircount, rest)) = xdr::decode_u32(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    let Ok((_maxcount, rest)) = xdr::decode_u32(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    let Ok((_bitmap, rest)) = xdr::decode_attr_bitmap(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    current_fh.map_or_else(
        || op_status_reply(NFS4ERR_BADHANDLE),
        |fh| {
            let dir_path = fh.to_path().unwrap_or_default();
            let Ok(entries) = ctx.list_dir_sync(&dir_path) else {
                return op_status_reply(NFS4ERR_IO);
            };

            let mut reply = op_status_reply(NFS4_OK);
            xdr::encode_u64(&mut reply, 0); // cookieverf

            for (i, entry) in entries.iter().enumerate() {
                let entry_cookie = u64::try_from(i).unwrap_or(u64::MAX) + 3;
                if entry_cookie <= cookie {
                    continue;
                }

                let child_path = if dir_path == "/" {
                    format!("/{}", entry.name)
                } else {
                    format!("{}/{}", dir_path, entry.name)
                };
                ctx.register_path(&child_path);

                xdr::encode_bool(&mut reply, true); // value follows
                xdr::encode_string(&mut reply, &entry.name);
                xdr::encode_u64(&mut reply, entry_cookie);

                let entry_attr = make_attributes(&child_path, entry.is_dir);
                let attr_bitmap = vec![FATTR4_TYPE, FATTR4_SIZE, FATTR4_FILEID, FATTR4_MODE];
                xdr::encode_fattr4(&mut reply, &attr_bitmap, &entry_attr);
            }

            xdr::encode_bool(&mut reply, false); // no more entries
            xdr::encode_bool(&mut reply, true); // EOF

            reply
        },
    )
}

/// Handle READ operation.
fn handle_read(cursor: &mut &[u8], ctx: &NfsContext, current_fh: Option<&NfsFh4>) -> Vec<u8> {
    let Ok((_stateid, rest)) = xdr::decode_stateid(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    let Ok((offset, rest)) = xdr::decode_u64(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    let Ok((count, rest)) = xdr::decode_u32(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    current_fh.map_or_else(
        || op_status_reply(NFS4ERR_BADHANDLE),
        |fh| {
            let path = fh.to_path().unwrap_or_default();
            let Ok(data) = fetch_file_data_sync(ctx, &path, offset, count) else {
                return op_status_reply(NFS4ERR_IO);
            };

            let eof = data.is_empty();
            let mut reply = op_status_reply(NFS4_OK);
            xdr::encode_bool(&mut reply, eof);
            xdr::encode_opaque(&mut reply, &data);
            reply
        },
    )
}

/// Handle WRITE operation.
///
/// The write path is not yet implemented. `unsupported_status` carries the
/// mode-dependent refusal: `NFS4ERR_ROFS` on a read-only export, otherwise
/// `NFS4ERR_NOTSUPP`.
fn handle_write(cursor: &mut &[u8], unsupported_status: u32) -> Vec<u8> {
    if let Ok((_, rest)) = xdr::decode_stateid(cursor)
        && let Ok((_, rest)) = xdr::decode_u64(rest)
        && let Ok((_, rest)) = xdr::decode_u32(rest)
        && let Ok((_, rest)) = xdr::decode_opaque(rest)
    {
        *cursor = rest;
    }
    op_status_reply(unsupported_status)
}

/// Handle OPEN operation (minimal — creates state but no real file I/O).
fn handle_open(
    cursor: &mut &[u8],
    ctx: &NfsContext,
    state_mgr: &Arc<StateManager>,
    current_fh: Option<&NfsFh4>,
) -> Vec<u8> {
    let Ok((_seqid, rest)) = xdr::decode_u32(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    let Ok((share_access, rest)) = xdr::decode_u32(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    let Ok((_share_deny, rest)) = xdr::decode_u32(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    let Ok((_clientid, rest)) = xdr::decode_u64(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    let Ok((_owner, rest)) = xdr::decode_opaque(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    let Ok((create_mode, rest)) = xdr::decode_u32(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    if create_mode == 0 || create_mode == 1 {
        if let Ok((_, rest)) = xdr::decode_attr_bitmap(cursor) {
            *cursor = rest;
        }
    } else if let Ok((_, rest)) = xdr::decode_u64(cursor) {
        // EXCLUSIVE: createverf (uint64).
        *cursor = rest;
    }

    let Ok((claim_type, rest)) = xdr::decode_u32(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    let file_name = if claim_type == 0 {
        let Ok((name, rest)) = xdr::decode_string(cursor) else {
            return op_status_reply(NFS4ERR_INVAL);
        };
        *cursor = rest;
        name
    } else {
        return op_status_reply(NFS4ERR_INVAL);
    };

    current_fh.map_or_else(
        || op_status_reply(NFS4ERR_BADHANDLE),
        |dir_fh| {
            let dir_path = dir_fh.to_path().unwrap_or_default();
            let file_path = if dir_path == "/" {
                format!("/{file_name}")
            } else {
                format!("{dir_path}/{file_name}")
            };

            ctx.register_path(&file_path);

            let sid = state_mgr.create_open(&file_path, share_access);
            let _file_attr = make_attributes(&file_path, false);

            let mut reply = op_status_reply(NFS4_OK);
            xdr::encode_stateid(&mut reply, &sid);
            // cinfo (change_info): atomic, before, after.
            xdr::encode_bool(&mut reply, true);
            xdr::encode_u64(&mut reply, 0);
            xdr::encode_u64(&mut reply, 1);
            // delegation: NONE.
            xdr::encode_u32(&mut reply, 0);
            // attrset: empty bitmap.
            xdr::encode_attr_bitmap(&mut reply, &[]);
            reply
        },
    )
}

/// Handle CLOSE operation.
fn handle_close(cursor: &mut &[u8], state_mgr: &Arc<StateManager>) -> Vec<u8> {
    let Ok((_seqid, rest)) = xdr::decode_u32(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    let Ok((sid, rest)) = xdr::decode_stateid(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    state_mgr.close_open(&sid);
    let mut reply = op_status_reply(NFS4_OK);
    xdr::encode_stateid(&mut reply, &sid);
    reply
}

/// Handle CREATE operation.
///
/// The write path is not yet implemented. `unsupported_status` carries the
/// mode-dependent refusal: `NFS4ERR_ROFS` on a read-only export, otherwise
/// `NFS4ERR_NOTSUPP`.
fn handle_create(
    cursor: &mut &[u8],
    current_fh: Option<&NfsFh4>,
    unsupported_status: u32,
) -> Vec<u8> {
    let Ok((_obj_type, rest)) = xdr::decode_u32(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    let Ok((_name, rest)) = xdr::decode_string(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    if let Ok((_, rest)) = xdr::decode_attr_bitmap(cursor) {
        *cursor = rest;
    }

    let _ = current_fh;
    op_status_reply(unsupported_status)
}

/// Handle REMOVE operation.
///
/// The write path is not yet implemented. `unsupported_status` carries the
/// mode-dependent refusal: `NFS4ERR_ROFS` on a read-only export, otherwise
/// `NFS4ERR_NOTSUPP`.
fn handle_remove(
    cursor: &mut &[u8],
    current_fh: Option<&NfsFh4>,
    unsupported_status: u32,
) -> Vec<u8> {
    let Ok((_name, rest)) = xdr::decode_string(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    let _ = current_fh;
    op_status_reply(unsupported_status)
}

/// Handle RENAME operation.
///
/// The write path is not yet implemented. `unsupported_status` carries the
/// mode-dependent refusal: `NFS4ERR_ROFS` on a read-only export, otherwise
/// `NFS4ERR_NOTSUPP`.
fn handle_rename(
    cursor: &mut &[u8],
    current_fh: Option<&NfsFh4>,
    saved_fh: Option<&NfsFh4>,
    unsupported_status: u32,
) -> Vec<u8> {
    let Ok((_oldname, rest)) = xdr::decode_string(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    let Ok((_newname, rest)) = xdr::decode_string(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    let _ = (current_fh, saved_fh);
    op_status_reply(unsupported_status)
}

/// Build attributes for a path.
fn make_attributes(path: &str, is_dir: bool) -> Fattr4 {
    super::xdr::make_fattr4(path, is_dir)
}

/// Build a minimal operation status reply.
fn op_status_reply(status: u32) -> Vec<u8> {
    let mut r = Vec::with_capacity(4);
    xdr::encode_u32(&mut r, status);
    r
}

/// Compute the parent path of a VFS path.
#[allow(clippy::string_slice)] // VFS paths are ASCII-safe; rfind('/') returns byte boundaries
fn parent_path(path: &str) -> String {
    if path == "/" || path.is_empty() {
        return "/".to_string();
    }
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(idx) => trimmed[..idx].to_string(),
    }
}

/// Synchronously fetch file data from the VFS backend.
fn fetch_file_data_sync(
    ctx: &NfsContext,
    path: &str,
    offset: u64,
    max_count: u32,
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

        let off = usize::try_from(offset).unwrap_or(usize::MAX);
        if off >= buf.len() {
            return Ok(Vec::new());
        }
        let remaining = buf
            .get(off..)
            .ok_or_else(|| anyhow::anyhow!("offset out of bounds"))?;
        let max = usize::try_from(max_count).unwrap_or(usize::MAX);
        let end = max.min(remaining.len());
        Ok(remaining
            .get(..end)
            .ok_or_else(|| anyhow::anyhow!("slice out of bounds"))?
            .to_vec())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nfs::context::NfsContext;
    use cascade_engine::backend::NullBackend;
    use cascade_engine::vfs::VfsTree;
    use std::sync::RwLock;

    fn test_ctx() -> (Arc<NfsContext>, Arc<StateManager>) {
        let vfs = Arc::new(RwLock::new(VfsTree::new(Arc::new(NullBackend::new(
            "test",
        )))));
        let ctx = Arc::new(NfsContext::new(vfs));
        ctx.register_path("/");
        let state_mgr = Arc::new(StateManager::new());
        (ctx, state_mgr)
    }

    fn build_compound(ops: &[(u32, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        xdr::encode_string(&mut buf, ""); // tag
        xdr::encode_u32(&mut buf, 0); // minorversion
        xdr::encode_u32(&mut buf, u32::try_from(ops.len()).unwrap_or(0));
        for &(opnum, args) in ops {
            xdr::encode_u32(&mut buf, opnum);
            buf.extend_from_slice(args);
        }
        buf
    }

    #[test]
    fn compound_putrootfh_getfh() {
        let (ctx, state_mgr) = test_ctx();
        let args = build_compound(&[(OP_PUTROOTFH, &[]), (OP_GETFH, &[])]);
        let reply = handle_compound(&args, &ctx, &state_mgr, NfsCacheMode::Minimal);
        let (status, _) = xdr::decode_u32(&reply).unwrap();
        assert_eq!(status, NFS4_OK);
    }

    #[test]
    fn compound_putrootfh_getattr() {
        let (ctx, state_mgr) = test_ctx();
        let mut bitmap_args = Vec::new();
        xdr::encode_attr_bitmap(&mut bitmap_args, &[FATTR4_TYPE, FATTR4_SIZE]);

        let args = build_compound(&[(OP_PUTROOTFH, &[]), (OP_GETATTR, &bitmap_args)]);
        let reply = handle_compound(&args, &ctx, &state_mgr, NfsCacheMode::Minimal);
        let (status, _) = xdr::decode_u32(&reply).unwrap();
        assert_eq!(status, NFS4_OK);
    }

    #[test]
    fn compound_lookup() {
        let (ctx, state_mgr) = test_ctx();
        let mut lookup_args = Vec::new();
        xdr::encode_string(&mut lookup_args, "Documents");

        let args = build_compound(&[
            (OP_PUTROOTFH, &[]),
            (OP_LOOKUP, &lookup_args),
            (OP_GETFH, &[]),
        ]);
        let reply = handle_compound(&args, &ctx, &state_mgr, NfsCacheMode::Minimal);
        let (status, _) = xdr::decode_u32(&reply).unwrap();
        assert_eq!(status, NFS4_OK);
    }

    #[test]
    fn compound_getfh_without_putfh_fails() {
        let (ctx, state_mgr) = test_ctx();
        let args = build_compound(&[(OP_GETFH, &[])]);
        let reply = handle_compound(&args, &ctx, &state_mgr, NfsCacheMode::Minimal);
        let (status, _) = xdr::decode_u32(&reply).unwrap();
        assert_eq!(status, NFS4ERR_BADHANDLE);
    }

    #[test]
    fn compound_access() {
        let (ctx, state_mgr) = test_ctx();
        let mut access_args = Vec::new();
        xdr::encode_u32(&mut access_args, ACCESS4_READ | ACCESS4_LOOKUP);

        let args = build_compound(&[(OP_PUTROOTFH, &[]), (OP_ACCESS, &access_args)]);
        let reply = handle_compound(&args, &ctx, &state_mgr, NfsCacheMode::Minimal);
        let (status, _) = xdr::decode_u32(&reply).unwrap();
        assert_eq!(status, NFS4_OK);
    }

    #[test]
    fn compound_savefh_restorefh() {
        let (ctx, state_mgr) = test_ctx();
        let mut lookup_args = Vec::new();
        xdr::encode_string(&mut lookup_args, "Documents");

        let args = build_compound(&[
            (OP_PUTROOTFH, &[]),
            (OP_SAVEFH, &[]),
            (OP_LOOKUP, &lookup_args),
            (OP_RESTOREFH, &[]),
            (OP_GETFH, &[]),
        ]);
        let reply = handle_compound(&args, &ctx, &state_mgr, NfsCacheMode::Minimal);
        let (status, _) = xdr::decode_u32(&reply).unwrap();
        assert_eq!(status, NFS4_OK);
    }

    fn write_compound_args() -> Vec<u8> {
        let mut write_args = Vec::new();
        xdr::encode_stateid(&mut write_args, &super::super::xdr::StateId::zero());
        xdr::encode_u64(&mut write_args, 0);
        xdr::encode_u32(&mut write_args, 0);
        xdr::encode_opaque(&mut write_args, b"test");
        build_compound(&[(OP_PUTROOTFH, &[]), (OP_WRITE, &write_args)])
    }

    #[test]
    fn write_on_read_only_export_returns_rofs() {
        let (ctx, state_mgr) = test_ctx();
        let args = write_compound_args();
        let reply = handle_compound(&args, &ctx, &state_mgr, NfsCacheMode::Off);
        let (status, _) = xdr::decode_u32(&reply).unwrap();
        assert_eq!(status, NFS4ERR_ROFS);
    }

    #[test]
    fn write_on_write_capable_export_returns_notsupp() {
        let (ctx, state_mgr) = test_ctx();
        for mode in [NfsCacheMode::Minimal, NfsCacheMode::Full] {
            let args = write_compound_args();
            let reply = handle_compound(&args, &ctx, &state_mgr, mode);
            let (status, _) = xdr::decode_u32(&reply).unwrap();
            assert_eq!(status, NFS4ERR_NOTSUPP);
        }
    }

    #[test]
    fn compound_close() {
        let (ctx, state_mgr) = test_ctx();
        let sid = state_mgr.create_open("/test.txt", 1);

        let mut close_args = Vec::new();
        xdr::encode_u32(&mut close_args, 0); // seqid
        xdr::encode_stateid(&mut close_args, &sid);

        let args = build_compound(&[(OP_PUTROOTFH, &[]), (OP_CLOSE, &close_args)]);
        let reply = handle_compound(&args, &ctx, &state_mgr, NfsCacheMode::Minimal);
        let (status, _) = xdr::decode_u32(&reply).unwrap();
        assert_eq!(status, NFS4_OK);

        assert!(state_mgr.lookup_open(&sid).is_none());
    }

    #[test]
    fn compound_lookupp() {
        let (ctx, state_mgr) = test_ctx();
        let mut lookup_args = Vec::new();
        xdr::encode_string(&mut lookup_args, "Documents");

        let args = build_compound(&[
            (OP_PUTROOTFH, &[]),
            (OP_LOOKUP, &lookup_args),
            (OP_LOOKUPP, &[]),
            (OP_GETFH, &[]),
        ]);
        let reply = handle_compound(&args, &ctx, &state_mgr, NfsCacheMode::Minimal);
        let (status, _) = xdr::decode_u32(&reply).unwrap();
        assert_eq!(status, NFS4_OK);
    }

    #[test]
    fn parent_path_calculation() {
        assert_eq!(parent_path("/"), "/");
        assert_eq!(parent_path("/Documents"), "/");
        assert_eq!(parent_path("/Documents/Reports"), "/Documents");
        assert_eq!(parent_path("/a/b/c"), "/a/b");
    }
}
