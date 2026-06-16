//! Terminal websocket route — drives the engine's exec provider (PTY) over a
//! bidirectional websocket.
//!
//! The route is `GET /v1/exec/ws` (the websocket upgrade). It requires the
//! dangerous `exec:pty` capability over the folder the terminal opens in (the
//! `folder` query parameter, which becomes the PTY's working directory). A
//! dangerous capability is never satisfied by a node-wide grant, and a
//! root-folder scope counts as node-wide, so `folder` must name a real
//! subdirectory the caller's grant covers — a terminal cannot be opened
//! node-wide.
//!
//! The wire protocol is JSON text frames: the client sends
//! `{"type":"spawn","shell":null,"cols":80,"rows":24}` to start a session,
//! then `{"type":"input","bytes":[...]}` for keystrokes, `{"type":"resize",
//! "cols":...,"rows":...}` for resize, and `{"type":"signal","signal":9}` to
//! kill. The server sends `{"type":"ready","session":42}`, then
//! `{"type":"output","stream":"stdout","bytes":[...]}` for data and
//! `{"type":"exited","code":0}` on exit.

use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use axum::routing::get;
use cascade_engine::manage::{
    Capability, CapabilityToken, DeviceId, ManageGrantStore, Scope, TokenClaims, TokenVerifyError,
};
use chrono::Utc;
use data_encoding::BASE64;
use serde::{Deserialize, Serialize};

use crate::error::{ApiError, ErrorCode};
use crate::state::AppState;

/// Register the exec routes.
pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/exec/ws", get(exec_ws_handler))
}

/// Query parameters for the websocket upgrade.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ExecWsQuery {
    /// The folder the terminal opens in — the PTY's working directory and the
    /// scope the `exec:pty` capability is authorised over. Must be a real
    /// subdirectory (not the root), because a dangerous capability is never
    /// satisfied node-wide. Required.
    pub folder: Option<String>,
    /// Optional shell override. When omitted the provider picks a default.
    pub shell: Option<String>,
    /// Initial terminal width in columns.
    pub cols: Option<u16>,
    /// Initial terminal height in rows.
    pub rows: Option<u16>,
    /// Base64-encoded capability token JSON (the browser WebSocket API cannot
    /// send custom headers, so the credential rides as a query param). Required.
    pub token: Option<String>,
    /// The bearer device id (same as the `X-Cascade-Bearer-Device` header,
    /// passed as a query param because the browser cannot set headers on a
    /// WebSocket upgrade). Required.
    pub bearer: Option<String>,
}

/// Messages the client sends over the websocket.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    /// Spawn a new PTY session. Sent once at the start.
    Spawn {
        shell: Option<String>,
        cols: u16,
        rows: u16,
    },
    /// Write bytes to the PTY's stdin.
    Input { bytes: Vec<u8> },
    /// Resize the PTY.
    Resize { cols: u16, rows: u16 },
    /// Send a signal to the PTY's child process.
    Signal { signal: i32 },
}

/// Messages the server sends over the websocket.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    /// A session was spawned.
    Ready { session: u64 },
    /// Output arrived from the process.
    Output {
        stream: &'static str,
        bytes: Vec<u8>,
    },
    /// The process exited.
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
    /// An error occurred.
    Error { message: String },
}

fn to_text(msg: &ServerMessage) -> Message {
    Message::Text(
        serde_json::to_string(msg)
            .unwrap_or_else(|_| "{}".to_owned())
            .into(),
    )
}

