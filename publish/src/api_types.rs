use serde::{Deserialize, Serialize};

/// Success response with URL
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuccessWithUrlResponse {
    /// Status message indicating success
    pub status: String,
    /// URL of the created or affected resource
    pub url: String,
}

/// Not found response with reason
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    pub run_id: Option<String>,
}

/// Simple error response, matching the shape used across janitor services.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
