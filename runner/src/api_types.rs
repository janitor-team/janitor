use crate::{ActiveRunJson, JanitorResultJson, QueueItem, VcsInfo};
use chrono::{DateTime, Utc};
use janitor::{Campaign, ChangeSetId, CodebaseName, RunId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Queue summary information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueSummary {
    /// Total number of items in queue
    pub total: i64,
    /// Count of items by status
    pub by_status: HashMap<String, i64>,
    /// Count of items by campaign
    pub by_campaign: HashMap<Campaign, i64>,
    /// Number of items currently processing
    pub processing: i64,
    /// Number of items pending
    pub pending: i64,
}

/// Response containing the count of affected items
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AffectedCountResponse {
    /// Number of items that are affected by a query or operation
    pub affected_count: i64,
}

/// Response containing an item's position in the queue
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuePositionResponse {
    /// Position in the queue (None if not in queue)
    pub queue_position: Option<i32>,
}

/// Response containing information about a run being processed
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessingRunResponse {
    /// Information about the run itself
    pub run: RunInfo,
    /// URL or identifier of the codebase being processed
    pub codebase: Option<CodebaseName>,
    /// Name of the campaign this run belongs to
    pub campaign: Option<Campaign>,
}

/// Basic information about a run
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunInfo {
    /// Unique identifier for this run
    pub id: RunId,
    /// Timestamp when the run started
    pub started_at: chrono::DateTime<chrono::Utc>,
}

/// Response indicating successful upload with count of items uploaded
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadSuccessResponse {
    /// Status message (typically "success")
    pub status: String,
    /// Number of items successfully uploaded
    pub uploaded: usize,
}

/// Simple response containing only a status message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimpleStatusResponse {
    /// Status message describing the result of an operation
    pub status: String,
}

/// Assignment details given to a worker to process
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerAssignment {
    /// Unique identifier for this run
    pub run_id: RunId,
    /// Name of the campaign to execute
    pub campaign: Campaign,
    /// URL or identifier of the codebase to work on
    pub codebase: CodebaseName,
    /// Command to execute for the codemod
    pub command: String,
    /// Version control system information
    pub vcs_info: crate::VcsInfo,
    /// Previous codemod result if resuming a run
    pub codemod_result: Option<serde_json::Value>,
}

/// Response when a worker is assigned work from the queue
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssignmentResponse {
    /// Unique identifier for this assignment
    pub assignment_id: String,
    /// Identifier of the queue item being processed
    pub queue_item_id: String,
    /// Information needed to resume a previous run if applicable
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume: Option<ResumeInfo>,
}

/// Information needed to resume a previously started run
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeInfo {
    /// Identifier of the run to resume
    pub run_id: RunId,
    /// Name of the campaign being resumed
    pub campaign: Campaign,
    /// URL or identifier of the codebase
    pub codebase: CodebaseName,
    /// Command that was being executed
    pub command: String,
    /// Result from the previous codemod execution
    pub codemod_result: serde_json::Value,
}

/// Request to update a worker's status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerStatusUpdate {
    /// New status for the worker (e.g., "processing", "idle", "error")
    pub status: String,
    /// Optional additional details about the status
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

/// Request to update a candidate's information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateUpdate {
    /// Campaign this candidate belongs to
    pub campaign: Campaign,
    /// Identifier for the set of changes this candidate belongs to
    pub change_set: Option<ChangeSetId>,
    /// Additional context or metadata for the candidate
    pub context: Option<String>,
    /// Numeric value associated with the candidate (e.g., priority)
    pub value: Option<i64>,
    /// Estimated probability of success (0.0 to 1.0)
    pub success_chance: Option<f64>,
}

/// Statistical information about runs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunStats {
    /// Total number of runs
    pub total_runs: i64,
    /// Number of runs that completed successfully
    pub successful_runs: i64,
    /// Number of runs that failed
    pub failed_runs: i64,
    /// Average duration of runs in seconds
    pub average_duration_seconds: Option<f64>,
}

/// Statistical information about a campaign
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CampaignStats {
    /// Name of the campaign
    pub campaign: Campaign,
    /// Total number of candidates in this campaign
    pub total_candidates: i64,
    /// Number of candidates that have been processed
    pub processed_candidates: i64,
    /// Success rate as a decimal (0.0 to 1.0)
    pub success_rate: f64,
}