/// `GET /v1/exec/ws` — websocket upgrade. Capability: `exec:pty` over the
/// `folder` query parameter.
///
/// The exec:pty capability is dangerous — it grants code execution. It is
/// never satisfied by a node-wide (or root-folder) grant, so the caller must
/// hold an explicit grant over the named subdirectory the terminal opens in.
///
/// Because the browser's `WebSocket` API cannot send custom headers, the
/// capability token and bearer-device id are passed as query parameters
/// (`?token=<base64>` and via the `X-Cascade-Bearer-Device` header when
/// possible, or the `bearer` query param as a fallback). The token is verified
/// the same way the HTTP `Session` extractor verifies it.
pub async fn exec_ws_handler(
    State(state): State<AppState>,
    Query(query): Query<ExecWsQuery>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let claims = verify_ws_token(&state, &query)?;

    // The terminal opens in `folder`: the PTY's working directory and the scope
    // the dangerous `exec:pty` capability is authorised over. A root folder
    // counts as node-wide and so can never satisfy a dangerous capability.
    let (folder, scope) = resolve_exec_folder_scope(&query)?;

    // Re-run the management-plane authorisation for exec:pty over the folder
    // scope, the same check `Session::require` makes.
    let now = Utc::now();
    let mut grants = state
        .engine
        .manage_grants()
        .map_err(|e| ApiError::internal(format!("could not read grants: {e}")))?;
    grants.push(claims.to_grant());

    if !cascade_engine::manage::authorises(
        &grants,
        &claims.bearer,
        Capability::ExecPty,
        &scope,
        now,
    ) {
        let holds_capability = grants.iter().any(|g| {
            g.grantee == claims.bearer && g.capability == Capability::ExecPty && !g.is_expired(now)
        });
        if holds_capability {
            return Err(ApiError::forbidden(format!(
                "caller holds {} but not over folder {folder}",
                Capability::ExecPty.as_wire()
            )));
        }
        return Err(ApiError::unauthorised(format!(
            "caller's verified claims do not satisfy {} over folder {folder}",
            Capability::ExecPty.as_wire()
        )));
    }

    let exec = state
        .engine
        .exec()
        .ok_or_else(|| ApiError::unavailable("no exec provider configured on this node"))?
        .clone();

    Ok(ws.on_upgrade(move |socket| run_terminal(socket, exec, folder)))
}

/// Resolve the folder a terminal opens in and the [`Scope`] to authorise
/// `exec:pty` over, extracted from the handler so the gate (folder required,
/// non-root) is testable without a live websocket upgrade.
///
/// Returns the folder string (to seed the PTY's working directory) and the
/// matching folder scope. A missing or blank folder is an authentication
/// failure; a root folder is forbidden because it normalises to node-wide and
/// a dangerous capability is never satisfied node-wide.
fn resolve_exec_folder_scope(query: &ExecWsQuery) -> Result<(String, Scope), ApiError> {
    let folder = query
        .folder
        .as_ref()
        .filter(|f| !f.trim().is_empty())
        .ok_or_else(|| ApiError::unauthorised("folder query parameter is required"))?;
    let scope = Scope::folder(folder.clone());
    if scope.is_node_wide() {
        return Err(ApiError::forbidden(format!(
            "{} requires a specific folder scope; a terminal cannot be opened node-wide",
            Capability::ExecPty.as_wire()
        )));
    }
    Ok((folder.clone(), scope))
}

/// Verify the capability token presented as a query parameter, mirroring the
/// `Session` extractor's verification path.
fn verify_ws_token(state: &AppState, query: &ExecWsQuery) -> Result<TokenClaims, ApiError> {
    let token_str = query
        .token
        .as_ref()
        .ok_or_else(|| ApiError::unauthorised("token query parameter is required"))?;
    let bearer_str = query
        .bearer
        .as_ref()
        .ok_or_else(|| ApiError::unauthorised("bearer query parameter is required"))?;

    let json = BASE64
        .decode(token_str.as_bytes())
        .map_err(|_| ApiError::unauthorised("token query parameter is not valid base64"))?;
    let token: CapabilityToken = serde_json::from_slice(&json)
        .map_err(|e| ApiError::unauthorised(format!("could not parse capability token: {e}")))?;

    let connected_device = DeviceId::new(bearer_str.clone());

    let node_device_id = state.identity.device_id().clone();
    let revoked = state
        .engine
        .manage_revoked_token_ids()
        .map_err(|e| ApiError::internal(format!("could not read token revocation list: {e}")))?;
    let is_revoked = |id: &str| revoked.contains(id);

    token
        .verify(&node_device_id, &connected_device, Utc::now(), &is_revoked)
        .map_err(|e| match &e {
            TokenVerifyError::BearerMismatch { .. } => ApiError::new(
                ErrorCode::BearerMismatch,
                format!("presented capability token rejected: {e}"),
            ),
            other => {
                ApiError::unauthorised(format!("presented capability token rejected: {other}"))
            }
        })
        .cloned()
}

