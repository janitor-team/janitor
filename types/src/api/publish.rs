//! Request and response types for the publish service, plus the
//! messages it broadcasts on its Redis pub/sub channels.

use crate::publish::{MergeProposalStatus, Mode};
use crate::{Campaign, CodebaseName, PublishId, RunId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use url::Url;

/// Entry broadcast on the `publish` Redis channel after a publish attempt
/// that was stored with its outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishedEntry {
    /// Unique identifier for the publish attempt.
    pub id: PublishId,
    /// Codebase being published.
    pub codebase: CodebaseName,
    /// Campaign the publish belongs to.
    pub campaign: Campaign,
    /// URL of the merge proposal, if any.
    pub proposal_url: Option<Url>,
    /// Publish mode used.
    pub mode: Mode,
    /// Main branch URL.
    pub main_branch_url: Url,
    /// Main branch web URL.
    pub main_branch_browse_url: Option<Url>,
    /// Branch name that was published.
    pub branch_name: Option<String>,
    /// Result code.
    pub result_code: String,
    /// Result payload.
    pub result: Option<serde_json::Value>,
    /// Role of the branch.
    pub role: String,
    /// ID of the run that produced the publish.
    pub run_id: RunId,
    /// Delay between the run and the publish, in seconds.
    pub publish_delay: Option<f64>,
}

impl super::PubSubMessage for PublishedEntry {
    fn channel() -> &'static str {
        "publish"
    }
}

/// Entry broadcast on the `publish` Redis channel after a failed publish.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishFailedEntry {
    /// Unique identifier for the publish attempt.
    pub id: PublishId,
    /// Publish mode that was attempted.
    pub mode: Mode,
    /// Result code.
    pub result_code: String,
    /// Human-readable description of the failure.
    pub description: String,
    /// Campaign the publish belongs to.
    pub campaign: Campaign,
    /// Main branch URL.
    pub main_branch_url: Url,
    /// Result payload.
    pub result: Option<serde_json::Value>,
    /// Codebase being published.
    pub codebase: CodebaseName,
}

/// Message broadcast on the `merge-proposal` Redis channel when a
/// proposal's stored state is updated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeProposalUpdate {
    /// URL of the merge proposal.
    pub url: Url,
    /// URL of the target branch.
    pub target_branch_url: Url,
    /// Rate limit bucket for the publishing forge.
    pub rate_limit_bucket: Option<String>,
    /// Status of the merge proposal.
    pub status: MergeProposalStatus,
    /// Codebase the merge proposal belongs to.
    pub codebase: Option<CodebaseName>,
    /// Name of the user who merged the proposal.
    pub merged_by: Option<String>,
    /// URL of the user who merged the proposal.
    pub merged_by_url: Option<Url>,
    /// Timestamp of the merge.
    pub merged_at: Option<DateTime<Utc>>,
    /// Campaign the merge proposal belongs to.
    pub campaign: Option<Campaign>,
}

impl super::PubSubMessage for MergeProposalUpdate {
    fn channel() -> &'static str {
        "merge-proposal"
    }
}

/// Event the publisher broadcasts on the `merge-proposal` Redis channel
/// after it (re)stores a merge proposal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeProposalEvent {
    /// URL of the merge proposal.
    pub url: Url,
    /// Web URL of the merge proposal.
    pub web_url: Option<Url>,
    /// Status of the merge proposal.
    pub status: MergeProposalStatus,
    /// The codebase this proposal belongs to.
    pub codebase: CodebaseName,
    /// The campaign this proposal belongs to.
    pub campaign: Campaign,
    /// The target branch URL.
    pub target_branch_url: Option<Url>,
    /// Optional target branch web URL.
    pub target_branch_web_url: Option<Url>,
    /// Timestamp of the event.
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl super::PubSubMessage for MergeProposalEvent {
    fn channel() -> &'static str {
        "merge-proposal"
    }
}

/// Message broadcast by the runner on the `publish-status` channel
/// when a run's publish_status column is updated via the runner's
/// `POST /runs/{run_id}` endpoint. The publisher acts only on `"approved"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishStatusMessage {
    /// Run id the status applies to.
    pub run_id: RunId,
    /// New publish_status value. The publisher acts only on `"approved"`.
    pub publish_status: String,
    /// Codebase the run belongs to.
    pub codebase: CodebaseName,
    /// Campaign (run.campaign) the run belongs to.
    pub campaign: Campaign,
}

impl super::PubSubMessage for PublishStatusMessage {
    fn channel() -> &'static str {
        "publish-status"
    }
}

/// Empty JSON object, returned by endpoints that have nothing to report.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct EmptyResponse {}

/// Success response with URL
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct SuccessWithUrlResponse {
    /// Status message indicating success
    pub status: String,
    /// URL of the created or affected resource
    pub url: String,
}

/// Not found response with reason
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct NotFoundResponse {
    /// Reason why the resource was not found
    pub reason: String,
    /// Name of the resource that was not found
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// URL of the resource that was not found
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// ID of the resource that was not found
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Run ID if relevant to the not found error
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
}

/// Modes accepted by `POST /{campaign}/{codebase}/publish`. A subset of
/// `janitor_types::publish::Mode`: `skip`, `build-only` and `bts` are not
/// selectable from the publish endpoint.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub enum PublishRequestMode {
    /// Push the change to a derived branch.
    PushDerived,
    /// Push the change directly to the target branch.
    Push,
    /// Open a merge proposal for the change.
    Propose,
    /// Push if the forge allows it, otherwise propose.
    AttemptPush,
}

impl From<PublishRequestMode> for Mode {
    fn from(mode: PublishRequestMode) -> Self {
        match mode {
            PublishRequestMode::PushDerived => Mode::PushDerived,
            PublishRequestMode::Push => Mode::Push,
            PublishRequestMode::Propose => Mode::Propose,
            PublishRequestMode::AttemptPush => Mode::AttemptPush,
        }
    }
}

/// Response for `POST /{campaign}/{codebase}/publish` when there is
/// nothing to publish.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct PublishNothingResponse {
    /// Run that would have been published.
    pub run_id: RunId,
    /// Always `done`.
    pub code: String,
    /// Always `Nothing to do`.
    pub description: String,
}

/// Response for `POST /{campaign}/{codebase}/publish` when publishing
/// was started.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct PublishStartedResponse {
    /// Run that is being published.
    pub run_id: RunId,
    /// Mode that was requested, if any.
    pub mode: Option<PublishRequestMode>,
    /// Publish id for each branch role.
    pub publish_ids: BTreeMap<String, PublishId>,
}

/// Response for `POST /autopublish`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct AutopublishResponse {
    /// Always `Autopublish started.`.
    pub message: String,
    /// Maximum number of pushes for this run.
    pub push_limit: Option<usize>,
}

/// Simple error response, matching the shape used across janitor services.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ErrorResponse {
    /// Human-readable error message.
    pub error: String,
    /// Optional machine-readable error code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl ErrorResponse {
    /// Construct a new error response with the given message.
    pub fn new(error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            code: None,
        }
    }

    /// Attach a machine-readable code.
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    /// Standard 500 payload.
    pub fn internal_server_error() -> Self {
        Self::new("Internal server error").with_code("internal_error")
    }
}
