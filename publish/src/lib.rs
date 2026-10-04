//! Publish crate for the Janitor project.
//!
//! This crate provides functionality for publishing changes and managing merge proposals.

#![deny(missing_docs)]

use crate::health::{BaseAppState, BasicHealthChecker, HealthCheckHandler};
use breezyshim::branch::Branch;
use breezyshim::error::Error as BrzError;
use breezyshim::forge::Forge;
use breezyshim::RevisionId;
use chrono::{DateTime, Utc};
use janitor::config::Campaign;
use janitor::publish::{MergeProposalStatus, Mode};
use janitor::state::Run;
use janitor::vcs::{VcsManager, VcsType};
use reqwest::header::HeaderMap;
use serde::ser::SerializeStruct;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

/// Request and response types for the HTTP API.
pub mod api_types;
/// Types and axum handlers for the `/health` and `/ready` endpoints.
pub mod health;
/// Prometheus metrics.
pub mod metrics;
/// Axum middleware.
pub mod middleware;
/// Read and update the `merge_proposal` rows the publisher scans.
pub mod proposal_info;
/// Drive a single publish attempt (worker-side, one revision at a time).
pub mod publish_one;
/// Background loops: `publish_pending_ready` and `check_stragglers`.
pub mod queue;
/// Bucket and forge rate limiters.
pub mod rate_limiter;
/// Redis pub/sub for the runner -> publisher approval hop.
pub mod redis;
/// Persist and query the `publish` / `merge_proposal` tables.
pub mod state;
/// Axum router and HTTP handlers.
pub mod web;

use rate_limiter::RateLimiter;

// Re-export from redis module to avoid import issues
pub use crate::redis::RedisConnectionManager;
// Import sqlx Row trait
use sqlx::Row;
// Import pyo3 for PyErr
use pyo3::{exceptions::PyRuntimeError, PyErr};

/// Calculate the next time to try publishing based on previous attempts.
///
/// This implements an exponential backoff strategy with a maximum delay.
///
/// # Arguments
/// * `finish_time` - The time of the last attempt
/// * `attempt_count` - The number of previous attempts
///
/// # Returns
/// The next time to try publishing
#[must_use = "the returned retry time should be stored on the run or used to gate the next attempt"]
pub fn calculate_next_try_time(finish_time: DateTime<Utc>, attempt_count: usize) -> DateTime<Utc> {
    if attempt_count == 0 {
        finish_time
    } else {
        // Use saturating arithmetic to prevent overflow
        let hours = if attempt_count >= 8 {
            // Cap at 7 days (168 hours) for attempt_count >= 8
            7 * 24
        } else {
            // 2^attempt_count hours, but cap at 7 days
            (2usize.pow(attempt_count as u32)).min(7 * 24)
        };
        let delta = chrono::Duration::hours(hours as i64);

        finish_time + delta
    }
}

/// Pure classification of a publish-failure code into the action
/// the caller should take. The original `handle_publish_failure`
/// branches on `code` and possibly on `unchanged_run`/`run.result_code`,
/// then performs DB rescheduling and rewrites the description.
/// Pull just the decision out so each branch can be unit-tested.
///
/// The classifier returns:
///
/// - `RescheduleRegular`  - call `do_schedule` for this run
/// - `RescheduleRegularRefresh` - same but with `refresh = true`
/// - `RescheduleControl { revision, refresh }` - call
///   `do_schedule_control` for the given (unchanged) revision
/// - `RewriteDescription { new_description }` - only update the
///   description; do not reschedule
/// - `NoAction` - neither reschedule nor rewrite (the default for
///   codes the publisher doesn't have a recovery for)
#[derive(Debug, PartialEq)]
pub(crate) enum PublishFailureAction {
    /// Reschedule a regular run for the same campaign/codebase,
    /// with an explanatory requester string.
    RescheduleRegular {
        /// Optional rewritten description.
        description: Option<String>,
        /// Requester string for the reschedule.
        requester: &'static str,
    },
    /// Reschedule a regular run with the refresh flag set.
    RescheduleRegularRefresh {
        /// Rewritten description (always set in this branch).
        description: String,
        /// Requester string for the reschedule.
        requester: &'static str,
    },
    /// Reschedule a control run targeting a specific (unchanged)
    /// revision.
    RescheduleControl {
        /// Rewritten description (always set in this branch).
        description: String,
        /// Whether to set the refresh flag.
        refresh: bool,
        /// Requester string for the reschedule.
        requester: &'static str,
        /// The revision the control run should target. `None` when
        /// the caller should use `run.main_branch_revision`.
        explicit_revision: Option<String>,
    },
    /// Rewrite the description only - no reschedule.
    RewriteDescription {
        /// The new description text.
        description: String,
    },
    /// No action needed.
    NoAction,
}

/// Pure classifier for `handle_publish_failure`. Inputs are the
/// failure `code`, the run's own `result_code` (used for the
/// missing-build-diff-self branch), and the result of looking up
/// an unchanged/control run at the same upstream revision (passed
/// as `(result_code, revision)`). Returns a [`PublishFailureAction`]
/// describing what the caller should do.
pub(crate) fn classify_publish_failure_action(
    code: &str,
    run_result_code: &str,
    unchanged_run: Option<(&str, &str)>,
) -> PublishFailureAction {
    match code {
        "merge-conflict" => PublishFailureAction::RescheduleRegular {
            description: None,
            requester: "publisher (pre-creation merge conflict)",
        },
        "diverged-branches" => PublishFailureAction::RescheduleRegular {
            description: None,
            requester: "publisher (diverged branches)",
        },
        "missing-build-diff-self" => {
            if run_result_code != "success" {
                PublishFailureAction::RewriteDescription {
                    description: "Missing build diff; run was not actually successful?".to_string(),
                }
            } else {
                PublishFailureAction::RescheduleRegularRefresh {
                    description: "Missing build artifacts, rescheduling".to_string(),
                    requester: "publisher (missing build artifacts - self)",
                }
            }
        }
        "missing-build-diff-control" => match unchanged_run {
            Some((unchanged_code, _)) if unchanged_code != "success" => {
                PublishFailureAction::RewriteDescription {
                    description: format!(
                        "Missing build diff; last control run failed ({}).",
                        unchanged_code
                    ),
                }
            }
            Some((_, unchanged_revision)) => PublishFailureAction::RescheduleControl {
                description: "Missing build diff due to control run, but successful \
                     control run exists. Rescheduling."
                    .to_string(),
                refresh: true,
                requester: "publisher (missing build artifacts - control)",
                explicit_revision: Some(unchanged_revision.to_string()),
            },
            None => PublishFailureAction::RescheduleControl {
                description: "Missing binary diff; requesting control run.".to_string(),
                refresh: false,
                requester: "publisher (missing control run for diff)",
                explicit_revision: None,
            },
        },
        _ => PublishFailureAction::NoAction,
    }
}

/// Classify a [`PublishError`] and, for failures that warrant a rerun,
/// reschedule the run so the next attempt picks up a fresh build.
///
/// Returns the effective `(code, description)` to record with the run;
/// for some codes the description is rewritten to explain what was
/// rescheduled.
pub async fn handle_publish_failure(
    e: &PublishError,
    conn: &sqlx::PgPool,
    run: &janitor::state::Run,
    bucket: &str,
) -> Result<(String, String), sqlx::Error> {
    let code = e.code().to_string();
    let mut description = e.description().to_string();

    let main_branch_revision_str = run.main_branch_revision.as_ref().map(|r| r.to_string());

    // Lookup the most recent successful unchanged/control run for this
    // codebase at the same upstream revision, used by the
    // missing-build-diff-control branch below.
    let unchanged_run: Option<(String, String)> = if let Some(rev) = &main_branch_revision_str {
        sqlx::query_as::<_, (String, String)>(
            "SELECT result_code, revision FROM last_runs \
             WHERE revision = $2 AND codebase = $1 AND result_code = 'success'",
        )
        .bind(&run.codebase)
        .bind(rev)
        .fetch_optional(conn)
        .await?
    } else {
        None
    };

    let action = classify_publish_failure_action(
        &code,
        &run.result_code,
        unchanged_run
            .as_ref()
            .map(|(c, r)| (c.as_str(), r.as_str())),
    );

    match action {
        PublishFailureAction::NoAction => {}
        PublishFailureAction::RewriteDescription {
            description: new_description,
        } => {
            description = new_description;
        }
        PublishFailureAction::RescheduleRegular {
            description: new_description,
            requester,
        } => {
            if let Some(d) = new_description {
                description = d;
            }
            log::info!("{}: rescheduling for {}", run.id, requester);
            if let Err(err) = janitor::schedule::do_schedule(
                conn,
                &run.suite,
                &run.codebase,
                bucket,
                Some(&run.change_set),
                None,
                false,
                Some(requester),
                None,
                None,
            )
            .await
            {
                log::warn!("Failed to reschedule {}: {}", run.id, err);
            }
        }
        PublishFailureAction::RescheduleRegularRefresh {
            description: new_description,
            requester,
        } => {
            description = new_description;
            if let Err(err) = janitor::schedule::do_schedule(
                conn,
                &run.suite,
                &run.codebase,
                bucket,
                Some(&run.change_set),
                None,
                true,
                Some(requester),
                None,
                None,
            )
            .await
            {
                log::warn!("Failed to reschedule (refresh) {}: {}", run.id, err);
            }
        }
        PublishFailureAction::RescheduleControl {
            description: new_description,
            refresh,
            requester,
            explicit_revision,
        } => {
            description = new_description;
            // Pick the revision: explicit one from the action, or
            // fall back to the run's main_branch_revision.
            let owned_explicit =
                explicit_revision.map(|s| breezyshim::RevisionId::from(s.into_bytes()));
            let revision_ref: Option<&breezyshim::RevisionId> = owned_explicit
                .as_ref()
                .or(run.main_branch_revision.as_ref());
            if let Some(revision) = revision_ref {
                if let Err(err) = janitor::schedule::do_schedule_control(
                    conn,
                    &run.codebase,
                    None,
                    Some(revision),
                    None,
                    refresh,
                    Some(bucket),
                    Some(requester),
                    None,
                )
                .await
                {
                    log::warn!("Failed to reschedule control run for {}: {}", run.id, err);
                }
            } else {
                log::warn!(
                    "Successful run ({}) does not have main branch revision set",
                    run.id
                );
            }
        }
    }

    Ok((code, description))
}

/// Errors that can occur when retrieving a debdiff.
#[derive(Debug)]
pub enum DebdiffError {
    /// An HTTP error occurred.
    Http(reqwest::Error),
    /// The run ID was missing.
    MissingRun(String),
    /// There is no unchanged/control run to compare against yet, e.g. a
    /// codebase's first run. Distinct from `MissingRun`: no request was
    /// made, and this is an expected state rather than a differ failure.
    NoUnchangedRun,
    /// The debdiff is unavailable.
    Unavailable(String),
}

impl From<reqwest::Error> for DebdiffError {
    fn from(e: reqwest::Error) -> Self {
        DebdiffError::Http(e)
    }
}

impl std::fmt::Display for DebdiffError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            DebdiffError::Http(e) => write!(f, "HTTP error: {}", e),
            DebdiffError::MissingRun(e) => write!(f, "Missing run: {}", e),
            DebdiffError::NoUnchangedRun => {
                write!(f, "No unchanged run to compare against yet")
            }
            DebdiffError::Unavailable(e) => write!(f, "Unavailable: {}", e),
        }
    }
}

impl std::error::Error for DebdiffError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DebdiffError::Http(e) => Some(e),
            _ => None,
        }
    }
}

/// Get a debdiff between two runs.
///
/// # Arguments
/// * `differ_url` - The URL of the differ service
/// * `unchanged_id` - The ID of the unchanged run, or `None` if there is no
///   unchanged run to compare against
/// * `log_id` - The ID of the changed run
///
/// # Returns
/// The debdiff as a byte vector, or an error
pub fn get_debdiff(
    differ_url: &url::Url,
    unchanged_id: Option<&str>,
    log_id: &str,
) -> Result<Vec<u8>, DebdiffError> {
    // The differ's /debdiff/{old_id}/{new_id} route has no representation
    // for "no control run" - an empty or missing old_id segment doesn't
    // match the route at all, so there's nothing useful to request.
    let Some(unchanged_id) = unchanged_id else {
        return Err(DebdiffError::NoUnchangedRun);
    };

    let debdiff_url = differ_url
        .join(&format!(
            "/debdiff/{}/{}?filter_boring=true",
            unchanged_id, log_id
        ))
        .map_err(|_e| DebdiffError::Unavailable("Invalid URL format".to_string()))?;

    let mut headers = HeaderMap::new();
    headers.insert(
        "Accept",
        "text/plain".parse().expect("Invalid header value"),
    );

    let client = reqwest::blocking::Client::new();
    let response = client.get(debdiff_url).headers(headers).send()?;

    match response.status() {
        reqwest::StatusCode::OK => Ok(response.bytes()?.to_vec()),
        reqwest::StatusCode::NOT_FOUND => {
            let run_id = response
                .headers()
                .get("unavailable_run_id")
                .and_then(|h| h.to_str().ok())
                .unwrap_or("unknown");
            Err(DebdiffError::MissingRun(run_id.to_string()))
        }
        reqwest::StatusCode::BAD_REQUEST
        | reqwest::StatusCode::INTERNAL_SERVER_ERROR
        | reqwest::StatusCode::BAD_GATEWAY
        | reqwest::StatusCode::SERVICE_UNAVAILABLE
        | reqwest::StatusCode::GATEWAY_TIMEOUT => {
            let error_text = response
                .text()
                .unwrap_or_else(|_| "Failed to read error response".to_string());
            Err(DebdiffError::Unavailable(error_text))
        }
        _e => Err(DebdiffError::Http(response.error_for_status().unwrap_err())),
    }
}

/// Request to publish a single run.
#[derive(Debug, serde::Deserialize, serde::Serialize, Clone)]
pub struct PublishOneRequest {
    /// The campaign name.
    pub campaign: String,
    /// The URL of the target branch.
    pub target_branch_url: url::Url,
    /// The role of the publisher.
    pub role: String,
    /// The ID of the log.
    pub log_id: String,
    /// Optional list of reviewers.
    pub reviewers: Option<Vec<String>>,
    /// The revision ID of the change.
    pub revision_id: RevisionId,
    /// The ID of the unchanged run.
    pub unchanged_id: Option<String>,
    /// Whether to require a binary diff.
    #[serde(rename = "require-binary-diff")]
    pub require_binary_diff: bool,
    /// The URL of the differ service.
    pub differ_url: url::Url,
    /// The name of the derived branch.
    pub derived_branch_name: String,
    /// Optional map of tags to revision IDs.
    pub tags: Option<HashMap<String, RevisionId>>,
    /// Whether to allow creating a new proposal.
    pub allow_create_proposal: bool,
    /// The URL of the source branch.
    pub source_branch_url: url::Url,
    /// The result of the codemod.
    pub codemod_result: serde_json::Value,
    /// Optional template for the commit message.
    pub commit_message_template: Option<String>,
    /// Optional template for the title.
    pub title_template: Option<String>,
    /// Optional URL of an existing merge proposal.
    pub existing_mp_url: Option<url::Url>,
    /// Optional extra context for the templates.
    pub extra_context: Option<serde_json::Value>,
    /// The mode of the publish operation.
    pub mode: Mode,
    /// The command that was run.
    pub command: String,
    /// Optional external URL for the publish operation.
    pub external_url: Option<url::Url>,
    /// Optional owner of the derived branch.
    pub derived_owner: Option<String>,
    /// Optional flag to automatically merge the proposal.
    pub auto_merge: Option<bool>,
}

/// Errors that can occur during publishing.
#[derive(Debug)]
pub enum PublishError {
    /// A failure occurred with a specific code and description.
    Failure {
        /// Error code that indicates the type of failure.
        code: String,
        /// Detailed description of the failure.
        description: String,
    },
    /// Nothing to do, with a reason.
    NothingToDo(String),
    /// The branch is already being used.
    BranchBusy(url::Url),
    /// Authentication failed.
    AuthenticationFailed,
    /// Network error occurred.
    NetworkError(String),
    /// Database error occurred.
    DatabaseError(sqlx::Error),
    /// Service temporarily unavailable.
    ServiceUnavailable(String),
}

impl PublishError {
    /// Get the error code.
    ///
    /// # Returns
    /// The error code as a string
    pub fn code(&self) -> &str {
        match self {
            PublishError::Failure { code, .. } => code,
            PublishError::NothingToDo(_) => "nothing-to-do",
            PublishError::BranchBusy(_) => "branch-busy",
            PublishError::AuthenticationFailed => "authentication-failed",
            PublishError::NetworkError(_) => "network-error",
            PublishError::DatabaseError(_) => "database-error",
            PublishError::ServiceUnavailable(_) => "service-unavailable",
        }
    }

    /// Get the error description.
    ///
    /// # Returns
    /// The error description as a string
    pub fn description(&self) -> &str {
        match self {
            PublishError::Failure { description, .. } => description,
            PublishError::NothingToDo(description) => description,
            PublishError::BranchBusy(_) => "Branch is busy",
            PublishError::AuthenticationFailed => "Authentication failed",
            PublishError::NetworkError(msg) => msg,
            PublishError::DatabaseError(_) => "Database error",
            PublishError::ServiceUnavailable(msg) => msg,
        }
    }
}

impl serde::Serialize for PublishError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            PublishError::Failure { code, description } => {
                let mut state = serializer.serialize_struct("PublishError", 2)?;
                state.serialize_field("code", code)?;
                state.serialize_field("description", description)?;
                state.end()
            }
            PublishError::NothingToDo(description) => {
                let mut state = serializer.serialize_struct("PublishError", 2)?;
                state.serialize_field("code", "nothing-to-do")?;
                state.serialize_field("description", description)?;
                state.end()
            }
            PublishError::BranchBusy(url) => {
                let mut state = serializer.serialize_struct("PublishError", 2)?;
                state.serialize_field("code", "branch-busy")?;
                state.serialize_field("description", &format!("Branch is busy: {}", url))?;
                state.end()
            }
            PublishError::AuthenticationFailed => {
                let mut state = serializer.serialize_struct("PublishError", 2)?;
                state.serialize_field("code", "authentication-failed")?;
                state.serialize_field("description", "Authentication failed")?;
                state.end()
            }
            PublishError::NetworkError(msg) => {
                let mut state = serializer.serialize_struct("PublishError", 2)?;
                state.serialize_field("code", "network-error")?;
                state.serialize_field("description", msg)?;
                state.end()
            }
            PublishError::DatabaseError(e) => {
                let mut state = serializer.serialize_struct("PublishError", 2)?;
                state.serialize_field("code", "database-error")?;
                state.serialize_field("description", &e.to_string())?;
                state.end()
            }
            PublishError::ServiceUnavailable(msg) => {
                let mut state = serializer.serialize_struct("PublishError", 2)?;
                state.serialize_field("code", "service-unavailable")?;
                state.serialize_field("description", msg)?;
                state.end()
            }
        }
    }
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            PublishError::Failure { code, description } => {
                write!(f, "PublishError::Failure: {}: {}", code, description)
            }
            PublishError::NothingToDo(description) => {
                write!(f, "PublishError::PublishNothingToDo: {}", description)
            }
            PublishError::BranchBusy(url) => {
                write!(f, "PublishError::BranchBusy: Branch is busy: {}", url)
            }
            PublishError::AuthenticationFailed => {
                write!(
                    f,
                    "PublishError::AuthenticationFailed: Authentication failed"
                )
            }
            PublishError::NetworkError(msg) => {
                write!(f, "PublishError::NetworkError: {}", msg)
            }
            PublishError::DatabaseError(e) => {
                write!(f, "PublishError::DatabaseError: {}", e)
            }
            PublishError::ServiceUnavailable(msg) => {
                write!(f, "PublishError::ServiceUnavailable: {}", msg)
            }
        }
    }
}

impl std::error::Error for PublishError {}

impl From<sqlx::Error> for PublishError {
    fn from(e: sqlx::Error) -> Self {
        PublishError::DatabaseError(e)
    }
}

/// Result of a publish operation.
#[derive(Debug, serde::Deserialize, serde::Serialize, Clone)]
pub struct PublishOneResult {
    /// The URL of the created merge proposal, if any.
    proposal_url: Option<url::Url>,
    /// The web URL of the created merge proposal, if any.
    proposal_web_url: Option<url::Url>,
    /// Whether the merge proposal is new.
    is_new: Option<bool>,
    /// The name of the branch.
    branch_name: String,
    /// The URL of the target branch.
    target_branch_url: url::Url,
    /// The web URL of the target branch, if any.
    target_branch_web_url: Option<url::Url>,
    /// The mode of the publish operation.
    mode: Mode,
}

impl PublishOneResult {
    /// URL of the created or updated merge proposal, if any.
    pub fn proposal_url(&self) -> Option<&url::Url> {
        self.proposal_url.as_ref()
    }

    /// Whether the proposal is brand new (vs. updated in place).
    pub fn is_new(&self) -> Option<bool> {
        self.is_new
    }

    /// Branch name on the source side.
    pub fn branch_name(&self) -> &str {
        &self.branch_name
    }

    /// Target branch URL after publishing.
    pub fn target_branch_url(&self) -> &url::Url {
        &self.target_branch_url
    }

    /// Optional human-friendly web URL of the target branch.
    pub fn target_branch_web_url(&self) -> Option<&url::Url> {
        self.target_branch_web_url.as_ref()
    }

    /// Mode the publisher actually used.
    pub fn mode(&self) -> Mode {
        self.mode
    }
}