/// Health and status information for a worker
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerHealthStatus {
    /// Unique identifier for the worker
    pub worker_id: String,
    /// Current status of the worker (e.g., "healthy", "unhealthy", "offline")
    pub status: String,
    /// Timestamp of last communication from the worker
    pub last_seen: chrono::DateTime<chrono::Utc>,
    /// ID of the run currently being processed, if any
    pub current_run: Option<RunId>,
}

/// Detailed information about an item in the queue
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueItemDetails {
    /// Unique identifier for this queue item
    pub id: String,
    /// URL or identifier of the codebase to process
    pub codebase: CodebaseName,
    /// Campaign this item belongs to
    pub campaign: Campaign,
    /// Priority level (higher values processed first)
    pub priority: i32,
    /// Current status of the queue item (e.g., "pending", "processing", "completed")
    pub status: String,
    /// Timestamp when this item was added to the queue
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Timestamp when processing started, if applicable
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Simple response containing only a message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageResponse {
    /// Message text to display to the user
    pub message: String,
}

/// Response when attempting to kill a running process
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KillRunResponse {
    /// Whether the kill operation was successful
    pub success: bool,
    /// Optional success message
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Optional error message if the operation failed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Response when an operation partially fails
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartialFailureResponse {
    /// Status message (typically "partial_failure")
    pub status: String,
    /// List of error messages for the failed parts
    pub errors: Vec<String>,
}

/// Queue position information including total queue size
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuePositionTotal {
    /// Current position in the queue
    pub position: i64,
    /// Total number of items in the queue
    pub total: i64,
}

/// Response containing queue status information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueStatusResponse {
    /// Number of items currently in the queue
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_length: Option<i64>,
    /// Number of runs currently being processed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_runs: Option<i64>,
    /// Overall status of the queue system
    pub status: String,
    /// Error message if queue is in error state
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Number of runs that succeeded
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub succeeded: Option<i64>,
    /// Number of runs that failed
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<i64>,
}

/// Error response: `{"error": ...}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// Error message.
    pub error: String,
}

/// Error response with additional detail: `{"reason": ..., "detail": ...}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasonDetailResponse {
    /// Reason for the failure.
    pub reason: String,
    /// Further detail.
    pub detail: String,
}

/// Response for `GET /queue/position`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueuePositionEta {
    /// Position in the queue, if queued.
    pub position: Option<i64>,
    /// Estimated wait per active run, in seconds.
    pub wait_time: Option<f64>,
    /// Cumulative wait time of the items ahead, in seconds.
    pub cumulative_wait_time: Option<f64>,
}

/// Response for `POST /schedule-control`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduledRun {
    /// Campaign the run was scheduled for.
    pub campaign: Campaign,
    /// Priority offset.
    pub offset: f64,
    /// Queue bucket.
    pub bucket: String,
    /// Codebase the run was scheduled for.
    pub codebase: CodebaseName,
    /// ID of the queue item.
    pub queue_id: i32,
    /// Estimated duration in seconds.
    pub estimated_duration_seconds: f64,
}

/// Response for `POST /schedule`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleResponse {
    /// The scheduled run.
    #[serde(flatten)]
    pub scheduled: ScheduledRun,
    /// Position in the queue.
    pub queue_position: Option<i32>,
    /// Estimated wait time in seconds.
    pub queue_wait_time: Option<f64>,
}

/// Response for `GET /status`, also published on the `queue` channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerStatus {
    /// Currently active runs.
    pub processing: Vec<ActiveRunJson>,
    /// Hosts that are avoided.
    pub avoid_hosts: Vec<String>,
    /// Rate-limited hosts, mapped to the time (RFC 3339) until which
    /// they are rate-limited.
    pub rate_limit_hosts: HashMap<String, String>,
}

/// Response for `GET /active-runs/{id}/current-stage`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentStageResponse {
    /// Stage the worker is in, if known.
    pub current_stage: Option<String>,
    /// Error fetching the stage from the worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A candidate in a `POST /candidates` request.
