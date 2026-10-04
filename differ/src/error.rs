//! Error types for the differ service.

use axum::http::header::{HeaderName, HeaderValue};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by the differ. `IntoResponse` maps each variant to a
/// specific HTTP status and a `{"reason": ..., "message": ...}` JSON body.
#[derive(Debug, Error)]
pub enum Error {
    #[error("run {0} not found")]
    RunNotFound(String),

    #[error("run {0} was not successful")]
    RunNotSuccessful(String),

    #[error("artifacts missing for run {0}")]
    ArtifactsMissing(String),

    #[error("timeout retrieving artifacts for run {0}")]
    ArtifactRetrievalTimeout(String),

    #[error("failed to retrieve artifacts for run {run_id}: {reason}")]
    ArtifactRetrievalFailed { run_id: String, reason: String },

    #[error("timeout running {0}")]
    DiffCommandTimeout(&'static str),

    #[error("{0} used too much memory")]
    DiffCommandMemoryError(&'static str),

    #[error("{command}: {reason}")]
    DiffCommandError {
        command: &'static str,
        reason: String,
    },

    #[error("no acceptable content type; offered: {offered}")]
    ContentNegotiationFailed { offered: String },

    #[error("invalid run id {0:?}")]
    InvalidRunId(String),

    #[error("database error: {0}")]
    Database(#[source] sqlx::Error),

    #[error(transparent)]
    Diffoscope(#[from] crate::diffoscope::DiffoscopeError),

    #[error(transparent)]
    Artifacts(#[from] janitor::artifacts::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        Error::Database(e)
    }
}

#[derive(Serialize)]
struct ErrorBody {
    reason: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    unavailable_run_id: Option<String>,
}

impl Error {
    /// Stable machine-readable identifier for this error variant.
    /// Emitted as the `"reason"` field of the JSON error body and used
    /// as a Prometheus label.
    pub fn reason(&self) -> &'static str {
        match self {
            Error::RunNotFound(_) => "run-not-found",
            Error::RunNotSuccessful(_) => "run-not-successful",
            Error::ArtifactsMissing(_) => "artifacts-missing",
            Error::ArtifactRetrievalTimeout(_) => "artifact-retrieval-timeout",
            Error::ArtifactRetrievalFailed { .. } => "artifact-retrieval-failed",
            Error::DiffCommandTimeout(_) => "diff-command-timeout",
            Error::DiffCommandMemoryError(_) => "diff-command-memory-error",
            Error::DiffCommandError { .. } => "diff-command-error",
            Error::ContentNegotiationFailed { .. } => "not-acceptable",
            Error::InvalidRunId(_) => "invalid-run-id",
            Error::Database(_) => "database-error",
            Error::Diffoscope(_) => "diffoscope-error",
            Error::Artifacts(_) => "artifact-error",
            Error::Io(_) => "internal-error",
            Error::Json(_) => "internal-error",
        }
    }

    /// HTTP status code this error maps to. Kept in sync with `IntoResponse`.
    pub fn status(&self) -> StatusCode {
        match self {
            Error::RunNotFound(_) => StatusCode::NOT_FOUND,
            Error::RunNotSuccessful(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Error::ArtifactsMissing(_) => StatusCode::NOT_FOUND,
            Error::ArtifactRetrievalTimeout(_) => StatusCode::GATEWAY_TIMEOUT,
            Error::ArtifactRetrievalFailed { .. } => StatusCode::BAD_GATEWAY,
            Error::DiffCommandTimeout(_) => StatusCode::GATEWAY_TIMEOUT,
            Error::DiffCommandMemoryError(_) => StatusCode::INSUFFICIENT_STORAGE,
            Error::DiffCommandError { .. } => StatusCode::BAD_REQUEST,
            Error::ContentNegotiationFailed { .. } => StatusCode::NOT_ACCEPTABLE,
            Error::InvalidRunId(_) => StatusCode::BAD_REQUEST,
            Error::Database(e) => {
                // SQLSTATE class 53 (insufficient resources) and local pool
                // exhaustion surface as 503, matching Python's
                // asyncpg_error_middleware.
                let unavailable = matches!(e, sqlx::Error::PoolTimedOut)
                    || matches!(e, sqlx::Error::Database(db) if db.code().is_some_and(|c| c.starts_with("53")));
                if unavailable {
                    StatusCode::SERVICE_UNAVAILABLE
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                }
            }
            Error::Diffoscope(e) => match e {
                crate::diffoscope::DiffoscopeError::Timeout => StatusCode::GATEWAY_TIMEOUT,
                crate::diffoscope::DiffoscopeError::Io(io)
                    if io.kind() == std::io::ErrorKind::OutOfMemory =>
                {
                    StatusCode::INSUFFICIENT_STORAGE
                }
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            },
            Error::Artifacts(_) => StatusCode::BAD_GATEWAY,
            Error::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Error::Json(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The run id that could not be served, when known. Emitted as the
    /// `unavailable_run_id` header on 4xx artifact/run responses.
    pub fn unavailable_run_id(&self) -> Option<&str> {
        match self {
            Error::RunNotFound(id)
            | Error::RunNotSuccessful(id)
            | Error::ArtifactsMissing(id)
            | Error::ArtifactRetrievalTimeout(id)
            | Error::ArtifactRetrievalFailed { run_id: id, .. } => Some(id),
            _ => None,
        }
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = self.status();
        let unavailable = self.unavailable_run_id().map(str::to_string);
        let body = ErrorBody {
            reason: self.reason(),
            message: self.to_string(),
            unavailable_run_id: unavailable.clone(),
        };
        let mut resp = (status, Json(body)).into_response();
        if let Some(id) = unavailable {
            if let Ok(value) = HeaderValue::from_str(&id) {
                resp.headers_mut()
                    .insert(HeaderName::from_static("unavailable_run_id"), value);
            }
        }
        resp
    }
}
