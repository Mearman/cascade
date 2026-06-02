//! ``NFSv3`` server — TCP listener on loopback with RPC dispatch.
//!
//! Listens for NFS and Mount protocol RPC messages, dispatches to the
//! appropriate handler, and returns XDR-encoded replies.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::context::NfsContext;
use super::mount;
use super::procedures;
use super::v4::compound;
use super::v4::state::StateManager;
use super::v4::xdr as v4_xdr;
use super::xdr::{
    MOUNT_PROGRAM, MOUNT_V3, NFS_PROGRAM, NFS_V3, RPC_ACCEPT_SUCCESS, RPC_AUTH_NONE, RPC_MSG_CALL,
    RPC_MSG_REPLY, RPC_REPLY_ACCEPTED, RPC_REPLY_DENIED, decode_u32, encode_u32,
};
use std::sync::Arc;

/// The VFS cache mode for an NFS export.
///
/// Controls write support and how eagerly content is cached on disk. `Off`
/// exports the tree read-only; `Minimal` and `Full` are write-capable, trading
/// disk usage for caching eagerness. `Minimal` is the default — on-demand
/// reads, reliable writes, minimal disk usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NfsCacheMode {
    /// Read-only export. Writes are refused.
    Off,
    /// Write-capable with minimal on-disk caching.
    #[default]
    Minimal,
    /// Write-capable, caching everything eagerly.
    Full,
}

impl NfsCacheMode {
    /// Whether this mode permits write operations on the export.
    #[must_use]
    pub const fn writes_permitted(self) -> bool {
        match self {
            Self::Off => false,
            Self::Minimal | Self::Full => true,
        }
    }
}

/// NFS server configuration.
#[derive(Debug)]
pub struct NfsServerConfig {
    /// Address to bind (typically 127.0.0.1:0 for OS-assigned port).
    pub bind_addr: SocketAddr,
    /// Export path for the mount protocol.
    pub export_path: String,
    /// The VFS cache mode governing write support for this export.
    pub cache_mode: NfsCacheMode,
}

impl Default for NfsServerConfig {
    fn default() -> Self {
        use std::net::{IpAddr, Ipv4Addr};
        Self {
            bind_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            export_path: "/".to_string(),
            cache_mode: NfsCacheMode::default(),
        }
    }
}

/// Running NFS server handle.
pub struct NfsServer {
    /// The actual address the server bound to (useful when port is 0).
    pub local_addr: SocketAddr,
    shutdown: tokio::sync::oneshot::Sender<()>,
    /// `NFSv4` state manager (kept alive for the server's lifetime).
    #[allow(dead_code)] // Held for lifetime, not directly accessed
    state_mgr: Arc<StateManager>,
}

impl std::fmt::Debug for NfsServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NfsServer")
            .field("local_addr", &self.local_addr)
            .finish_non_exhaustive()
    }
}

impl NfsServer {
    /// Start the NFS server. Returns a handle with the bound address.
    ///
    /// # Errors
    ///
    /// Returns an error if the TCP listener cannot bind.
    pub async fn start(config: NfsServerConfig, ctx: Arc<NfsContext>) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(config.bind_addr).await?;
        let local_addr = listener.local_addr()?;
        let cache_mode = config.cache_mode;

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

        let state_mgr = Arc::new(StateManager::new());
        let state_mgr_clone = Arc::clone(&state_mgr);

        tokio::spawn(async move {
            if let Err(e) =
                run_server(listener, shutdown_rx, ctx, state_mgr_clone, cache_mode).await
            {
                tracing::error!(error = %e, "NFS server error");
            }
        });

        tracing::info!(addr = %local_addr, ?cache_mode, "NFS server started (v3 + v4)");

        Ok(Self {
            local_addr,
            shutdown: shutdown_tx,
            state_mgr,
        })
    }

    /// Stop the NFS server.
    ///
    /// # Errors
    ///
    /// This function currently always succeeds but returns `Result` for API
    /// consistency with other presenter stop methods.
    pub fn stop(self) -> anyhow::Result<()> {
        let _ = self.shutdown.send(());
        tracing::info!("NFS server stopped");
        Ok(())
    }
}