///
/// Names are kept as plain strings so that invalid ones are reported
/// per candidate rather than rejecting the whole request body.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CandidateUpload {
    /// Codebase name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codebase: Option<String>,
    /// Campaign name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub campaign: Option<String>,
    /// Change set ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_set: Option<String>,
    /// Command to run; falls back to the campaign's command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Value of the candidate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<i64>,
    /// Name of the publish policy.
    #[serde(
        default,
        rename = "publish-policy",
        skip_serializing_if = "Option::is_none"
    )]
    pub publish_policy: Option<String>,
    /// Free-form context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Estimated chance of success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success_chance: Option<f64>,
    /// Queue bucket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    /// Extra requester information.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester: Option<String>,
    /// IDs of the candidates this is a followup for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub followup_for: Option<Vec<i64>>,
}

/// A candidate that was scheduled by `POST /candidates`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduledCandidate {
    /// Campaign name.
    pub campaign: Campaign,
    /// Codebase name.
    pub codebase: CodebaseName,
    /// Queue bucket.
    pub bucket: String,
    /// Change set ID.
    pub change_set: Option<ChangeSetId>,
    /// Priority offset.
    pub offset: f64,
    /// Estimated duration in seconds.
    pub estimated_duration: i64,
    /// ID of the queue item.
    #[serde(rename = "queue-id")]
    pub queue_id: i32,
    /// Whether the run was scheduled as a refresh.
    pub refresh: bool,
}

/// Response for `POST /candidates`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UploadCandidatesResponse {
    /// Candidates that were scheduled.
    pub success: Vec<ScheduledCandidate>,
    /// Commands that were invalid.
    pub invalid_command: Vec<String>,
    /// Values that were invalid.
    pub invalid_value: Vec<i64>,
    /// Campaigns that are not known.
    pub unknown_campaigns: Vec<Campaign>,
    /// Codebases that are not known.
    pub unknown_codebases: Vec<CodebaseName>,
    /// Publish policies that are not known.
    pub unknown_publish_policies: Vec<String>,
}

/// Response for `GET /runs/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    /// Codebase name.
    pub codebase: CodebaseName,
    /// Campaign name.
    pub campaign: Campaign,
    /// Publish status of the run.
    pub publish_status: janitor::api::PublishStatus,
}

/// Response for `GET /active-runs/+peek`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeekResponse {
    /// The next queue item.
    pub queue_item: QueueItem,
    /// VCS information for the codebase.
    pub vcs_info: VcsInfo,
    /// Build configuration.
    pub build_config: HashMap<String, String>,
    /// Estimated duration in seconds.
    pub estimated_duration: Option<u64>,
}

/// An entry in the `GET /queue` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueEntry {
    /// ID of the queue item.
    pub queue_id: i32,
    /// Codebase name.
    pub codebase: CodebaseName,
    /// Campaign name.
    pub campaign: Campaign,
    /// Free-form context.
    pub context: Option<String>,
    /// Command to run.
    pub command: Option<String>,
}

/// A worker in the `GET /workers` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicWorkerInfo {
    /// Worker name.
    pub name: String,
    /// "active" or "idle".
    pub status: String,
    /// When the worker was last seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<DateTime<Utc>>,
}

/// A worker in the `GET /admin/workers` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminWorkerInfo {
    /// Worker name.
    pub name: String,
    /// Link to the worker.
    pub link: Option<String>,
    /// "failed", "active" or "idle".
    pub status: String,
    /// When the worker was last seen.
    pub last_seen: DateTime<Utc>,
}

/// Worker counts in a worker list response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerCounts {
    /// Total number of workers.
    pub total: usize,
    /// Number of active workers.
    pub active: usize,
    /// Number of idle workers.
    pub idle: usize,
    /// Number of failed workers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<usize>,
}

/// Response for `GET /workers` and `GET /admin/workers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerListResponse<W> {
    /// Error listing the workers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The workers.
    pub workers: Vec<W>,
    /// Total number of workers.
    pub total_workers: usize,
    /// Number of active workers.
    pub active_workers: usize,
    /// Number of idle workers.
    pub idle_workers: usize,
    /// Worker counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<WorkerCounts>,
    /// Time of the response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
}

impl<W> WorkerListResponse<W> {
    /// Response for when listing the workers failed.
    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            error: Some(error.into()),
            workers: Vec::new(),
            total_workers: 0,
            active_workers: 0,
            idle_workers: 0,
            summary: None,
            timestamp: None,
        }
    }
}

/// Response for `POST /admin/runs/cleanup`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupRunsResponse {
    /// Number of matched runs.
    pub matched: usize,
    /// Number of rescheduled runs.
    pub rescheduled: i64,
    /// Number of deleted runs.
    pub deleted: i64,
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// Result code that was matched.
    pub result_code: String,
    /// IDs of the matched runs; only set for dry runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_ids: Option<Vec<RunId>>,
}

