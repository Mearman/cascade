//! Peer route — surfaces the F1 data-verb grants and the F2 explicit-control
//! table per peer.

use std::collections::BTreeMap;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::get;
use cascade_engine::manage::{Capability, Scope};
use chrono::Utc;

use crate::auth::Session;
use crate::error::ApiError;
use crate::schemas::peers::{FolderDirection, PeerView, PeersResponse};
use crate::state::AppState;

/// Register the peers route.
pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/peers", get(list))
}

/// `GET /v1/peers` — capability: `status:read`.
async fn list(
    State(state): State<AppState>,
    session: Session,
) -> Result<Json<PeersResponse>, ApiError> {
    session.require(&state, Capability::StatusRead, &Scope::Node)?;
    let db = state.engine.db();
    let now = Utc::now();

    let peers = db
        .list_peers()
        .map_err(|e| ApiError::internal(format!("could not list peers: {e}")))?;
    let data_grants = db
        .list_data_grants()
        .map_err(|e| ApiError::internal(format!("could not list data grants: {e}")))?;
    let explicit = db
        .list_data_explicit_control()
        .map_err(|e| ApiError::internal(format!("could not list explicit-control rows: {e}")))?;

    let views = peers
        .into_iter()
        .map(|peer| {
            // Per-folder (read, write) flags from this peer's unexpired data
            // grants. The grant scope path is already the canonical folder id.
            let mut by_folder: BTreeMap<String, (bool, bool)> = BTreeMap::new();
            for record in &data_grants {
                let grant = &record.grant;
                if grant.grantee.as_str() != peer.device_id || grant.is_expired(now) {
                    continue;
                }
                if let Scope::Folder { path } = &grant.scope {
                    let entry = by_folder.entry(path.clone()).or_insert((false, false));
                    match grant.capability {
                        Capability::DataRead => entry.0 = true,
                        Capability::DataWrite => entry.1 = true,
                        _ => {}
                    }
                }
            }
            let data_verb_grants = by_folder
                .into_iter()
                .map(|(folder, (data_read, data_write))| FolderDirection {
                    folder,
                    data_read,
                    data_write,
                })
                .collect();

            let explicit_control = explicit
                .iter()
                .filter(|row| row.peer_device == peer.device_id)
                .map(|row| FolderDirection {
                    folder: row.folder_id.clone(),
                    data_read: row.data_read,
                    data_write: row.data_write,
                })
                .collect();

            PeerView {
                device_id: peer.device_id,
                name: peer.name,
                online: peer.online,
                last_seen: peer.last_seen,
                data_verb_grants,
                explicit_control,
            }
        })
        .collect();

    Ok(Json(PeersResponse { peers: views }))
}