/// Error returned by the publish_one operation.
#[derive(Debug, serde::Deserialize, serde::Serialize, Clone)]
pub struct PublishOneError {
    /// The error code.
    code: String,
    /// A description of the error.
    description: String,
}

/// Worker for publishing changes.
#[derive(Clone)]
pub struct PublishWorker {
    /// Optional path to the template environment.
    pub template_env_path: Option<PathBuf>,
    /// Optional external URL for the publish operation.
    pub external_url: Option<url::Url>,
    /// URL of the differ service.
    pub differ_url: url::Url,
    /// Optional Redis connection manager.
    pub redis: Option<RedisConnectionManager>,
    /// Redis manager for pub/sub operations.
    pub redis_manager: Option<Arc<janitor::redis::RedisManager>>,
    /// Optional lock manager for coordinating publish operations.
    pub lock_manager: Option<rslock::LockManager>,
}

/// Errors that can occur when interacting with a worker process.
#[derive(Debug)]
pub enum WorkerInvalidResponse {
    /// An I/O error occurred.
    Io(std::io::Error),
    /// An error occurred during serialization or deserialization.
    Serde(serde_json::Error),
    /// An error returned by the worker process.
    WorkerError(String),
}

impl From<std::io::Error> for WorkerInvalidResponse {
    fn from(e: std::io::Error) -> Self {
        WorkerInvalidResponse::Io(e)
    }
}

impl From<serde_json::Error> for WorkerInvalidResponse {
    fn from(e: serde_json::Error) -> Self {
        WorkerInvalidResponse::Serde(e)
    }
}

impl From<String> for WorkerInvalidResponse {
    fn from(e: String) -> Self {
        WorkerInvalidResponse::WorkerError(e)
    }
}

impl std::fmt::Display for WorkerInvalidResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            WorkerInvalidResponse::Io(e) => write!(f, "IO error: {}", e),
            WorkerInvalidResponse::Serde(e) => write!(f, "Serde error: {}", e),
            WorkerInvalidResponse::WorkerError(e) => write!(f, "Worker error: {}", e),
        }
    }
}

impl std::error::Error for WorkerInvalidResponse {}

/// Run a worker process with the given arguments and request.
///
/// # Arguments
/// * `args` - The command line arguments for the worker process
/// * `request` - The publish request to send to the worker
///
/// # Returns
/// A tuple of the exit code and the response value, or an error
async fn run_worker_process(
    args: Vec<String>,
    request: PublishOneRequest,
) -> Result<(i32, serde_json::Value), WorkerInvalidResponse> {
    let mut p = tokio::process::Command::new(&args[0])
        .args(&args[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    use tokio::io::AsyncWriteExt;
    if let Some(stdin) = p.stdin.as_mut() {
        let request_json =
            serde_json::to_string(&request).map_err(|e| WorkerInvalidResponse::Serde(e))?;
        stdin.write_all(request_json.as_bytes()).await?;
    } else {
        return Err(WorkerInvalidResponse::from(
            "Failed to get stdin handle".to_string(),
        ));
    }

    let status = p.wait().await?;

    if status.success() {
        let mut stdout = p.stdout.take().ok_or_else(|| {
            WorkerInvalidResponse::from("Failed to get stdout handle".to_string())
        })?;
        let _stderr = p.stderr.take();
        let mut output = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut stdout, &mut output).await?;

        let response =
            serde_json::from_reader(&mut output.as_slice()).map_err(WorkerInvalidResponse::from)?;
        Ok((status.code().unwrap_or(-1), response))
    } else if status.code() == Some(1) {
        let mut stdout = p.stdout.take().ok_or_else(|| {
            WorkerInvalidResponse::from("Failed to get stdout handle".to_string())
        })?;
        let mut stderr = p.stderr.take().ok_or_else(|| {
            WorkerInvalidResponse::from("Failed to get stderr handle".to_string())
        })?;
        let mut output = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut stdout, &mut output).await?;
        let response =
            serde_json::from_reader(&mut output.as_slice()).map_err(WorkerInvalidResponse::from)?;
        let mut error = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut stderr, &mut error).await?;
        use std::io::Write;
        std::io::stderr().write_all(&error)?;
        return Ok((status.code().unwrap_or(1), response));
    } else {
        let mut stderr = p.stderr.take().ok_or_else(|| {
            WorkerInvalidResponse::from("Failed to get stderr handle".to_string())
        })?;
        let mut error = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut stderr, &mut error).await?;
        let error_string = String::from_utf8(error)
            .unwrap_or_else(|_| "Failed to decode error output".to_string());
        return Err(WorkerInvalidResponse::from(error_string));
    }
}

impl PublishWorker {
    /// Create a new publish worker.
    ///
    /// # Arguments
    /// * `template_env_path` - Optional path to the template environment
    /// * `external_url` - Optional external URL for the publish operation
    /// * `differ_url` - URL of the differ service
    /// * `redis` - Optional Redis connection manager
    /// * `lock_manager` - Optional lock manager for coordinating publish operations
    ///
    /// # Returns
    /// A new PublishWorker instance
    pub async fn new(
        template_env_path: Option<PathBuf>,
        external_url: Option<url::Url>,
        differ_url: url::Url,
        redis: Option<RedisConnectionManager>,
        redis_manager: Option<Arc<janitor::redis::RedisManager>>,
        lock_manager: Option<rslock::LockManager>,
    ) -> Self {
        Self {
            template_env_path,
            external_url,
            differ_url,
            redis,
            redis_manager,
            lock_manager,
        }
    }

    /// Publish a single run in some form.
    ///
    /// # Arguments
    /// * `campaign` - The campaign name
    /// * `command` - Command that was run
    /// Publish a single run.
    ///
    /// Takes `&self` rather than `&mut self`: the body only reads
    /// from `self.template_env_path`, `self.lock_manager`, and
    /// `self.redis_manager`. Keeping the receiver immutable lets
    /// callers like `check_existing_mp` hold a `&PublishWorker` from
    /// `AppState` without needing exclusive access.
    #[allow(clippy::too_many_arguments)]
    pub async fn publish_one(
        &self,
        campaign: &str,
        codebase: &str,
        command: &str,
        target_branch_url: &url::Url,
        mode: Mode,
        role: &str,
        revision: &RevisionId,
        log_id: &str,
        unchanged_id: &str,
        derived_branch_name: &str,
        rate_limit_bucket: Option<&str>,
        vcs_manager: &dyn VcsManager,
        mut bucket_rate_limiter: Option<&mut dyn RateLimiter>,
        require_binary_diff: bool,
        allow_create_proposal: bool,
        reviewers: Option<Vec<&str>>,
        tags: Option<Vec<(String, RevisionId)>>,
        commit_message_template: Option<&str>,
        title_template: Option<&str>,
        codemod_result: &serde_json::Value,
        existing_mp_url: Option<&url::Url>,
        extra_context: Option<&serde_json::Value>,
        derived_owner: Option<&str>,
        auto_merge: Option<bool>,
    ) -> Result<PublishOneResult, PublishError> {
        let local_branch_url =
            vcs_manager.get_branch_url(codebase, &format!("{}/{}", campaign, role));

        let request = PublishOneRequest {
            campaign: campaign.to_owned(),
            command: command.to_owned(),
            codemod_result: codemod_result.clone(),
            target_branch_url: target_branch_url.clone(),
            source_branch_url: local_branch_url,
            existing_mp_url: existing_mp_url.cloned(),
            derived_branch_name: derived_branch_name.to_owned(),
            mode,
            role: role.to_owned(),
            log_id: log_id.to_owned(),
            unchanged_id: Some(unchanged_id.to_owned()),
            require_binary_diff,
            allow_create_proposal,
            external_url: self.external_url.clone(),
            differ_url: self.differ_url.clone(),
            revision_id: revision.clone(),
            reviewers: reviewers.map(|r| r.iter().map(|s| s.to_string()).collect()),
            commit_message_template: commit_message_template.map(|s| s.to_string()),
            title_template: title_template.map(|s| s.to_string()),
            extra_context: extra_context.cloned(),
            tags: tags.map(|t| t.into_iter().collect()),
            derived_owner: derived_owner.map(|s| s.to_string()),
            auto_merge,
        };

        let mut args = vec!["janitor-publish-one".to_string()];

        if let Some(template_env_path) = self.template_env_path.as_ref() {
            args.push(format!(
                "--template-env-path={}",
                template_env_path.display()
            ));
        }

        let (returncode, response) = if let Some(lock_manager) = &self.lock_manager {
            match lock_manager
                .lock(
                    format!("publish:{}", target_branch_url).as_bytes(),
                    std::time::Duration::from_secs(60),
                )
                .await
            {
                Ok(rl) => {
                    let (returncode, response) = match run_worker_process(args, request).await {
                        Ok((returncode, response)) => (returncode, response),
                        Err(e) => {
                            return Err(PublishError::Failure {
                                code: "publisher-invalid-response".to_string(),
                                description: e.to_string(),
                            });
                        }
                    };
                    lock_manager.unlock(&rl).await;
                    (returncode, response)
                }
                Err(_) => {
                    return Err(PublishError::BranchBusy(target_branch_url.clone()));
                }
            }
        } else {
            match run_worker_process(args, request).await {
                Ok((returncode, response)) => (returncode, response),
                Err(e) => {
                    return Err(PublishError::Failure {
                        code: "publisher-invalid-response".to_string(),
                        description: e.to_string(),
                    });
                }
            }
        };

        if returncode == 1 {
            let error: PublishOneError =
                serde_json::from_value(response).map_err(|e| PublishError::Failure {
                    code: "publisher-invalid-response".to_string(),
                    description: e.to_string(),
                })?;
            return Err(PublishError::Failure {
                code: error.code,
                description: error.description,
            });
        }

        if returncode == 0 {
            let result: PublishOneResult =
                serde_json::from_value(response).map_err(|e| PublishError::Failure {
                    code: "publisher-invalid-response".to_string(),
                    description: e.to_string(),
                })?;

            if result.proposal_url.is_some() && result.is_new.unwrap_or(false) {
                // Publish merge proposal event to Redis
                if let Some(redis_manager) = self.redis_manager.as_ref() {
                    let event = crate::redis::MergeProposalEvent {
                        url: result
                            .proposal_url
                            .as_ref()
                            .expect("proposal_url just checked to be Some")
                            .to_string(),
                        web_url: result.proposal_web_url.as_ref().map(|u| u.to_string()),
                        status: "open".to_string(),
                        codebase: codebase.to_string(),
                        campaign: campaign.to_string(),
                        target_branch_url: result.target_branch_url.to_string(),
                        target_branch_web_url: result
                            .target_branch_web_url
                            .as_ref()
                            .map(|u| u.to_string()),
                        timestamp: chrono::Utc::now(),
                    };

                    let publisher = crate::redis::RedisPublisher::new(redis_manager.clone());
                    if let Err(e) = publisher.publish_merge_proposal(&event).await {
                        log::warn!("Failed to publish merge proposal event to Redis: {}", e);
                    }
                }

                if let Some(bucket) = rate_limit_bucket {
                    if let Some(rate_limiter) = bucket_rate_limiter.as_mut() {
                        rate_limiter.inc(bucket);
                    }
                }
            }

            return Ok(result);
        }

        unreachable!();
    }
}

/// Check if a run is sufficient to create a merge proposal based on its value.
///
/// # Arguments
/// * `campaign_config` - The campaign configuration
/// * `run_value` - The value associated with the run
///
/// # Returns
/// `true` if the run is sufficient to create a merge proposal, `false` otherwise
pub fn run_sufficient_for_proposal(campaign_config: &Campaign, run_value: Option<i32>) -> bool {
    if let (Some(run_value), Some(threshold)) =
        (run_value, &campaign_config.merge_proposal.value_threshold)
    {
        run_value >= *threshold
    } else {
        // Assume yes, if the run doesn't have an associated value or if there is no threshold configured.
        true
    }
}

/// Compute the derived branch name to use for a published run.
///
/// * Single-branch runs use `campaign.branch_name`.
/// * Multi-branch runs append `/<role>` to disambiguate.
/// * When the codebase shares a hosting URL with another codebase,
///   append `/<codebase>` so the names don't collide on the forge.
pub async fn derived_branch_name(
    conn: &sqlx::PgPool,
    campaign_config: &Campaign,
    run: &janitor::state::Run,
    role: &str,
) -> Result<String, sqlx::Error> {
    let campaign_branch = campaign_config
        .branch_name
        .clone()
        .or_else(|| campaign_config.name.clone())
        .unwrap_or_default();

    let result_branch_count = run.result_branches.as_ref().map(|b| b.len()).unwrap_or(0);
    let base = if result_branch_count == 1 {
        campaign_branch
    } else {
        format!("{}/{}", campaign_branch, role)
    };

    let cotenants = has_cotenants(conn, &run.codebase, &run.branch_url).await?;

    if cotenants == Some(true) {
        Ok(format!("{}/{}", base, run.codebase))
    } else {
        Ok(base)
    }
}

/// Return whether `codebase` shares its branch URL with any other codebase.
///
/// * `Some(true)`: the URL is reused by a different codebase (either
///   several rows match, or the single matching row is a different name).
/// * `Some(false)`: the URL is uniquely owned by `codebase`.
/// * `None`: no rows matched, so we can't tell.
async fn has_cotenants(
    conn: &sqlx::PgPool,
    codebase: &str,
    branch_url: &str,
) -> Result<Option<bool>, sqlx::Error> {
    let url = match url::Url::parse(branch_url) {
        Ok(u) => breezyshim::urlutils::split_segment_parameters(&u)
            .0
            .to_string(),
        Err(_) => branch_url.to_string(),
    };

    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT name FROM codebase WHERE branch_url = $1 OR url = $1")
            .bind(url.trim_end_matches('/'))
            .fetch_all(conn)
            .await?;

    Ok(match rows.len() {
        0 => {
            log::warn!(
                "Unable to figure out if {} has cotenants on {}",
                codebase,
                url
            );
            None
        }
        1 => Some(rows[0].0 != codebase),
        _ => Some(true),
    })
}

/// Get the URL for a role branch.
///
/// # Arguments
/// * `url` - The base URL
/// * `remote_branch_name` - Optional name of the remote branch
///
/// # Returns
/// The URL for the role branch
pub fn role_branch_url(url: &url::Url, remote_branch_name: Option<&str>) -> url::Url {
    if let Some(remote_branch_name) = remote_branch_name {
        let parsed_url = match url.to_string().trim_end_matches('/').parse() {
            Ok(url) => url,
            Err(_) => return url.clone(), // Return original URL if parsing fails
        };
        let (base_url, mut params) = breezyshim::urlutils::split_segment_parameters(&parsed_url);

        params.insert(
            "branch".to_owned(),
            breezyshim::urlutils::escape_utf8(remote_branch_name, Some("")),
        );

        breezyshim::urlutils::join_segment_parameters(&base_url, params)
    } else {
        url.clone()
    }
}

/// Resolve redirects for a URL to get the canonical URL.
///
/// # Arguments
/// * `url` - The URL to resolve
///
/// # Returns
/// The canonical URL after following redirects, or the original URL if resolution fails
fn resolve_redirects(url: &url::Url) -> url::Url {
    // Only attempt redirect resolution for HTTP/HTTPS URLs
    if !url.scheme().starts_with("http") {
        return url.clone();
    }

    // Use a blocking HTTP client to follow redirects
    if let Ok(client) = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(5))
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        if let Ok(response) = client.head(url.as_str()).send() {
            if let Ok(final_url) = url::Url::parse(response.url().as_str()) {
                return final_url;
            }
        }
    }

    // Return original URL if redirect resolution fails
    url.clone()
}

/// Check if two branch URLs refer to the same branch.
///
/// # Arguments
/// * `url_a` - The first branch URL
/// * `url_b` - The second branch URL
///
/// # Returns
/// `true` if the branches match, `false` otherwise
pub fn branches_match(url_a: Option<&url::Url>, url_b: Option<&url::Url>) -> bool {
    use silver_platter::vcs::{open_branch, BranchOpenError};
    if url_a == url_b {
        return true;
    }
    let (url_a, url_b) = match (url_a, url_b) {
        (Some(a), Some(b)) => (a, b),
        _ => return false,
    };
    let parsed_url_a = match url_a.to_string().trim_end_matches('/').parse() {
        Ok(url) => url,
        Err(_) => return false,
    };
    let (base_url_a, _params_a) = breezyshim::urlutils::split_segment_parameters(&parsed_url_a);

    let parsed_url_b = match url_b.to_string().trim_end_matches('/').parse() {
        Ok(url) => url,
        Err(_) => return false,
    };
    let (base_url_b, _params_b) = breezyshim::urlutils::split_segment_parameters(&parsed_url_b);
    // Support following redirects by normalizing URLs
    let normalized_url_a = resolve_redirects(&base_url_a);
    let normalized_url_b = resolve_redirects(&base_url_b);

    if normalized_url_a.to_string().trim_end_matches('/')
        != normalized_url_b.to_string().trim_end_matches('/')
    {
        return false;
    }
    let branch_a = match open_branch(url_a, None, None, None) {
        Ok(branch) => branch,
        Err(BranchOpenError::Missing { .. }) => return false,
        Err(e) => panic!("Unexpected error: {:?}", e),
    };
    let branch_b = match open_branch(url_b, None, None, None) {
        Ok(branch) => branch,
        Err(BranchOpenError::Missing { .. }) => return false,
        Err(e) => panic!("Unexpected error: {:?}", e),
    };
    branch_a.name() == branch_b.name()
}