/// Response for `POST /resume/check`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeCheckResponse {
    /// Whether resume information is available.
    pub resume_available: bool,
    /// The resume information.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_info: Option<crate::resume::ResumeInfo>,
    /// Message when no resume information is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Response for `GET /resume/chain/{run_id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeChainResponse {
    /// Run ID.
    pub run_id: RunId,
    /// Chain of runs this run resumes from.
    pub resume_chain: Vec<RunId>,
}

/// Response for `GET /resume/descendants/{run_id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeDescendantsResponse {
    /// Run ID.
    pub run_id: RunId,
    /// Runs that resume from this run.
    pub descendants: Vec<RunId>,
}

/// Response for `GET /resume/validate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeConsistencyResponse {
    /// "consistent" or "inconsistent".
    pub status: String,
    /// Message when consistent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Inconsistencies found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<String>>,
}

/// Response for a successful `POST /active-runs/{id}/finish`.
pub type FinishResponse = janitor::api::runner::FinishResponse<JanitorResultJson>;

/// 409 response for a `POST /active-runs/{id}/finish` of a run that was
/// already stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinishConflictResponse {
    /// Run ID.
    pub id: RunId,
    /// The stored result; omitted on the public API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<JanitorResultJson>,
    /// Reason for the conflict.
    pub reason: String,
}

/// 400 response for an assignment request with a missing or unknown
/// worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignWorkerError {
    /// Error code.
    pub error: String,
    /// The unknown worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
    /// Explanation.
    pub reason: String,
}

/// Response for an assignment request while the forge is rate-limiting
/// us.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimitedResponse {
    /// Reason ("rate limited").
    pub reason: String,
    /// Rate-limited host.
    pub host: String,
    /// Seconds after which to retry, if known.
    pub retry_after: Option<f64>,
}

/// Branch to work on, in a work assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignmentBranch {
    /// URL of the cached copy of the branch.
    pub cached_url: Option<String>,
    /// VCS type.
    pub vcs_type: Option<String>,
    /// URL of the branch.
    pub url: Option<String>,
    /// Path inside the branch.
    pub subpath: String,
    /// Colocated branches to fetch along with the main branch.
    pub additional_colocated_branches: Option<Vec<String>>,
    /// Whether to start from an empty branch if there is none.
    #[serde(rename = "default-empty")]
    pub default_empty: bool,
}

/// Repository to push results to, in a work assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignmentTargetRepository {
    /// URL of the repository.
    pub url: Option<String>,
    /// VCS type.
    pub vcs_type: Option<String>,
}

/// Resume information in a work assignment: `{run_id, result,
/// branch_url, branches}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeAssignment {
    /// Run to resume from.
    pub run_id: RunId,
    /// Previous run's `result` JSON blob.
    pub result: Option<serde_json::Value>,
    /// URL of the resume branch as opened via the forge or the VCS
    /// manager fallback.
    pub branch_url: String,
    /// Per-role result branches:
    /// `[(role, remote_name, base_revision, revision), ...]`.
    pub branches: Vec<(String, Option<String>, Option<String>, Option<String>)>,
}

/// Debian build configuration in a work assignment. Field names match
/// those of `janitor::api::worker::DebianBuildConfig`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebianBuildAssignment {
    /// Distribution to build for.
    #[serde(rename = "build-distribution")]
    pub build_distribution: String,
    /// Suffix for the version.
    #[serde(rename = "build-suffix")]
    pub build_suffix: String,
    /// Build command; the worker does not build without one.
    #[serde(
        default,
        rename = "build-command",
        skip_serializing_if = "Option::is_none"
    )]
    pub build_command: Option<String>,
    /// Chroot to build in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chroot: Option<String>,
    /// Extra apt repositories, as sources.list lines.
    #[serde(rename = "build-extra-repositories")]
    pub extra_repositories: Vec<String>,
    /// URL of the dependency server.
    pub dep_server_url: Option<String>,
}

/// Generic build configuration in a work assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenericBuildAssignment {
    /// URL of the dependency server.
    pub dep_server_url: Option<String>,
}

/// Build configuration in a work assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BuildAssignmentConfig {
    /// Debian build configuration.
    Debian(DebianBuildAssignment),
    /// Generic build configuration.
    Generic(GenericBuildAssignment),
}

