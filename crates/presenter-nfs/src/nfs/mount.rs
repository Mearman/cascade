//! Mount protocol (RFC 1814).
//!
//! Three procedures: MOUNT, DUMP, UNMOUNT.
//! The mount protocol runs on a separate TCP port from NFS.

use super::context::NfsContext;
use super::xdr::*;
use std::sync::Arc;

/// Handle a mount protocol request.
/// Returns XDR-encoded reply.
pub fn handle_mount_call(proc: u32, args: &[u8], ctx: &Arc<NfsContext>) -> Vec<u8> {
    match proc {
        MOUNTPROC_NULL => vec![],
        MOUNTPROC_MNT => handle_mount(args, ctx),
        MOUNTPROC_DUMP => handle_dump(),
        MOUNTPROC_UMNT => handle_umnt(args),
        MOUNTPROC_UMNTALL => vec![],
        MOUNTPROC_EXPORT => handle_export(),
        _ => vec![],
    }
}

/// MOUNT procedure — validate path, return root file handle.
fn handle_mount(args: &[u8], ctx: &Arc<NfsContext>) -> Vec<u8> {
    let path_result = decode_string(args);
    let mut reply = Vec::new();

    match path_result {
        Ok((path, _)) => {
            tracing::info!(path = %path, "mount request");
            // Accept any path — in Phase 1 there's one root.
            let root_key = ctx.root_key();
            let fh = NfsFh3::from_item_id(&root_key.to_string());
            encode_u32(&mut reply, MNTPROC_OK);
            encode_fh(&mut reply, &fh);
            // No auth flavor list for now (empty).
            encode_u32(&mut reply, 0);
        }
        Err(_) => {
            encode_u32(&mut reply, MNT3ERR_ACCES);
            // No file handle on error.
        }
    }

    reply
}

/// DUMP procedure — list active mounts.
fn handle_dump() -> Vec<u8> {
    // Return empty list (no active mounts tracked in Phase 1).
    let mut reply = Vec::new();
    encode_bool(&mut reply, false); // no more entries
    reply
}

/// UMNT procedure — release mount.
fn handle_umnt(args: &[u8]) -> Vec<u8> {
    if let Ok((path, _)) = decode_string(args) {
        tracing::info!(path = %path, "unmount request");
    }
    vec![]
}

/// EXPORT procedure — list exported paths.
fn handle_export() -> Vec<u8> {
    let mut reply = Vec::new();
    // One export: "/" (the root).
    encode_bool(&mut reply, true); // more entries
    encode_string(&mut reply, "/");
    encode_u32(&mut reply, 0); // no groups
    encode_bool(&mut reply, false); // no more exports
    reply
}