/// Main server loop — accept connections and handle RPC calls.
async fn run_server(
    listener: TcpListener,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
    ctx: Arc<NfsContext>,
    state_mgr: Arc<StateManager>,
    cache_mode: NfsCacheMode,
) -> anyhow::Result<()> {
    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, addr) = result?;
                let ctx = ctx.clone();
                let state_mgr = state_mgr.clone();
                tracing::debug!(peer = %addr, "NFS connection accepted");
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, &ctx, &state_mgr, cache_mode).await {
                        tracing::warn!(peer = %addr, error = %e, "connection error");
                    }
                });
            }
            _ = &mut shutdown => {
                tracing::info!("NFS server shutting down");
                return Ok(());
            }
        }
    }
}

/// Handle a single NFS TCP connection.
/// NFS over TCP uses a simple framing: 4-byte big-endian length prefix
/// followed by the RPC message (which includes the RPC call header and
/// then the NFS/Mount procedure arguments).
async fn handle_connection(
    mut stream: tokio::net::TcpStream,
    ctx: &Arc<NfsContext>,
    state_mgr: &Arc<StateManager>,
    cache_mode: NfsCacheMode,
) -> anyhow::Result<()> {
    loop {
        // Read length-prefixed RPC message.
        let mut len_buf = [0u8; 4];
        match stream.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // Client closed connection — normal.
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }

        let len = usize::try_from(u32::from_be_bytes(len_buf)).unwrap_or(usize::MAX);
        if len > 1_048_576 {
            anyhow::bail!("RPC message too large: {len} bytes");
        }

        let mut msg_buf = vec![0u8; len];
        stream.read_exact(&mut msg_buf).await?;

        // Parse and dispatch.
        let reply = dispatch_rpc(&msg_buf, ctx, state_mgr, cache_mode);

        // Send length-prefixed reply.
        let reply_len = u32::try_from(reply.len()).unwrap_or(u32::MAX);
        stream.write_all(&reply_len.to_be_bytes()).await?;
        stream.write_all(&reply).await?;
        stream.flush().await?;
    }
}

/// Dispatch an RPC call to the correct handler (`NFSv3`, `NFSv4`, or Mount).
/// The input is the RPC call body (after the length prefix).
/// Returns the complete RPC reply body (without length prefix).
fn dispatch_rpc(
    msg: &[u8],
    ctx: &Arc<NfsContext>,
    state_mgr: &Arc<StateManager>,
    cache_mode: NfsCacheMode,
) -> Vec<u8> {
    // Parse RPC call header:
    //   xid (u32) + call body (msg_type=0 + rpc_version + program + version + procedure + auth)
    let Ok((xid, rest)) = decode_u32(msg) else {
        return make_rpc_error(0, RPC_REPLY_DENIED);
    };

    // msg_type must be CALL (0).
    let Ok((msg_type, rest)) = decode_u32(rest) else {
        return make_rpc_error(xid, RPC_REPLY_DENIED);
    };
    if msg_type != RPC_MSG_CALL {
        return make_rpc_error(xid, RPC_REPLY_DENIED);
    }

    // RPC version must be 2.
    let Ok((rpc_version, rest)) = decode_u32(rest) else {
        return make_rpc_error(xid, RPC_REPLY_DENIED);
    };
    if rpc_version != 2 {
        return make_rpc_error(xid, RPC_REPLY_DENIED);
    }

    // Program and version.
    let Ok((program, rest)) = decode_u32(rest) else {
        return make_rpc_error(xid, RPC_REPLY_DENIED);
    };
    let Ok((version, rest)) = decode_u32(rest) else {
        return make_rpc_error(xid, RPC_REPLY_DENIED);
    };

    // Procedure.
    let Ok((procedure, rest)) = decode_u32(rest) else {
        return make_rpc_error(xid, RPC_REPLY_DENIED);
    };

    // Skip auth (opaque_auth: flavor + body).
    let Some((_, args_offset)) = skip_auth(rest) else {
        return make_rpc_error(xid, RPC_REPLY_DENIED);
    };

    // args_offset is relative to `rest` (which starts after the procedure field).
    // Convert to absolute offset within `msg`.
    let abs_offset = msg.len() - rest.len() + args_offset;
    let args = msg.get(abs_offset..).unwrap_or(&[]);

    // Dispatch based on program and version.
    let result = match (program, version) {
        (NFS_PROGRAM, ver) if ver == NFS_V3 => {
            procedures::handle_nfs_call(procedure, args, ctx, cache_mode)
        }
        (prog, ver) if prog == v4_xdr::NFS4_PROGRAM && ver == v4_xdr::NFS_V4 => {
            handle_nfs4_call(procedure, args, ctx, state_mgr)
        }
        (MOUNT_PROGRAM, ver) if ver == MOUNT_V3 => mount::handle_mount_call(procedure, args, ctx),
        _ => return make_rpc_error(xid, RPC_REPLY_DENIED),
    };

    // Build successful RPC reply.
    let mut reply = Vec::new();
    encode_u32(&mut reply, xid);
    encode_u32(&mut reply, RPC_MSG_REPLY);
    encode_u32(&mut reply, RPC_REPLY_ACCEPTED);
    // verifier auth (AUTH_NONE).
    encode_u32(&mut reply, RPC_AUTH_NONE);
    encode_u32(&mut reply, 0); // empty auth body
    encode_u32(&mut reply, RPC_ACCEPT_SUCCESS);
    reply.extend_from_slice(&result);
    reply
}

