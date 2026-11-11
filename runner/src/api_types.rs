//! Request and response bodies of the runner's HTTP API.

use chrono::{DateTime, Utc};
use janitor::vcs::VcsType;
use janitor_types::{Campaign, ChangeSetId, CodebaseName, LogFilename, RevisionId, Role, RunId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::ToSchema;

/// Error body carrying an `error` message.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ErrorResponse {
    /// Description of the error.
    pub error: String,
}

impl ErrorResponse {
    /// Create an error response with the given message.
    pub fn new(error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
        }
    }
}

/// Error body carrying a `reason` message.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReasonResponse {
    /// Why the request could not be served.
    pub reason: String,
    /// Further details, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl ReasonResponse {
    /// Create a response with the given reason and no detail.
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            detail: None,
        }
    }
}

/// Simple response containing only a message
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MessageResponse {
    /// Message text to display to the user
    pub message: String,
}

/// Empty JSON object, returned by endpoints that have nothing to report.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct EmptyResponse {}

/// Response for `GET /queue/position`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct QueuePositionResponse {
    /// Position in the queue; null when the item is not queued.
    pub position: Option<i64>,
    /// Estimated wait in seconds, divided over the active runs.
    pub wait_time: Option<f64>,
    /// Estimated wait in seconds for all items ahead in the queue.
    pub cumulative_wait_time: Option<f64>,
}

/// Response for `POST /schedule-control`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ScheduleControlResponse {
    /// Campaign that was scheduled; always `control`.
    pub campaign: Campaign,
    /// Queue offset.
    pub offset: f64,
    /// Queue bucket.
    pub bucket: String,
    /// Codebase that was scheduled.
    pub codebase: CodebaseName,
    /// Id of the queue item.
    pub queue_id: i32,
    /// Estimated duration of the run, in seconds; null when it is zero.
    pub estimated_duration_seconds: Option<f64>,
}

/// Response for `POST /schedule`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ScheduleResponse {
    /// Campaign that was scheduled.
    pub campaign: Campaign,
    /// Queue offset.
    pub offset: f64,
    /// Queue bucket.
    pub bucket: String,
    /// Codebase that was scheduled.
    pub codebase: CodebaseName,
    /// Id of the queue item.
    pub queue_id: i32,
    /// Estimated duration of the run, in seconds; null when it is zero.
    pub estimated_duration_seconds: Option<f64>,
    /// Position of the new item in the queue, if known.
    pub queue_position: Option<i32>,
    /// Estimated wait before the item is processed, in seconds.
    pub queue_wait_time: Option<f64>,
}

/// An active run, as reported by `/status`, `/active-runs` and `/kill`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ActiveRunResponse {
    /// Id of the queue item being processed.
    pub queue_id: i64,
    /// Run id.
    pub id: RunId,
    /// Codebase being processed.
    pub codebase: CodebaseName,
    /// Change set the run belongs to.
    pub change_set: Option<ChangeSetId>,
    /// Campaign of the run.
    pub campaign: Campaign,
    /// Codemod command.
    pub command: String,
    /// Estimated duration in seconds.
    pub estimated_duration: Option<f64>,
    /// Time spent on the run so far, in seconds.
    pub current_duration: f64,
    /// Start time, in RFC 3339 format.
    pub start_time: String,
    /// Name of the worker processing the run.
    pub worker: String,
    /// Link to the worker's page for this run.
    pub worker_link: Option<String>,
    /// VCS location of the codebase.
    #[schema(value_type = Object)]
    pub vcs: janitor::queue::VcsInfo,
    /// How to reach the worker.
    #[schema(value_type = Object)]
    pub backchannel: serde_json::Value,
    /// Context the run was scheduled with.
    #[schema(value_type = Option<Object>)]
    pub instigated_context: Option<serde_json::Value>,
    /// Run this run resumes from.
    pub resume_from: Option<RunId>,
}

/// Response for `GET /status`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct StatusResponse {
    /// Runs that are currently active.
    pub processing: Vec<ActiveRunResponse>,
    /// Hosts that the runner avoids.
    pub avoid_hosts: Vec<String>,
    /// Rate-limited hosts, mapped to the RFC 3339 time the limit expires.
    pub rate_limit_hosts: BTreeMap<String, String>,
}