/// Get the URL for a user who merged a branch.
///
/// # Arguments
/// * `url` - The branch URL
/// * `user` - The username
///
/// # Returns
/// The user's URL, or None if not available
pub fn get_merged_by_user_url(url: &url::Url, user: &str) -> Result<Option<url::Url>, BrzError> {
    let hostname = if let Some(host) = url.host_str() {
        host
    } else {
        return Ok(None);
    };

    let forge = match breezyshim::forge::get_forge_by_hostname(hostname) {
        Ok(forge) => forge,
        Err(BrzError::UnsupportedForge(..)) => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(Some(forge.get_user_url(user)?))
}

/// Process the publish queue in a loop.
///
/// # Arguments
/// * `state` - The application state
/// * `interval` - The interval at which to process the queue
/// * `auto_publish` - Whether to automatically publish changes
/// * `push_limit` - Optional limit on the number of pushes
/// * `modify_mp_limit` - Optional limit on the number of merge proposals to modify
/// * `require_binary_diff` - Whether to require binary diffs
pub async fn process_queue_loop(
    state: Arc<AppState>,
    interval: chrono::Duration,
    auto_publish: bool,
    push_limit: Option<usize>,
    modify_mp_limit: Option<i32>,
    require_binary_diff: bool,
) {
    queue::process_queue_loop(
        state,
        interval,
        auto_publish,
        push_limit,
        modify_mp_limit,
        require_binary_diff,
    )
    .await;
}

/// Publish all pending ready changes.
///
/// # Arguments
/// * `state` - The application state
/// * `push_limit` - Optional limit on the number of pushes
/// * `require_binary_diff` - Whether to require binary diffs
///
/// # Returns
/// Ok(()) if successful, or a PublishError
pub async fn publish_pending_ready(
    state: Arc<AppState>,
    push_limit: Option<usize>,
    require_binary_diff: bool,
) -> Result<(), PublishError> {
    queue::publish_pending_ready(state, push_limit, require_binary_diff).await
}

/// Refresh the counts of merge proposals per bucket.
///
/// # Arguments
/// * `state` - The application state
///
/// # Returns
/// Ok(()) if successful, or a sqlx::Error
pub async fn refresh_bucket_mp_counts(state: Arc<AppState>) -> Result<(), sqlx::Error> {
    let mut per_bucket: HashMap<janitor::publish::MergeProposalStatus, HashMap<String, usize>> =
        HashMap::new();

    let rows = sqlx::query_as::<_, (String, String, i64)>(
        r#"
        SELECT
        rate_limit_bucket AS rate_limit_bucket,
        status AS status,
        count(*) as c
        FROM merge_proposal
        GROUP BY 1, 2
        "#,
    )
    .fetch_all(&state.conn)
    .await?;

    for row in rows {
        if let Ok(status) = row.1.parse() {
            per_bucket
                .entry(status)
                .or_default()
                .insert(row.0, row.2 as usize);
        } else {
            log::warn!("Invalid merge proposal status in database: {}", row.1);
        }
    }
    if let Ok(mut limiter) = state.bucket_rate_limiter.lock() {
        limiter.set_mps_per_bucket(&per_bucket);
    } else {
        log::error!("Failed to acquire bucket rate limiter lock");
    }
    Ok(())
}

/// Listen to the runner for new changes to publish.
///
/// # Arguments
/// * `state` - The application state
/// * `shutdown_rx` - Channel for receiving shutdown signals
pub async fn listen_to_runner(
    state: Arc<AppState>,
    shutdown_rx: tokio::sync::mpsc::Receiver<()>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let redis_manager = state
        .redis_manager
        .clone()
        .ok_or_else(|| "Redis manager not configured")?;
    let subscriber = redis::RedisSubscriber::new(redis_manager, shutdown_rx);
    subscriber.listen_to_runner(state).await?;
    Ok(())
}

/// Get the status of a merge proposal.
///
/// # Arguments
/// * `mp` - The merge proposal to check
///
/// # Returns
/// String representing the status: "merged", "closed", or "open"
pub async fn get_mp_status(mp: &breezyshim::forge::MergeProposal) -> Result<String, BrzError> {
    let is_merged = tokio::task::spawn_blocking({
        let mp = mp.clone();
        move || mp.is_merged()
    })
    .await
    .map_err(|_| BrzError::Other(PyErr::new::<PyRuntimeError, _>("Task join error")))??;

    if is_merged {
        return Ok("merged".to_string());
    }

    let is_closed = tokio::task::spawn_blocking({
        let mp = mp.clone();
        move || mp.is_closed()
    })
    .await
    .map_err(|_| BrzError::Other(PyErr::new::<PyRuntimeError, _>("Task join error")))??;

    if is_closed {
        Ok("closed".to_string())
    } else {
        Ok("open".to_string())
    }
}

/// Abandon a merge proposal by updating its status and closing it.
///
/// # Arguments
/// * `proposal_info_manager` - Manager for proposal information
/// * `mp` - The merge proposal to abandon
/// * `revision` - The revision ID
/// * `codebase` - Optional codebase name
/// * `target_branch_url` - URL of the target branch
/// * `campaign` - Optional campaign name
/// * `can_be_merged` - Whether the proposal can be merged
/// * `rate_limit_bucket` - Optional rate limit bucket
/// * `comment` - Optional comment to post
///
/// # Returns
/// Ok(()) if successful, or a BrzError
pub async fn abandon_mp(
    proposal_info_manager: &mut proposal_info::ProposalInfoManager,
    mp: &breezyshim::forge::MergeProposal,
    revision: &RevisionId,
    codebase: Option<&str>,
    target_branch_url: &str,
    campaign: Option<&str>,
    can_be_merged: Option<bool>,
    rate_limit_bucket: Option<&str>,
    comment: Option<&str>,
) -> Result<(), BrzError> {
    let mp_url = mp.url()?;

    if let Some(comment_text) = comment {
        log::info!("{}: {}", mp_url, comment_text);
    }

    // Update proposal info in database
    proposal_info_manager
        .update_proposal_info(
            mp,
            janitor::publish::MergeProposalStatus::Abandoned,
            Some(revision),
            codebase,
            &url::Url::parse(target_branch_url).map_err(|e| {
                BrzError::Other(PyErr::new::<PyRuntimeError, _>(format!(
                    "URL parse error: {}",
                    e
                )))
            })?,
            campaign.unwrap_or(""),
            can_be_merged,
            rate_limit_bucket,
        )
        .await
        .map_err(|e| {
            BrzError::Other(PyErr::new::<PyRuntimeError, _>(format!(
                "Database error: {}",
                e
            )))
        })?;

    // Post comment if provided. PermissionDenied is logged but non-fatal.
    if let Some(comment_text) = comment {
        let post_result = tokio::task::spawn_blocking({
            let mp = mp.clone();
            let comment = comment_text.to_string();
            move || mp.post_comment(&comment)
        })
        .await
        .map_err(|_| BrzError::Other(PyErr::new::<PyRuntimeError, _>("Task join error")))?;
        match post_result {
            Ok(()) => {}
            Err(BrzError::PermissionDenied(_, msg)) => {
                log::warn!(
                    "Permission denied posting comment to {}: {}",
                    mp_url,
                    msg.unwrap_or_default()
                );
            }
            Err(e) => log::warn!("Failed to post comment to {}: {}", mp_url, e),
        }
    }

    // Close the merge proposal. PermissionDenied is reraised - the
    // Python caller treats it as the abandon/close having failed.
    let close_result = tokio::task::spawn_blocking({
        let mp = mp.clone();
        move || mp.close()
    })
    .await
    .map_err(|_| BrzError::Other(PyErr::new::<PyRuntimeError, _>("Task join error")))?;
    match close_result {
        Ok(()) => Ok(()),
        Err(BrzError::PermissionDenied(path, msg)) => {
            log::warn!(
                "Permission denied closing merge request {}: {}",
                mp_url,
                msg.as_deref().unwrap_or("")
            );
            Err(BrzError::PermissionDenied(path, msg))
        }
        Err(e) => Err(e),
    }
}

/// Close an applied merge proposal by updating its status and closing it.
///
/// # Arguments
/// * `proposal_info_manager` - Manager for proposal information
/// * `mp` - The merge proposal to close
/// * `revision` - The revision ID
/// * `codebase` - Optional codebase name
/// * `target_branch_url` - URL of the target branch
/// * `campaign` - Optional campaign name
/// * `can_be_merged` - Whether the proposal can be merged
/// * `rate_limit_bucket` - Optional rate limit bucket
/// * `comment` - Optional comment to post
///
/// # Returns
/// Ok(()) if successful, or a BrzError
pub async fn close_applied_mp(
    proposal_info_manager: &mut proposal_info::ProposalInfoManager,
    mp: &breezyshim::forge::MergeProposal,
    revision: &RevisionId,
    codebase: Option<&str>,
    target_branch_url: &str,
    campaign: Option<&str>,
    can_be_merged: Option<bool>,
    rate_limit_bucket: Option<&str>,
    comment: Option<&str>,
) -> Result<(), BrzError> {
    let mp_url = mp.url()?;

    // Update proposal info in database with "applied" status
    proposal_info_manager
        .update_proposal_info(
            mp,
            janitor::publish::MergeProposalStatus::Applied,
            Some(revision),
            codebase,
            &url::Url::parse(target_branch_url).map_err(|e| {
                BrzError::Other(PyErr::new::<PyRuntimeError, _>(format!(
                    "URL parse error: {}",
                    e
                )))
            })?,
            campaign.unwrap_or(""),
            can_be_merged,
            rate_limit_bucket,
        )
        .await
        .map_err(|e| {
            BrzError::Other(PyErr::new::<PyRuntimeError, _>(format!(
                "Database error: {}",
                e
            )))
        })?;

    // Post comment if provided. PermissionDenied is logged but non-fatal.
    if let Some(comment_text) = comment {
        let post_result = tokio::task::spawn_blocking({
            let mp = mp.clone();
            let comment = comment_text.to_string();
            move || mp.post_comment(&comment)
        })
        .await
        .map_err(|_| BrzError::Other(PyErr::new::<PyRuntimeError, _>("Task join error")))?;
        match post_result {
            Ok(()) => {}
            Err(BrzError::PermissionDenied(_, msg)) => {
                log::warn!(
                    "Permission denied posting comment to {}: {}",
                    mp_url,
                    msg.unwrap_or_default()
                );
            }
            Err(e) => log::warn!("Failed to post comment to {}: {}", mp_url, e),
        }
    }

    // Close the merge proposal. PermissionDenied is reraised - the
    // Python caller treats it as the abandon/close having failed.
    let close_result = tokio::task::spawn_blocking({
        let mp = mp.clone();
        move || mp.close()
    })
    .await
    .map_err(|_| BrzError::Other(PyErr::new::<PyRuntimeError, _>("Task join error")))?;
    match close_result {
        Ok(()) => Ok(()),
        Err(BrzError::PermissionDenied(path, msg)) => {
            log::warn!(
                "Permission denied closing merge request {}: {}",
                mp_url,
                msg.as_deref().unwrap_or("")
            );
            Err(BrzError::PermissionDenied(path, msg))
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn test_calculate_next_try_time_zero_attempts() {
        let finish_time = Utc::now();
        let next_try_time = calculate_next_try_time(finish_time, 0);
        assert_eq!(finish_time, next_try_time);
    }

    #[test]
    fn test_calculate_next_try_time_exponential_backoff() {
        let finish_time = Utc::now();
        assert_eq!(
            calculate_next_try_time(finish_time, 1),
            finish_time + chrono::Duration::hours(2)
        );
        assert_eq!(
            calculate_next_try_time(finish_time, 2),
            finish_time + chrono::Duration::hours(4)
        );
        assert_eq!(
            calculate_next_try_time(finish_time, 3),
            finish_time + chrono::Duration::hours(8)
        );
        assert_eq!(
            calculate_next_try_time(finish_time, 4),
            finish_time + chrono::Duration::hours(16)
        );
    }

    #[test]
    fn test_calculate_next_try_time_max_7_days() {
        let finish_time = Utc::now();
        // 2^10 = 1024 hours, but capped at 7*24 = 168 hours
        let next_try_time = calculate_next_try_time(finish_time, 10);
        assert_eq!(next_try_time, finish_time + chrono::Duration::days(7));
        // Even higher attempts should also be capped
        let next_try_time = calculate_next_try_time(finish_time, 20);
        assert_eq!(next_try_time, finish_time + chrono::Duration::days(7));
    }

    #[test]
    fn test_calculate_next_try_time_monotonically_increasing() {
        let finish_time = Utc::now();
        let mut prev = finish_time;
        for i in 0..=10 {
            let next = calculate_next_try_time(finish_time, i);
            assert!(
                next >= prev,
                "attempt {} should be >= attempt {}",
                i,
                i.saturating_sub(1)
            );
            prev = next;
        }
    }

    #[test]
    fn test_publish_error_failure_code() {
        let err = PublishError::Failure {
            code: "merge-conflict".to_string(),
            description: "Could not merge".to_string(),
        };
        assert_eq!(err.code(), "merge-conflict");
        assert_eq!(err.description(), "Could not merge");
    }

    #[test]
    fn test_publish_error_nothing_to_do() {
        let err = PublishError::NothingToDo("No changes detected".to_string());
        assert_eq!(err.code(), "nothing-to-do");
        assert_eq!(err.description(), "No changes detected");
    }

    #[test]
    fn test_publish_error_branch_busy() {
        let url = url::Url::parse("https://github.com/foo/bar").unwrap();
        let err = PublishError::BranchBusy(url);
        assert_eq!(err.code(), "branch-busy");
        assert_eq!(err.description(), "Branch is busy");
    }

    #[test]
    fn test_publish_error_display() {
        let err = PublishError::Failure {
            code: "merge-conflict".to_string(),
            description: "Could not merge".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "PublishError::Failure: merge-conflict: Could not merge"
        );

        let err = PublishError::NothingToDo("No changes".to_string());
        assert_eq!(
            err.to_string(),
            "PublishError::PublishNothingToDo: No changes"
        );

        let url = url::Url::parse("https://github.com/foo/bar").unwrap();
        let err = PublishError::BranchBusy(url);
        assert_eq!(
            err.to_string(),
            "PublishError::BranchBusy: Branch is busy: https://github.com/foo/bar"
        );
    }

    #[test]
    fn test_publish_error_serialize() {
        let err = PublishError::Failure {
            code: "merge-conflict".to_string(),
            description: "Could not merge".to_string(),
        };
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["code"], "merge-conflict");
        assert_eq!(json["description"], "Could not merge");

        let err = PublishError::NothingToDo("No changes".to_string());
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["code"], "nothing-to-do");
        assert_eq!(json["description"], "No changes");

        let url = url::Url::parse("https://github.com/foo/bar").unwrap();
        let err = PublishError::BranchBusy(url);
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["code"], "branch-busy");
        assert_eq!(
            json["description"],
            "Branch is busy: https://github.com/foo/bar"
        );
    }

    #[test]
    fn test_publish_one_request_serde() {
        let request = PublishOneRequest {
            campaign: "lintian-fixes".to_string(),
            target_branch_url: url::Url::parse("https://salsa.debian.org/foo/bar").unwrap(),
            role: "main".to_string(),
            log_id: "log-123".to_string(),
            reviewers: Some(vec!["alice".to_string()]),
            revision_id: breezyshim::RevisionId::from(b"rev-1".to_vec()),
            unchanged_id: Some("unchanged-456".to_string()),
            require_binary_diff: true,
            differ_url: url::Url::parse("https://differ.example.com").unwrap(),
            derived_branch_name: "lintian-fixes".to_string(),
            tags: None,
            allow_create_proposal: true,
            source_branch_url: url::Url::parse("https://salsa.debian.org/fork/bar").unwrap(),
            codemod_result: serde_json::json!({"applied": 3}),
            commit_message_template: Some("Fix lintian issues".to_string()),
            title_template: Some("Fix lintian issues in {{source}}".to_string()),
            existing_mp_url: None,
            extra_context: None,
            mode: Mode::Propose,
            command: "lintian-brush".to_string(),
            external_url: None,
            derived_owner: None,
            auto_merge: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        let roundtripped: PublishOneRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped.campaign, "lintian-fixes");
        assert_eq!(roundtripped.mode, Mode::Propose);
        assert_eq!(roundtripped.require_binary_diff, true);
        assert_eq!(roundtripped.reviewers, Some(vec!["alice".to_string()]));
    }

    #[test]
    fn test_publish_one_result_serde() {
        let result = PublishOneResult {
            proposal_url: Some(url::Url::parse("https://github.com/foo/bar/pull/1").unwrap()),
            proposal_web_url: Some(url::Url::parse("https://github.com/foo/bar/pull/1").unwrap()),
            is_new: Some(true),
            branch_name: "lintian-fixes".to_string(),
            target_branch_url: url::Url::parse("https://github.com/foo/bar").unwrap(),
            target_branch_web_url: None,
            mode: Mode::Propose,
        };
        let json = serde_json::to_string(&result).unwrap();
        let roundtripped: PublishOneResult = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped.branch_name, "lintian-fixes");
        assert_eq!(roundtripped.is_new, Some(true));
        assert_eq!(roundtripped.mode, Mode::Propose);
        assert!(roundtripped.proposal_url.is_some());
    }

    #[test]
    fn test_publish_one_error_serde() {
        let err = PublishOneError {
            code: "merge-conflict".to_string(),
            description: "Could not merge".to_string(),
        };
        let json = serde_json::to_string(&err).unwrap();
        let roundtripped: PublishOneError = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped.code, "merge-conflict");
        assert_eq!(roundtripped.description, "Could not merge");
    }

    #[test]
    fn test_check_mp_error_display() {
        let err = CheckMpError::NoRunForMergeProposal(
            url::Url::parse("https://github.com/foo/bar/pull/1").unwrap(),
        );
        assert_eq!(
            err.to_string(),
            "No run for merge proposal: https://github.com/foo/bar/pull/1"
        );

        let err = CheckMpError::BranchRateLimited {
            retry_after: Some(chrono::Duration::minutes(30)),
        };
        assert_eq!(err.to_string(), "Branch is rate limited");

        let err = CheckMpError::UnexpectedHttpStatus;
        assert_eq!(err.to_string(), "Unexpected HTTP status");

        let err = CheckMpError::ForgeLoginRequired;
        assert_eq!(err.to_string(), "Forge login required");
    }

    #[test]
    fn test_debdiff_error_display() {
        let err = DebdiffError::MissingRun("run-123".to_string());
        assert_eq!(err.to_string(), "Missing run: run-123");

        let err = DebdiffError::NoUnchangedRun;
        assert_eq!(err.to_string(), "No unchanged run to compare against yet");

        let err = DebdiffError::Unavailable("service down".to_string());
        assert_eq!(err.to_string(), "Unavailable: service down");
    }

    #[test]
    fn test_get_debdiff_without_unchanged_id_skips_the_request() {
        // The differ has no route for a missing control run, so get_debdiff
        // must report it directly rather than making a request that can
        // only ever come back as an unrelated 404. A URL that refuses
        // connections proves no request was attempted: any other error
        // variant here would mean one was. This is also the normal state
        // for a codebase's first-ever run, not a differ failure, so it
        // gets its own variant rather than an empty MissingRun id.
        let differ_url = url::Url::parse("http://127.0.0.1:1").unwrap();
        let err = get_debdiff(&differ_url, None, "log-123").unwrap_err();
        assert!(matches!(err, DebdiffError::NoUnchangedRun));
        assert_eq!(err.to_string(), "No unchanged run to compare against yet");
    }

    #[test]
    fn test_worker_invalid_response_display() {
        let err = WorkerInvalidResponse::WorkerError("process crashed".to_string());
        assert_eq!(err.to_string(), "Worker error: process crashed");

        let err = WorkerInvalidResponse::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "file not found",
        ));
        assert_eq!(err.to_string(), "IO error: file not found");
    }

    #[test]
    fn test_run_sufficient_for_proposal_no_threshold() {
        // When there's no threshold configured, should always be sufficient
        let config_text = r#"name: "test" command: "test-cmd""#;
        let config: janitor::config::Campaign =
            protobuf::text_format::parse_from_str(config_text).unwrap();
        assert!(run_sufficient_for_proposal(&config, Some(0)));
        assert!(run_sufficient_for_proposal(&config, Some(100)));
        assert!(run_sufficient_for_proposal(&config, None));
    }

    #[test]
    fn test_run_sufficient_for_proposal_with_threshold() {
        let config_text =
            r#"name: "test" command: "test-cmd" merge_proposal { value_threshold: 50 }"#;
        let config: janitor::config::Campaign =
            protobuf::text_format::parse_from_str(config_text).unwrap();
        assert!(run_sufficient_for_proposal(&config, Some(50)));
        assert!(run_sufficient_for_proposal(&config, Some(100)));
        assert!(!run_sufficient_for_proposal(&config, Some(10)));
        // None value with threshold should still be true (assume yes)
        assert!(run_sufficient_for_proposal(&config, None));
    }

    #[test]
    fn test_role_branch_url_no_branch() {
        let url = url::Url::parse("https://example.com/repo").unwrap();
        let result = role_branch_url(&url, None);
        assert_eq!(result, url);
    }

    // --- consider_publish_run pure helper tests ---

    fn ub(role: &str, mode: Option<&str>) -> crate::state::UnpublishedBranch {
        crate::state::UnpublishedBranch {
            role: role.to_string(),
            remote_name: None,
            base_revision: None,
            revision: None,
            publish_mode: mode.map(str::to_string),
            max_frequency_days: None,
            name: None,
        }
    }

    #[test]
    fn test_wants_push_mode_empty() {
        assert!(!wants_push_mode(&[]));
    }

    #[test]
    fn test_wants_push_mode_only_propose() {
        assert!(!wants_push_mode(&[ub("main", Some("propose"))]));
    }

    #[test]
    fn test_wants_push_mode_push() {
        assert!(wants_push_mode(&[ub("main", Some("push"))]));
    }

    #[test]
    fn test_wants_push_mode_attempt_push() {
        assert!(wants_push_mode(&[ub("main", Some("attempt-push"))]));
    }

    #[test]
    fn test_wants_push_mode_mixed_picks_up_push() {
        assert!(wants_push_mode(&[
            ub("aux", Some("propose")),
            ub("main", Some("push")),
        ]));
    }

    #[test]
    fn test_wants_push_mode_none_mode_is_ignored() {
        assert!(!wants_push_mode(&[ub("main", None)]));
    }

    #[test]
    fn test_should_skip_for_push_limit_no_limit() {
        assert!(!should_skip_for_push_limit(true, None));
        assert!(!should_skip_for_push_limit(false, None));
    }

    #[test]
    fn test_should_skip_for_push_limit_zero_with_push() {
        assert!(should_skip_for_push_limit(true, Some(0)));
    }

    #[test]
    fn test_should_skip_for_push_limit_zero_without_push() {
        // No branch wants push -> the budget doesn't matter.
        assert!(!should_skip_for_push_limit(false, Some(0)));
    }

    #[test]
    fn test_should_skip_for_push_limit_positive() {
        // Positive limit means budget remaining, never skip.
        assert!(!should_skip_for_push_limit(true, Some(5)));
        assert!(!should_skip_for_push_limit(false, Some(5)));
    }

    #[test]
    fn test_previous_mp_blocks_publish_empty() {
        assert!(!previous_mp_blocks_publish(&[]));
    }

    #[test]
    fn test_previous_mp_blocks_publish_rejected() {
        let statuses = vec![("main".to_string(), "rejected".to_string())];
        assert!(previous_mp_blocks_publish(&statuses));
    }

    #[test]
    fn test_previous_mp_blocks_publish_closed() {
        let statuses = vec![("main".to_string(), "closed".to_string())];
        assert!(previous_mp_blocks_publish(&statuses));
    }

    #[test]
    fn test_previous_mp_blocks_publish_open_does_not_block() {
        let statuses = vec![
            ("main".to_string(), "open".to_string()),
            ("aux".to_string(), "merged".to_string()),
        ];
        assert!(!previous_mp_blocks_publish(&statuses));
    }

    #[test]
    fn test_previous_mp_blocks_publish_any_blocking_wins() {
        // If one MP was rejected and another is still open, we
        // should still skip - Python checks `any`.
        let statuses = vec![
            ("main".to_string(), "open".to_string()),
            ("aux".to_string(), "rejected".to_string()),
        ];
        assert!(previous_mp_blocks_publish(&statuses));
    }

    #[test]
    fn test_resolve_target_branch_url_explicit() {
        assert_eq!(
            resolve_target_branch_url(Some("https://target/r"), "https://run/r"),
            "https://target/r"
        );
    }

    #[test]
    fn test_resolve_target_branch_url_falls_back_when_none() {
        assert_eq!(
            resolve_target_branch_url(None, "https://run/r"),
            "https://run/r"
        );
    }

    #[test]
    fn test_resolve_target_branch_url_falls_back_when_empty() {
        // Empty string is treated like missing.
        assert_eq!(
            resolve_target_branch_url(Some(""), "https://run/r"),
            "https://run/r"
        );
    }

    #[test]
    fn test_should_skip_main_after_aux_failure_no_aux() {
        let modes: HashMap<String, Option<String>> = HashMap::new();
        assert!(!should_skip_main_after_aux_failure("main", &modes));
    }

    #[test]
    fn test_should_skip_main_after_aux_failure_aux_succeeded() {
        let mut modes: HashMap<String, Option<String>> = HashMap::new();
        modes.insert("aux".to_string(), Some("push".to_string()));
        assert!(!should_skip_main_after_aux_failure("main", &modes));
    }

    #[test]
    fn test_should_skip_main_after_aux_failure_aux_failed() {
        let mut modes: HashMap<String, Option<String>> = HashMap::new();
        modes.insert("aux".to_string(), None);
        assert!(should_skip_main_after_aux_failure("main", &modes));
    }

    #[test]
    fn test_should_skip_main_after_aux_failure_role_is_aux() {
        // The guard is main-only: an aux role iterating after
        // another failed aux is fine.
        let mut modes: HashMap<String, Option<String>> = HashMap::new();
        modes.insert("aux1".to_string(), None);
        assert!(!should_skip_main_after_aux_failure("aux2", &modes));
    }

    // --- classify_publish_failure_action tests ---

    #[test]
    fn test_classify_merge_conflict_reschedules_regular() {
        let action = classify_publish_failure_action("merge-conflict", "success", None);
        match action {
            PublishFailureAction::RescheduleRegular {
                description,
                requester,
            } => {
                assert!(description.is_none());
                assert!(requester.contains("merge conflict"));
            }
            other => panic!("expected RescheduleRegular, got {:?}", other),
        }
    }

    #[test]
    fn test_classify_diverged_branches_reschedules_regular() {
        let action = classify_publish_failure_action("diverged-branches", "success", None);
        match action {
            PublishFailureAction::RescheduleRegular {
                description,
                requester,
            } => {
                assert!(description.is_none());
                assert!(requester.contains("diverged"));
            }
            other => panic!("expected RescheduleRegular, got {:?}", other),
        }
    }

    #[test]
    fn test_classify_missing_build_diff_self_success_reschedules_refresh() {
        let action = classify_publish_failure_action("missing-build-diff-self", "success", None);
        match action {
            PublishFailureAction::RescheduleRegularRefresh {
                description,
                requester,
            } => {
                assert_eq!(description, "Missing build artifacts, rescheduling");
                assert!(requester.contains("self"));
            }
            other => panic!("expected RescheduleRegularRefresh, got {:?}", other),
        }
    }

    #[test]
    fn test_classify_missing_build_diff_self_failed_only_rewrites() {
        // When the run itself isn't actually success, the right
        // action is to rewrite the description, not reschedule.
        let action = classify_publish_failure_action("missing-build-diff-self", "failure", None);
        match action {
            PublishFailureAction::RewriteDescription { description } => {
                assert!(description.contains("not actually successful"));
            }
            other => panic!("expected RewriteDescription, got {:?}", other),
        }
    }

    #[test]
    fn test_classify_missing_build_diff_control_failed_unchanged_only_rewrites() {
        // Control run for the same upstream revision exists but
        // failed -> just rewrite, don't reschedule.
        let action = classify_publish_failure_action(
            "missing-build-diff-control",
            "success",
            Some(("failure", "rev-abc")),
        );
        match action {
            PublishFailureAction::RewriteDescription { description } => {
                assert!(description.contains("last control run failed"));
                assert!(description.contains("failure"));
            }
            other => panic!("expected RewriteDescription, got {:?}", other),
        }
    }

    #[test]
    fn test_classify_missing_build_diff_control_success_unchanged_reschedules_control() {
        // Control run exists and succeeded -> reschedule with refresh.
        let action = classify_publish_failure_action(
            "missing-build-diff-control",
            "success",
            Some(("success", "rev-abc")),
        );
        match action {
            PublishFailureAction::RescheduleControl {
                description,
                refresh,
                explicit_revision,
                requester,
            } => {
                assert!(description.contains("Rescheduling"));
                assert!(refresh);
                assert_eq!(explicit_revision, Some("rev-abc".to_string()));
                assert!(requester.contains("control"));
            }
            other => panic!("expected RescheduleControl, got {:?}", other),
        }
    }

    #[test]
    fn test_classify_missing_build_diff_control_no_unchanged_requests_control() {
        // No control run yet -> schedule one without refresh, using
        // the run's own main_branch_revision (signaled by
        // explicit_revision: None).
        let action = classify_publish_failure_action("missing-build-diff-control", "success", None);
        match action {
            PublishFailureAction::RescheduleControl {
                description,
                refresh,
                explicit_revision,
                requester,
            } => {
                assert!(description.contains("requesting control run"));
                assert!(!refresh);
                assert_eq!(explicit_revision, None);
                assert!(requester.contains("control run"));
            }
            other => panic!("expected RescheduleControl, got {:?}", other),
        }
    }

    #[test]
    fn test_classify_unknown_code_is_no_action() {
        assert_eq!(
            classify_publish_failure_action("some-other-error", "success", None),
            PublishFailureAction::NoAction
        );
        assert_eq!(
            classify_publish_failure_action("", "success", None),
            PublishFailureAction::NoAction
        );
    }

    // --- change_set_state_allows_publish ---

    #[test]
    fn test_change_set_state_allows_publish_publishing() {
        assert!(change_set_state_allows_publish("publishing"));
    }

    #[test]
    fn test_change_set_state_allows_publish_ready() {
        assert!(change_set_state_allows_publish("ready"));
    }

    #[test]
    fn test_change_set_state_allows_publish_other_states_block() {
        // Any state other than publishing/ready should block.
        assert!(!change_set_state_allows_publish("done"));
        assert!(!change_set_state_allows_publish("working"));
        assert!(!change_set_state_allows_publish("draft"));
        assert!(!change_set_state_allows_publish(""));
        assert!(!change_set_state_allows_publish("PUBLISHING")); // case-sensitive
    }

    // --- rate_limit_remaining ---

    #[test]
    fn test_rate_limit_remaining_both_known() {
        assert_eq!(rate_limit_remaining(Some(3), Some(10)), Some(7));
        assert_eq!(rate_limit_remaining(Some(0), Some(10)), Some(10));
        assert_eq!(rate_limit_remaining(Some(10), Some(10)), Some(0));
    }

    #[test]
    fn test_rate_limit_remaining_current_missing() {
        assert_eq!(rate_limit_remaining(None, Some(10)), None);
    }

    #[test]
    fn test_rate_limit_remaining_max_missing() {
        assert_eq!(rate_limit_remaining(Some(3), None), None);
    }

    #[test]
    fn test_rate_limit_remaining_both_missing() {
        assert_eq!(rate_limit_remaining(None, None), None);
    }

    #[test]
    fn test_rate_limit_remaining_saturates_when_current_exceeds_max() {
        // A slow race could let current_open exceed max_open
        // momentarily. Don't underflow usize - clamp at 0.
        assert_eq!(rate_limit_remaining(Some(15), Some(10)), Some(0));
    }

    // Note: The new functions (get_mp_status, abandon_mp, close_applied_mp) require
    // actual MergeProposal instances from breezyshim, which cannot be easily mocked
    // in unit tests. These functions should be tested with integration tests that
    // have access to real forge connections and database instances.

    // #[test]
    // fn test_merge_proposal_status_functions_exist() {
    //     // This test simply verifies the functions can be referenced and have the correct signatures
    //     // Actual testing requires integration tests with forge connections
    //
    //     // Verify function signatures by creating function pointers
    //     // NOTE: These are async functions, so we can't create simple function pointers
    //     // This test is disabled as it's not meaningful for async functions
    //
    //     // If we get here, the functions exist with the expected signatures
    //     assert!(true);
    // }

    // --- check_existing_mp freshness gate ---

    /// No persisted record -> can't skip; we have nothing to trust.
    #[test]
    fn test_check_existing_mp_should_skip_no_last_scanned() {
        let now = chrono::Utc::now();
        assert!(!check_existing_mp_should_skip_per_mp_fetch(
            breezyshim::forge::MergeProposalStatus::Open,
            "open",
            None,
            now,
            chrono::Duration::hours(24),
        ));
    }

    /// Recent scan + matching status -> skip. The fast path that
    /// keeps the cycle sub-linear in the number of MPs we own.
    #[test]
    fn test_check_existing_mp_should_skip_recent_matching_open() {
        let now = chrono::Utc::now();
        let last = now - chrono::Duration::hours(1);
        assert!(check_existing_mp_should_skip_per_mp_fetch(
            breezyshim::forge::MergeProposalStatus::Open,
            "open",
            Some(last),
            now,
            chrono::Duration::hours(24),
        ));
    }

    /// Recent scan but the forge says the MP is now Merged while
    /// our DB still has it as `open` -> can't skip: we need the
    /// per-MP fetch so we update the row + record the merge.
    #[test]
    fn test_check_existing_mp_should_not_skip_when_status_changed() {
        let now = chrono::Utc::now();
        let last = now - chrono::Duration::hours(1);
        assert!(!check_existing_mp_should_skip_per_mp_fetch(
            breezyshim::forge::MergeProposalStatus::Merged,
            "open",
            Some(last),
            now,
            chrono::Duration::hours(24),
        ));
    }

    /// Persisted scan older than the threshold -> can't skip,
    /// re-scan to refresh the local view of forge state.
    #[test]
    fn test_check_existing_mp_should_not_skip_stale() {
        let now = chrono::Utc::now();
        let last = now - chrono::Duration::days(3);
        assert!(!check_existing_mp_should_skip_per_mp_fetch(
            breezyshim::forge::MergeProposalStatus::Open,
            "open",
            Some(last),
            now,
            chrono::Duration::hours(24),
        ));
    }

    /// Carry-forward variants (`abandoned|applied|rejected`) are
    /// the publisher's interpretation of a `closed` forge status,
    /// so a recent scan with one of those + forge=Closed is still
    /// a match. Without this branch every `applied` MP would be
    /// re-scanned every cycle for no reason.
    #[test]
    fn test_check_existing_mp_should_skip_closed_family_carry_forward() {
        let now = chrono::Utc::now();
        let last = now - chrono::Duration::hours(1);
        for persisted in &["closed", "abandoned", "applied", "rejected"] {
            assert!(
                check_existing_mp_should_skip_per_mp_fetch(
                    breezyshim::forge::MergeProposalStatus::Closed,
                    persisted,
                    Some(last),
                    now,
                    chrono::Duration::hours(24),
                ),
                "persisted={} should match forge Closed",
                persisted
            );
        }
    }

    /// A terminal status (merged/closed) that agrees with the
    /// persisted record is skipped regardless of `last_scanned` age -
    /// a merged MP can't un-merge, and the enumeration already
    /// re-confirms the status. Without this the ~17k merged/closed MPs
    /// we own re-run the full per-MP fetch every pass once their
    /// imported timestamps age out, and a scan never completes.
    #[test]
    fn test_check_existing_mp_should_skip_terminal_regardless_of_age() {
        let now = chrono::Utc::now();
        let stale = now - chrono::Duration::days(90);
        // Stale merged.
        assert!(check_existing_mp_should_skip_per_mp_fetch(
            breezyshim::forge::MergeProposalStatus::Merged,
            "merged",
            Some(stale),
            now,
            chrono::Duration::hours(24),
        ));
        // Merged with no recorded scan at all (historical import).
        assert!(check_existing_mp_should_skip_per_mp_fetch(
            breezyshim::forge::MergeProposalStatus::Merged,
            "merged",
            None,
            now,
            chrono::Duration::hours(24),
        ));
        // Stale closed-family variants.
        for persisted in &["closed", "abandoned", "applied", "rejected"] {
            assert!(
                check_existing_mp_should_skip_per_mp_fetch(
                    breezyshim::forge::MergeProposalStatus::Closed,
                    persisted,
                    Some(stale),
                    now,
                    chrono::Duration::hours(24),
                ),
                "persisted={} + stale forge Closed should still skip",
                persisted
            );
        }
    }

    /// Unknown persisted status (a future enum value, a typo in
    /// the DB) -> don't skip. Treat unknown as "we don't recognise
    /// this; better do the full fetch and let the rest of
    /// `check_existing_mp` make sense of it" rather than silently
    /// stop refreshing the row.
    #[test]
    fn test_check_existing_mp_should_not_skip_unknown_persisted_status() {
        let now = chrono::Utc::now();
        let last = now - chrono::Duration::hours(1);
        assert!(!check_existing_mp_should_skip_per_mp_fetch(
            breezyshim::forge::MergeProposalStatus::Open,
            "deleted",
            Some(last),
            now,
            chrono::Duration::hours(24),
        ));
    }
}