/// Handle an `NFSv4` procedure call.
/// `NFSv4` has only two procedures: NULL (0) and COMPOUND (1).
fn handle_nfs4_call(
    proc: u32,
    args: &[u8],
    ctx: &Arc<NfsContext>,
    state_mgr: &Arc<StateManager>,
) -> Vec<u8> {
    match proc {
        v4_xdr::NFSPROC4_NULL => vec![],
        v4_xdr::NFSPROC4_COMPOUND => compound::handle_compound(args, ctx, state_mgr),
        _ => {
            let mut r = Vec::new();
            v4_xdr::encode_u32(&mut r, v4_xdr::NFS4ERR_INVAL);
            r
        }
    }
}

/// Build an RPC error reply.
fn make_rpc_error(xid: u32, reject_stat: u32) -> Vec<u8> {
    let mut reply = Vec::new();
    encode_u32(&mut reply, xid);
    encode_u32(&mut reply, RPC_MSG_REPLY);
    encode_u32(&mut reply, reject_stat);
    // For rejected replies: RPC_MISMATCH or AUTH_ERROR.
    // We use the minimal rejection.
    encode_u32(&mut reply, 0); // mismatch low
    encode_u32(&mut reply, 2); // mismatch high (supported version)
    reply
}

/// Skip the RPC `opaque_auth` (flavor + length + body).
/// Returns the rest of the message after auth and the absolute offset where args begin.
fn skip_auth(data: &[u8]) -> Option<(&[u8], usize)> {
    let (_flavor, rest) = decode_u32(data).ok()?;
    let (body_len, rest) = decode_u32(rest).ok()?;
    let body_len = usize::try_from(body_len).ok()?;
    if rest.len() < body_len {
        return None;
    }
    // Pad to 4-byte boundary.
    let pad = (4 - (body_len % 4)) % 4;
    let padded = body_len + pad;
    let after_auth_rest = rest.get(padded.min(rest.len())..).unwrap_or(&[]);
    let offset = data.len() - after_auth_rest.len();
    Some((after_auth_rest, offset))
}

#[cfg(test)]
mod tests {
    use super::super::context::NfsContext;
    use super::super::xdr::{NFS3PROC_NULL, RPC_REPLY_ACCEPTED};
    use super::{
        NFS_PROGRAM, NFS_V3, NfsCacheMode, NfsServer, NfsServerConfig, RPC_ACCEPT_SUCCESS,
        RPC_AUTH_NONE, RPC_MSG_CALL, RPC_MSG_REPLY, decode_u32, dispatch_rpc, encode_u32,
    };
    use cascade_engine::backend::NullBackend;
    use cascade_engine::vfs::VfsTree;
    use std::sync::{Arc, RwLock};

    #[test]
    fn dispatch_null_procedure() {
        let mut call = Vec::new();
        encode_u32(&mut call, 1); // xid
        encode_u32(&mut call, RPC_MSG_CALL);
        encode_u32(&mut call, 2); // rpc version
        encode_u32(&mut call, NFS_PROGRAM);
        encode_u32(&mut call, NFS_V3);
        encode_u32(&mut call, NFS3PROC_NULL);
        // AUTH_NONE
        encode_u32(&mut call, RPC_AUTH_NONE);
        encode_u32(&mut call, 0);

        let state_mgr = Arc::new(super::super::v4::state::StateManager::new());
        let reply = dispatch_rpc(&call, &test_ctx(), &state_mgr, NfsCacheMode::Minimal);
        let (xid, rest) = decode_u32(&reply).unwrap();
        assert_eq!(xid, 1);
        let (msg_type, rest) = decode_u32(rest).unwrap();
        assert_eq!(msg_type, RPC_MSG_REPLY);
        let (_accept_stat, _) = decode_u32(rest).unwrap();
        // Skip verifier auth, then accept_stat.
        // After msg_type we have: reply_stat + verifier_auth(flavor+len) + accept_stat
        let (_, rest) = decode_u32(rest).unwrap(); // reply_stat
        let (_, rest) = decode_u32(rest).unwrap(); // verifier flavor
        let (_, rest) = decode_u32(rest).unwrap(); // verifier len
        let (accept, _) = decode_u32(rest).unwrap();
        assert_eq!(accept, RPC_ACCEPT_SUCCESS);
    }