/// Response for `GET /active-runs/{id}/current-stage`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CurrentStageResponse {
    /// Stage the worker is in, if known.
    pub current_stage: Option<String>,
    /// Why the stage could not be retrieved from the worker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A candidate that was scheduled by `POST /candidates`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ScheduledCandidate {
    /// Campaign of the candidate.
    pub campaign: Campaign,
    /// Codebase of the candidate.
    pub codebase: CodebaseName,
    /// Queue bucket.
    pub bucket: String,
    /// Change set of the candidate.
    pub change_set: Option<ChangeSetId>,
    /// Queue offset.
    pub offset: f64,
    /// Estimated duration of the run, in seconds.
    pub estimated_duration: i64,
    /// Id of the queue item.
    #[serde(rename = "queue-id")]
    pub queue_id: i32,
    /// Whether the run was scheduled as a refresh.
    pub refresh: bool,
}

/// Response for `POST /candidates`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UploadCandidatesResponse {
    /// Candidates that were stored and scheduled.
    pub success: Vec<ScheduledCandidate>,
    /// Commands that were rejected.
    pub invalid_command: Vec<String>,
    /// Values that were rejected.
    pub invalid_value: Vec<i64>,
    /// Campaigns that are not in the configuration.
    pub unknown_campaigns: Vec<Campaign>,
    /// Codebases that are not known.
    pub unknown_codebases: Vec<CodebaseName>,
    /// Publish policies that are not known.
    pub unknown_publish_policies: Vec<String>,
}

/// One entry in a run's `result_branches`, mirroring the
/// `new_result_branch` row for a given role.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResultBranchEntry {
    /// Role of the branch (e.g. `main`, `pristine-tar`).
    pub role: Role,
    /// Name of the branch on the remote, if known.
    pub remote_name: Option<String>,
    /// Revision the branch was based on before the run modified it.
    #[schema(value_type = Option<String>)]
    pub base_revision: Option<RevisionId>,
    /// Revision the branch pointed to after the run.
    #[schema(value_type = Option<String>)]
    pub revision: Option<RevisionId>,
}

/// One tag a run produced, mirroring the `result_tag` composite type
/// stored in `run.result_tags`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RunResultTag {
    /// Actual tag name created in the VCS.
    pub actual_name: String,
    /// Revision the tag points at.
    #[schema(value_type = String)]
    pub revision: RevisionId,
}

/// Debian-specific fields for a run, from the `debian_build` table.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RunDebianBuild {
    /// Debian version text of the built package.
    pub version: String,
    /// Distribution the package was built for.
    pub distribution: String,
    /// Raw lintian output, if recorded.
    #[schema(value_type = Option<Object>)]
    pub lintian_result: Option<serde_json::Value>,
    /// Binary packages produced, if recorded.
    pub binary_packages: Option<Vec<String>>,
}

/// Response for `GET /runs/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RunDetailResponse {
    /// Run identifier.
    pub id: RunId,
    /// Codebase of the run.
    pub codebase: CodebaseName,
    /// Campaign of the run.
    pub campaign: Campaign,
    /// Publish status of the run.
    pub publish_status: String,
    /// Result code (e.g. `success`, `missing-upstream-tarball`).
    pub result_code: String,
    /// Revision the main branch pointed to at the start of the run.
    #[schema(value_type = Option<String>)]
    pub main_branch_revision: Option<RevisionId>,
    /// Revision the result branch points to.
    #[schema(value_type = Option<String>)]
    pub revision: Option<RevisionId>,
    /// VCS type of the codebase, if known.
    pub vcs_type: Option<VcsType>,
    /// Branch URL of the codebase at the time of the run.
    pub branch_url: Option<String>,
    /// Target branch URL the run was proposing against, if any.
    pub target_branch_url: Option<String>,
    /// When the run started.
    pub start_time: DateTime<Utc>,
    /// When the run finished.
    pub finish_time: DateTime<Utc>,
    /// Command that was executed.
    pub command: Option<String>,
    /// Human-readable description of the result.
    pub description: Option<String>,
    /// Context the run started with, if any.
    pub context: Option<String>,
    /// Structured result returned by the codemod.
    #[schema(value_type = Option<Object>)]
    pub result: Option<serde_json::Value>,
    /// Context the run was scheduled with (i.e. before any resume).
    pub instigated_context: Option<String>,
    /// Log files produced by the run.
    pub logfilenames: Vec<LogFilename>,
    /// Name of the worker that executed the run.
    pub worker_name: Option<String>,
    /// Link to the worker, if recorded on the row.
    pub worker_link: Option<String>,
    /// Change set the run belongs to.
    pub change_set: ChangeSetId,
    /// Stage at which the run failed, if any.
    pub failure_stage: Option<String>,
    /// Whether the failure is believed to be transient.
    pub failure_transient: Option<bool>,
    /// Run this one resumed from, if any.
    pub resume_from: Option<String>,
    /// Tags produced by the run, if any.
    pub result_tags: Option<Vec<RunResultTag>>,
    /// Result branches produced by the run, keyed by role.
    pub result_branches: Vec<ResultBranchEntry>,
    /// Debian-specific build info, if the run produced a Debian build.
    pub debian_build: Option<RunDebianBuild>,
}