/// Application state for the publish service.
#[derive(Clone)]
pub struct AppState {
    /// Database connection pool.
    pub conn: sqlx::PgPool,
    /// Rate limiter for buckets.
    pub bucket_rate_limiter: Arc<Mutex<Box<dyn rate_limiter::RateLimiter>>>,
    /// Rate limiter for forges.
    pub forge_rate_limiter: Arc<RwLock<HashMap<String, chrono::DateTime<Utc>>>>,
    /// Optional limit on the number of pushes.
    pub push_limit: Option<usize>,
    /// Optional Redis connection manager.
    pub redis: Option<RedisConnectionManager>,
    /// Redis manager for pub/sub operations.
    pub redis_manager: Option<Arc<janitor::redis::RedisManager>>,
    /// Configuration for the service.
    pub config: &'static janitor::config::Config,
    /// Worker for publishing changes.
    pub publish_worker: PublishWorker,
    /// Map of VCS managers by type.
    pub vcs_managers: Arc<HashMap<VcsType, Box<dyn VcsManager>>>,
    /// Optional limit on the number of merge proposals to modify.
    pub modify_mp_limit: Option<i32>,
    /// Optional limit on the number of unexpected errors.
    pub unexpected_mp_limit: Option<i32>,
    /// GPG context for signing commits.
    pub gpg: Arc<breezyshim::gpg::GPGContext>,
    /// Whether to require binary diffs.
    pub require_binary_diff: bool,
    /// Health checker.
    pub health_checker: Arc<BasicHealthChecker>,
    /// Completion time of the most recent *full* `check_existing` pass
    /// (one that walked every merge proposal we own and refreshed the
    /// bucket rate limiter). The publish loop only creates proposals
    /// while this is recent - so it never rate-limits against a stale or
    /// incomplete view of the proposals we already own. The scan runs on
    /// its own loop and refreshes this on every completed pass; if scans
    /// stop completing, the timestamp ages out and publishing pauses
    /// until a fresh scan lands. `None` until the first pass completes.
    pub last_full_scan_at: tokio::sync::watch::Sender<Option<chrono::DateTime<chrono::Utc>>>,
}

impl BaseAppState for AppState {
    fn service_name(&self) -> &str {
        "publish"
    }

    fn service_version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn health_checker(&self) -> Arc<dyn HealthCheckHandler> {
        self.health_checker.clone()
    }
}

/// Errors that can occur when checking a merge proposal.
#[derive(Debug)]
pub enum CheckMpError {
    /// No run was found for the merge proposal.
    NoRunForMergeProposal(url::Url),
    /// The branch is rate limited.
    BranchRateLimited {
        /// Optional duration after which to retry.
        retry_after: Option<chrono::Duration>,
    },
    /// An unexpected HTTP status was received.
    UnexpectedHttpStatus,
    /// Login is required for the forge.
    ForgeLoginRequired,
    /// A database operation failed.
    Database(sqlx::Error),
    /// A breezyshim/forge call failed in a way we couldn't classify.
    Brz(String),
}

impl From<BrzError> for CheckMpError {
    fn from(e: BrzError) -> Self {
        match e {
            BrzError::UnexpectedHttpStatus { .. } => CheckMpError::UnexpectedHttpStatus,
            BrzError::ForgeLoginRequired => CheckMpError::ForgeLoginRequired,
            other => CheckMpError::Brz(other.to_string()),
        }
    }
}

impl From<sqlx::Error> for CheckMpError {
    fn from(e: sqlx::Error) -> Self {
        CheckMpError::Database(e)
    }
}

impl std::fmt::Display for CheckMpError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            CheckMpError::NoRunForMergeProposal(url) => {
                write!(f, "No run for merge proposal: {}", url)
            }
            CheckMpError::BranchRateLimited { retry_after: _ } => {
                write!(f, "Branch is rate limited")
            }
            CheckMpError::UnexpectedHttpStatus => write!(f, "Unexpected HTTP status"),
            CheckMpError::ForgeLoginRequired => write!(f, "Forge login required"),
            CheckMpError::Database(e) => write!(f, "Database error: {}", e),
            CheckMpError::Brz(msg) => write!(f, "Forge error: {}", msg),
        }
    }
}

impl std::error::Error for CheckMpError {}

/// Number of days after which a previously-failed run is eligible to
/// be retried by `check_existing_mp`. Matches
/// `py/janitor/publish.py::EXISTING_RUN_RETRY_INTERVAL`.
const EXISTING_RUN_RETRY_INTERVAL_DAYS: i64 = 30;

