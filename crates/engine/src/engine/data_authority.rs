//! Data-plane authority: the engine as the BEP sync path's access gate.
//!
//! The engine resolves a peer's directional read/write access to a folder from
//! the on-node data grants, the token revocation list, and any signed
//! data-verb token the peer presented on its sync `ClusterConfig`.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::Engine;
use crate::db::QuarantineRecord;
use crate::manage::{
    DataAccess, DataAuthority, DeviceId, ExplicitControlState, data_access_with_explicit_control,
    verify_data_token,
};

/// The engine is the data-plane authority for the BEP sync path: it resolves a
/// peer's directional read/write access to a folder from the on-node data
/// grants, the token revocation list, and any signed data-verb token the peer
/// presented on its sync `ClusterConfig`.
///
/// The decision is **default-open** (see [`data_access_with_explicit_control`]):
/// a trusted peer with no data grant configured keeps full bidirectional
/// access, and the feature only ever narrows. Both the grant rows and the
/// revocation list are read on every call, so revoking or expiring a grant
/// takes effect at the next frame rather than at restart. The F2
/// explicit-control bit is consulted on every call too, so a verified-token
/// restriction survives the token revocation or expiry that prompted it.
#[async_trait]
impl DataAuthority for Engine {
    async fn data_access(
        &self,
        peer: &DeviceId,
        folder: &str,
        presented_token: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<DataAccess> {
        // On-node data grants. Read every call so a freshly added or revoked
        // grant is honoured promptly. A grant row carries no token, so the
        // revocation list does not touch these — only a presented token below.
        let mut grants: Vec<crate::manage::Grant> = self
            .db
            .list_data_grants()?
            .into_iter()
            .map(|record| record.grant)
            .collect();

        // Fold in the peer's presented data-verb token, if it verifies against
        // this node (signed by us or a chain rooting in us, unexpired, not
        // revoked, bearer == this peer). A token that does not verify, or that
        // carries a non-data verb, confers nothing — it can never widen access.
        if let Some(token_json) = presented_token
            && let Some(token_grant) = verify_data_token(self, peer, token_json, now)
        {
            // The F2 invariant: a successful verify pins the peer into
            // explicit-control mode for the folder. Record the bit so the
            // absent direction stays denied even if the token is later
            // revoked or allowed to expire. The data-plane gate keys on
            // `folder`, the runtime value the BEP session is bound to, not
            // the token's carried scope — the verify path's scope-cover
            // check has already confirmed the two agree.
            self.db.record_data_explicit_control(
                peer.as_str(),
                folder,
                matches!(token_grant.capability, crate::manage::Capability::DataRead),
                matches!(token_grant.capability, crate::manage::Capability::DataWrite),
                now,
            )?;
            grants.push(token_grant);
        }

        let explicit_control: Vec<ExplicitControlState> = self
            .db
            .list_data_explicit_control()?
            .into_iter()
            .map(|record| ExplicitControlState {
                peer: record.peer_device,
                folder: record.folder_id,
                data_read: record.data_read,
                data_write: record.data_write,
            })
            .collect();

        Ok(data_access_with_explicit_control(
            &grants,
            peer,
            folder,
            now,
            &explicit_control,
        ))
    }

    async fn quarantine_received(
        &self,
        peer: &DeviceId,
        folder: &str,
        path: &str,
        file_json: &str,
        observed_at: DateTime<Utc>,
    ) -> Result<()> {
        self.db.upsert_quarantine(&QuarantineRecord {
            folder_id: folder.to_string(),
            peer_device: peer.as_str().to_string(),
            path: path.to_string(),
            file_json: file_json.to_string(),
            observed_at,
        })
    }
}