/// One entry in a `GET /runs` listing.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, sqlx::FromRow)]
pub struct RunSummary {
    /// Run identifier.
    pub id: RunId,
    /// Codebase of the run.
    pub codebase: CodebaseName,
    /// Campaign the run belongs to.
    pub campaign: Campaign,
    /// Result code (`success`, `missing-upstream-tarball`, ...).
    pub result_code: String,
    /// When the run started.
    pub start_time: DateTime<Utc>,
    /// When the run finished.
    pub finish_time: DateTime<Utc>,
    /// `finish_time - start_time`, in whole seconds.
    pub duration_seconds: i64,
    /// Human-readable description of the result, if any.
    pub description: Option<String>,
    /// Name of the worker that executed the run, if known.
    pub worker_name: Option<String>,
    /// Link to the worker, if recorded on the row.
    pub worker_link: Option<String>,
    /// Whether the failure (if any) is considered transient.
    pub failure_transient: Option<bool>,
    /// Revision the result branch points to, if any.
    pub revision: Option<String>,
    /// Revision the main branch was at when the run started, if any.
    pub main_branch_revision: Option<String>,
}

/// Response for `GET /runs`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RunListResponse {
    /// Matching runs, newest first.
    pub runs: Vec<RunSummary>,
}

/// One follow-up candidate of a run, keyed by (codebase, campaign).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, sqlx::FromRow)]
pub struct RunFollowup {
    /// Codebase of the follow-up candidate.
    pub codebase: CodebaseName,
    /// Campaign the follow-up candidate targets.
    pub campaign: Campaign,
}

/// Response for `GET /runs/{id}/followups`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RunFollowupsResponse {
    /// Follow-up candidates scheduled off this run.
    pub followups: Vec<RunFollowup>,
}

/// Response for `POST /runs/{id}`; also sent on the `publish-status`
/// pub/sub channel.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PublishStatusUpdate {
    /// Run that was updated.
    pub run_id: RunId,
    /// New publish status.
    pub publish_status: String,
    /// Codebase of the run.
    pub codebase: CodebaseName,
    /// Campaign of the run.
    pub campaign: Campaign,
}

/// A queue item, as previewed by `GET /active-runs/+peek`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct QueueItemResponse {
    /// Queue item id.
    pub id: i64,
    /// Context the item was scheduled with.
    #[schema(value_type = Option<Object>)]
    pub context: Option<serde_json::Value>,
    /// Codemod command.
    pub command: String,
    /// Estimated duration in seconds.
    pub estimated_duration: Option<f64>,
    /// Campaign of the item.
    pub campaign: Campaign,
    /// Whether this is a refresh.
    pub refresh: bool,
    /// Who scheduled the item.
    pub requester: Option<String>,
    /// Change set of the item.
    pub change_set: Option<ChangeSetId>,
    /// Codebase of the item.
    pub codebase: CodebaseName,
}

/// Response for `GET /active-runs/+peek`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PeekResponse {
    /// The next queue item.
    pub queue_item: QueueItemResponse,
    /// VCS location of the codebase.
    #[schema(value_type = Object)]
    pub vcs_info: janitor::queue::VcsInfo,
    /// Build configuration; contains `builder_kind` when a builder
    /// could be created.
    pub build_config: BTreeMap<String, String>,
    /// Estimated duration in seconds.
    pub estimated_duration: Option<u64>,
}

/// Entry in the `GET /queue` response.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct QueueEntry {
    /// Queue item id.
    pub queue_id: i32,
    /// Codebase of the item.
    pub codebase: CodebaseName,
    /// Campaign of the item.
    pub campaign: Campaign,
    /// Context the item was scheduled with.
    pub context: Option<String>,
    /// Codemod command.
    pub command: Option<String>,
}

/// A worker, as listed by `/workers` and `/admin/workers`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct WorkerInfo {
    /// Worker name.
    pub name: String,
    /// `active`, `idle` or (admin only) `failed`.
    pub status: String,
    /// Link to the worker's page; only included in the admin listing.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub link: Option<Option<String>>,
    /// When the worker was last seen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<DateTime<Utc>>,
    /// Number of completed runs recorded for this worker.
    pub run_count: i64,
}

