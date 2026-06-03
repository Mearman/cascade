//! `NFSv4` COMPOUND procedure handler.
//!
//! Implements the COMPOUND procedure which chains multiple `NFSv4`
//! operations into a single request. This is the core of `NFSv4` —
//! unlike `NFSv3`'s individual procedures, `NFSv4` uses COMPOUND for
//! everything.

use super::state::StateManager;
use super::xdr::{
    self, ACCESS4_DELETE, ACCESS4_EXECUTE, ACCESS4_LOOKUP, ACCESS4_MODIFY, ACCESS4_READ,
    FATTR4_FILEID, FATTR4_MODE, FATTR4_SIZE, FATTR4_TYPE, FILE_SYNC4, Fattr4, NF4DIR, NFS4_OK,
    NFS4ERR_ACCES, NFS4ERR_BADHANDLE, NFS4ERR_EXIST, NFS4ERR_INVAL, NFS4ERR_IO, NFS4ERR_NOSPC,
    NFS4ERR_NOTEMPTY, NFS4ERR_ROFS, NFS4ERR_STALE, NfsFh4, OP_ACCESS, OP_CLOSE, OP_COMMIT,
    OP_CREATE, OP_GETATTR, OP_GETFH, OP_LINK, OP_LOCK, OP_LOCKT, OP_LOCKU, OP_LOOKUP, OP_LOOKUPP,
    OP_OPEN, OP_PUTFH, OP_PUTROOTFH, OP_READ, OP_READDIR, OP_READLINK, OP_REMOVE, OP_RENAME,
    OP_RESTOREFH, OP_SAVEFH, OP_SETATTR, OP_WRITE,
};
use crate::nfs::context::NfsContext;
use crate::nfs::server::NfsCacheMode;
use crate::nfs::write::{self, WriteError};
use std::sync::Arc;

