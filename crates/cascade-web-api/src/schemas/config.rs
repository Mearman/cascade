//! Config-push schemas.

use serde::{Deserialize, Serialize};

/// The `.cascade` fragment format a config push carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigFormat {
    /// gitignore-style ignore rules.
    Gitignore,
    /// A TOML fragment.
    Toml,
    /// A YAML fragment.
    Yaml,
    /// A JSON fragment.
    Json,
}

/// `POST /v1/config/push` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ConfigPushPost {
    /// The target folder the fragment is rooted at.
    pub folder: String,
    /// The fragment format.
    pub format: ConfigFormat,
    /// The fragment body.
    pub body: String,
}

/// `POST /v1/config/push` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ConfigPushResponse {
    /// The engine's summary of the merge.
    pub summary: String,
}
