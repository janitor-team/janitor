use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Worker status information
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct WorkerStatusInfo {
    pub alive: bool,
    pub current_run_id: Option<String>,
    pub status: String,
    pub uptime_seconds: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub metadata: Option<HashMap<String, serde_json::Value>>,
    /// Where the worker is in the run lifecycle right now —
    /// `"setup"` while cloning, `"codemod"` while make_changes
    /// runs, `"build"` while sbuild runs, `"push"` while pushing
    /// the result branch, `"finish"` after upload starts. Set by
    /// `lib::run_worker` at each transition. Consumed by the
    /// active-run page to decide which log file to auto-open
    /// (build.log during build, worker.log otherwise) so the
    /// operator's eye lands on the file currently being written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_stage: Option<String>,
}

/// Assignment acceptance response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssignmentAcceptance {
    pub accepted: bool,
    pub assignment_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Worker result submission
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerResult {
    pub success: bool,
    pub artifacts: Vec<String>,
    pub logs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codemod_result: Option<serde_json::Value>,
}

/// Worker health check response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerHealthCheck {
    pub healthy: bool,
    pub checks: HashMap<String, HealthCheckResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckResult {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Worker configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerConfig {
    pub max_jobs: u32,
    pub supported_campaigns: Vec<String>,
    pub capabilities: Vec<String>,
}

/// Job progress update
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobProgress {
    pub job_id: String,
    pub progress: f32,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Resource usage information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceUsage {
    pub cpu_percent: f32,
    pub memory_mb: u64,
    pub disk_usage_mb: u64,
}

/// Assignment details
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssignmentDetails {
    pub assignment_id: String,
    pub queue_item: serde_json::Value,
    pub config: serde_json::Value,
}

/// Simple success response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimpleSuccessResponse {
    pub success: bool,
}

/// Error details response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorDetails {
    pub error: String,
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<HashMap<String, serde_json::Value>>,
}