/// Worker counts in the worker listing.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct WorkerSummary {
    /// Number of registered workers.
    pub total: usize,
    /// Number of workers with an active run.
    pub active: usize,
    /// Number of workers without an active run.
    pub idle: usize,
    /// Number of workers that have not been seen recently; only included
    /// in the admin listing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed: Option<usize>,
}

/// Response for `GET /workers` and `GET /admin/workers`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct WorkerListResponse {
    /// Set when the workers could not be listed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The workers.
    pub workers: Vec<WorkerInfo>,
    /// Number of registered workers.
    pub total_workers: usize,
    /// Number of workers with an active run.
    pub active_workers: usize,
    /// Number of idle workers.
    pub idle_workers: usize,
    /// Worker counts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<WorkerSummary>,
    /// When the listing was generated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
}

/// Response for `POST /admin/runs/cleanup`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CleanupRunsResponse {
    /// Number of runs that matched the filter.
    pub matched: usize,
    /// Number of queue items that were (re)scheduled.
    pub rescheduled: i64,
    /// Number of runs that were deleted.
    pub deleted: i64,
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// Result code that was matched.
    pub result_code: String,
    /// Matching run ids; only included for dry runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_ids: Option<Vec<RunId>>,
}

/// Response for `POST /resume/check`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResumeCheckResponse {
    /// Whether there is a run to resume from.
    pub resume_available: bool,
    /// The run to resume from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_info: Option<crate::resume::ResumeInfo>,
    /// Set when there is nothing to resume from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Response for `GET /resume/chain/{run_id}`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResumeChainResponse {
    /// The run that was asked about.
    pub run_id: RunId,
    /// Runs that `run_id` (transitively) resumes from.
    pub resume_chain: Vec<RunId>,
}

/// Response for `GET /resume/descendants/{run_id}`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResumeDescendantsResponse {
    /// The run that was asked about.
    pub run_id: RunId,
    /// Runs that resume from `run_id`.
    pub descendants: Vec<RunId>,
}

/// Response for `GET /resume/validate`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResumeValidationResponse {
    /// `consistent` or `inconsistent`.
    pub status: String,
    /// Set when the resume relationships are consistent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The problems found, when inconsistent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<String>>,
}

/// Response for finishing a run.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FinishResponse {
    /// Run ID.
    pub id: RunId,
    /// Uploaded filenames.
    pub filenames: Vec<String>,
    /// Log filenames.
    pub logs: Vec<String>,
    /// Artifact names; null when storing the artifacts failed.
    pub artifacts: Option<Vec<String>>,
    /// Result information.
    // TODO: describe the result with a schema rather than as an opaque object.
    #[schema(value_type = Object)]
    pub result: serde_json::Value,
}

/// Response for a `/finish` of a run that was already stored.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FinishConflictResponse {
    /// Run ID.
    pub id: RunId,
    /// The stored result; not included in the public API.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub result: Option<serde_json::Value>,
    /// Why the request was refused.
    pub reason: String,
}

/// Response when an assignment was refused because a forge rate-limited
/// the runner.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RateLimitedResponse {
    /// Always `rate limited`.
    pub reason: String,
    /// Host that rate-limited the runner.
    pub host: String,
    /// Seconds until the host can be retried, if the forge said.
    pub retry_after: Option<f64>,
}

/// Response when the private `/active-runs` endpoint rejects the worker.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AssignRejectedResponse {
    /// Machine-readable error code.
    pub error: String,
    /// The worker name that was rejected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
    /// Human-readable explanation.
    pub reason: String,
}

/// Resume information in an assignment.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResumeAssignment {
    /// Run to resume from.
    pub run_id: RunId,
    /// Previous run's `result` JSON blob.
    #[schema(value_type = Option<Object>)]
    pub result: Option<serde_json::Value>,
    /// URL of the resume branch as opened via the forge or the VCS
    /// manager fallback.
    pub branch_url: String,
    /// Per-role result branches:
    /// `[(role, remote_name, base_revision, revision), ...]`.
    #[schema(value_type = Vec<Vec<Option<String>>>)]
    pub branches: Vec<(String, Option<String>, Option<String>, Option<String>)>,
}

/// Branch to work on in an assignment.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AssignmentBranch {
    /// URL of the cached copy of the branch.
    pub cached_url: Option<String>,
    /// VCS type.
    pub vcs_type: Option<String>,
    /// URL of the branch; null for nascent branches.
    pub url: Option<String>,
    /// Path inside the branch.
    pub subpath: String,
    /// Colocated branches to fetch alongside the main branch.
    pub additional_colocated_branches: Option<Vec<String>>,
    /// Whether to start from an empty branch.
    #[serde(rename = "default-empty")]
    pub default_empty: bool,
}