/// Build to run, in a work assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildAssignment {
    /// Build target ("debian" or "generic").
    pub target: String,
    /// Build configuration.
    pub config: BuildAssignmentConfig,
    /// Build environment.
    pub environment: HashMap<String, String>,
}

/// Work assignment handed to a worker by `POST /active-runs`. See
/// `janitor::api::worker::Assignment` for the worker side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssignmentBody {
    /// Run ID.
    pub id: RunId,
    /// Human-readable description.
    pub description: String,
    /// ID of the queue item.
    pub queue_id: i64,
    /// Campaign name.
    pub campaign: Campaign,
    /// Codebase name.
    pub codebase: CodebaseName,
    /// Whether to build even if the codemod made no changes.
    #[serde(rename = "force-build")]
    pub force_build: bool,
    /// Branch to work on.
    pub branch: AssignmentBranch,
    /// Run to resume from.
    pub resume: Option<ResumeAssignment>,
    /// Repository to push results to.
    pub target_repository: AssignmentTargetRepository,
    /// Whether to skip validating the setup.
    #[serde(rename = "skip-setup-validation")]
    pub skip_setup_validation: bool,
    /// Codemod command.
    pub command: String,
    /// Codemod to run.
    pub codemod: janitor::api::worker::Codemod,
    /// Environment for both the codemod and the build.
    pub env: HashMap<String, String>,
    /// Build to run.
    pub build: BuildAssignment,
}

/// Public view of a run's health in `GET /watchdog/health`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicRunHealth {
    /// Run ID.
    pub log_id: RunId,
    /// Worker processing the run.
    pub worker_name: String,
    /// When the run started.
    pub start_time: DateTime<Utc>,
    /// Estimated duration in seconds.
    pub estimated_duration: Option<u64>,
    /// Number of consecutive health check failures.
    pub failure_count: u32,
    /// Maximum allowed failures before termination.
    pub max_failures: u32,
    /// Whether the worker is alive.
    pub alive: bool,
    /// Worker status.
    pub status: String,
    /// Last successful ping.
    pub last_ping: Option<DateTime<Utc>>,
}

/// Response for `GET /watchdog/health`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchdogHealthResponse {
    /// "ok" or "error".
    pub status: String,
    /// Number of active runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_runs: Option<usize>,
    /// Health of each active run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_statuses: Option<Vec<PublicRunHealth>>,
    /// Error message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Request for resume information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeInfoRequest {
    /// Campaign name (stored as `run.suite` in the schema).
    pub campaign: Campaign,
    /// Revision id of the resume branch's tip. Used together with
    /// `campaign` to form the `(suite, revision)` resume lookup key.
    pub resume_revision: String,
}

/// JSON body for `POST /schedule`. Callers may send either a `run_id`
/// (in which case campaign+codebase are derived from the run row) or
/// an explicit (campaign, codebase) pair, plus optional scheduling
/// knobs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScheduleRequest {
    /// Run to derive the campaign and codebase from.
    pub run_id: Option<RunId>,
    /// Campaign name.
    pub campaign: Option<Campaign>,
    /// Codebase name.
    pub codebase: Option<CodebaseName>,
    /// Whether to refresh.
    pub refresh: Option<bool>,
    /// Change set ID.
    pub change_set: Option<ChangeSetId>,
    /// Who requested the run.
    pub requester: Option<String>,
    /// Queue bucket.
    pub bucket: Option<String>,
    /// Priority offset.
    pub offset: Option<f64>,
    /// Estimated duration in seconds.
    pub estimated_duration: Option<f64>,
}

/// JSON body for `POST /schedule-control`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScheduleControlRequest {
    /// Run to derive the codebase and main branch revision from.
    pub run_id: Option<RunId>,
    /// Codebase name.
    pub codebase: Option<CodebaseName>,
    /// Revision of the main branch to build.
    pub main_branch_revision: Option<String>,
    /// Change set ID.
    pub change_set: Option<ChangeSetId>,
    /// Priority offset.
    pub offset: Option<f64>,
    /// Who requested the run.
    pub requester: Option<String>,
    /// Whether to refresh.
    pub refresh: Option<bool>,
    /// Queue bucket.
    pub bucket: Option<String>,
    /// Estimated duration in seconds.
    pub estimated_duration: Option<f64>,
}
