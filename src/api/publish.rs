//! Messages the publish service broadcasts on its Redis pub/sub channels.

use crate::publish::{MergeProposalStatus, Mode};
use chrono::{DateTime, Utc};
use janitor_types::{Campaign, CodebaseName, PublishId, RunId};
use serde::{Deserialize, Serialize};
use url::Url;

/// Entry broadcast on the `publish` Redis channel after a publish attempt
/// that was stored with its outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishedEntry {
    pub id: PublishId,
    pub codebase: CodebaseName,
    pub campaign: Campaign,
    pub proposal_url: Option<Url>,
    pub mode: Mode,
    pub main_branch_url: Url,
    pub main_branch_browse_url: Option<Url>,
    pub branch_name: Option<String>,
    pub result_code: String,
    pub result: Option<serde_json::Value>,
    pub role: String,
    pub run_id: RunId,
    pub publish_delay: Option<f64>,
}

/// Entry broadcast on the `publish` Redis channel after a failed publish.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishFailedEntry {
    pub id: PublishId,
    pub mode: Mode,
    pub result_code: String,
    pub description: String,
    pub campaign: Campaign,
    pub main_branch_url: Url,
    pub result: Option<serde_json::Value>,
    pub codebase: CodebaseName,
}

/// Message broadcast on the `merge-proposal` Redis channel when a
/// proposal's stored state is updated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeProposalUpdate {
    pub url: Url,
    pub target_branch_url: Url,
    pub rate_limit_bucket: Option<String>,
    pub status: MergeProposalStatus,
    pub codebase: Option<CodebaseName>,
    pub merged_by: Option<String>,
    pub merged_by_url: Option<Url>,
    pub merged_at: Option<DateTime<Utc>>,
    pub campaign: Option<Campaign>,
}
