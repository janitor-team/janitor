//! Request and response bodies of the publisher's HTTP API.

use janitor_types::{Campaign, CodebaseName, PublishId, RunId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::ToSchema;

/// Empty JSON object, returned by endpoints that have nothing to report.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct EmptyResponse {}

/// Success response with URL
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SuccessWithUrlResponse {
    /// Status message indicating success
    pub status: String,
    /// URL of the created or affected resource
    pub url: String,
}

/// Not found response with reason
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
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
    pub run_id: Option<RunId>,
    /// Campaign name if relevant to the not found error
    #[serde(skip_serializing_if = "Option::is_none")]
    pub campaign: Option<Campaign>,
    /// Codebase name if relevant to the not found error
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codebase: Option<CodebaseName>,
}

/// Response for `POST /{campaign}/{codebase}/publish` with an unknown
/// mode.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InvalidModeResponse {
    /// Always `Invalid mode`.
    pub error: String,
    /// The mode that was requested.
    pub mode: String,
}

/// Response for `POST /{campaign}/{codebase}/publish` when there is
/// nothing to publish.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PublishStartedResponse {
    /// Run that is being published.
    pub run_id: RunId,
    /// Mode that was requested, if any.
    pub mode: Option<String>,
    /// Publish id for each branch role.
    pub publish_ids: BTreeMap<String, PublishId>,
}

/// Response for `POST /autopublish`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AutopublishResponse {
    /// Always `Autopublish started.`.
    pub message: String,
    /// Maximum number of pushes for this run.
    pub push_limit: Option<usize>,
}

/// Simple error response, matching the shape used across janitor services.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
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