/// Publish a single (run, role) under a campaign policy. Faithful
/// port of `py/janitor/publish.py::publish_from_policy`.
///
/// The entry point for both `consider_publish_run` (when the main
/// queue loop decides a run is ready) and the publish-status Redis
/// listener (when the runner approves a run for immediate publish).
/// Unlike `publish_and_store`, this function applies the full policy
/// gauntlet:
///
///   * If the run's command no longer matches the campaign's current
///     command, reschedule under the new command and skip publishing.
///   * Drop BuildOnly / Skip / unset modes at the top.
///   * `already_published` short-circuit.
///   * For Propose / AttemptPush: check the bucket rate limit and
///     `max_frequency_days` and downgrade to BuildOnly if either
///     guard fires.
///   * Look up `unchanged_run_id` from `last_runs` at `base_revision`
///     (not `main_branch_revision` - this one takes the role's base,
///     mirroring Python).
///
/// Returns the effective mode on success (which may differ from the
/// requested mode - e.g. AttemptPush collapses to Propose/Push), or
/// None when nothing was published.
#[allow(clippy::too_many_arguments)]
pub async fn publish_from_policy(
    conn: &sqlx::PgPool,
    redis_manager: Option<&Arc<janitor::redis::RedisManager>>,
    campaign_config: &Campaign,
    publish_worker: &PublishWorker,
    bucket_rate_limiter: &Mutex<Box<dyn rate_limiter::RateLimiter>>,
    vcs_managers: &HashMap<VcsType, Box<dyn VcsManager>>,
    run: &janitor::state::Run,
    role: &str,
    rate_limit_bucket: Option<&str>,
    target_branch_url: &url::Url,
    mode: Mode,
    max_frequency_days: Option<i32>,
    command: &str,
    require_binary_diff: bool,
    force: bool,
    requester: Option<&str>,
) -> Result<Option<Mode>, Box<dyn std::error::Error + Send + Sync>> {
    if command.is_empty() {
        log::warn!("no command set for {}", run.id);
        return Ok(None);
    }
    if command != run.command {
        log::warn!(
            "Not publishing {}/{}: command has changed. Build used {:?}, now: {:?}. Rescheduling.",
            run.codebase,
            run.suite,
            run.command,
            command
        );
        crate::metrics::COMMAND_CHANGED_COUNT.inc();
        let reschedule_requester = format!(
            "publisher (changed policy: {:?} -> {:?})",
            run.command, command
        );
        if let Err(e) = janitor::schedule::do_schedule(
            conn,
            &run.suite,
            &run.codebase,
            "update-new-mp",
            Some(&run.change_set),
            None,
            true,
            Some(&reschedule_requester),
            None,
            Some(command),
        )
        .await
        {
            log::warn!("Failed to reschedule after command change: {}", e);
        }
        return Ok(None);
    }

    let publish_id = uuid::Uuid::new_v4().to_string();
    if matches!(mode, Mode::BuildOnly | Mode::Skip) {
        return Ok(None);
    }

    if run.result_branches.is_none() {
        log::warn!("no result branches for {}", run.id);
        crate::metrics::NO_RESULT_BRANCHES_COUNT.inc();
        return Ok(None);
    }
    let (remote_branch_name, base_revision, revision) = match run.get_result_branch(role) {
        Some(t) => t,
        None => {
            log::warn!("unable to find branch with role {}: {}", role, run.id);
            crate::metrics::MISSING_MAIN_RESULT_BRANCH_COUNT.inc();
            return Ok(None);
        }
    };
    let revision = match revision {
        Some(r) => r,
        None => {
            log::warn!(
                "run {} role {} has no tip revision; skipping publish",
                run.id,
                role
            );
            return Ok(None);
        }
    };
    let target_branch_url = role_branch_url(target_branch_url, Some(&remote_branch_name));

    // Bail if this run has already been published for this
    // (target_url, branch_name, revision) tuple.
    if !force {
        let modes_to_check: Vec<Mode> = if mode == Mode::AttemptPush {
            vec![Mode::Propose, Mode::Push]
        } else {
            vec![mode]
        };
        let branch_name_for_lookup = campaign_config.branch_name.as_deref().unwrap_or("");
        if crate::state::already_published(
            conn,
            &target_branch_url,
            branch_name_for_lookup,
            &revision,
            &modes_to_check,
        )
        .await?
        {
            return Ok(None);
        }
    }

    // Propose / AttemptPush policy guards: downgrade to BuildOnly
    // when the bucket is over its limit or we published too recently.
    let mut mode = mode;
    if matches!(mode, Mode::Propose | Mode::AttemptPush) {
        let branch_name_for_open = campaign_config.branch_name.as_deref().unwrap_or("");
        let open_mp =
            crate::state::get_open_merge_proposal(conn, &run.codebase, branch_name_for_open)
                .await?;
        if open_mp.is_none() {
            if let Some(bucket) = rate_limit_bucket {
                let limiter_decision = {
                    match bucket_rate_limiter.lock() {
                        Ok(guard) => Some(guard.check_allowed(bucket)),
                        Err(e) => {
                            log::error!(
                                "publish_from_policy: bucket_rate_limiter mutex poisoned: {}",
                                e
                            );
                            None
                        }
                    }
                };
                if let Some(decision) = limiter_decision {
                    if !decision.is_allowed() {
                        log::debug!(
                            "Not creating proposal for {}/{}: bucket {} rate-limited",
                            run.codebase,
                            run.suite,
                            bucket
                        );
                        mode = Mode::BuildOnly;
                    }
                }
            }
            if let Some(max_days) = max_frequency_days {
                if let Some(last_published) =
                    crate::state::check_last_published(conn, &run.suite, &run.codebase).await?
                {
                    let age = chrono::Utc::now() - last_published;
                    if age.num_days() < max_days as i64 {
                        log::debug!(
                            "Not creating proposal for {}/{}: was published already in last {} days (at {})",
                            run.codebase,
                            run.suite,
                            max_days,
                            last_published
                        );
                        mode = Mode::BuildOnly;
                    }
                }
            }
        }
    }
    if matches!(mode, Mode::BuildOnly | Mode::Skip) {
        return Ok(None);
    }

    // unchanged_run: most recent successful last_runs row at this
    // role's base revision. Python uses last_runs, not run.
    let unchanged_run: Option<(String, String)> = if let Some(br) = base_revision.as_ref() {
        sqlx::query_as::<_, (String, String)>(
            "SELECT id, result_code FROM last_runs \
             WHERE codebase = $1 AND revision = $2 AND result_code = 'success'",
        )
        .bind(&run.codebase)
        .bind(br.to_string())
        .fetch_optional(conn)
        .await?
    } else {
        None
    };

    // Scoped hack: skip the binary diff requirement when the control run
    // failed with the metadata-invalid error on the lintian-fixes campaign.
    let mut require_binary_diff = require_binary_diff;
    if let Some((_, ref code)) = unchanged_run {
        if code == "debian-upstream-metadata-invalid" && run.suite == "lintian-fixes" {
            require_binary_diff = false;
        }
    }

    let derived_name = derived_branch_name(conn, campaign_config, run, role).await?;

    let vcs_type_parsed = match <VcsType as std::str::FromStr>::from_str(&run.vcs_type) {
        Ok(v) => v,
        Err(e) => {
            log::warn!(
                "publish_from_policy: unknown vcs_type {:?} for run {}: {}",
                run.vcs_type,
                run.id,
                e
            );
            return Ok(None);
        }
    };
    let vcs_manager = match vcs_managers.get(&vcs_type_parsed) {
        Some(m) => m.as_ref(),
        None => {
            log::warn!(
                "publish_from_policy: no VcsManager registered for vcs_type {:?}",
                vcs_type_parsed
            );
            return Ok(None);
        }
    };

    let tags: Option<Vec<(String, RevisionId)>> = run.result_tags.as_ref().map(|t| {
        t.iter()
            .map(|(name, rev)| (name.clone(), RevisionId::from(rev.as_bytes().to_vec())))
            .collect()
    });
    let codemod_result = run.result.clone().unwrap_or(serde_json::Value::Null);
    let extra_context = serde_json::json!({});

    log::info!(
        "Publishing {} / {:?} / {} (mode: {})",
        run.codebase,
        run.command,
        role,
        mode
    );

    let publish_outcome = publish_worker
        .publish_one(
            &run.suite,
            &run.codebase,
            &run.command,
            &target_branch_url,
            mode,
            role,
            &revision,
            &run.id,
            unchanged_run
                .as_ref()
                .map(|(id, _)| id.as_str())
                .unwrap_or(""),
            &derived_name,
            rate_limit_bucket,
            vcs_manager,
            None, // bucket_rate_limiter: !Send guard would poison the future
            require_binary_diff,
            run_sufficient_for_proposal(campaign_config, run.value),
            None, // reviewers
            tags,
            campaign_config.merge_proposal.commit_message.as_deref(),
            campaign_config.merge_proposal.title.as_deref(),
            &codemod_result,
            None, // existing_mp_url
            Some(&extra_context),
            None, // derived_owner
            None, // auto_merge
        )
        .await;

    // Fold the outcome into (code, description, Option<publish_result>).
    // On PublishFailure, Python routes the exception through
    // handle_publish_failure and then creates a synthetic PublishResult
    // with description "Nothing to do" before falling through to
    // store_publish and pubsub. We do the same, except we track the
    // fact that there's no real PublishOneResult and fill the
    // store_publish fields with our known locals.
    let (code, description, publish_result_opt) = match publish_outcome {
        Ok(r) => ("success".to_string(), "Success".to_string(), Some(r)),
        Err(PublishError::BranchBusy(url)) => {
            log::info!("Branch {} was busy", url);
            return Ok(None);
        }
        Err(e @ PublishError::Failure { .. }) => {
            let (code, description) =
                handle_publish_failure(&e, conn, run, "update-new-mp").await?;
            if code == "nothing-to-do" {
                log::info!("Nothing to do.");
            } else {
                log::info!("Failed({}): {}", code, description);
            }
            (code, description, None)
        }
        Err(other) => return Err(Box::new(other)),
    };

    // MODE_ATTEMPT_PUSH collapses now that we know whether a
    // proposal was created.
    let effective_mode = if mode == Mode::AttemptPush {
        if publish_result_opt
            .as_ref()
            .and_then(|r| r.proposal_url())
            .is_some()
        {
            Mode::Propose
        } else {
            Mode::Push
        }
    } else {
        mode
    };

    // Persist the outcome. Fields that only exist on a real
    // PublishOneResult are pulled from it when available; otherwise
    // we fall back to the locals we used to call publish_one.
    let branch_name_for_store = publish_result_opt
        .as_ref()
        .map(|r| r.branch_name().to_string())
        .or_else(|| campaign_config.branch_name.clone());
    let target_url_for_store = publish_result_opt
        .as_ref()
        .map(|r| r.target_branch_url().clone())
        .unwrap_or_else(|| target_branch_url.clone());
    let target_web_url_for_store: Option<String> = publish_result_opt
        .as_ref()
        .and_then(|r| r.target_branch_web_url().map(|u| u.to_string()));
    let proposal_url_for_store: Option<url::Url> = publish_result_opt
        .as_ref()
        .and_then(|r| r.proposal_url().cloned());

    crate::state::store_publish(
        conn,
        &run.change_set,
        &run.codebase,
        branch_name_for_store.as_deref(),
        Some(&target_url_for_store),
        target_web_url_for_store.as_deref(),
        base_revision.as_ref(),
        Some(&revision),
        role,
        effective_mode,
        &code,
        &description,
        proposal_url_for_store.as_ref(),
        Some(&publish_id),
        requester,
        Some(&run.id),
    )
    .await?;

    // Bucket bump: matches publish_one's internal behavior when
    // is_new=true. We still do it here because we pass None for
    // bucket_rate_limiter into publish_one (see the !Send note).
    // Matches the Prometheus counter bumps at
    // py/janitor/publish.py:414-420 (new_merge_proposal_count,
    // merge_proposal_count{status=open}, open_proposal_count,
    // bucket_proposal_count{bucket}) - each `.inc()` fires once
    // per successful new proposal.
    if let Some(r) = publish_result_opt.as_ref() {
        if r.is_new().unwrap_or(false) && r.proposal_url().is_some() {
            crate::metrics::NEW_MERGE_PROPOSAL_COUNT.inc();
            crate::metrics::MERGE_PROPOSAL_COUNT
                .with_label_values(&["open"])
                .inc();
            crate::metrics::OPEN_PROPOSAL_COUNT.inc();
            if let Some(bucket) = rate_limit_bucket {
                if let Ok(mut guard) = bucket_rate_limiter.lock() {
                    guard.inc(bucket);
                } else {
                    log::error!("publish_from_policy: bucket_rate_limiter mutex poisoned");
                }
                crate::metrics::BUCKET_PROPOSAL_COUNT
                    .with_label_values(&[bucket])
                    .inc();
            }
        }
    }

    let publish_delay_secs: Option<f64> = if code == "success" {
        let secs = (chrono::Utc::now() - run.finish_time).num_milliseconds() as f64 / 1000.0;
        crate::metrics::PUBLISH_LATENCY.observe(secs);
        Some(secs)
    } else {
        None
    };

    if let Some(manager) = redis_manager {
        let entry = serde_json::json!({
            "id": publish_id,
            "codebase": run.codebase,
            "campaign": run.suite,
            "proposal_url": proposal_url_for_store.as_ref().map(|u| u.to_string()),
            "mode": effective_mode.to_string(),
            "main_branch_url": target_url_for_store.to_string(),
            "main_branch_browse_url": target_web_url_for_store,
            "branch_name": branch_name_for_store,
            "result_code": code,
            "result": run.result.clone().unwrap_or(serde_json::Value::Null),
            "role": role,
            "run_id": run.id,
            "publish_delay": publish_delay_secs,
        });
        let payload = serde_json::to_string(&entry)?;
        manager
            .publisher()
            .publish_to_channel("publish", &payload)
            .await?;
    }

    if code == "success" {
        Ok(Some(effective_mode))
    } else {
        Ok(None)
    }
}

/// Publish a single run under a given mode/role and persist the result.
///
/// Looks up the role-specific branch, resolves an `unchanged_run_id`
/// from the main-branch revision, calls `publish_worker.publish_one`,
/// writes the outcome to the `publish` / `merge_proposal` tables via
/// `state::store_publish`, and publishes a JSON entry on the `"publish"`
/// Redis channel for the site service's pubsub forwarder. On
/// `BranchBusy`, returns Ok(()) without writing anything.
#[allow(clippy::too_many_arguments)]
pub async fn publish_and_store(
    conn: &sqlx::PgPool,
    redis_manager: Option<&Arc<janitor::redis::RedisManager>>,
    campaign_config: &Campaign,
    publish_worker: &PublishWorker,
    publish_id: &str,
    run: &janitor::state::Run,
    mode: Mode,
    role: &str,
    rate_limit_bucket: Option<&str>,
    vcs_managers: &HashMap<VcsType, Box<dyn VcsManager>>,
    bucket_rate_limiter: &Mutex<Box<dyn rate_limiter::RateLimiter>>,
    allow_create_proposal: Option<bool>,
    require_binary_diff: bool,
    requester: Option<&str>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Resolve the role-specific source branch + tip revision from
    // the run's result_branches. If the run didn't touch this role
    // there's nothing to publish - log and return.
    let (remote_branch_name, _base_revision, revision) = match run.get_result_branch(role) {
        Some(t) => t,
        None => {
            log::warn!(
                "publish_and_store: run {} has no result branch for role {}",
                run.id,
                role
            );
            return Ok(());
        }
    };
    let revision = match revision {
        Some(r) => r,
        None => {
            log::warn!(
                "publish_and_store: run {} role {} has no tip revision",
                run.id,
                role
            );
            return Ok(());
        }
    };

    // target_branch_url is the run's target_branch_url (or branch_url
    // fallback) with the role's remote branch name appended as a
    // segment parameter.
    let base_target_url: url::Url = run
        .target_branch_url
        .as_deref()
        .unwrap_or(&run.branch_url)
        .parse()
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("invalid target/branch URL for run {}: {}", run.id, e).into()
        })?;
    let target_branch_url = role_branch_url(&base_target_url, Some(&remote_branch_name));

    // Python: `if allow_create_proposal is None: allow_create_proposal = run_sufficient_for_proposal(...)`.
    // In practice callers always pass a concrete bool, but preserve
    // the fallback for parity.
    let allow_create_proposal = allow_create_proposal
        .unwrap_or_else(|| run_sufficient_for_proposal(campaign_config, run.value));

    // Most recent successful run on the same codebase at the same
    // main-branch revision. Used as `unchanged_id` so the publisher
    // can diff against the pre-codemod tree. Matches the inline SQL
    // in py/janitor/publish.py::publish_and_store.
    let unchanged_run_id: Option<String> = if let Some(main_rev) = run.main_branch_revision.as_ref()
    {
        sqlx::query_scalar::<_, String>(
            "SELECT id FROM run \
                 WHERE codebase = $1 AND revision = $2 AND result_code = 'success' \
                 ORDER BY finish_time DESC LIMIT 1",
        )
        .bind(&run.codebase)
        .bind(main_rev.to_string())
        .fetch_optional(conn)
        .await?
    } else {
        None
    };

    let derived_name = derived_branch_name(conn, campaign_config, run, role).await?;

    // Resolve the right VcsManager for the run's vcs_type.
    let vcs_type_parsed = match <VcsType as std::str::FromStr>::from_str(&run.vcs_type) {
        Ok(v) => v,
        Err(e) => {
            log::warn!(
                "publish_and_store: unknown vcs_type {:?} for run {}: {}",
                run.vcs_type,
                run.id,
                e
            );
            return Ok(());
        }
    };
    let vcs_manager = match vcs_managers.get(&vcs_type_parsed) {
        Some(m) => m.as_ref(),
        None => {
            log::warn!(
                "publish_and_store: no VcsManager registered for vcs_type {:?}",
                vcs_type_parsed
            );
            return Ok(());
        }
    };

    // Convert result_tags from (String, String) -> (String, RevisionId).
    let tags: Option<Vec<(String, RevisionId)>> = run.result_tags.as_ref().map(|t| {
        t.iter()
            .map(|(name, rev)| (name.clone(), RevisionId::from(rev.as_bytes().to_vec())))
            .collect()
    });

    let codemod_result = run.result.clone().unwrap_or(serde_json::Value::Null);
    let extra_context = serde_json::json!({});

    // Don't pass the bucket_rate_limiter through publish_one: its
    // std::sync::Mutex guard is !Send and can't be held across the
    // publish_one .await without poisoning the surrounding future.
    // Instead, on a successful publish that created a new proposal,
    // increment the bucket counter below in a tight sync scope.
    let publish_outcome = publish_worker
        .publish_one(
            &run.suite,
            &run.codebase,
            &run.command,
            &target_branch_url,
            mode,
            role,
            &revision,
            &run.id,
            unchanged_run_id.as_deref().unwrap_or(""),
            &derived_name,
            rate_limit_bucket,
            vcs_manager,
            None, // bucket_rate_limiter: bumped below on success
            require_binary_diff,
            allow_create_proposal,
            None, // reviewers
            tags,
            campaign_config.merge_proposal.commit_message.as_deref(),
            campaign_config.merge_proposal.title.as_deref(),
            &codemod_result,
            None, // existing_mp_url
            Some(&extra_context),
            None, // derived_owner
            None, // auto_merge
        )
        .await;

    match publish_outcome {
        Err(PublishError::BranchBusy(url)) => {
            log::debug!("Branch {} was busy while publishing", url);
            Ok(())
        }
        Err(PublishError::Failure { code, description }) => {
            crate::state::store_publish(
                conn,
                &run.change_set,
                &run.codebase,
                campaign_config.branch_name.as_deref(),
                Some(&target_branch_url),
                None, // target_branch_web_url
                run.main_branch_revision.as_ref(),
                run.revision.as_ref(),
                role,
                mode,
                &code,
                &description,
                None, // merge_proposal_url
                Some(publish_id),
                requester,
                Some(&run.id),
            )
            .await?;

            if let Some(manager) = redis_manager {
                let entry = serde_json::json!({
                    "id": publish_id,
                    "mode": mode.to_string(),
                    "result_code": code,
                    "description": description,
                    "campaign": run.suite,
                    "main_branch_url": target_branch_url.to_string(),
                    "result": run.result.clone().unwrap_or(serde_json::Value::Null),
                    "codebase": run.codebase,
                });
                let payload = serde_json::to_string(&entry)?;
                manager
                    .publisher()
                    .publish_to_channel("publish", &payload)
                    .await?;
            }
            Ok(())
        }
        Err(other) => Err(Box::new(other)),
        Ok(publish_result) => {
            // MODE_ATTEMPT_PUSH collapses to propose or push based on
            // whether publish_one ended up creating a proposal.
            let effective_mode = if mode == Mode::AttemptPush {
                if publish_result.proposal_url().is_some() {
                    Mode::Propose
                } else {
                    Mode::Push
                }
            } else {
                mode
            };

            crate::state::store_publish(
                conn,
                &run.change_set,
                &run.codebase,
                Some(publish_result.branch_name()),
                Some(publish_result.target_branch_url()),
                publish_result.target_branch_web_url().map(|u| u.as_str()),
                run.main_branch_revision.as_ref(),
                run.revision.as_ref(),
                role,
                effective_mode,
                "success",
                "Success",
                publish_result.proposal_url(),
                Some(publish_id),
                requester,
                Some(&run.id),
            )
            .await?;

            // Rate-limiter bump: match publish_one's internal behavior
            // in Python (is_new=True + proposal_url + bucket set).
            if publish_result.is_new().unwrap_or(false) && publish_result.proposal_url().is_some() {
                if let Some(bucket) = rate_limit_bucket {
                    if let Ok(mut guard) = bucket_rate_limiter.lock() {
                        guard.inc(bucket);
                    } else {
                        log::error!("publish_and_store: bucket_rate_limiter mutex poisoned");
                    }
                }
            }

            let publish_delay_secs =
                (chrono::Utc::now() - run.finish_time).num_milliseconds() as f64 / 1000.0;
            crate::metrics::PUBLISH_LATENCY.observe(publish_delay_secs);

            if let Some(manager) = redis_manager {
                let entry = serde_json::json!({
                    "id": publish_id,
                    "campaign": run.suite,
                    "proposal_url": publish_result.proposal_url().map(|u| u.to_string()),
                    "mode": effective_mode.to_string(),
                    "main_branch_url": publish_result.target_branch_url().to_string(),
                    "main_branch_browse_url": publish_result.target_branch_web_url().map(|u| u.to_string()),
                    "branch_name": publish_result.branch_name(),
                    "result_code": "success",
                    "result": run.result.clone().unwrap_or(serde_json::Value::Null),
                    "role": role,
                    "publish_delay": publish_delay_secs,
                    "run_id": run.id,
                    "codebase": run.codebase,
                });
                let payload = serde_json::to_string(&entry)?;
                manager
                    .publisher()
                    .publish_to_channel("publish", &payload)
                    .await?;
            }
            Ok(())
        }
    }
}

