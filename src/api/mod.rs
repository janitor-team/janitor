//! This module contains the API definitions for the various janitor components.
pub mod publish;
pub mod runner;
pub mod worker;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// The publish status of a run.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub enum RunPublishStatus {
    #[serde(rename = "unknown")]
    Unknown,

    #[serde(rename = "blocked")]
    Blocked,

    #[serde(rename = "needs-manual-review")]
    NeedsManualReview,

    #[serde(rename = "rejected")]
    Rejected,

    #[serde(rename = "approved")]
    Approved,

    #[serde(rename = "ignored")]
    Ignored,
}

/// Simple error response used across all services
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl ErrorResponse {
    pub fn new(error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            details: None,
            code: None,
        }
    }

    pub fn with_details(mut self, details: impl Into<String>) -> Self {
        self.details = Some(details.into());
        self
    }

    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    pub fn internal_server_error() -> Self {
        Self::new("Internal server error").with_code("internal_error")
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(message).with_code("bad_request")
    }

    pub fn not_found(resource: impl Into<String>) -> Self {
        Self::new(format!("{} not found", resource.into())).with_code("not_found")
    }

    pub fn unauthorized() -> Self {
        Self::new("Unauthorized").with_code("unauthorized")
    }

    pub fn forbidden() -> Self {
        Self::new("Forbidden").with_code("forbidden")
    }
}

/// Success response with optional data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuccessResponse<T = ()> {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl<T> SuccessResponse<T> {
    pub fn new(data: T) -> Self {
        Self {
            success: true,
            data: Some(data),
            message: None,
        }
    }

    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }
}

impl SuccessResponse<()> {
    pub fn ok() -> Self {
        Self {
            success: true,
            data: None,
            message: None,
        }
    }

    pub fn ok_with_message(message: impl Into<String>) -> Self {
        Self {
            success: true,
            data: None,
            message: Some(message.into()),
        }
    }
}

/// Health check response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub services: Option<HashMap<String, ServiceHealthStatus>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceHealthStatus {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_check: Option<chrono::DateTime<chrono::Utc>>,
}

impl HealthCheckResponse {
    pub fn healthy() -> Self {
        Self {
            status: "healthy".to_string(),
            version: None,
            services: None,
            details: None,
        }
    }

    pub fn unhealthy(reason: impl Into<String>) -> Self {
        #[derive(Serialize)]
        struct Details {
            reason: String,
        }

        Self {
            status: "unhealthy".to_string(),
            version: None,
            services: None,
            details: Some(
                serde_json::to_value(Details {
                    reason: reason.into(),
                })
                .unwrap(),
            ),
        }
    }

    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    pub fn with_services(mut self, services: HashMap<String, ServiceHealthStatus>) -> Self {
        self.services = Some(services);
        self
    }
}

/// Generic list response with items
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListResponse<T> {
    pub items: Vec<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
}

impl<T> ListResponse<T> {
    pub fn new(items: Vec<T>) -> Self {
        Self {
            items,
            total: None,
            offset: None,
            limit: None,
        }
    }

    pub fn with_pagination(mut self, total: i64, offset: i64, limit: i64) -> Self {
        self.total = Some(total);
        self.offset = Some(offset);
        self.limit = Some(limit);
        self
    }
}

/// Status response for async operations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl StatusResponse {
    pub fn new(status: impl Into<String>) -> Self {
        Self {
            status: status.into(),
            progress: None,
            message: None,
            details: None,
        }
    }

    pub fn in_progress(progress: f32) -> Self {
        Self {
            status: "in_progress".to_string(),
            progress: Some(progress),
            message: None,
            details: None,
        }
    }

    pub fn completed() -> Self {
        Self::new("completed")
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self::new("failed").with_message(message)
    }

    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }
}

/// Validation error details
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationError {
    pub field: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// Validation errors response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationErrorsResponse {
    pub error: String,
    pub validation_errors: Vec<ValidationError>,
}

impl ValidationErrorsResponse {
    pub fn new(errors: Vec<ValidationError>) -> Self {
        Self {
            error: "Validation failed".to_string(),
            validation_errors: errors,
        }
    }
}

/// Empty response (for 204 No Content responses)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmptyResponse;

/// Generic ID response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdResponse {
    pub id: String,
}

/// Count response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CountResponse {
    pub count: i64,
}

/// Rate limit info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitInfo {
    pub limit: i64,
    pub remaining: i64,
    pub reset: chrono::DateTime<chrono::Utc>,
}

/// Worker status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerStatus {
    pub id: String,
    pub name: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_job: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<chrono::DateTime<chrono::Utc>>,
}

/// Queue statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueStats {
    pub total: i64,
    pub pending: i64,
    pub processing: i64,
    pub completed: i64,
    pub failed: i64,
}

/// Build result info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildResult {
    pub success: bool,
    pub log_url: Option<String>,
    pub artifacts: Vec<String>,
    pub duration_seconds: Option<f64>,
}

/// Merge proposal info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeProposalInfo {
    pub url: String,
    pub status: String,
    pub title: String,
    pub description: Option<String>,
    pub author: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_response() {
        let error = ErrorResponse::internal_server_error();
        let json = serde_json::to_string(&error).unwrap();
        assert!(json.contains("\"error\":\"Internal server error\""));
        assert!(json.contains("\"code\":\"internal_error\""));
    }

    #[test]
    fn test_success_response() {
        let response = SuccessResponse::new("test data");
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"success\":true"));
        assert!(json.contains("\"data\":\"test data\""));
    }

    #[test]
    fn test_health_check_response() {
        let response = HealthCheckResponse::healthy().with_version("1.0.0");
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"status\":\"healthy\""));
        assert!(json.contains("\"version\":\"1.0.0\""));
    }

    #[test]
    fn test_list_response() {
        let items = vec!["item1", "item2", "item3"];
        let response = ListResponse::new(items).with_pagination(100, 0, 10);
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"total\":100"));
        assert!(json.contains("\"offset\":0"));
        assert!(json.contains("\"limit\":10"));
    }
}
