//! NFSv3 server — TCP listener on loopback with RPC dispatch.
//!
//! Listens for NFS and Mount protocol RPC messages, dispatches to the
//! appropriate handler, and returns XDR-encoded replies.

use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::mount;
use super::procedures;
use super::xdr::*;

/// NFS server configuration.
#[derive(Debug)]
pub struct NfsServerConfig {
    /// Address to bind (typically 127.0.0.1:0 for OS-assigned port).
    pub bind_addr: SocketAddr,
    /// Export path for the mount protocol.
    pub export_path: String,
}

impl Default for NfsServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            export_path: "/".to_string(),
        }
    }
}

/// Running NFS server handle.
pub struct NfsServer {
    /// The actual address the server bound to (useful when port is 0).
    pub local_addr: SocketAddr,
    shutdown: tokio::sync::oneshot::Sender<()>,
}

impl NfsServer {
    /// Start the NFS server. Returns a handle with the bound address.
    pub async fn start(config: NfsServerConfig) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(config.bind_addr).await?;
        let local_addr = listener.local_addr()?;

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            if let Err(e) = run_server(listener, shutdown_rx).await {
                tracing::error!(error = %e, "NFS server error");
            }
        });

        tracing::info!(addr = %local_addr, "NFS server started");

        Ok(Self {
            local_addr,
            shutdown: shutdown_tx,
        })
    }

    /// Stop the NFS server.
    pub async fn stop(self) -> anyhow::Result<()> {
        let _ = self.shutdown.send(());
        tracing::info!("NFS server stopped");
        Ok(())
    }
}

/// Main server loop — accept connections and handle RPC calls.
async fn run_server(
    listener: TcpListener,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, addr) = result?;
                tracing::debug!(peer = %addr, "NFS connection accepted");
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream).await {
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
async fn handle_connection(mut stream: tokio::net::TcpStream) -> anyhow::Result<()> {
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

        let len = u32::from_be_bytes(len_buf) as usize;
        if len > 1_048_576 {
            anyhow::bail!("RPC message too large: {len} bytes");
        }

        let mut msg_buf = vec![0u8; len];
        stream.read_exact(&mut msg_buf).await?;

        // Parse and dispatch.
        let reply = dispatch_rpc(&msg_buf);

        // Send length-prefixed reply.
        stream.write_all(&(reply.len() as u32).to_be_bytes()).await?;
        stream.write_all(&reply).await?;
        stream.flush().await?;
    }
}

/// Dispatch an RPC call to the correct handler (NFS or Mount).
/// The input is the RPC call body (after the length prefix).
/// Returns the complete RPC reply body (without length prefix).
fn dispatch_rpc(msg: &[u8]) -> Vec<u8> {
    // Parse RPC call header:
    //   xid (u32) + call body (msg_type=0 + rpc_version + program + version + procedure + auth)
    let (xid, rest) = match decode_u32(msg) {
        Ok(r) => r,
        Err(_) => return make_rpc_error(0, RPC_REPLY_DENIED),
    };

    // msg_type must be CALL (0).
    let (msg_type, rest) = match decode_u32(rest) {
        Ok(r) => r,
        Err(_) => return make_rpc_error(xid, RPC_REPLY_DENIED),
    };
    if msg_type != RPC_MSG_CALL {
        return make_rpc_error(xid, RPC_REPLY_DENIED);
    }

    // RPC version must be 2.
    let (rpc_version, rest) = match decode_u32(rest) {
        Ok(r) => r,
        Err(_) => return make_rpc_error(xid, RPC_REPLY_DENIED),
    };
    if rpc_version != 2 {
        return make_rpc_error(xid, RPC_REPLY_DENIED);
    }

    // Program and version.
    let (program, rest) = match decode_u32(rest) {
        Ok(r) => r,
        Err(_) => return make_rpc_error(xid, RPC_REPLY_DENIED),
    };
    let (version, rest) = match decode_u32(rest) {
        Ok(r) => r,
        Err(_) => return make_rpc_error(xid, RPC_REPLY_DENIED),
    };

    // Procedure.
    let (procedure, rest) = match decode_u32(rest) {
        Ok(r) => r,
        Err(_) => return make_rpc_error(xid, RPC_REPLY_DENIED),
    };

    // Skip auth (opaque_auth: flavor + body).
    let (_auth_rest, args_offset) = match skip_auth(rest) {
        Some(r) => r,
        None => return make_rpc_error(xid, RPC_REPLY_DENIED),
    };

    let args = &msg[args_offset..];

    // Dispatch based on program.
    let result = match program {
        NFS_PROGRAM if version == NFS_V3 => procedures::handle_nfs_call(procedure, args),
        MOUNT_PROGRAM if version == MOUNT_V3 => mount::handle_mount_call(procedure, args),
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

/// Skip the RPC opaque_auth (flavor + length + body).
/// Returns the rest of the message after auth and the absolute offset where args begin.
fn skip_auth(data: &[u8]) -> Option<(&[u8], usize)> {
    let (_flavor, rest) = decode_u32(data).ok()?;
    let (body_len, rest) = decode_u32(rest).ok()?;
    let body_len = body_len as usize;
    if rest.len() < body_len {
        return None;
    }
    let _after_auth = &rest[body_len..];
    // Pad to 4-byte boundary.
    let pad = (4 - (body_len % 4)) % 4;
    let padded = body_len + pad;
    let after_auth_rest = &rest[padded.min(rest.len())..];
    let offset = data.len() - after_auth_rest.len();
    Some((after_auth_rest, offset))
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let reply = dispatch_rpc(&call);
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

    #[tokio::test]
    async fn server_starts_and_stops() {
        let config = NfsServerConfig::default();
        let server = NfsServer::start(config).await.unwrap();
        assert!(server.local_addr.port() > 0);
        server.stop().await.unwrap();
    }
}
