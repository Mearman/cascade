//! Route handlers, one module per resource, plus the folder-resolution helpers
//! the F1/F3/F4 rules share.

pub mod audit;
pub mod auth;
pub mod backends;
pub mod cache;
pub mod config;
pub mod files;
pub mod grants;
pub mod health;
pub mod peers;
pub mod pins;
pub mod policies;
pub mod session;
pub mod shares;
pub mod tokens;

use cascade_engine::db::BackendRecord;
use data_encoding::BASE64URL_NOPAD;

use crate::error::ApiError;
use crate::state::AppState;

/// Encode an opaque pagination cursor from a row id.
#[must_use]
pub fn encode_cursor(after_id: i64) -> String {
    BASE64URL_NOPAD.encode(after_id.to_string().as_bytes())
}

/// Decode an opaque pagination cursor into the row id it marks, failing with
/// `422 unprocessable` when it is not a cursor this server minted.
pub fn decode_cursor(cursor: &str) -> Result<i64, ApiError> {
    let bytes = BASE64URL_NOPAD
        .decode(cursor.as_bytes())
        .map_err(|_| ApiError::unprocessable("malformed pagination cursor"))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| ApiError::unprocessable("malformed pagination cursor"))?;
    text.parse::<i64>()
        .map_err(|_| ApiError::unprocessable("malformed pagination cursor"))
}

/// The backend type that owns a canonical BEP folder id.
const P2P_BACKEND_TYPE: &str = "p2p";

/// The canonical BEP folder id for a P2P backend named `name`.
#[must_use]
fn folder_id_for(name: &str) -> String {
    format!("{P2P_BACKEND_TYPE}-{name}")
}

/// The canonical BEP folder id of a backend record, or `None` for a non-P2P
/// backend.
#[must_use]
pub fn backend_folder_id(record: &BackendRecord) -> Option<String> {
    (record.backend_type == P2P_BACKEND_TYPE).then(|| folder_id_for(&record.id))
}

/// Every registered P2P backend, as `(operator-name, canonical-folder-id)`.
fn known_p2p_folders(state: &AppState) -> Result<Vec<(String, String)>, ApiError> {
    let backends = state
        .engine
        .db()
        .list_backends()
        .map_err(|e| ApiError::internal(format!("could not list backends: {e}")))?;
    Ok(backends
        .into_iter()
        .filter(|record| record.backend_type == P2P_BACKEND_TYPE)
        .map(|record| {
            let id = folder_id_for(&record.id);
            (record.id, id)
        })
        .collect())
}

/// Confirm `folder_id` is the canonical id of a registered P2P backend.
///
/// Folder-route path parameters are already canonical BEP ids (`p2p-<name>`);
/// an unknown one is `404 not_found`.
pub fn require_known_folder(state: &AppState, folder_id: &str) -> Result<(), ApiError> {
    let known = known_p2p_folders(state)?;
    if known.iter().any(|(_, id)| id == folder_id) {
        Ok(())
    } else {
        Err(ApiError::not_found(format!(
            "no folder with id `{folder_id}` is registered"
        )))
    }
}

/// Resolve an operator-facing P2P backend name to its canonical BEP folder id
/// (the F1 fix), failing with `422 unknown_folder` and `details.folders_known`
/// when the name is not a registered P2P backend.
///
/// This is the single resolution path `POST /v1/shares` and `POST /v1/grants`
/// take before storing a `Scope::folder` value, mirroring `cascade share add`
/// and `cascade grant add` so the stored scope lands in the same namespace the
/// runtime data-plane gate consults.
pub fn resolve_folder_name(state: &AppState, name: &str) -> Result<String, ApiError> {
    let known = known_p2p_folders(state)?;
    if let Some((_, id)) = known
        .iter()
        .find(|(operator_name, _)| operator_name == name)
    {
        return Ok(id.clone());
    }
    let folders_known: Vec<&str> = known
        .iter()
        .map(|(operator_name, _)| operator_name.as_str())
        .collect();
    Err(ApiError::new(
        crate::error::ErrorCode::UnknownFolder,
        format!("no P2P folder named `{name}` is registered"),
    )
    .with_details(serde_json::json!({ "folders_known": folders_known })))
}

/// The F3 gate: data-plane routes return `503 data_plane_not_ready` until the
/// readiness bit flips. Management routes are unaffected.
pub fn require_data_plane_ready(state: &AppState) -> Result<(), ApiError> {
    if state.readiness.data_plane_ready() {
        Ok(())
    } else {
        Err(ApiError::data_plane_not_ready(
            "the data plane is not yet ready to serve file content",
        ))
    }
}
