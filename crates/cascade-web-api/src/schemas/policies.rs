//! Lifecycle-policy schemas.

use serde::{Deserialize, Serialize};

/// One lifecycle policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PolicyView {
    /// The policy row id.
    pub id: i64,
    /// The path glob the policy applies to.
    pub path_glob: String,
    /// The maximum age in seconds before eviction, if set.
    pub max_age_secs: Option<i64>,
    /// The maximum file size in bytes before eviction, if set.
    pub max_file_size: Option<i64>,
    /// The policy priority (higher wins).
    pub priority: i32,
}

/// `GET /v1/policies` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PoliciesResponse {
    /// The configured lifecycle policies.
    pub policies: Vec<PolicyView>,
}

/// `POST /v1/policies` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PolicyPost {
    /// The path glob to apply the policy to.
    pub path_glob: String,
    /// The maximum age in seconds before eviction, if any.
    #[serde(default)]
    pub max_age_secs: Option<i64>,
    /// The maximum file size in bytes before eviction, if any.
    #[serde(default)]
    pub max_file_size: Option<i64>,
    /// The policy priority.
    pub priority: i32,
}