/// Verify a previously-published merge proposal is still in good shape,
/// updating our local state and republishing if a newer run has arrived.
///
/// Walks the proposal's source revision / target branch / can_be_merged
/// state, recovers the codebase + rate-limit bucket through several
/// progressively-weaker fallbacks, carries forward any
/// abandoned/applied/rejected status from the publisher's own state,
/// and either bumps `last_scanned` or pushes a full update through
/// `ProposalInfoManager::update_proposal_info`. For open proposals
/// not in `check_only` mode the function then runs the action loop:
/// orphaned-MP recovery, last-run failure handling, abandon on
/// insufficient value, role / target rename, and (when a newer run
/// is available) the `publish_one` republish path.
async fn check_existing_mp(
    conn: &sqlx::PgPool,
    redis: Option<RedisConnectionManager>,
    config: &janitor::config::Config,
    publish_worker: &crate::PublishWorker,
    mp: &breezyshim::forge::MergeProposal,
    status: breezyshim::forge::MergeProposalStatus,
    vcs_managers: &HashMap<VcsType, Box<dyn VcsManager>>,
    bucket_rate_limiter: &Mutex<Box<dyn crate::rate_limiter::RateLimiter>>,
    check_only: bool,
    mps_per_bucket: Option<
        &mut HashMap<janitor::publish::MergeProposalStatus, HashMap<String, usize>>,
    >,
    mut possible_transports: Option<&mut Vec<breezyshim::transport::Transport>>,
) -> Result<bool, CheckMpError> {
    let mp_url = mp.url().map_err(CheckMpError::from)?;
    log::debug!("Checking existing merge proposal: {}", mp_url);

    // Load whatever we already know about this proposal.
    let mut proposal_info_manager =
        crate::proposal_info::ProposalInfoManager::new(conn.clone(), redis.clone()).await;
    let old_proposal_info = proposal_info_manager.get_proposal_info(&mp_url).await?;

    let (mut codebase, mut rate_limit_bucket) = if let Some(info) = &old_proposal_info {
        // info.codebase is now Option<String> - the merge_proposal
        // row's codebase can be NULL for forge-discovered proposals
        // we haven't matched to a candidate yet. Pass through as-is;
        // the guess-from-revision pass below will populate it.
        (info.codebase.clone(), info.rate_limit_bucket.clone())
    } else {
        (None, None)
    };

    // Freshness gate: skip the per-MP forge fetches + source
    // branch open when we already have a recent persisted record
    // and the forge-reported status matches the persisted family.
    // Without this, every cycle re-runs ~5 forge API calls and a
    // network branch open for every one of ~14k MPs we own -
    // consistently outpacing the configured cycle interval and
    // starving `publish_pending_ready` (the step that actually
    // creates new proposals). A 24h staleness window means a
    // forge-side status flip is reflected at most one threshold
    // late, which is acceptable for the MP lifecycle (open ->
    // merged/closed). Express the threshold as a function so the
    // pure helper below stays unit-testable without a clock.
    if let Some(info) = &old_proposal_info {
        if check_existing_mp_should_skip_per_mp_fetch(
            status,
            &info.status,
            info.last_scanned,
            chrono::Utc::now(),
            CHECK_EXISTING_MP_FRESH_THRESHOLD,
        ) {
            log::debug!(
                "Skipping per-MP forge fetch for {} (already scanned recently with matching status)",
                mp_url
            );
            bump_last_scanned(conn, &mp_url).await?;
            // The persisted status string maps to one of the
            // publisher-level enum variants - fall back to Open if
            // it's an unknown value (the gate above already
            // rejected such rows, but keep the parse infallible).
            let effective_status = info
                .status
                .parse::<janitor::publish::MergeProposalStatus>()
                .unwrap_or(janitor::publish::MergeProposalStatus::Open);
            return finish_check_existing_mp_no_action(
                status,
                effective_status,
                rate_limit_bucket.as_deref(),
                mps_per_bucket,
                check_only,
            );
        }
    }

    // Forge calls: source revision, source branch URL, can_be_merged,
    // target branch URL. Each runs on a blocking thread because
    // breezyshim is synchronous. tokio::join! kicks all four off
    // concurrently; the GIL still serialises the Python work, but each
    // call releases it during the underlying network I/O.
    //
    // Log-and-continue on failure: bailing out of the whole MP would
    // mean last_scanned never gets updated and the next scan retries
    // the same broken MP forever.
    let (revision_res, source_branch_url_res, can_be_merged_res, target_branch_url_res) = tokio::join!(
        tokio::task::spawn_blocking({
            let mp = mp.clone();
            move || mp.get_source_revision()
        }),
        tokio::task::spawn_blocking({
            let mp = mp.clone();
            move || mp.get_source_branch_url()
        }),
        tokio::task::spawn_blocking({
            let mp = mp.clone();
            move || mp.can_be_merged()
        }),
        tokio::task::spawn_blocking({
            let mp = mp.clone();
            move || mp.get_target_branch_url()
        }),
    );

    let mut revision: Option<RevisionId> = revision_res
        .map_err(|_| CheckMpError::Brz("get_source_revision task join error".to_string()))?
        .unwrap_or_else(|e| {
            log::debug!("Failed to get source revision for {}: {}", mp_url, e);
            None
        });
    let source_branch_url = source_branch_url_res
        .map_err(|_| CheckMpError::Brz("get_source_branch_url task join error".to_string()))?
        .unwrap_or_else(|e| {
            log::debug!("Failed to get source branch URL for {}: {}", mp_url, e);
            None
        });
    let can_be_merged = can_be_merged_res
        .map_err(|_| CheckMpError::Brz("can_be_merged task join error".to_string()))?
        .ok();
    let target_branch_url = target_branch_url_res
        .map_err(|_| CheckMpError::Brz("get_target_branch_url task join error".to_string()))?
        .unwrap_or_else(|e| {
            log::debug!("Failed to get target branch URL for {}: {}", mp_url, e);
            None
        });

    // Resolve the source branch name. Python prefers the
    // opened branch's `.name` (set in the silver_platter fallback
    // below) over the URL segment params, so leave this None for
    // now - the fallback may fill it in.
    let mut source_branch_name: Option<String> = None;

    // If get_source_revision returned None, try to open the source branch
    // directly and read its tip revision and name. open_branch is
    // synchronous and blocks briefly on the network round-trip; threading
    // possible_transports through lets repeated calls in the same scan
    // reuse forge transports.
    if revision.is_none() {
        if let Some(source_url) = source_branch_url.as_ref() {
            let result = silver_platter::vcs::open_branch(
                source_url,
                possible_transports.as_deref_mut(),
                None,
                None,
            );
            match result {
                Ok(branch) => {
                    revision = Some(branch.last_revision());
                    source_branch_name = branch.name();
                }
                Err(silver_platter::vcs::BranchOpenError::Missing { .. })
                | Err(silver_platter::vcs::BranchOpenError::Unavailable { .. })
                | Err(silver_platter::vcs::BranchOpenError::TemporarilyUnavailable { .. }) => {
                    // Branch missing or unavailable: leave revision unset.
                }
                Err(e) => log::warn!("Failed to open source branch for {}: {}", mp_url, e),
            }
        }
    }

    // If the source branch name is still unknown but we know
    // the source URL, fall back to the URL's segment params.
    if source_branch_name.is_none() {
        if let Some(url) = &source_branch_url {
            let segment_params = breezyshim::urlutils::split_segment_parameters(url).1;
            source_branch_name = segment_params
                .get("branch")
                .map(|b| breezyshim::urlutils::unescape_utf8(b));
        }
    }

    // Final revision fallback: when neither the forge nor an
    // open of the source branch produced a revision, fall back to
    // the persisted proposal_info.revision.
    if revision.is_none() {
        if let Some(info) = &old_proposal_info {
            if let Some(rev_str) = &info.revision {
                revision = Some(RevisionId::from(rev_str.as_bytes().to_vec()));
            }
        }
    }

    // Codebase + bucket fallback chain. Note: Python tries the
    // *target* branch URL here, not the source - that's the URL
    // codebase rows are keyed on. We keep parity.
    if rate_limit_bucket.is_none() {
        if let Some(target_url) = &target_branch_url {
            match crate::state::guess_codebase_from_branch_url(conn, target_url, None).await {
                Ok(Some(cb)) => {
                    log::info!(
                        "Guessed codebase ({}) for {} from target branch URL.",
                        cb,
                        mp_url
                    );
                    codebase = Some(cb);
                }
                Ok(None) => {}
                Err(e) => log::warn!(
                    "guess_codebase_from_branch_url failed for {}: {}",
                    mp_url,
                    e
                ),
            }
        }
        if codebase.is_none() {
            if let Some(rev) = &revision {
                match crate::state::guess_proposal_info_from_revision(conn, rev).await {
                    Ok((Some(cb), bucket)) => {
                        log::info!(
                            "Guessed codebase ({}) for {} based on revision.",
                            cb,
                            mp_url
                        );
                        codebase = Some(cb);
                        rate_limit_bucket = bucket;
                    }
                    Ok((None, _)) => {
                        log::warn!("No codebase known for {} ({:?})", mp_url, target_branch_url);
                    }
                    Err(e) => log::warn!(
                        "guess_proposal_info_from_revision failed for {}: {}",
                        mp_url,
                        e
                    ),
                }
            }
        } else if let (Some(cb), Some(branch_name)) = (&codebase, source_branch_name.as_deref()) {
            match crate::state::guess_rate_limit_bucket(conn, cb, branch_name).await {
                Ok(bucket) => rate_limit_bucket = bucket,
                Err(e) => log::warn!("guess_rate_limit_bucket failed for {}: {}", mp_url, e),
            }
        }
    }

    // Status carry-forward. Map the forge status into our richer
    // enum, then promote it back to whatever publisher-level status
    // we had previously persisted (abandoned / applied / rejected)
    // when the forge has merely told us the proposal is closed.
    let mut effective_status: janitor::publish::MergeProposalStatus = match status {
        breezyshim::forge::MergeProposalStatus::Open => janitor::publish::MergeProposalStatus::Open,
        breezyshim::forge::MergeProposalStatus::Merged => {
            janitor::publish::MergeProposalStatus::Merged
        }
        breezyshim::forge::MergeProposalStatus::Closed => {
            janitor::publish::MergeProposalStatus::Closed
        }
        breezyshim::forge::MergeProposalStatus::All => janitor::publish::MergeProposalStatus::Open,
    };
    if let Some(info) = &old_proposal_info {
        if effective_status == janitor::publish::MergeProposalStatus::Closed
            && matches!(info.status.as_str(), "abandoned" | "applied" | "rejected")
        {
            if let Ok(richer) = info.status.parse::<janitor::publish::MergeProposalStatus>() {
                effective_status = richer;
            }
        }
    }

    // Decide whether the persisted state needs an update or just a
    // last_scanned bump.
    let needs_update = match &old_proposal_info {
        None => true,
        Some(info) => {
            info.status != effective_status.to_string()
                || info.revision != revision.as_ref().map(|r| r.to_string())
                || info.target_branch_url != target_branch_url.as_ref().map(|u| u.to_string())
                || info.rate_limit_bucket != rate_limit_bucket
                || info.can_be_merged != can_be_merged
        }
    };

    // Look up the run that produced this proposal - it's needed
    // here even when no update is required so the bottom half can
    // know whether to act on an open MP.
    let mut mp_run = if needs_update {
        // `merge_proposal.codebase` has a FK to codebase(name); empty
        // string fails the FK. Pass None when we couldn't identify
        // the local codebase so sqlx binds NULL - schema allows it
        // (`codebase text references codebase(name) on delete set null`).
        // Python passes Optional[str] here for the same reason.
        let codebase_for_update: Option<&str> = codebase.as_deref();
        let mp_run = crate::state::get_merge_proposal_run(conn, &mp_url).await?;
        let campaign_for_update = mp_run.as_ref().map(|r| r.campaign.as_str()).unwrap_or("");
        let target_url_for_update = match &target_branch_url {
            Some(u) => u.clone(),
            None => {
                // Without a target branch URL we can't update the
                // ProposalInfoManager row; fall back to bumping last_scanned
                // only and treat this like the unchanged path.
                log::warn!(
                    "No target branch URL for {}, skipping ProposalInfoManager update",
                    mp_url
                );
                bump_last_scanned(conn, &mp_url).await?;
                return finish_check_existing_mp_no_action(
                    status,
                    effective_status,
                    rate_limit_bucket.as_deref(),
                    mps_per_bucket,
                    check_only,
                );
            }
        };
        proposal_info_manager
            .update_proposal_info(
                mp,
                effective_status,
                revision.as_ref(),
                codebase_for_update,
                &target_url_for_update,
                campaign_for_update,
                can_be_merged,
                rate_limit_bucket.as_deref(),
            )
            .await?;
        mp_run
    } else {
        bump_last_scanned(conn, &mp_url).await?;
        None
    };

    // Update the bucket counter.
    if let (Some(mps_per_bucket), Some(bucket)) = (mps_per_bucket, rate_limit_bucket.as_deref()) {
        *mps_per_bucket
            .entry(effective_status)
            .or_default()
            .entry(bucket.to_string())
            .or_insert(0) += 1;
    }

    // Non-open / check_only proposals: nothing else to do.
    if status != breezyshim::forge::MergeProposalStatus::Open || check_only {
        return Ok(false);
    }

    // Active loop entry. Make sure mp_run is loaded - it might
    // not have been if the diff-detection skipped the update path.
    if mp_run.is_none() {
        mp_run = crate::state::get_merge_proposal_run(conn, &mp_url).await?;
    }

    // Orphaned-MP recovery. If there's no run row but we know the
    // codebase + source_branch_name, look up the matching campaign by
    // branch name and either reschedule a fresh run for it or fall
    // through with a synthetic mp_run so the republish path can act.
    // Without those we cannot proceed and raise NoRunForMergeProposal.
    if mp_run.is_none() {
        let cb = codebase.as_deref();
        let branch = source_branch_name.as_deref();
        match (cb, branch) {
            (Some(cb), Some(branch_name)) => {
                if let Some((campaign_name, role)) =
                    config.find_campaign_by_branch_name(branch_name)
                {
                    log::warn!(
                        "Recovered orphaned merge proposal {} (codebase={}, campaign={})",
                        mp_url,
                        cb,
                        campaign_name
                    );
                    let last_run =
                        crate::state::get_last_effective_run(conn, cb, campaign_name).await?;
                    if last_run.is_none() {
                        match janitor::schedule::do_schedule(
                            conn,
                            campaign_name,
                            cb,
                            "update-existing-mp",
                            None,
                            None,
                            true,
                            Some("publisher (orphaned merge proposal)"),
                            None,
                            None,
                        )
                        .await
                        {
                            Ok(_) => {
                                log::warn!("Rescheduled orphaned merge proposal {}", mp_url);
                                return Ok(false);
                            }
                            Err(janitor::schedule::Error::CandidateUnavailable { .. }) => {
                                log::warn!(
                                    "Candidate unavailable while attempting to reschedule \
                                     orphaned {} ({}/{})",
                                    mp_url,
                                    cb,
                                    campaign_name
                                );
                                return Err(CheckMpError::NoRunForMergeProposal(mp_url.clone()));
                            }
                            Err(e) => {
                                log::warn!(
                                    "Failed to reschedule orphaned merge proposal {}: {}",
                                    mp_url,
                                    e
                                );
                                return Err(CheckMpError::NoRunForMergeProposal(mp_url.clone()));
                            }
                        }
                    }
                    // We have a last_run but no mp_run. Python builds
                    // a synthetic mp_run dict from the campaign + role
                    // it just recovered, the proposal's forge state,
                    // and the source revision, and lets the republish
                    // path take over.
                    let synth_revision = match revision.as_ref() {
                        Some(r) => r.clone(),
                        None => {
                            log::warn!(
                                "Recovered orphan {} has no source revision; not acting",
                                mp_url
                            );
                            return Ok(false);
                        }
                    };
                    let synth_branch_url = match target_branch_url.as_ref() {
                        Some(u) => u.to_string(),
                        None => {
                            log::warn!(
                                "Recovered orphan {} has no target branch URL; not acting",
                                mp_url
                            );
                            return Ok(false);
                        }
                    };
                    log::warn!("Going ahead with dummy old run for {}", mp_url);
                    mp_run = Some(crate::state::MergeProposalRun {
                        id: String::new(),
                        campaign: campaign_name.to_string(),
                        branch_url: synth_branch_url,
                        command: String::new(),
                        value: 0,
                        role: role.to_string(),
                        remote_branch_name: String::new(),
                        revision: synth_revision,
                        codebase: cb.to_string(),
                        change_set: String::new(),
                    });
                } else {
                    return Err(CheckMpError::NoRunForMergeProposal(mp_url.clone()));
                }
            }
            _ => return Err(CheckMpError::NoRunForMergeProposal(mp_url.clone())),
        }
    }

    let mp_run = mp_run.expect("checked above that mp_run is Some");

    // Resolve mp_remote_branch_name. Prefer the value the run recorded;
    // if it's empty, fall back to opening the target branch and reading
    // its `.name`.
    let mut mp_remote_branch_name: Option<String> = if mp_run.remote_branch_name.is_empty() {
        None
    } else {
        Some(mp_run.remote_branch_name.clone())
    };
    if mp_remote_branch_name.is_none() {
        let candidate_target = target_branch_url
            .clone()
            .or_else(|| mp_run.branch_url.parse().ok());
        if let Some(target_url) = candidate_target {
            match silver_platter::vcs::open_branch(
                &target_url,
                possible_transports.as_deref_mut(),
                None,
                None,
            ) {
                Ok(branch) => {
                    mp_remote_branch_name = branch.name();
                }
                Err(silver_platter::vcs::BranchOpenError::Missing { .. })
                | Err(silver_platter::vcs::BranchOpenError::Unavailable { .. })
                | Err(silver_platter::vcs::BranchOpenError::TemporarilyUnavailable { .. }) => {
                    // Branch missing or unavailable: leave revision unset.
                }
                Err(e) => log::warn!("Failed to open target branch for {}: {}", mp_url, e),
            }
        } else {
            log::warn!(
                "{}: no target branch URL available to resolve remote branch name",
                mp_url
            );
        }
    }

    // Fetch the latest effective run for this campaign +
    // codebase so we can compare it against the run that produced
    // the proposal.
    let last_run =
        match crate::state::get_last_effective_run(conn, &mp_run.codebase, &mp_run.campaign).await?
        {
            Some(run) => run,
            None => {
                log::warn!(
                    "{}: Unable to find any relevant runs (codebase={}, campaign={}).",
                    mp_url,
                    mp_run.codebase,
                    mp_run.campaign
                );
                return Ok(false);
            }
        };

    // Resolve campaign config (needed both for value-threshold and
    // commit-message templating downstream).
    let campaign_config = match config.get_campaign(&mp_run.campaign) {
        Some(c) => c,
        None => {
            log::warn!(
                "Campaign {} no longer in config, leaving merge proposal {} alone",
                mp_run.campaign,
                mp_url
            );
            return Ok(false);
        }
    };

    let mut target_branch_url = target_branch_url
        .clone()
        .or_else(|| mp_run.branch_url.parse().ok());

    // Closed-as-applied: a follow-up run came in and produced
    // nothing, meaning the changes have already been merged in some
    // other way. Close the proposal as "applied".
    if last_run.result_code == "nothing-to-do" {
        log::info!(
            "{}: Last run produced no changes; closing proposal as applied.",
            mp_url
        );
        if let (Some(rev), Some(target)) = (revision.as_ref(), target_branch_url.as_ref()) {
            match abandon_or_close_mp_async(
                &mut proposal_info_manager,
                mp,
                rev,
                Some(mp_run.codebase.as_str()),
                &target.to_string(),
                Some(mp_run.campaign.as_str()),
                can_be_merged,
                rate_limit_bucket.as_deref(),
                Some(
                    "This merge proposal will be closed, since all remaining changes have been applied independently.",
                ),
                AbandonMode::Applied,
            )
            .await
            {
                Ok(()) => return Ok(true),
                Err(BrzError::PermissionDenied(_, _)) => return Ok(false),
                Err(e) => {
                    log::warn!("Failed to close merge proposal {}: {}", mp_url, e);
                    return Ok(false);
                }
            }
        } else {
            log::warn!(
                "{}: Cannot close-as-applied without revision + target URL",
                mp_url
            );
            return Ok(false);
        }
    }

    // Last run failed: reschedule transient or long-ago
    // failures, otherwise leave the proposal alone.
    if last_run.result_code != "success" {
        let last_run_age = chrono::Utc::now() - last_run.finish_time;
        let transient = last_run.failure_transient.unwrap_or(false);
        let reschedule_reason: Option<&'static str> = if transient {
            log::info!(
                "{}: Last run failed with transient error ({}). Rescheduling.",
                mp_url,
                last_run.result_code
            );
            Some("publisher (transient error)")
        } else if last_run_age.num_days() > EXISTING_RUN_RETRY_INTERVAL_DAYS {
            log::info!(
                "{}: Last run failed ({}) {} days ago. Rescheduling.",
                mp_url,
                last_run.result_code,
                last_run_age.num_days()
            );
            Some("publisher (retrying old failed run)")
        } else {
            log::info!(
                "{}: Last run failed ({}). Not touching merge proposal.",
                mp_url,
                last_run.result_code
            );
            None
        };
        if let Some(requester) = reschedule_reason {
            if let Err(e) = janitor::schedule::do_schedule(
                conn,
                &last_run.suite,
                &last_run.codebase,
                "update-existing-mp",
                Some(&last_run.change_set),
                None,
                false,
                Some(requester),
                None,
                None,
            )
            .await
            {
                log::warn!(
                    "Failed to reschedule {}/{} after failed run: {}",
                    last_run.codebase,
                    last_run.suite,
                    e
                );
            }
        }
        return Ok(false);
    }

    // Insufficient value: the run still succeeds but the value
    // dropped below the campaign threshold (e.g. only trivial fixes
    // are left). Abandon the proposal politely.
    if !run_sufficient_for_proposal(campaign_config, mp_run.value.try_into().ok()) {
        log::info!(
            "{}: Run value {} fell below threshold; abandoning.",
            mp_url,
            mp_run.value
        );
        if let (Some(rev), Some(target)) = (revision.as_ref(), target_branch_url.as_ref()) {
            match abandon_or_close_mp_async(
                &mut proposal_info_manager,
                mp,
                rev,
                Some(mp_run.codebase.as_str()),
                &target.to_string(),
                Some(mp_run.campaign.as_str()),
                can_be_merged,
                rate_limit_bucket.as_deref(),
                Some("This merge proposal will be closed, since only trivial changes are left."),
                AbandonMode::Abandoned,
            )
            .await
            {
                Ok(()) => return Ok(true),
                Err(BrzError::PermissionDenied(_, _)) => return Ok(false),
                Err(e) => {
                    log::warn!("Failed to abandon merge proposal {}: {}", mp_url, e);
                    return Ok(false);
                }
            }
        } else {
            log::warn!("{}: Cannot abandon without revision + target URL", mp_url);
            return Ok(false);
        }
    }

    // Resolve the result branch matching this proposal's role.
    let (last_run_remote_branch_name, last_run_base_revision, last_run_revision) =
        match last_run.get_result_branch(&mp_run.role) {
            Some(t) => t,
            None => {
                log::warn!(
                    "{}: merge proposal run {} had role {} but it is gone now ({})",
                    mp_url,
                    mp_run.id,
                    mp_run.role,
                    last_run.id
                );
                return Ok(false);
            }
        };

    // Role / remote-branch-name change. Match Python: enter whenever
    // the two names differ and last_run_remote_branch_name is present.
    // Inside, when mp_run.remote_branch_name is empty we refuse to act
    // (Python's `return False` branch) rather than falling through into
    // downstream comparisons with a missing value.
    if last_run_remote_branch_name.as_str() != mp_run.remote_branch_name
        && !last_run_remote_branch_name.is_empty()
    {
        log::warn!(
            "{}: Remote branch name has changed: {} -> {}",
            mp_url,
            mp_run.remote_branch_name,
            last_run_remote_branch_name
        );
        if mp_run.remote_branch_name.is_empty() {
            return Ok(false);
        }
        let new_name = last_run_remote_branch_name.clone();
        let rename_result = tokio::task::spawn_blocking({
            let mp = mp.clone();
            let new_name = new_name.clone();
            move || mp.set_target_branch_name(&new_name)
        })
        .await
        .map_err(|_| CheckMpError::Brz("set_target_branch_name task join error".to_string()))?;
        match rename_result {
            Ok(()) => {
                log::info!(
                    "{}: Updated proposal target branch name to {}",
                    mp_url,
                    new_name
                );
                // Point the working target URL at the new branch, so
                // any downstream forge comparisons see the repointed
                // value. Python does the equivalent
                // `target_branch_url = role_branch_url(mp_run["branch_url"], mp_remote_branch_name)`
                // right here.
                if let Ok(base) = mp_run.branch_url.parse::<url::Url>() {
                    target_branch_url =
                        Some(role_branch_url(&base, Some(&mp_run.remote_branch_name)));
                }
            }
            Err(_) => {
                log::info!(
                    "{}: Forge does not support renaming target branch; closing proposal.",
                    mp_url
                );
                if let (Some(rev), Some(target)) = (revision.as_ref(), target_branch_url.as_ref()) {
                    let comment = format!(
                        "This merge proposal will be closed, since the branch for the role '{}' has changed from {} to {}.",
                        mp_run.role, mp_run.remote_branch_name, new_name
                    );
                    match abandon_or_close_mp_async(
                        &mut proposal_info_manager,
                        mp,
                        rev,
                        Some(mp_run.codebase.as_str()),
                        &target.to_string(),
                        Some(mp_run.campaign.as_str()),
                        can_be_merged,
                        rate_limit_bucket.as_deref(),
                        Some(&comment),
                        AbandonMode::Abandoned,
                    )
                    .await
                    {
                        Ok(()) => return Ok(true),
                        Err(BrzError::PermissionDenied(_, _)) => return Ok(false),
                        Err(e) => {
                            log::warn!("Failed to abandon merge proposal {}: {}", mp_url, e);
                            return Ok(false);
                        }
                    }
                }
                return Ok(false);
            }
        }
    }

    // Branch-URL drift between the run that produced the proposal and
    // the latest run for the same codebase/campaign. When the new run
    // points to a different branch, we can't republish the same
    // proposal against it, so skip.
    let mp_branch = mp_run.branch_url.parse::<url::Url>().ok();
    let last_run_branch = last_run.branch_url.parse::<url::Url>().ok();
    if !branches_match(mp_branch.as_ref(), last_run_branch.as_ref()) {
        log::warn!(
            "{}: Remote branch URL appears to have moved: {:?} -> {:?}",
            mp_url,
            mp_run.branch_url,
            last_run.branch_url,
        );
        return Ok(false);
    }

    // New run available: republish the proposal.
    if last_run.id != mp_run.id {
        if last_run_revision.as_ref().map(|r| r.to_string()) == Some(mp_run.revision.to_string()) {
            log::warn!(
                "{} ({}): old run ({}/{}) has same revision as new run ({}/{}): {:?}",
                mp_url,
                mp_run.codebase,
                mp_run.id,
                mp_run.role,
                last_run.id,
                mp_run.role,
                mp_run.revision
            );
            return Ok(false);
        }

        let publish_id = uuid::Uuid::new_v4().to_string();
        log::info!(
            "{} ({}) needs to be updated ({} -> {}).",
            mp_url,
            mp_run.codebase,
            mp_run.id,
            last_run.id
        );

        // Resolve target_branch_url - we need a real URL here to call
        // publish_one. If the proposal didn't carry one we already
        // bailed out earlier in section (7i)'s branches_match check,
        // but be defensive.
        let target_branch_url_for_publish = match target_branch_url.as_ref() {
            Some(u) => u.clone(),
            None => {
                log::warn!(
                    "{}: no target branch URL available for republish; skipping",
                    mp_url
                );
                return Ok(false);
            }
        };

        // The revision passed to publish_one is the revision of the
        // result branch for this role on the new run. We already
        // computed last_run_revision above but it may be None for
        // legacy rows; without it we can't proceed.
        let last_run_revision_value = match last_run_revision.as_ref() {
            Some(r) => r.clone(),
            None => {
                log::warn!(
                    "{}: new run {} has no revision for role {}; skipping",
                    mp_url,
                    last_run.id,
                    mp_run.role
                );
                return Ok(false);
            }
        };

        // derived_branch_name: prefer the source branch name we
        // extracted from the MP earlier; fall back to computing it
        // from the new run + campaign config.
        let derived_name = match source_branch_name.as_deref() {
            Some(name) => name.to_string(),
            None => derived_branch_name(conn, campaign_config, &last_run, &mp_run.role).await?,
        };

        // unchanged_run_id: most recent successful run on the same
        // codebase + main_branch_revision.
        let unchanged_run_id: Option<String> =
            if let Some(main_rev) = last_run.main_branch_revision.as_ref() {
                sqlx::query_scalar::<_, String>(
                    "SELECT id FROM run \
                     WHERE codebase = $1 AND revision = $2 AND result_code = 'success' \
                     ORDER BY finish_time DESC LIMIT 1",
                )
                .bind(&last_run.codebase)
                .bind(main_rev.to_string())
                .fetch_optional(conn)
                .await?
            } else {
                None
            };

        // Resolve the right VcsManager for the new run's vcs_type.
        let vcs_type_for_publish =
            match <VcsType as std::str::FromStr>::from_str(&last_run.vcs_type) {
                Ok(v) => v,
                Err(e) => {
                    log::warn!(
                        "{}: unknown vcs_type {:?} for new run {}: {}",
                        mp_url,
                        last_run.vcs_type,
                        last_run.id,
                        e
                    );
                    return Ok(false);
                }
            };
        let vcs_manager_for_publish = match vcs_managers.get(&vcs_type_for_publish) {
            Some(m) => m.as_ref(),
            None => {
                log::warn!(
                    "{}: no VcsManager registered for vcs_type {:?}",
                    mp_url,
                    vcs_type_for_publish
                );
                return Ok(false);
            }
        };

        // Convert the run's result_tags from Vec<(String, String)> to
        // Vec<(String, RevisionId)> the way publish_one wants them.
        let tags_for_publish: Option<Vec<(String, RevisionId)>> =
            last_run.result_tags.as_ref().map(|tags| {
                tags.iter()
                    .map(|(name, rev)| (name.clone(), RevisionId::from(rev.as_bytes().to_vec())))
                    .collect()
            });

        let codemod_result = last_run.result.clone().unwrap_or(serde_json::Value::Null);
        let extra_context = serde_json::json!({});

        // Don't pass the bucket_rate_limiter through publish_one
        // here: the std::sync::Mutex guard around it is !Send and
        // can't be held across the publish_one .await without making
        // the entire check_existing_mp future !Send. publish_one
        // only ever increments the rate limiter when it creates a
        // brand-new proposal (is_new=true), which in the
        // check_existing_mp republish path is the rare "default
        // branch changed" edge case rather than the common path.
        // Skipping the increment here means that one edge case
        // doesn't count toward the rate-limit budget; the regular
        // open-new-proposal path elsewhere still does.
        let publish_outcome = publish_worker
            .publish_one(
                &last_run.suite,
                &last_run.codebase,
                &last_run.command,
                &target_branch_url_for_publish,
                Mode::Propose,
                &mp_run.role,
                &last_run_revision_value,
                &last_run.id,
                unchanged_run_id.as_deref().unwrap_or(""),
                &derived_name,
                rate_limit_bucket.as_deref(),
                vcs_manager_for_publish,
                None,  // bucket_rate_limiter: see comment above
                false, // require_binary_diff
                true,  // allow_create_proposal
                None,  // reviewers
                tags_for_publish,
                campaign_config.merge_proposal.commit_message.as_deref(),
                campaign_config.merge_proposal.title.as_deref(),
                &codemod_result,
                Some(&mp_url),
                Some(&extra_context),
                None, // derived_owner
                None, // auto_merge
            )
            .await;

        // Route the publish_one outcome through store_publish - and,
        // for the empty-merge-proposal failure code, close the
        // proposal as applied like Python does.
        match publish_outcome {
            Ok(publish_result) => {
                let description = "Successfully updated".to_string();
                if let Err(e) = crate::state::store_publish(
                    conn,
                    &last_run.change_set,
                    &last_run.codebase,
                    Some(publish_result.branch_name()),
                    Some(publish_result.target_branch_url()),
                    publish_result.target_branch_web_url().map(|u| u.as_str()),
                    last_run_base_revision.as_ref(),
                    last_run_revision.as_ref(),
                    &mp_run.role,
                    Mode::Propose,
                    "success",
                    &description,
                    publish_result.proposal_url(),
                    Some(publish_id.as_str()),
                    Some("publisher (regular refresh)"),
                    Some(&last_run.id),
                )
                .await
                {
                    log::warn!("store_publish failed for {}: {}", mp_url, e);
                }
                if publish_result.is_new() == Some(true) {
                    log::warn!(
                        "Intended to update proposal {}, but created {:?}",
                        mp_url,
                        publish_result.proposal_url()
                    );
                }
                return Ok(true);
            }
            Err(PublishError::BranchBusy(busy_url)) => {
                log::info!("{}: Branch {} was busy while publishing", mp_url, busy_url);
                return Ok(false);
            }
            Err(publish_err) => {
                let (mut code, mut description) = crate::handle_publish_failure(
                    &publish_err,
                    conn,
                    &last_run,
                    "update-existing-mp",
                )
                .await?;
                if code == "empty-merge-proposal" {
                    log::info!("{}: Empty merge proposal; closing as applied.", mp_url);
                    if let Some(rev) = revision.as_ref() {
                        match abandon_or_close_mp_async(
                            &mut proposal_info_manager,
                            mp,
                            rev,
                            codebase.as_deref(),
                            target_branch_url_for_publish.as_str(),
                            Some(mp_run.campaign.as_str()),
                            can_be_merged,
                            rate_limit_bucket.as_deref(),
                            Some(
                                "This merge proposal will be closed, since all remaining changes have been applied independently.",
                            ),
                            AbandonMode::Applied,
                        )
                        .await
                        {
                            Ok(()) => {
                                code = "success".to_string();
                                description =
                                    "Closing merge request for which changes were applied independently"
                                        .to_string();
                            }
                            Err(BrzError::PermissionDenied(_, msg)) => {
                                let msg = msg.unwrap_or_default();
                                log::warn!(
                                    "Permission denied closing merge request {}: {}",
                                    mp_url,
                                    msg
                                );
                                code = "empty-failed-to-close".to_string();
                                description = format!(
                                    "Permission denied closing merge request: {}",
                                    msg
                                );
                            }
                            Err(e) => {
                                log::warn!(
                                    "Failed to close empty merge proposal {}: {}",
                                    mp_url,
                                    e
                                );
                            }
                        }
                    }
                }
                if code != "success" {
                    log::info!(
                        "{}: Updating merge proposal failed: {} ({})",
                        mp_url,
                        code,
                        description
                    );
                }
                if let Err(e) = crate::state::store_publish(
                    conn,
                    &last_run.change_set,
                    &last_run.codebase,
                    campaign_config.branch_name.as_deref(),
                    Some(&target_branch_url_for_publish),
                    None,
                    last_run_base_revision.as_ref(),
                    last_run_revision.as_ref(),
                    &mp_run.role,
                    Mode::Propose,
                    &code,
                    &description,
                    Some(&mp_url),
                    Some(publish_id.as_str()),
                    Some("publisher (regular refresh)"),
                    Some(&last_run.id),
                )
                .await
                {
                    log::warn!("store_publish failed for {}: {}", mp_url, e);
                }
                return Ok(true);
            }
        }
    }

    // Same run as before. If the proposal can't currently be
    // merged we treat it as a conflict and reschedule.
    if can_be_merged == Some(false) {
        log::info!("{} can not be merged (conflict?). Rescheduling.", mp_url);
        if let Err(e) = janitor::schedule::do_schedule(
            conn,
            &mp_run.campaign,
            &mp_run.codebase,
            "update-existing-mp",
            Some(&mp_run.change_set),
            None,
            true,
            Some("publisher (merge conflict)"),
            None,
            None,
        )
        .await
        {
            log::warn!(
                "Failed to reschedule {}/{} after merge conflict: {}",
                mp_run.codebase,
                mp_run.campaign,
                e
            );
        }
    }

    Ok(false)
}

