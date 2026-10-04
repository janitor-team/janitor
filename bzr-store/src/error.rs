//! Error handling for the BZR Store service

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use thiserror::Error;

/// Main error type for the BZR Store service
#[derive(Debug, Error)]
pub enum BzrError {
    /// Python/breezy errors surfaced through the breezyshim shim.
    #[error("Python error: {0}")]
    Python(String),

    /// Database errors
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    /// Repository operation errors
    #[error("Repository error: {message}")]
    Repository {
        /// Error message describing the repository operation failure
        message: String,
    },

    /// Authentication failures
    #[error("Authentication failed")]
    AuthenticationFailed,

    /// Subprocess operation errors
    #[error("Subprocess error: {0}")]
    Subprocess(String),

    /// IO errors
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Template rendering errors
    #[error("Template error: {0}")]
    Template(#[from] tera::Error),

    /// Path not found
    #[error("Path not found: {path}")]
    PathNotFound {
        /// The path that was not found
        path: String,
    },

    /// Invalid request
    #[error("Invalid request: {message}")]
    InvalidRequest {
        /// Error message describing why the request was invalid
        message: String,
    },

    /// Internal server error
    #[error("Internal server error: {message}")]
    Internal {
        /// Error message describing the internal failure
        message: String,
    },

    /// JSON serialization error
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

impl BzrError {
    /// Create a repository error
    pub fn repository<S: Into<String>>(message: S) -> Self {
        Self::Repository {
            message: message.into(),
        }
    }

    /// Create a subprocess error
    pub fn subprocess<S: Into<String>>(message: S) -> Self {
        Self::Subprocess(message.into())
    }

    /// Create an invalid request error
    pub fn invalid_request<S: Into<String>>(message: S) -> Self {
        Self::InvalidRequest {
            message: message.into(),
        }
    }

    /// Create an internal error
    pub fn internal<S: Into<String>>(message: S) -> Self {
        Self::Internal {
            message: message.into(),
        }
    }
}

impl IntoResponse for BzrError {
    fn into_response(self) -> Response {
        let (status, error_message) = match self {
            BzrError::AuthenticationFailed => (StatusCode::UNAUTHORIZED, "Authentication required"),
            BzrError::PathNotFound { .. } => (StatusCode::NOT_FOUND, "Repository not found"),
            BzrError::InvalidRequest { .. } => (StatusCode::BAD_REQUEST, "Invalid request"),
            BzrError::Repository { .. } => (StatusCode::NOT_FOUND, "Repository error"),
            BzrError::Subprocess(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Operation failed"),
            BzrError::Python(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Python error"),
            BzrError::Database(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Database error"),
            BzrError::Io(_) => (StatusCode::INTERNAL_SERVER_ERROR, "IO error"),
            BzrError::Template(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Template error"),
            BzrError::Internal { .. } => (StatusCode::INTERNAL_SERVER_ERROR, "Internal error"),
            BzrError::Json(_) => (StatusCode::INTERNAL_SERVER_ERROR, "JSON error"),
        };

        let body = Json(json!({
            "error": error_message,
            "details": self.to_string(),
        }));

        (status, body).into_response()
    }
}

/// Result type alias for BZR Store operations
pub type Result<T> = std::result::Result<T, BzrError>;