/// Map a [`WriteError`] to the appropriate `NFS4ERR_*` status code.
const fn write_error_status(err: WriteError) -> u32 {
    match err {
        WriteError::Traversal | WriteError::Forbidden => NFS4ERR_ACCES,
        WriteError::NotFound => NFS4ERR_STALE,
        WriteError::Conflict => NFS4ERR_EXIST,
        WriteError::Invalid => NFS4ERR_INVAL,
        WriteError::NoSpace => NFS4ERR_NOSPC,
        WriteError::Io => NFS4ERR_IO,
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
    let writes_permitted = cache_mode.writes_permitted();
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
                        let size = if is_dir {
                            0
                        } else {
                            file_size_sync(ctx, &path)
                        };
                        let attr = xdr::make_fattr4_with_size(&path, is_dir, size);
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
            OP_WRITE => handle_write(&mut cursor, ctx, current_fh.as_ref(), writes_permitted),
            OP_OPEN => handle_open(&mut cursor, ctx, state_mgr, current_fh.as_ref()),
            OP_CLOSE => handle_close(&mut cursor, state_mgr),
            OP_CREATE => handle_create(&mut cursor, ctx, current_fh.as_ref(), writes_permitted),
            OP_REMOVE => handle_remove(&mut cursor, ctx, current_fh.as_ref(), writes_permitted),
            OP_RENAME => handle_rename(
                &mut cursor,
                ctx,
                current_fh.as_ref(),
                saved_fh.as_ref(),
                writes_permitted,
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
            OP_SETATTR => handle_setattr(&mut cursor, ctx, current_fh.as_ref(), writes_permitted),
            OP_READLINK | OP_LOCK | OP_LOCKT | OP_LOCKU => op_status_reply(NFS4ERR_INVAL),
            OP_LINK => op_status_reply(if writes_permitted {
                NFS4ERR_INVAL
            } else {
                NFS4ERR_ROFS
            }),
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

/// Handle WRITE operation (RFC 7530 §16.36).
///
/// Args: `stateid` + `offset (uint64)` + `stable (uint32)` + `data (opaque<>)`.
/// On a read-only export it returns `NFS4ERR_ROFS`; otherwise it writes through
/// the shared engine path and replies `count` + `committed` + `writeverf`.
fn handle_write(
    cursor: &mut &[u8],
    ctx: &NfsContext,
    current_fh: Option<&NfsFh4>,
    writes_permitted: bool,
) -> Vec<u8> {
    let Ok((_stateid, rest)) = xdr::decode_stateid(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    let Ok((offset, rest)) = xdr::decode_u64(rest) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    let Ok((_stable, rest)) = xdr::decode_u32(rest) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    let Ok((data, rest)) = xdr::decode_opaque(rest) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    if !writes_permitted {
        return op_status_reply(NFS4ERR_ROFS);
    }

    current_fh.map_or_else(
        || op_status_reply(NFS4ERR_BADHANDLE),
        |fh| {
            let path = fh.to_path().unwrap_or_default();
            match write::write_file(ctx, &path, offset, data) {
                Ok(_new_size) => {
                    let written = u32::try_from(data.len()).unwrap_or(u32::MAX);
                    let mut r = op_status_reply(NFS4_OK);
                    xdr::encode_u32(&mut r, written); // count
                    xdr::encode_u32(&mut r, FILE_SYNC4); // committed
                    xdr::encode_u64(&mut r, 0); // writeverf
                    r
                }
                Err(e) => op_status_reply(write_error_status(e)),
            }
        },
    )
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

/// Handle CREATE operation (RFC 7530 §16.4).
///
/// `NFSv4` CREATE makes non-regular objects (directories, symlinks, …); regular
/// files are created via OPEN. Cascade supports directory creation here.
///
/// Args: `objtype (createtype4)` + `objname` + `createattrs (fattr4)`. For a
/// directory the object type carries no extra union arm. On success the reply
/// is `cinfo (change_info4)` + `attrset (bitmap4)`; the new directory becomes
/// the current file handle.
fn handle_create(
    cursor: &mut &[u8],
    ctx: &NfsContext,
    current_fh: Option<&NfsFh4>,
    writes_permitted: bool,
) -> Vec<u8> {
    let Ok((obj_type, rest)) = xdr::decode_u32(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    let Ok((name, rest)) = xdr::decode_string(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    if let Ok((_, rest)) = xdr::decode_attr_bitmap(cursor) {
        *cursor = rest;
    }

    if !writes_permitted {
        return op_status_reply(NFS4ERR_ROFS);
    }

    // Only NF4DIR is supported; other object types (symlink, device, …) are not
    // modelled by the backends.
    if obj_type != NF4DIR {
        return op_status_reply(NFS4ERR_INVAL);
    }

    current_fh.map_or_else(
        || op_status_reply(NFS4ERR_BADHANDLE),
        |dir_fh| {
            let parent_path = dir_fh.to_path().unwrap_or_default();
            match write::make_dir(ctx, &parent_path, &name) {
                Ok(_child_path) => {
                    let mut r = op_status_reply(NFS4_OK);
                    // cinfo (change_info4): atomic, before, after.
                    xdr::encode_bool(&mut r, true);
                    xdr::encode_u64(&mut r, 0);
                    xdr::encode_u64(&mut r, 1);
                    // attrset: empty bitmap.
                    xdr::encode_attr_bitmap(&mut r, &[]);
                    r
                }
                Err(e) => op_status_reply(write_error_status(e)),
            }
        },
    )
}

/// Handle REMOVE operation (RFC 7530 §16.25).
///
/// Args: `target (component4)`. The target is removed from the directory named
/// by the current file handle. On success the reply is `cinfo (change_info4)`.
fn handle_remove(
    cursor: &mut &[u8],
    ctx: &NfsContext,
    current_fh: Option<&NfsFh4>,
    writes_permitted: bool,
) -> Vec<u8> {
    let Ok((name, rest)) = xdr::decode_string(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    if !writes_permitted {
        return op_status_reply(NFS4ERR_ROFS);
    }

    current_fh.map_or_else(
        || op_status_reply(NFS4ERR_BADHANDLE),
        |dir_fh| {
            let parent_path = dir_fh.to_path().unwrap_or_default();
            match write::remove_any(ctx, &parent_path, &name) {
                Ok(child_path) => {
                    ctx.remove_path(NfsContext::path_to_key(&child_path));
                    let mut r = op_status_reply(NFS4_OK);
                    // cinfo (change_info4): atomic, before, after.
                    xdr::encode_bool(&mut r, true);
                    xdr::encode_u64(&mut r, 0);
                    xdr::encode_u64(&mut r, 1);
                    r
                }
                Err(e) => {
                    let status = match e {
                        WriteError::Conflict => NFS4ERR_NOTEMPTY,
                        other => write_error_status(other),
                    };
                    op_status_reply(status)
                }
            }
        },
    )
}

/// Handle RENAME operation (RFC 7530 §16.26).
///
/// Args: `oldname (component4)` + `newname (component4)`. The source directory
/// is the saved file handle; the destination directory is the current file
/// handle. On success the reply is `source_cinfo` + `target_cinfo`.
fn handle_rename(
    cursor: &mut &[u8],
    ctx: &NfsContext,
    current_fh: Option<&NfsFh4>,
    saved_fh: Option<&NfsFh4>,
    writes_permitted: bool,
) -> Vec<u8> {
    let Ok((oldname, rest)) = xdr::decode_string(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    let Ok((newname, rest)) = xdr::decode_string(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    if !writes_permitted {
        return op_status_reply(NFS4ERR_ROFS);
    }

    let (Some(src_fh), Some(dst_fh)) = (saved_fh, current_fh) else {
        return op_status_reply(NFS4ERR_BADHANDLE);
    };
    let from_parent = src_fh.to_path().unwrap_or_default();
    let to_parent = dst_fh.to_path().unwrap_or_default();

    match write::rename_entry(ctx, &from_parent, &oldname, &to_parent, &newname) {
        Ok((src_path, _dst_path)) => {
            ctx.remove_path(NfsContext::path_to_key(&src_path));
            let mut r = op_status_reply(NFS4_OK);
            // source_cinfo.
            xdr::encode_bool(&mut r, true);
            xdr::encode_u64(&mut r, 0);
            xdr::encode_u64(&mut r, 1);
            // target_cinfo.
            xdr::encode_bool(&mut r, true);
            xdr::encode_u64(&mut r, 0);
            xdr::encode_u64(&mut r, 1);
            r
        }
        Err(e) => op_status_reply(write_error_status(e)),
    }
}

/// Handle SETATTR operation (RFC 7530 §16.32).
///
/// Args: `stateid` + `obj_attributes (fattr4)`. The only mutation Cascade
/// honours is a `size` change (truncate / extend); other attribute changes are
/// accepted as no-ops. On success the reply is `attrsset (bitmap4)`.
fn handle_setattr(
    cursor: &mut &[u8],
    ctx: &NfsContext,
    current_fh: Option<&NfsFh4>,
    writes_permitted: bool,
) -> Vec<u8> {
    let Ok((_stateid, rest)) = xdr::decode_stateid(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = rest;

    let Ok((bitmap, rest)) = xdr::decode_attr_bitmap(cursor) else {
        return op_status_reply(NFS4ERR_INVAL);
    };

    // The attribute values follow the bitmap directly, in ascending
    // attribute-number order — matching this codebase's `encode_fattr4`, which
    // does not wrap the values in an `opaque<>`. Consume the whole values blob
    // (not just the size) so the next operation in the COMPOUND stays framed
    // even when the client sets multiple attributes (e.g. size + mtime).
    let Ok((new_size, after_values)) = xdr::decode_setattr_values(&bitmap, rest) else {
        return op_status_reply(NFS4ERR_INVAL);
    };
    *cursor = after_values;

    if !writes_permitted {
        return op_status_reply(NFS4ERR_ROFS);
    }

    current_fh.map_or_else(
        || op_status_reply(NFS4ERR_BADHANDLE),
        |fh| {
            let path = fh.to_path().unwrap_or_default();
            new_size.map_or_else(
                || {
                    let mut r = op_status_reply(NFS4_OK);
                    xdr::encode_attr_bitmap(&mut r, &[]);
                    r
                },
                |size| match write::truncate_file(ctx, &path, size) {
                    Ok(()) => {
                        let mut r = op_status_reply(NFS4_OK);
                        xdr::encode_attr_bitmap(&mut r, &[FATTR4_SIZE]);
                        r
                    }
                    Err(e) => op_status_reply(write_error_status(e)),
                },
            )
        },
    )
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

/// Fetch the current size of a file via the backend, returning 0 when the file
/// is absent or its size is unknown.
fn file_size_sync(ctx: &NfsContext, path: &str) -> u64 {
    ctx.metadata_sync(path)
        .ok()
        .and_then(|entry| entry.size)
        .unwrap_or(0)
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

/// `NFSv4` write-path integration tests backed by a real `LocalBackend`.
#[cfg(test)]
mod write_tests {
    use super::*;
    use crate::nfs::context::NfsContext;
    use crate::nfs::v4::state::StateManager;
    use cascade_engine::vfs::VfsTree;
    use std::sync::RwLock;
    use tempfile::TempDir;

    fn writable_ctx() -> (Arc<NfsContext>, Arc<StateManager>, TempDir) {
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
        (ctx, Arc::new(StateManager::new()), dir)
    }

    fn build_compound(ops: &[(u32, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        xdr::encode_string(&mut buf, "");
        xdr::encode_u32(&mut buf, 0);
        xdr::encode_u32(&mut buf, u32::try_from(ops.len()).unwrap_or(0));
        for &(opnum, args) in ops {
            xdr::encode_u32(&mut buf, opnum);
            buf.extend_from_slice(args);
        }
        buf
    }

    fn run(
        ctx: &Arc<NfsContext>,
        state_mgr: &Arc<StateManager>,
        ops: &[(u32, &[u8])],
        mode: NfsCacheMode,
    ) -> Vec<u8> {
        let args = build_compound(ops);
        tokio::task::block_in_place(|| handle_compound(&args, ctx, state_mgr, mode))
    }

    fn lookup_op(name: &str) -> Vec<u8> {
        let mut a = Vec::new();
        xdr::encode_string(&mut a, name);
        a
    }

    fn write_op(offset: u64, data: &[u8]) -> Vec<u8> {
        let mut a = Vec::new();
        xdr::encode_stateid(&mut a, &super::super::xdr::StateId::zero());
        xdr::encode_u64(&mut a, offset);
        xdr::encode_u32(&mut a, 0); // UNSTABLE4
        xdr::encode_opaque(&mut a, data);
        a
    }

    /// Resolve the file size via a PUTROOTFH/LOOKUP/GETATTR compound.
    fn getattr_size(ctx: &Arc<NfsContext>, state_mgr: &Arc<StateManager>, name: &str) -> u64 {
        let mut bitmap = Vec::new();
        xdr::encode_attr_bitmap(&mut bitmap, &[super::super::xdr::FATTR4_SIZE]);
        let lookup = lookup_op(name);
        let reply = run(
            ctx,
            state_mgr,
            &[
                (OP_PUTROOTFH, &[]),
                (OP_LOOKUP, &lookup),
                (OP_GETATTR, &bitmap),
            ],
            NfsCacheMode::Minimal,
        );
        // Walk the compound reply to the GETATTR op result. Layout: overall
        // status + tag(string) + numops, then per-op: opnum-less status blocks
        // appended by `op_status_reply`. Each op reply here begins with its
        // status; the GETATTR op reply is status + fattr4 (bitmap + size).
        let (overall, rest) = xdr::decode_u32(&reply).unwrap();
        assert_eq!(overall, NFS4_OK);
        let (_tag, rest) = xdr::decode_string(rest).unwrap();
        let (_numops, rest) = xdr::decode_u32(rest).unwrap();
        // PUTROOTFH op: status only.
        let (s0, rest) = xdr::decode_u32(rest).unwrap();
        assert_eq!(s0, NFS4_OK);
        // LOOKUP op: status only.
        let (s1, rest) = xdr::decode_u32(rest).unwrap();
        assert_eq!(s1, NFS4_OK);
        // GETATTR op: status + bitmap + size value.
        let (s2, rest) = xdr::decode_u32(rest).unwrap();
        assert_eq!(s2, NFS4_OK);
        let (_returned_bitmap, rest) = xdr::decode_attr_bitmap(rest).unwrap();
        let (size, _) = xdr::decode_u64(rest).unwrap();
        size
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn write_then_getattr_reflects_size() {
        let (ctx, state_mgr, _dir) = writable_ctx();
        let lookup = lookup_op("v4.txt");
        let write = write_op(0, b"abcdef");
        let reply = run(
            &ctx,
            &state_mgr,
            &[
                (OP_PUTROOTFH, &[]),
                (OP_LOOKUP, &lookup),
                (OP_WRITE, &write),
            ],
            NfsCacheMode::Minimal,
        );
        assert_eq!(xdr::decode_u32(&reply).unwrap().0, NFS4_OK);

        assert_eq!(getattr_size(&ctx, &state_mgr, "v4.txt"), 6);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_makes_directory() {
        let (ctx, state_mgr, _dir) = writable_ctx();
        // CREATE objtype=NF4DIR, name, empty createattrs.
        let mut create = Vec::new();
        xdr::encode_u32(&mut create, NF4DIR);
        xdr::encode_string(&mut create, "v4dir");
        xdr::encode_attr_bitmap(&mut create, &[]);

        let reply = run(
            &ctx,
            &state_mgr,
            &[(OP_PUTROOTFH, &[]), (OP_CREATE, &create)],
            NfsCacheMode::Minimal,
        );
        assert_eq!(xdr::decode_u32(&reply).unwrap().0, NFS4_OK);

        // The directory must now resolve via LOOKUP.
        let lookup = lookup_op("v4dir");
        let reply = run(
            &ctx,
            &state_mgr,
            &[(OP_PUTROOTFH, &[]), (OP_LOOKUP, &lookup)],
            NfsCacheMode::Minimal,
        );
        assert_eq!(xdr::decode_u32(&reply).unwrap().0, NFS4_OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn setattr_truncate_changes_size() {
        let (ctx, state_mgr, _dir) = writable_ctx();
        let lookup = lookup_op("trunc.txt");
        let write = write_op(0, b"0123456789");
        run(
            &ctx,
            &state_mgr,
            &[
                (OP_PUTROOTFH, &[]),
                (OP_LOOKUP, &lookup),
                (OP_WRITE, &write),
            ],
            NfsCacheMode::Minimal,
        );
        assert_eq!(getattr_size(&ctx, &state_mgr, "trunc.txt"), 10);

        // SETATTR size=3: stateid + bitmap([SIZE]) + size value.
        let mut setattr = Vec::new();
        xdr::encode_stateid(&mut setattr, &super::super::xdr::StateId::zero());
        xdr::encode_attr_bitmap(&mut setattr, &[super::super::xdr::FATTR4_SIZE]);
        xdr::encode_u64(&mut setattr, 3);

        let lookup = lookup_op("trunc.txt");
        let reply = run(
            &ctx,
            &state_mgr,
            &[
                (OP_PUTROOTFH, &[]),
                (OP_LOOKUP, &lookup),
                (OP_SETATTR, &setattr),
            ],
            NfsCacheMode::Minimal,
        );
        assert_eq!(xdr::decode_u32(&reply).unwrap().0, NFS4_OK);

        assert_eq!(getattr_size(&ctx, &state_mgr, "trunc.txt"), 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn multi_attr_setattr_keeps_following_op_framed() {
        let (ctx, state_mgr, _dir) = writable_ctx();
        let lookup = lookup_op("multi.txt");
        let write = write_op(0, b"0123456789");
        run(
            &ctx,
            &state_mgr,
            &[
                (OP_PUTROOTFH, &[]),
                (OP_LOOKUP, &lookup),
                (OP_WRITE, &write),
            ],
            NfsCacheMode::Minimal,
        );
        assert_eq!(getattr_size(&ctx, &state_mgr, "multi.txt"), 10);

        // SETATTR setting size (4) AND time_modify (53): stateid + bitmap + size
        // value (u64) + nfstime4 (u64 secs + u32 nsecs), in ascending id order.
        let mut setattr = Vec::new();
        xdr::encode_stateid(&mut setattr, &super::super::xdr::StateId::zero());
        xdr::encode_attr_bitmap(
            &mut setattr,
            &[
                super::super::xdr::FATTR4_SIZE,
                super::super::xdr::FATTR4_TIME_MODIFY,
            ],
        );
        xdr::encode_u64(&mut setattr, 4); // size
        xdr::encode_u64(&mut setattr, 1_700_000_000); // mtime seconds
        xdr::encode_u32(&mut setattr, 0); // mtime nseconds

        // A GETATTR follows in the same compound; if SETATTR mis-frames the
        // trailing time bytes the GETATTR would parse garbage and the op (or the
        // whole compound) would fail.
        let lookup = lookup_op("multi.txt");
        let mut bitmap = Vec::new();
        xdr::encode_attr_bitmap(&mut bitmap, &[super::super::xdr::FATTR4_SIZE]);
        let reply = run(
            &ctx,
            &state_mgr,
            &[
                (OP_PUTROOTFH, &[]),
                (OP_LOOKUP, &lookup),
                (OP_SETATTR, &setattr),
                (OP_GETATTR, &bitmap),
            ],
            NfsCacheMode::Minimal,
        );

        // Whole compound succeeded and the truncate took effect.
        let (overall, rest) = xdr::decode_u32(&reply).unwrap();
        assert_eq!(overall, NFS4_OK);
        let (_tag, rest) = xdr::decode_string(rest).unwrap();
        let (_numops, rest) = xdr::decode_u32(rest).unwrap();
        let (s0, rest) = xdr::decode_u32(rest).unwrap(); // PUTROOTFH
        assert_eq!(s0, NFS4_OK);
        let (s1, rest) = xdr::decode_u32(rest).unwrap(); // LOOKUP
        assert_eq!(s1, NFS4_OK);
        let (s2, rest) = xdr::decode_u32(rest).unwrap(); // SETATTR
        assert_eq!(s2, NFS4_OK);
        let (_attrset, rest) = xdr::decode_attr_bitmap(rest).unwrap();
        // GETATTR op: status + bitmap + size value.
        let (s3, rest) = xdr::decode_u32(rest).unwrap();
        assert_eq!(s3, NFS4_OK);
        let (_returned_bitmap, rest) = xdr::decode_attr_bitmap(rest).unwrap();
        let (size, _) = xdr::decode_u64(rest).unwrap();
        assert_eq!(size, 4);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remove_on_non_empty_directory_returns_notempty() {
        let (ctx, state_mgr, dir) = writable_ctx();

        // CREATE a directory, then write a file inside it.
        let mut create = Vec::new();
        xdr::encode_u32(&mut create, NF4DIR);
        xdr::encode_string(&mut create, "box");
        xdr::encode_attr_bitmap(&mut create, &[]);
        run(
            &ctx,
            &state_mgr,
            &[(OP_PUTROOTFH, &[]), (OP_CREATE, &create)],
            NfsCacheMode::Minimal,
        );

        // PUTROOTFH, LOOKUP box, LOOKUP child.txt, WRITE — writing a file inside
        // the directory so it is genuinely non-empty.
        let lookup_box = lookup_op("box");
        let lookup_child = lookup_op("child.txt");
        let write = write_op(0, b"keep");
        run(
            &ctx,
            &state_mgr,
            &[
                (OP_PUTROOTFH, &[]),
                (OP_LOOKUP, &lookup_box),
                (OP_LOOKUP, &lookup_child),
                (OP_WRITE, &write),
            ],
            NfsCacheMode::Minimal,
        );

        // REMOVE box (current fh = root) must fail NOTEMPTY, not wipe it.
        let remove = lookup_op("box");
        let reply = run(
            &ctx,
            &state_mgr,
            &[(OP_PUTROOTFH, &[]), (OP_REMOVE, &remove)],
            NfsCacheMode::Minimal,
        );
        assert_eq!(xdr::decode_u32(&reply).unwrap().0, NFS4ERR_NOTEMPTY);

        // The directory and its child survive.
        assert!(dir.path().join("box").is_dir());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remove_deletes_file() {
        let (ctx, state_mgr, _dir) = writable_ctx();
        let lookup = lookup_op("gone.txt");
        let write = write_op(0, b"x");
        run(
            &ctx,
            &state_mgr,
            &[
                (OP_PUTROOTFH, &[]),
                (OP_LOOKUP, &lookup),
                (OP_WRITE, &write),
            ],
            NfsCacheMode::Minimal,
        );

        // REMOVE operates on the directory in the current fh (root).
        let remove = lookup_op("gone.txt");
        let reply = run(
            &ctx,
            &state_mgr,
            &[(OP_PUTROOTFH, &[]), (OP_REMOVE, &remove)],
            NfsCacheMode::Minimal,
        );
        assert_eq!(xdr::decode_u32(&reply).unwrap().0, NFS4_OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rename_moves_via_savefh() {
        let (ctx, state_mgr, _dir) = writable_ctx();
        let lookup = lookup_op("src.txt");
        let write = write_op(0, b"data");
        run(
            &ctx,
            &state_mgr,
            &[
                (OP_PUTROOTFH, &[]),
                (OP_LOOKUP, &lookup),
                (OP_WRITE, &write),
            ],
            NfsCacheMode::Minimal,
        );

        // RENAME: SAVEFH (root as source dir), PUTROOTFH (root as dest dir),
        // RENAME oldname/newname.
        let mut rename = Vec::new();
        xdr::encode_string(&mut rename, "src.txt");
        xdr::encode_string(&mut rename, "dst.txt");
        let reply = run(
            &ctx,
            &state_mgr,
            &[
                (OP_PUTROOTFH, &[]),
                (OP_SAVEFH, &[]),
                (OP_PUTROOTFH, &[]),
                (OP_RENAME, &rename),
            ],
            NfsCacheMode::Minimal,
        );
        assert_eq!(xdr::decode_u32(&reply).unwrap().0, NFS4_OK);

        assert_eq!(getattr_size(&ctx, &state_mgr, "dst.txt"), 4);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn write_on_read_only_export_returns_rofs() {
        let (ctx, state_mgr, _dir) = writable_ctx();
        let lookup = lookup_op("ro.txt");
        let write = write_op(0, b"nope");
        let reply = run(
            &ctx,
            &state_mgr,
            &[
                (OP_PUTROOTFH, &[]),
                (OP_LOOKUP, &lookup),
                (OP_WRITE, &write),
            ],
            NfsCacheMode::Off,
        );
        assert_eq!(xdr::decode_u32(&reply).unwrap().0, NFS4ERR_ROFS);
    }
}
