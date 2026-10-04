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
    pub by_campaign: HashMap<String, i64>,
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
    pub codebase: Option<String>,
    /// Name of the campaign this run belongs to
    pub campaign: Option<String>,
}

/// Basic information about a run
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunInfo {
    /// Unique identifier for this run
    pub id: String,
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
    pub run_id: String,
    /// Name of the campaign to execute
    pub campaign: String,
    /// URL or identifier of the codebase to work on
    pub codebase: String,
    /// Command to execute for the codemod
    pub command: String,
    /// Version control system information (format depends on VCS type)
    pub vcs_info: serde_json::Value,
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
    pub run_id: String,
    /// Name of the campaign being resumed
    pub campaign: String,
    /// URL or identifier of the codebase
    pub codebase: String,
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
    pub campaign: String,
    /// Identifier for the set of changes this candidate belongs to
    pub change_set: Option<String>,
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
    pub campaign: String,
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
    pub current_run: Option<String>,
}

/// Detailed information about an item in the queue
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueItemDetails {
    /// Unique identifier for this queue item
    pub id: String,
    /// URL or identifier of the codebase to process
    pub codebase: String,
    /// Campaign this item belongs to
    pub campaign: String,
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
}