/// Repository to push results to in an assignment.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AssignmentTargetRepository {
    /// Repository URL.
    pub url: Option<String>,
    /// VCS type.
    pub vcs_type: Option<String>,
}

/// Codemod to run in an assignment.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AssignmentCodemod {
    /// Command to run.
    pub command: String,
    /// Extra environment for the command.
    pub environment: BTreeMap<String, String>,
}

/// Debian build configuration in an assignment.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DebianBuildAssignment {
    /// Distribution to label the build with.
    #[serde(rename = "build-distribution")]
    pub build_distribution: String,
    /// Suffix for the build version.
    #[serde(rename = "build-suffix")]
    pub build_suffix: String,
    /// Build command; only sent when the campaign or the distribution
    /// defines one.
    #[serde(rename = "build-command", skip_serializing_if = "Option::is_none")]
    pub build_command: Option<String>,
    /// Chroot to build in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chroot: Option<String>,
    /// Extra apt sources, as `deb ...` lines.
    #[serde(rename = "build-extra-repositories")]
    pub extra_repositories: Vec<String>,
    /// URL of the dependency server.
    pub dep_server_url: Option<String>,
}

/// Generic build configuration in an assignment.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GenericBuildAssignment {
    /// URL of the dependency server.
    pub dep_server_url: Option<String>,
}

/// Build configuration in an assignment; which one is sent depends on
/// the build target.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum BuildAssignmentConfig {
    /// Configuration for the `debian` target.
    Debian(DebianBuildAssignment),
    /// Configuration for the `generic` target.
    Generic(GenericBuildAssignment),
}

/// Build to run in an assignment.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AssignmentBuild {
    /// `debian` or `generic`.
    pub target: String,
    /// Target-specific configuration.
    pub config: BuildAssignmentConfig,
    /// Environment for the build.
    pub environment: BTreeMap<String, String>,
}

/// Work assigned to a worker by `POST /active-runs`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AssignmentResponse {
    /// Run id.
    pub id: RunId,
    /// Human-readable description of the run.
    pub description: String,
    /// Id of the queue item.
    pub queue_id: i64,
    /// Campaign to run.
    pub campaign: Campaign,
    /// Codebase to work on.
    pub codebase: CodebaseName,
    /// Whether to build even when the codemod made no changes.
    #[serde(rename = "force-build")]
    pub force_build: bool,
    /// Branch to work on.
    pub branch: AssignmentBranch,
    /// Previous run to resume from.
    pub resume: Option<ResumeAssignment>,
    /// Repository to push to.
    pub target_repository: AssignmentTargetRepository,
    /// Whether to skip validating the setup.
    #[serde(rename = "skip-setup-validation")]
    pub skip_setup_validation: bool,
    /// Codemod command.
    pub command: String,
    /// Codemod to run.
    pub codemod: AssignmentCodemod,
    /// Environment for both the codemod and the build.
    pub env: BTreeMap<String, String>,
    /// Build to run.
    pub build: AssignmentBuild,
}

/// Health of a single active run in `GET /watchdog/health`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RunHealthSummary {
    /// Run id.
    pub log_id: RunId,
    /// Worker processing the run.
    pub worker_name: String,
    /// When the run started.
    pub start_time: DateTime<Utc>,
    /// Estimated duration in seconds.
    pub estimated_duration: Option<u64>,
    /// Number of consecutive failed health checks.
    pub failure_count: u32,
    /// Failed health checks after which the run is terminated.
    pub max_failures: u32,
    /// Whether the worker responded.
    pub alive: bool,
    /// Status reported by the worker, or `unknown`.
    pub status: String,
    /// Last successful ping.
    pub last_ping: Option<DateTime<Utc>>,
}

/// Response for `GET /watchdog/health`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct WatchdogHealthResponse {
    /// `ok` or `error`.
    pub status: String,
    /// Number of active runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_runs: Option<usize>,
    /// Health of each active run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_statuses: Option<Vec<RunHealthSummary>>,
    /// Set when the health could not be determined.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Response for `GET /queue/stats`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct QueueStatsResponse {
    /// Number of items in the queue.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_length: Option<i64>,
    /// Number of active runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_runs: Option<i64>,
    /// Number of successful runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub succeeded: Option<i64>,
    /// Number of failed runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed: Option<i64>,
    /// `operational` or `error`.
    pub status: String,
    /// Set when the statistics could not be retrieved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