    #[test]
    fn dispatch_nfs4_null_procedure() {
        use super::super::v4::xdr as v4_xdr;

        let mut call = Vec::new();
        encode_u32(&mut call, 42); // xid
        encode_u32(&mut call, RPC_MSG_CALL);
        encode_u32(&mut call, 2); // rpc version
        encode_u32(&mut call, v4_xdr::NFS4_PROGRAM);
        encode_u32(&mut call, v4_xdr::NFS_V4);
        encode_u32(&mut call, v4_xdr::NFSPROC4_NULL);
        // AUTH_NONE
        encode_u32(&mut call, RPC_AUTH_NONE);
        encode_u32(&mut call, 0);

        let state_mgr = Arc::new(super::super::v4::state::StateManager::new());
        let reply = dispatch_rpc(&call, &test_ctx(), &state_mgr, NfsCacheMode::Minimal);
        let (xid, rest) = decode_u32(&reply).unwrap();
        assert_eq!(xid, 42);
        let (msg_type, _) = decode_u32(rest).unwrap();
        assert_eq!(msg_type, RPC_MSG_REPLY);
    }

    #[test]
    fn dispatch_nfs4_compound() {
        use super::super::v4::xdr as v4_xdr;

        // Build a COMPOUND with PUTROOTFH + GETFH.
        let mut compound_args = Vec::new();
        v4_xdr::encode_string(&mut compound_args, ""); // tag
        v4_xdr::encode_u32(&mut compound_args, 0); // minorversion
        v4_xdr::encode_u32(&mut compound_args, 2); // num ops
        v4_xdr::encode_u32(&mut compound_args, v4_xdr::OP_PUTROOTFH);
        v4_xdr::encode_u32(&mut compound_args, v4_xdr::OP_GETFH);

        let mut call = Vec::new();
        encode_u32(&mut call, 100); // xid
        encode_u32(&mut call, RPC_MSG_CALL);
        encode_u32(&mut call, 2); // rpc version
        encode_u32(&mut call, v4_xdr::NFS4_PROGRAM);
        encode_u32(&mut call, v4_xdr::NFS_V4);
        encode_u32(&mut call, v4_xdr::NFSPROC4_COMPOUND);
        // AUTH_NONE
        encode_u32(&mut call, RPC_AUTH_NONE);
        encode_u32(&mut call, 0);
        call.extend_from_slice(&compound_args);

        let state_mgr = Arc::new(super::super::v4::state::StateManager::new());
        let reply = dispatch_rpc(&call, &test_ctx(), &state_mgr, NfsCacheMode::Minimal);

        // Verify RPC reply header.
        let (xid, rest) = decode_u32(&reply).unwrap();
        assert_eq!(xid, 100);
        let (msg_type, rest) = decode_u32(rest).unwrap();
        assert_eq!(msg_type, RPC_MSG_REPLY);
        // reply_stat
        let (reply_stat, rest) = decode_u32(rest).unwrap();
        assert_eq!(reply_stat, RPC_REPLY_ACCEPTED);
        // verifier
        let (_, rest) = decode_u32(rest).unwrap(); // flavor
        let (_, rest) = decode_u32(rest).unwrap(); // len
        let (accept, rest) = decode_u32(rest).unwrap();
        assert_eq!(accept, RPC_ACCEPT_SUCCESS);

        // Parse COMPOUND reply: status.
        let (compound_status, _) = v4_xdr::decode_u32(rest).unwrap();
        assert_eq!(compound_status, v4_xdr::NFS4_OK);
    }

    #[tokio::test]
    async fn server_starts_and_stops() {
        let config = NfsServerConfig::default();
        let ctx = test_ctx();
        let server = NfsServer::start(config, ctx).await.unwrap();
        assert!(server.local_addr.port() > 0);
        server.stop().unwrap();
    }

    fn test_ctx() -> Arc<NfsContext> {
        let vfs = Arc::new(RwLock::new(VfsTree::new(Arc::new(NullBackend::new(
            "test",
        )))));
        Arc::new(NfsContext::new(vfs))
    }
}
