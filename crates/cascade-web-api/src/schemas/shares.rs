//! Share (data-verb grant) schemas.

use serde::{Deserialize, Serialize};

/// A peer's effective sharing posture for a folder.
///
/// The variants are `kebab-case` because the contract's posture values are
/// `read-only`, `write-only`, `read-write`. This is a `rename_all` (not
/// per-field renames), so it does not breach the "no per-field rename" rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SharePosture {
    /// The peer may read our data but not write.
    ReadOnly,
    /// The peer may write but not read (a drop sink).
    WriteOnly,
    /// Full bidirectional sharing.
    ReadWrite,
}

impl SharePosture {
    /// The data verbs this posture maps to.
    #[must_use]
    pub const fn grants_read(self) -> bool {
        matches!(self, Self::ReadOnly | Self::ReadWrite)
    }

    /// Whether this posture confers `data:write`.
    #[must_use]
    pub const fn grants_write(self) -> bool {
        matches!(self, Self::WriteOnly | Self::ReadWrite)
    }

    /// The human-readable label (`read-only`, `write-only`, `read-write`), for
    /// audit command text.
    #[must_use]
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WriteOnly => "write-only",
            Self::ReadWrite => "read-write",
        }
    }

    /// Derive the posture from the two directional flags, if any verb is held.
    #[must_use]
    pub const fn from_flags(read: bool, write: bool) -> Option<Self> {
        match (read, write) {
            (true, true) => Some(Self::ReadWrite),
            (true, false) => Some(Self::ReadOnly),
            (false, true) => Some(Self::WriteOnly),
            (false, false) => None,
        }
    }
}

/// The operator-facing view of one peer's data-verb posture for a folder.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ShareView {
    /// The peer device the share applies to.
    pub peer_device_id: String,
    /// The operator-facing folder name.
    pub folder: String,
    /// The canonical BEP folder id (`p2p-<name>`).
    pub folder_id: String,
    /// The effective posture.
    pub posture: SharePosture,
    /// The device that granted the share.
    pub granted_by: String,
    /// When the share expires, if ever.
    pub expires: Option<chrono::DateTime<chrono::Utc>>,
    /// The underlying grant row ids the posture is built from.
    pub grant_ids: Vec<i64>,
}

/// `GET /v1/shares` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SharesResponse {
    /// Every data-verb share, denormalised per peer and folder.
    pub shares: Vec<ShareView>,
}

/// `POST /v1/shares` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SharePost {
    /// The peer device to share with.
    pub peer_device_id: String,
    /// The operator-facing folder name (resolved to `p2p-<name>`).
    pub folder: String,
    /// The posture to confer.
    pub posture: SharePosture,
    /// When the share should expire, if ever.
    #[serde(default)]
    pub expires: Option<chrono::DateTime<chrono::Utc>>,
}
