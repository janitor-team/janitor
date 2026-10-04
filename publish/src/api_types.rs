use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Rate limit information for all rate limits
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitsInfo {
    /// Rate limits organized by bucket name
    pub per_bucket: HashMap<String, BucketRateLimit>,
    /// Rate limit reset times for each forge
    pub per_forge: HashMap<String, chrono::DateTime<chrono::Utc>>,
    /// Maximum number of pushes allowed
    pub push_limit: Option<usize>,
}

/// Rate limit information for a specific bucket
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BucketRateLimit {
    /// Number of currently open items in this bucket
    pub open: Option<usize>,
    /// Maximum number of open items allowed in this bucket
    pub max_open: Option<usize>,
    /// Number of remaining items that can be opened in this bucket
    pub remaining: Option<usize>,
}

/// Absorbed run information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbsorbedRun {
    /// Unique identifier of the absorbed run
    pub run_id: String,
    /// URL of the branch that was absorbed
    pub branch_url: String,
    /// VCS revision identifier of the absorbed change
    pub revision: String,
    /// Timestamp when the run was absorbed
    pub absorbed_at: chrono::DateTime<chrono::Utc>,
}

/// Success with ID response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishSuccessResponse {
    /// Whether the operation was successful
    pub success: bool,
    /// Identifier of the created or updated resource
    pub id: String,
}

/// Status update response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusUpdateResponse {
    /// Current status of the resource
    pub status: String,
    /// Human-readable description of the status
    pub description: String,
}

/// Publish policy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishPolicy {
    /// Unique name of the policy
    pub name: String,
    /// Human-readable description of what this policy does
    pub description: Option<String>,
    /// Whether this policy is currently active
    pub enabled: bool,
    /// Priority of this policy (higher values take precedence)
    pub priority: i32,
    /// JSON conditions that must be met for this policy to apply
    pub conditions: serde_json::Value,
    /// JSON actions to take when conditions are met
    pub actions: serde_json::Value,
}

/// Publish settings update request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishSettingsUpdate {
    /// Command to execute for publishing
    pub command: Option<String>,
    /// Whether to include diff in merge proposals
    pub diff: Option<bool>,
    /// Whether binary diffs are required for publishing
    pub require_binary_diff: Option<bool>,
    /// List of reviewers to assign to merge proposals
    pub reviewers: Option<Vec<String>>,
    /// Custom tags to apply to published changes
    pub tags: Option<HashMap<String, String>>,
}

/// Queue item status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueItemStatus {
    /// Unique identifier of the queue item
    pub queue_id: i64,
    /// Current status of the queue item
    pub status: String,
    /// Priority of the queue item (higher values processed first)
    pub priority: i32,
    /// Timestamp when the item was added to the queue
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Publish result details
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishResultDetails {
    /// Publishing mode used (e.g., "merge-proposal", "push")
    pub mode: String,
    /// URL of the created merge proposal, if applicable
    pub merge_proposal_url: Option<String>,
    /// Result code indicating success or failure type
    pub result_code: String,
    /// Human-readable description of the result
    pub description: String,
    /// Additional details as JSON
    pub details: Option<serde_json::Value>,
}

/// Forge status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgeStatus {
    /// Name of the forge (e.g., "github", "gitlab")
    pub forge: String,
    /// Current operational status of the forge
    pub status: String,
    /// Whether the forge is currently rate limiting us
    pub rate_limited: bool,
    /// When the rate limit will reset, if rate limited
    pub rate_limit_reset: Option<chrono::DateTime<chrono::Utc>>,
}

/// Run status for publish
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunPublishStatus {
    /// Unique identifier of the run
    pub run_id: String,
    /// Current publish status of the run
    pub status: String,
    /// URL of the merge proposal if one was created
    pub proposal_url: Option<String>,
    /// Timestamp of the last publish attempt
    pub last_attempt: Option<chrono::DateTime<chrono::Utc>>,
}

/// Blocker information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockerInfo<T> {
    /// The blocker that is preventing publishing
    pub blocker: T,
    /// Number of items blocked by this blocker
    pub count: i64,
}

/// Simple message response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageResponse {
    /// The message content
    pub message: String,
}

/// Campaign blockers
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CampaignBlockers {
    /// Name of the campaign
    pub campaign: String,
    /// List of blockers affecting this campaign
    pub blockers: Vec<String>,
    /// Total number of runs blocked for this campaign
    pub total_blocked: i64,
}

/// Publish queue entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishQueueEntry {
    /// Unique identifier of the queue entry
    pub id: i64,
    /// Run identifier this queue entry is for
    pub run_id: String,
    /// Campaign name associated with the run
    pub campaign: String,
    /// Codebase where the change will be published
    pub codebase: String,
    /// Priority of this entry in the queue
    pub priority: i32,
    /// Timestamp when this entry was added to the queue
    pub queued_at: chrono::DateTime<chrono::Utc>,
}

/// Success response with name
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuccessWithNameResponse {
    /// Status message indicating success
    pub status: String,
    /// Name of the created or affected resource
    pub name: String,
}

/// Success response with URL
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuccessWithUrlResponse {
    /// Status message indicating success
    pub status: String,
    /// URL of the created or affected resource
    pub url: String,
}

/// Not found response with reason
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Bucket name if relevant to the not found error
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    /// Run ID if relevant to the not found error
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Campaign name if relevant to the not found error
    #[serde(skip_serializing_if = "Option::is_none")]
    pub campaign: Option<String>,
    /// Codebase name if relevant to the not found error
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codebase: Option<String>,
}

/// Conflict response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictResponse {
    /// Reason for the conflict
    pub reason: String,
    /// Name of the conflicting resource
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Run ID involved in the conflict
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Whether the operation can be forced to override the conflict
    #[serde(skip_serializing_if = "Option::is_none")]
    pub force_required: Option<bool>,
}

/// Publish dry run response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishDryRunResponse {
    /// Status of the dry run operation
    pub status: String,
    /// Run ID that was tested
    pub run_id: String,
    /// Campaign associated with the run
    pub campaign: String,
    /// Codebase where the change would be published
    pub codebase: String,
    /// Whether this run would be published if not a dry run
    pub would_publish: bool,
    /// Publishing mode that would be used
    pub mode: String,
}

/// Publish response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishResponse {
    /// Status of the publish operation
    pub status: String,
    /// Run ID that was published
    pub run_id: String,
    /// Campaign associated with the run
    pub campaign: String,
    /// Codebase where the change was published
    pub codebase: String,
    /// Results of each publish action, keyed by branch name
    pub results: HashMap<String, Option<String>>,
    /// Number of branches that were not published
    pub unpublished_branches_count: usize,
}

/// Publish error response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishErrorResponse {
    /// Error message describing what went wrong
    pub error: String,
    /// Run ID that failed to publish
    pub run_id: String,
}

/// Autopublish response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutopublishResponse {
    /// Number of runs successfully published
    pub published_runs: u32,
    /// Total number of runs processed
    pub processed_runs: u32,
    /// Count of each action type performed during autopublish
    pub actions: HashMap<String, u32>,
}

/// Simple error response, matching the shape used across janitor services.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// Human-readable error message.
    pub error: String,
    /// Optional additional details.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    /// Optional machine-readable error code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl ErrorResponse {
    /// Construct a new error response with the given message.
    pub fn new(error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            details: None,
            code: None,
        }
    }

    /// Attach additional details.
    pub fn with_details(mut self, details: impl Into<String>) -> Self {
        self.details = Some(details.into());
        self
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