/// Drive the websocket: spawn the PTY in `cwd`, forward client keystrokes as
/// pty writes, and forward PTY output to the client.
async fn run_terminal(
    mut socket: WebSocket,
    exec: Arc<dyn cascade_exec::ExecProvider>,
    cwd: String,
) {
    // Wait for the initial Spawn message before creating the PTY.
    let spawn_msg = match socket.recv().await {
        Some(Ok(Message::Text(text))) => serde_json::from_str::<ClientMessage>(&text).ok(),
        _ => None,
    };

    let Some(ClientMessage::Spawn { shell, cols, rows }) = spawn_msg else {
        let _ = socket
            .send(to_text(&ServerMessage::Error {
                message: "expected a spawn message first".to_owned(),
            }))
            .await;
        return;
    };

    let spec = cascade_exec::PtySpec {
        shell,
        argv: Vec::new(),
        cwd: Some(cwd),
        env: Vec::new(),
        cols,
        rows,
    };

    let session_id = match exec.pty_spawn(spec).await {
        Ok(id) => id,
        Err(e) => {
            let _ = socket
                .send(to_text(&ServerMessage::Error {
                    message: format!("could not spawn PTY: {e}"),
                }))
                .await;
            return;
        }
    };

    // Notify the client the session is ready.
    let _ = socket
        .send(to_text(&ServerMessage::Ready {
            session: session_id.0,
        }))
        .await;

    // Subscribe to output events from the PTY. If subscription fails the session
    // was already spawned, so kill it before bailing — otherwise the orphaned
    // PTY would keep running on the node with no consumer.
    let Some(mut event_rx) = exec.subscribe(session_id) else {
        let _ = exec.pty_kill(session_id, 15).await;
        let _ = socket
            .send(to_text(&ServerMessage::Error {
                message: "could not subscribe to PTY output".to_owned(),
            }))
            .await;
        return;
    };

    loop {
        tokio::select! {
            // Client → PTY: input, resize, signal.
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let Ok(cmd) = serde_json::from_str::<ClientMessage>(&text) else {
                            continue;
                        };
                        match cmd {
                            ClientMessage::Input { bytes } => {
                                if exec.pty_write(session_id, &bytes).await.is_err() {
                                    break;
                                }
                            }
                            ClientMessage::Resize { cols, rows } => {
                                if exec.pty_resize(session_id, cols, rows).await.is_err() {
                                    break;
                                }
                            }
                            ClientMessage::Signal { signal } => {
                                if exec.pty_kill(session_id, signal).await.is_err() {
                                    break;
                                }
                            }
                            ClientMessage::Spawn { .. } => {
                                // Ignore a second spawn.
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
            // PTY → client: output, exit.
            event = event_rx.recv() => {
                let Some(event) = event else { break };
                let msg = match event {
                    cascade_exec::ExecEvent::Output { stream, bytes } => {
                        let stream_name = match stream {
                            cascade_exec::ExecStreamKind::Stdout => "stdout",
                            cascade_exec::ExecStreamKind::Stderr => "stderr",
                            cascade_exec::ExecStreamKind::Stdin => "stdin",
                        };
                        to_text(&ServerMessage::Output {
                            stream: stream_name,
                            bytes,
                        })
                    }
                    cascade_exec::ExecEvent::Exited { code, signal } => {
                        let m = to_text(&ServerMessage::Exited { code, signal });
                        let _ = socket.send(m).await;
                        break;
                    }
                };
                if socket.send(msg).await.is_err() {
                    break;
                }
            }
        }
    }

    // Clean up: ensure the session is killed if the client disconnects.
    let _ = exec.pty_kill(session_id, 15).await;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn query(folder: Option<&str>) -> ExecWsQuery {
        ExecWsQuery {
            folder: folder.map(str::to_owned),
            shell: None,
            cols: None,
            rows: None,
            token: None,
            bearer: None,
        }
    }

    #[test]
    fn missing_folder_is_unauthorised() {
        let err = resolve_exec_folder_scope(&query(None)).expect_err("missing folder must reject");
        assert_eq!(err.code, ErrorCode::Unauthorised);
    }

    #[test]
    fn blank_folder_is_unauthorised() {
        let err =
            resolve_exec_folder_scope(&query(Some("   "))).expect_err("blank folder must reject");
        assert_eq!(err.code, ErrorCode::Unauthorised);
    }

    #[test]
    fn root_folder_is_forbidden_as_node_wide() {
        // A dangerous capability is never satisfied node-wide, and the root
        // folder normalises to node-wide, so the gate must forbid it rather
        // than advertise a scope no grant could ever cover. (A blank/empty
        // folder is caught earlier as a missing-folder auth failure.)
        for root in ["/", "//", "/."] {
            let err = resolve_exec_folder_scope(&query(Some(root)))
                .expect_err("a root folder must be forbidden");
            assert_eq!(
                err.code,
                ErrorCode::Forbidden,
                "root {root:?} should be node-wide-forbidden"
            );
        }
    }

    #[test]
    fn real_folder_resolves_to_a_non_node_wide_scope() {
        let (folder, scope) = resolve_exec_folder_scope(&query(Some("work")))
            .expect("a real subdirectory is a valid terminal folder");
        assert_eq!(folder, "work");
        assert!(!scope.is_node_wide());
    }
}