/// Distinguish between abandon ("we are giving up on this proposal")
/// and close-as-applied ("the changes have already been merged in
/// some other way"). Used by [`abandon_or_close_mp_async`] to pick
/// the appropriate persisted status.
#[derive(Debug, Clone, Copy)]
enum AbandonMode {
    Abandoned,
    Applied,
}

/// Tiny wrapper that picks abandon_mp vs close_applied_mp based on
/// `mode`. Lets [`check_existing_mp`] call a single function in
/// the half-dozen places it transitions a proposal out of the
/// `open` state.
#[allow(clippy::too_many_arguments)]
async fn abandon_or_close_mp_async(
    proposal_info_manager: &mut proposal_info::ProposalInfoManager,
    mp: &breezyshim::forge::MergeProposal,
    revision: &RevisionId,
    codebase: Option<&str>,
    target_branch_url: &str,
    campaign: Option<&str>,
    can_be_merged: Option<bool>,
    rate_limit_bucket: Option<&str>,
    comment: Option<&str>,
    mode: AbandonMode,
) -> Result<(), BrzError> {
    match mode {
        AbandonMode::Abandoned => {
            abandon_mp(
                proposal_info_manager,
                mp,
                revision,
                codebase,
                target_branch_url,
                campaign,
                can_be_merged,
                rate_limit_bucket,
                comment,
            )
            .await
        }
        AbandonMode::Applied => {
            close_applied_mp(
                proposal_info_manager,
                mp,
                revision,
                codebase,
                target_branch_url,
                campaign,
                can_be_merged,
                rate_limit_bucket,
                comment,
            )
            .await
        }
    }
}

/// Helper: bump `merge_proposal.last_scanned` for a proposal that
/// didn't need a full update_proposal_info refresh.
async fn bump_last_scanned(conn: &sqlx::PgPool, mp_url: &url::Url) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE merge_proposal SET last_scanned = NOW() WHERE url = $1")
        .bind(mp_url.to_string())
        .execute(conn)
        .await?;
    Ok(())
}

/// Freshness window for the `check_existing_mp` skip gate. MPs
/// whose persisted `last_scanned` is within this window and whose
/// persisted status still matches the forge-reported one bypass
/// the per-MP forge fetches. 24h is conservative - long enough
/// to amortise the scan cost across multiple cycles, short enough
/// that a maintainer's merge action lands in our DB the next day.
const CHECK_EXISTING_MP_FRESH_THRESHOLD: chrono::Duration = chrono::Duration::hours(24);

/// Pure helper: should `check_existing_mp` skip the expensive
/// per-MP forge fetches because the persisted state is recent and
/// the forge-reported status still matches?
///
/// `forge_status` is the coarse `Open / Merged / Closed` flavour
/// the forge yields. `persisted_status` is whichever
/// publisher-level value (`open|merged|closed|abandoned|applied|
/// rejected`) the publisher last persisted - `closed` and the
/// closed-family values (`abandoned|applied|rejected`) all map to
/// the forge's `Closed`.
///
/// Returning `true` means "trust the persisted record; just bump
/// last_scanned and move on". `false` means we need the full scan
/// (no record, missing timestamp, stale, mismatched status, or
/// unknown persisted value).
pub(crate) fn check_existing_mp_should_skip_per_mp_fetch(
    forge_status: breezyshim::forge::MergeProposalStatus,
    persisted_status: &str,
    last_scanned: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
    threshold: chrono::Duration,
) -> bool {
    let persisted_kind = match persisted_status {
        "open" => breezyshim::forge::MergeProposalStatus::Open,
        "merged" => breezyshim::forge::MergeProposalStatus::Merged,
        // Forge `Closed` is the publisher's `closed` plus the
        // carry-forward variants (the publisher can never tell
        // them apart from the forge alone, so we treat them as
        // the same family for skip purposes).
        "closed" | "abandoned" | "applied" | "rejected" => {
            breezyshim::forge::MergeProposalStatus::Closed
        }
        _ => return false,
    };
    // The persisted status has to agree with what the forge is
    // reporting on this pass; a mismatch is a transition we still need
    // the per-MP fetch to record.
    if persisted_kind != forge_status {
        return false;
    }
    // Terminal statuses (merged/closed and the carry-forward variants)
    // never change again, and `iter_all_mps` already re-confirms the
    // current status on every pass - so once we hold a persisted record
    // there is nothing left to fetch per-MP. Skip regardless of age
    // (and regardless of a missing `last_scanned`). This is what keeps
    // a full scan tractable: the ~17k merged/closed proposals we own
    // would otherwise each re-run ~5 network round-trips whenever their
    // (historically imported) last_scanned aged past the freshness
    // window, so a single pass never finished and the bucket counts
    // that gate publishing were never refreshed.
    if forge_status != breezyshim::forge::MergeProposalStatus::Open {
        return true;
    }
    // Open proposals are volatile (new revisions, can_be_merged, an
    // impending merge/close): re-fetch once the record ages past the
    // freshness window.
    match last_scanned {
        Some(last_scanned) => now.signed_duration_since(last_scanned) <= threshold,
        None => false,
    }
}

/// Helper: finish a check_existing_mp call early when there's
/// nothing more we can do (missing target URL, non-open status,
/// check_only). Bumps `mps_per_bucket` and returns the appropriate
/// "no action taken" success flag.
fn finish_check_existing_mp_no_action(
    status: breezyshim::forge::MergeProposalStatus,
    effective_status: janitor::publish::MergeProposalStatus,
    rate_limit_bucket: Option<&str>,
    mps_per_bucket: Option<
        &mut HashMap<janitor::publish::MergeProposalStatus, HashMap<String, usize>>,
    >,
    check_only: bool,
) -> Result<bool, CheckMpError> {
    if let (Some(mps_per_bucket), Some(bucket)) = (mps_per_bucket, rate_limit_bucket) {
        *mps_per_bucket
            .entry(effective_status)
            .or_default()
            .entry(bucket.to_string())
            .or_insert(0) += 1;
    }
    if status != breezyshim::forge::MergeProposalStatus::Open || check_only {
        return Ok(false);
    }
    Ok(false)
}

/// Iterate over all merge proposals.
///
/// # Arguments
/// * `statuses` - Optional list of statuses to filter by
///
/// # Returns
/// An iterator over results containing the forge, merge proposal, and status
pub fn iter_all_mps(
    statuses: Option<&[breezyshim::forge::MergeProposalStatus]>,
) -> impl Iterator<
    Item = Result<
        (
            Forge,
            breezyshim::forge::MergeProposal,
            breezyshim::forge::MergeProposalStatus,
        ),
        BrzError,
    >,
> + '_ {
    let statuses = statuses.unwrap_or(&[
        breezyshim::forge::MergeProposalStatus::Open,
        breezyshim::forge::MergeProposalStatus::Closed,
        breezyshim::forge::MergeProposalStatus::Merged,
    ]);

    // Lazy variant: each item from `iter_my_proposals_lazy` is itself
    // a `Result<MergeProposal, Error>`, so a per-item Python exception
    // (the GitHub plugin checks auth on first `__next__`, GitLab can
    // 502 mid-page, ...) is surfaced as a normal stream error instead of
    // panicking out of the iterator. The eager `iter_my_proposals`
    // wrapper short-circuits on the first such error and loses every
    // proposal after it; the lazy form lets us keep going.
    //
    // Top-level errors (e.g. login required for the whole forge)
    // still come back from the call itself and short-circuit the
    // forge - we want that, since a credential failure is per-forge,
    // not per-MP.
    breezyshim::forge::iter_forge_instances().flat_map(move |forge| {
        statuses
            .iter()
            .filter_map(
                move |&status| match forge.iter_my_proposals_lazy(Some(status), None) {
                    Ok(proposals) => {
                        let forge_for_items = forge.clone();
                        let forge_for_errors = forge.clone();
                        Some(proposals.filter_map(move |item| match item {
                            Ok(proposal) => Some(Ok((forge_for_items.clone(), proposal, status))),
                            Err(e) => {
                                log::warn!(
                                    "Skipping a proposal on forge {} ({}): {}",
                                    forge_for_errors.forge_name(),
                                    status,
                                    e
                                );
                                None
                            }
                        }))
                    }
                    Err(BrzError::ForgeLoginRequired) => {
                        log::info!(
                            "Skipping forge {}, no credentials known",
                            forge.forge_name()
                        );
                        None
                    }
                    Err(BrzError::UnexpectedHttpStatus { .. }) => {
                        log::warn!(
                            "Got unexpected HTTP status, skipping forge {}",
                            forge.forge_name()
                        );
                        None
                    }
                    Err(BrzError::UnsupportedForge(msg)) => {
                        log::warn!("Unsupported forge {}: {}", forge.forge_name(), msg);
                        None
                    }
                    Err(e) => {
                        log::error!(
                            "Error iterating proposals for forge {}: {}",
                            forge.forge_name(),
                            e
                        );
                        None
                    }
                },
            )
            .flatten()
    })
}

async fn check_existing(
    conn: sqlx::PgPool,
    redis: Option<RedisConnectionManager>,
    config: &janitor::config::Config,
    publish_worker: &crate::PublishWorker,
    bucket_rate_limiter: &Mutex<Box<dyn crate::rate_limiter::RateLimiter>>,
    forge_rate_limiter: Arc<RwLock<HashMap<String, chrono::DateTime<Utc>>>>,
    vcs_managers: &HashMap<VcsType, Box<dyn VcsManager>>,
    modify_limit: Option<i32>,
    unexpected_limit: Option<i32>,
) -> bool {
    let mut mps_per_bucket = maplit::hashmap! {
        MergeProposalStatus::Open => maplit::hashmap! {},
        MergeProposalStatus::Closed => maplit::hashmap! {},
        MergeProposalStatus::Merged => maplit::hashmap! {},
        MergeProposalStatus::Applied => maplit::hashmap! {},
        MergeProposalStatus::Abandoned => maplit::hashmap! {},
        MergeProposalStatus::Rejected => maplit::hashmap! {},
    };
    let mut possible_transports = Vec::new();
    let mut status_count = maplit::hashmap! {
        MergeProposalStatus::Open => 0,
        MergeProposalStatus::Closed => 0,
        MergeProposalStatus::Merged => 0,
        MergeProposalStatus::Applied => 0,
        MergeProposalStatus::Abandoned => 0,
        MergeProposalStatus::Rejected => 0,
    };

    let mut modified_mps = 0;
    let mut unexpected = 0;
    let mut check_only = false;
    let mut was_forge_ratelimited = false;
    // Count MPs whose `revision` doesn't join to any `run` row and log
    // one summary warning at the end of the scan instead of ~1 warn per
    // MP. Production scans emitted >1300 such lines per 6h, drowning
    // every other publish signal in the logs (BUGS.md #6). Keeps the
    // first-offender url so a quick `grep -F` still hits a sample.
    let mut no_run_for_mp: usize = 0;
    let mut no_run_first: Option<url::Url> = None;
    // iter_my_proposals_lazy yields Result items. The pre-fix loop did
    // `filter_map(Result::ok)` which discarded Errs silently - and the
    // GitLab/GitHub plugins return None on the next __next__ after a
    // Python exception, so a single transient forge error terminated
    // the whole scan with 0 log lines and Open|Closed|Merged left
    // un-rescanned for the rest of the cycle (production: 678/18904
    // MPs covered before silent stop). Walk explicitly so each Err
    // gets logged and the loop continues.
    let mut iter_errors: usize = 0;
    for item in iter_all_mps(None) {
        let (forge, mp, status) = match item {
            Ok(t) => t,
            Err(e) => {
                iter_errors += 1;
                if iter_errors <= 5 {
                    log::warn!("iter_all_mps yielded error (continuing): {}", e);
                } else if iter_errors == 6 {
                    log::warn!("iter_all_mps: further errors suppressed; will summarise at end");
                }
                continue;
            }
        };
        *status_count.entry(status.into()).or_insert(0) += 1;
        // Key the per-forge rate-limit map on `base_url` rather than
        // the human-readable `forge_name` string. Two forges can share
        // a `forge_name` (e.g. two GitHub Enterprise deployments), so
        // using the name alone would let one forge's 429 silence the
        // other. Python uses object identity for the same reason;
        // `base_url` is the closest stable approximation we have.
        let forge_key = forge.base_url().to_string();
        let retry_after = match forge_rate_limiter.read() {
            Ok(guard) => guard.get(&forge_key).copied(),
            Err(e) => {
                log::error!("RwLock poisoned in check_all_mps: {}", e);
                continue;
            }
        };
        if let Some(retry_after) = retry_after {
            if chrono::Utc::now() < retry_after {
                match forge_rate_limiter.write() {
                    Ok(mut guard) => guard.remove(&forge_key),
                    Err(e) => {
                        log::error!("RwLock poisoned when removing rate limit: {}", e);
                        continue;
                    }
                };
            } else {
                was_forge_ratelimited = true;
                continue;
            }
        }
        let modified = match check_existing_mp(
            &conn,
            redis.clone(),
            config,
            publish_worker,
            &mp,
            status,
            vcs_managers,
            bucket_rate_limiter,
            check_only,
            Some(&mut mps_per_bucket),
            Some(&mut possible_transports),
        )
        .await
        {
            Ok(modified) => modified,
            Err(CheckMpError::NoRunForMergeProposal(url)) => {
                no_run_for_mp += 1;
                if no_run_first.is_none() {
                    no_run_first = Some(url.clone());
                }
                log::debug!("Unable to find metadata for {}, skipping.", url);
                false
            }
            Err(CheckMpError::ForgeLoginRequired) => {
                log::warn!("Login required, skipping.");
                false
            }
            Err(CheckMpError::BranchRateLimited { retry_after }) => {
                let mp_url_str = mp
                    .url()
                    .map(|u| u.to_string())
                    .unwrap_or_else(|_| "<unknown URL>".to_string());
                log::warn!(
                    "Rate-limited accessing {}. Skipping {:?} for this cycle.",
                    mp_url_str,
                    forge
                );
                let retry_after = if let Some(retry_after) = retry_after {
                    retry_after
                } else {
                    chrono::Duration::minutes(30)
                };
                match forge_rate_limiter.write() {
                    Ok(mut limiter) => {
                        limiter.insert(
                            forge.base_url().to_string(),
                            chrono::Utc::now() + retry_after,
                        );
                    }
                    Err(e) => {
                        log::error!("Failed to acquire write lock on rate limiter: {}", e);
                        // Continue anyway - better to skip rate limiting than panic
                    }
                }
                crate::metrics::FORGE_RATE_LIMITED_COUNT
                    .with_label_values(&[forge.forge_name().as_str()])
                    .inc();
                continue;
            }
            Err(CheckMpError::UnexpectedHttpStatus) => {
                let mp_url = mp
                    .url()
                    .map(|u| u.to_string())
                    .unwrap_or_else(|_| "unknown".to_string());
                log::warn!("Got unexpected HTTP status for {}", mp_url);
                // Enhanced error logging with available context information
                log::debug!(
                    "Unexpected HTTP status context: MP URL: {}, Forge: {}",
                    mp_url,
                    forge.forge_name()
                );
                crate::metrics::UNEXPECTED_HTTP_RESPONSE_COUNT.inc();
                unexpected += 1;
                true
            }
            Err(CheckMpError::Database(e)) => {
                log::error!("Database error checking merge proposal: {}", e);
                unexpected += 1;
                false
            }
            Err(CheckMpError::Brz(msg)) => {
                log::warn!("Forge call failed checking merge proposal: {}", msg);
                false
            }
        };

        if let Some(limit) = unexpected_limit {
            if unexpected > limit {
                log::warn!(
                    "Saw {} unexpected HTTP responses, over threshold of {}. Giving up for now.",
                    unexpected,
                    limit,
                );
                // Bailed mid-pass: bucket counts are incomplete, so this
                // is not a full scan and must not open the publish gate.
                return false;
            }
        }

        if modified {
            modified_mps += 1;
            if modify_limit.map(|ml| modified_mps > ml).unwrap_or(false) {
                log::warn!(
                    "Already modified {} merge proposals, waiting with the rest.",
                    modified_mps,
                );
                check_only = true;
            }
        }
    }

    let scanned_total: i32 = status_count.values().sum();
    log::info!(
        "Successfully scanned {} existing merge proposals ({} iter errors)",
        scanned_total,
        iter_errors,
    );
    if no_run_for_mp > 0 {
        log::warn!(
            "{} merge proposal(s) have no matching run row in this scan (e.g. {}); \
             candidates for reaping or schema cascade (BUGS.md #6).",
            no_run_for_mp,
            no_run_first
                .as_ref()
                .map(|u| u.as_str())
                .unwrap_or("<none>"),
        );
    }
    // Unix seconds since epoch. Safe to set before or after the
    // rate-limit branch: dashboards plotting "time since last
    // successful scan" only care about the timestamp.
    crate::metrics::LAST_SCAN_EXISTING_SUCCESS.set(chrono::Utc::now().timestamp() as f64);

    if !was_forge_ratelimited {
        // Push the fresh bucket counts into the rate limiter so future
        // publish attempts see an up-to-date view of open proposals
        // per bucket. Python does this at publish.py:3328 after the
        // check_existing walk; without it, FixedRateLimiter /
        // SlowStartRateLimiter stay in their conservative initial
        // state and gate off publishes that would otherwise be
        // allowed.
        match bucket_rate_limiter.lock() {
            Ok(mut limiter) => limiter.set_mps_per_bucket(&mps_per_bucket),
            Err(e) => log::error!("check_existing: bucket_rate_limiter mutex poisoned: {}", e),
        }

        // Republish Prometheus gauges. Matches Python
        // publish.py:3325-3334: merge_proposal_count{status} for
        // every status bucket, bucket_proposal_count{bucket} for
        // the open buckets, and open_proposal_count as the sum.
        for (status, bucket_counts) in &mps_per_bucket {
            let total_for_status: usize = bucket_counts.values().sum();
            crate::metrics::MERGE_PROPOSAL_COUNT
                .with_label_values(&[&status.to_string()])
                .set(total_for_status as f64);
        }
        let mut total_open = 0usize;
        if let Some(open_buckets) = mps_per_bucket.get(&janitor::publish::MergeProposalStatus::Open)
        {
            for (bucket, count) in open_buckets {
                total_open += count;
                crate::metrics::BUCKET_PROPOSAL_COUNT
                    .with_label_values(&[bucket.as_str()])
                    .set(*count as f64);
            }
        }
        crate::metrics::OPEN_PROPOSAL_COUNT.set(total_open as f64);
        log::debug!("Total open merge proposals across buckets: {}", total_open);
    } else {
        log::info!(
            "Rate-Limited for forges {:?}. Not updating stats",
            forge_rate_limiter
        );
    }

    // A pass counts as complete only when we walked the whole
    // enumeration without a forge rate-limit cutting it short; only then
    // is `mps_per_bucket` a full picture the rate limiter (and the
    // publish gate) can trust.
    !was_forge_ratelimited
}

/// Decide whether to publish a single publish-ready run, then dispatch
/// one `publish_from_policy` call per role.
///
/// Returns a `role -> Option<mode>` map. A `Some(mode)` entry means
/// `publish_from_policy` actually published something for that role
/// (with `mode` the effective mode; AttemptPush may collapse to Propose
/// or Push). A `None` entry means the publish was gated off
/// (rate-limited, max frequency, already published, etc.), not that it
/// failed.
///
/// Pre-flight bails return a single-entry map `{"__status": Some(reason)}`.
/// Reasons: `exponential_backoff`, `push_limit_reached`,
/// `rejected_last_mp`, `missing_branch_url`, `no_revision`.
#[must_use]
pub(crate) fn wants_push_mode(branches: &[crate::state::UnpublishedBranch]) -> bool {
    branches.iter().any(|b| {
        matches!(
            b.publish_mode.as_deref(),
            Some("push") | Some("attempt-push")
        )
    })
}

/// Skip this run because the global push budget is exhausted: at least
/// one branch wants push and the limit is zero.
#[must_use]
pub(crate) fn should_skip_for_push_limit(wants_push: bool, push_limit: Option<usize>) -> bool {
    matches!(push_limit, Some(0)) && wants_push
}

/// Is the change_set in a state that allows publication? Only
/// `publishing` and `ready` do.
#[must_use]
pub(crate) fn change_set_state_allows_publish(state: &str) -> bool {
    matches!(state, "publishing" | "ready")
}

/// Compute `remaining` for the rate-limit endpoint:
/// `Some(max.saturating_sub(current))` when both are known, `None`
/// otherwise. Saturating matters: a slow race can let `current` exceed
/// `max` momentarily, and we don't want to underflow `usize`.
pub(crate) fn rate_limit_remaining(
    current_open: Option<usize>,
    max_open: Option<usize>,
) -> Option<usize> {
    match (current_open, max_open) {
        (Some(c), Some(m)) => Some(m.saturating_sub(c)),
        _ => None,
    }
}

/// Does the previous merge-proposal status block a republish? Any prior
/// MP marked `rejected` or `closed` blocks. Empty input returns false.
#[must_use]
pub(crate) fn previous_mp_blocks_publish(statuses: &[(String, String)]) -> bool {
    statuses
        .iter()
        .any(|(_role, status)| status == "rejected" || status == "closed")
}

/// Pick the URL `publish_from_policy` should publish to: the explicit
/// `target_branch_url` wins, but a missing or empty value falls through
/// to the run's `branch_url`.
pub(crate) fn resolve_target_branch_url<'a>(
    target_branch_url: Option<&'a str>,
    branch_url: &'a str,
) -> &'a str {
    target_branch_url
        .filter(|s| !s.is_empty())
        .unwrap_or(branch_url)
}

/// Should the per-branch loop skip the `main` role because an earlier
/// auxiliary branch failed? True when `role == "main"` and any
/// previously-published branch in this run came back with no effective
/// mode.
#[must_use]
pub(crate) fn should_skip_main_after_aux_failure(
    role: &str,
    actual_modes: &HashMap<String, Option<String>>,
) -> bool {
    role == "main" && actual_modes.values().any(|v| v.is_none())
}

#[allow(clippy::too_many_arguments)]
async fn consider_publish_run(
    conn: &sqlx::PgPool,
    _redis: Option<RedisConnectionManager>,
    config: &janitor::config::Config,
    publish_worker: &crate::PublishWorker,
    vcs_managers: &HashMap<VcsType, Box<dyn VcsManager>>,
    bucket_rate_limiter: &Mutex<Box<dyn crate::rate_limiter::RateLimiter>>,
    run: &janitor::state::Run,
    rate_limit_bucket: &str,
    unpublished_branches: &[crate::state::UnpublishedBranch],
    command: &str,
    push_limit: Option<usize>,
    require_binary_diff: bool,
) -> Result<HashMap<String, Option<String>>, PublishError> {
    log::info!(
        "Considering publish for run {} (campaign: {}, codebase: {})",
        run.id,
        run.suite,
        run.codebase
    );

    // Helper: return a single-key "__status" map so the caller can
    // tell pre-flight skips apart from the real per-role result map.
    fn status_only(reason: &str) -> HashMap<String, Option<String>> {
        let mut m = HashMap::new();
        m.insert("__status".to_string(), Some(reason.to_string()));
        m
    }

    if run.revision.is_none() {
        log::warn!(
            "Run {} is publish ready, but does not have revision set.",
            run.id
        );
        return Ok(status_only("no_revision"));
    }

    let campaign_config = match config.campaign.iter().find(|c| c.name() == run.suite) {
        Some(c) => c,
        None => {
            log::warn!("No campaign configuration found for suite {}", run.suite);
            return Ok(status_only("no_campaign_config"));
        }
    };

    // Exponential backoff: skip the run if the previous attempt was
    // too recent. Matches py/janitor/publish.py::consider_publish_run.
    let attempt_count = match run.revision.as_ref() {
        Some(rev) => get_publish_attempt_count(conn, rev, &["differ-unreachable"]).await?,
        None => 0,
    };
    let next_try_time = calculate_next_try_time(run.finish_time, attempt_count);
    if chrono::Utc::now() < next_try_time {
        log::info!(
            "Not attempting to push {} / {} ({}) due to exponential backoff. Next try at {}.",
            run.codebase,
            run.suite,
            run.id,
            next_try_time
        );
        crate::metrics::EXPONENTIAL_BACKOFF_COUNT.inc();
        return Ok(status_only("exponential_backoff"));
    }

    // Push limit: if any branch wants push/attempt-push and the
    // caller has burned through its push budget, bail out.
    let wants_push = wants_push_mode(unpublished_branches);
    if should_skip_for_push_limit(wants_push, push_limit) {
        log::info!(
            "Not pushing {} / {}: push limit reached",
            run.codebase,
            run.suite
        );
        crate::metrics::PUSH_LIMIT_COUNT.inc();
        return Ok(status_only("push_limit_reached"));
    }

    // Missing branch_url: can't publish anywhere.
    if run.branch_url.is_empty() {
        log::warn!(
            "{}: considering publishing for branch without branch url",
            run.id
        );
        crate::metrics::MISSING_BRANCH_URL_COUNT.inc();
        return Ok(status_only("missing_branch_url"));
    }

    // Rejected/closed previous MP: the maintainer has explicitly
    // said no, so don't try again for this campaign/codebase.
    let previous_mp_status =
        crate::state::get_previous_mp_status(conn, &run.codebase, &run.suite).await?;
    if previous_mp_blocks_publish(&previous_mp_status) {
        log::warn!(
            "{}: last merge proposal was rejected by maintainer: {:?}",
            run.id,
            previous_mp_status
        );
        crate::metrics::REJECTED_LAST_MP_COUNT.inc();
        return Ok(status_only("rejected_last_mp"));
    }

    // Resolve the target branch URL that publish_from_policy will
    // feed into role_branch_url for each role. Python uses
    // `run.target_branch_url or run.branch_url`.
    let target_branch_url_str =
        resolve_target_branch_url(run.target_branch_url.as_deref(), &run.branch_url);
    let target_branch_url: url::Url = match target_branch_url_str.parse() {
        Ok(u) => u,
        Err(e) => {
            log::warn!(
                "{}: invalid branch_url {:?}: {}",
                run.id,
                target_branch_url_str,
                e
            );
            return Ok(status_only("invalid_branch_url"));
        }
    };

    let mut actual_modes: HashMap<String, Option<String>> = HashMap::new();

    for branch in unpublished_branches {
        let role = &branch.role;
        let mode_str = match branch.publish_mode.as_deref() {
            Some(m) if !m.is_empty() => m,
            _ => {
                log::warn!("{}: No publish mode for branch with role {}", run.id, role);
                crate::metrics::MISSING_PUBLISH_MODE_COUNT
                    .with_label_values(&[role.as_str()])
                    .inc();
                continue;
            }
        };

        // Main-branch-after-aux-failed guard: if any earlier aux branch
        // recorded a None outcome (failed or no-op), skip main.
        if should_skip_main_after_aux_failure(role, &actual_modes) {
            log::warn!(
                "{}: Skipping branch with role main, as not all auxiliary branches were published.",
                run.id
            );
            crate::metrics::UNPUBLISHED_AUX_BRANCHES_COUNT
                .with_label_values(&[role.as_str()])
                .inc();
            continue;
        }

        let policy_mode = match <Mode as std::str::FromStr>::from_str(mode_str) {
            Ok(m) => m,
            Err(e) => {
                log::warn!(
                    "{}: unknown publish mode {:?} for role {}: {}",
                    run.id,
                    mode_str,
                    role,
                    e
                );
                continue;
            }
        };

        let outcome = publish_from_policy(
            conn,
            publish_worker.redis_manager.as_ref(),
            campaign_config,
            publish_worker,
            bucket_rate_limiter,
            vcs_managers,
            run,
            role,
            Some(rate_limit_bucket),
            &target_branch_url,
            policy_mode,
            branch.max_frequency_days,
            command,
            require_binary_diff,
            false, // force: main queue loop is not an override
            Some("publisher (publish pending)"),
        )
        .await;

        match outcome {
            Ok(Some(effective_mode)) => {
                actual_modes.insert(role.clone(), Some(effective_mode.to_string()));
            }
            Ok(None) => {
                actual_modes.insert(role.clone(), None);
            }
            Err(e) => {
                log::warn!(
                    "publish_from_policy failed for {}/{}/{}: {}",
                    run.codebase,
                    run.suite,
                    role,
                    e
                );
                return Err(PublishError::ServiceUnavailable(format!(
                    "publish_from_policy failed for {}/{}/{}: {}",
                    run.codebase, run.suite, role, e
                )));
            }
        }
    }

    Ok(actual_modes)
}

/// Get the count of previous publish attempts for a revision.
///
/// # Arguments
/// * `conn` - Database connection
/// * `revision` - The revision ID to check
/// * `exclude_codes` - Result codes to exclude from the count
///
/// # Returns
/// The number of previous attempts
async fn get_publish_attempt_count(
    conn: &sqlx::PgPool,
    revision: &breezyshim::RevisionId,
    exclude_codes: &[&str],
) -> Result<usize, sqlx::Error> {
    let revision_str = revision.to_string();

    let count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)
        FROM publish
        WHERE revision = $1
        AND (result_code IS NULL OR NOT (result_code = ANY($2)))
        "#,
    )
    .bind(&revision_str)
    .bind(exclude_codes)
    .fetch_one(conn)
    .await? as usize;

    Ok(count)
}

/// Fetch the publish policy for a (codebase, campaign) pair: the
/// per-branch `role -> (mode, frequency_days)` map, the campaign's
/// current command, and the rate-limit bucket key.
///
/// `named_publish_policy.per_branch_policy` is a PostgreSQL composite
/// array (`branch_publish_policy[]`), not JSON: `UNNEST` at the SQL
/// level so each row comes back as `(role, mode, frequency_days)`.
pub async fn get_publish_policy(
    conn: &sqlx::PgPool,
    codebase: &str,
    campaign: &str,
) -> Result<
    Option<(
        HashMap<String, (String, Option<i32>)>,
        Option<String>,
        Option<String>,
    )>,
    sqlx::Error,
> {
    // First resolve command + rate_limit_bucket from the join, and
    // confirm a candidate row exists. Missing candidate -> None.
    let head: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        r#"
        SELECT candidate.command, named_publish_policy.rate_limit_bucket
        FROM candidate
        LEFT JOIN named_publish_policy
          ON named_publish_policy.name = candidate.publish_policy
        WHERE codebase = $1 AND suite = $2
        "#,
    )
    .bind(codebase)
    .bind(campaign)
    .fetch_optional(conn)
    .await?;

    let (command, rate_limit_bucket) = match head {
        Some(t) => t,
        None => return Ok(None),
    };

    // Expand per_branch_policy into rows via UNNEST. publish_mode
    // is a custom enum; read it as text and rebuild the map.
    let rows: Vec<(String, String, Option<i32>)> = sqlx::query_as(
        r#"
        SELECT pp.role, pp.mode::text, pp.frequency_days
        FROM candidate
        JOIN named_publish_policy
          ON named_publish_policy.name = candidate.publish_policy
        CROSS JOIN UNNEST(named_publish_policy.per_branch_policy) AS pp
        WHERE candidate.codebase = $1 AND candidate.suite = $2
        "#,
    )
    .bind(codebase)
    .bind(campaign)
    .fetch_all(conn)
    .await?;

    let mut policy_map: HashMap<String, (String, Option<i32>)> = HashMap::new();
    for (role, mode, frequency_days) in rows {
        policy_map.insert(role, (mode, frequency_days));
    }

    Ok(Some((policy_map, command, rate_limit_bucket)))
}

// Re-export the test module for testing
#[cfg(test)]
#[path = "lib_tests.rs"]
mod lib_tests;
