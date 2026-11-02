//! Shared error types and utilities for the Janitor platform

use axum::response::IntoResponse;

/// Common error type for all Janitor services
#[derive(Debug)]
pub enum JanitorError {
    /// Database-related errors
    Database(sqlx::Error),

    /// I/O errors
    Io(std::io::Error),

    /// JSON parsing errors
    Json(serde_json::Error),

    /// HTTP client errors
    Http(reqwest::Error),

    /// Configuration errors
    Config(String),

    /// Authentication/authorization errors
    Auth(String),

    /// Validation errors for user input
    Validation(String),

    /// External service errors (VCS, build tools, etc.)
    ExternalService { service: String, message: String },

    /// Rate limiting errors
    RateLimit(String),

    /// Timeout errors
    Timeout(String),

    /// Resource not found
    NotFound { resource: String, id: String },

    /// Resource already exists
    AlreadyExists { resource: String, id: String },

    /// Permission denied
    PermissionDenied(String),

    /// Internal server errors
    Internal(String),

    /// Redis connection/operation errors
    Redis(redis::RedisError),

    /// External process execution errors
    Process { command: String, reason: String },

    /// Archive/package operations
    Archive(String),

    /// Upload service errors
    Upload(String),

    /// GPG operations
    Gpg(String),

    /// Package scanning errors
    PackageScanning(String),

    /// Repository operations
    Repository(String),

    /// Compression/decompression errors
    Compression(String),

    /// Resource limit exceeded
    ResourceLimit(String),

    /// Artifact management errors
    Artifact(String),
}

impl std::fmt::Display for JanitorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(e) => write!(f, "Database error: {}", e),
            Self::Io(e) => write!(f, "I/O error: {}", e),
            Self::Json(e) => write!(f, "JSON error: {}", e),
            Self::Http(e) => write!(f, "HTTP client error: {}", e),
            Self::Config(msg) => write!(f, "Configuration error: {}", msg),
            Self::Auth(msg) => write!(f, "Authentication error: {}", msg),
            Self::Validation(msg) => write!(f, "Validation error: {}", msg),
            Self::ExternalService { service, message } => {
                write!(f, "External service error: {}: {}", service, message)
            }
            Self::RateLimit(msg) => write!(f, "Rate limited: {}", msg),
            Self::Timeout(msg) => write!(f, "Operation timed out: {}", msg),
            Self::NotFound { resource, id } => write!(f, "Not found: {} '{}'", resource, id),
            Self::AlreadyExists { resource, id } => {
                write!(f, "Already exists: {} '{}'", resource, id)
            }
            Self::PermissionDenied(msg) => write!(f, "Permission denied: {}", msg),
            Self::Internal(msg) => write!(f, "Internal error: {}", msg),
            Self::Redis(e) => write!(f, "Redis error: {}", e),
            Self::Process { command, reason } => {
                write!(f, "Process error: '{}' failed: {}", command, reason)
            }
            Self::Archive(msg) => write!(f, "Archive error: {}", msg),
            Self::Upload(msg) => write!(f, "Upload error: {}", msg),
            Self::Gpg(msg) => write!(f, "GPG error: {}", msg),
            Self::PackageScanning(msg) => write!(f, "Package scanning error: {}", msg),
            Self::Repository(msg) => write!(f, "Repository error: {}", msg),
            Self::Compression(msg) => write!(f, "Compression error: {}", msg),
            Self::ResourceLimit(msg) => write!(f, "Resource limit exceeded: {}", msg),
            Self::Artifact(msg) => write!(f, "Artifact error: {}", msg),
        }
    }
}

impl std::error::Error for JanitorError {}

impl From<sqlx::Error> for JanitorError {
    fn from(e: sqlx::Error) -> Self {
        Self::Database(e)
    }
}

impl From<std::io::Error> for JanitorError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for JanitorError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

impl From<reqwest::Error> for JanitorError {
    fn from(e: reqwest::Error) -> Self {
        Self::Http(e)
    }
}

impl From<redis::RedisError> for JanitorError {
    fn from(e: redis::RedisError) -> Self {
        Self::Redis(e)
    }
}

impl JanitorError {
    /// Create a configuration error
    pub fn config(msg: impl Into<String>) -> Self {
        Self::Config(msg.into())
    }

    /// Create an authentication error
    pub fn auth(msg: impl Into<String>) -> Self {
        Self::Auth(msg.into())
    }

    /// Create a validation error
    pub fn validation(msg: impl Into<String>) -> Self {
        Self::Validation(msg.into())
    }

    /// Create an external service error
    pub fn external_service(service: impl Into<String>, message: impl Into<String>) -> Self {
        Self::ExternalService {
            service: service.into(),
            message: message.into(),
        }
    }

    /// Create a Redis error from a string message
    pub fn redis_msg(msg: impl Into<String>) -> Self {
        Self::Redis(redis::RedisError::from((
            redis::ErrorKind::Io,
            "Redis operation failed",
            msg.into(),
        )))
    }

    /// Create a not found error
    pub fn not_found(resource: impl Into<String>, id: impl Into<String>) -> Self {
        Self::NotFound {
            resource: resource.into(),
            id: id.into(),
        }
    }

    /// Create an already exists error
    pub fn already_exists(resource: impl Into<String>, id: impl Into<String>) -> Self {
        Self::AlreadyExists {
            resource: resource.into(),
            id: id.into(),
        }
    }

    /// Create a permission denied error
    pub fn permission_denied(msg: impl Into<String>) -> Self {
        Self::PermissionDenied(msg.into())
    }

    /// Create an internal error
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }

    /// Create a process error
    pub fn process(command: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Process {
            command: command.into(),
            reason: reason.into(),
        }
    }

    /// Create an archive error
    pub fn archive(msg: impl Into<String>) -> Self {
        Self::Archive(msg.into())
    }

    /// Create an upload error
    pub fn upload(msg: impl Into<String>) -> Self {
        Self::Upload(msg.into())
    }

    /// Create a GPG error
    pub fn gpg(msg: impl Into<String>) -> Self {
        Self::Gpg(msg.into())
    }

    /// Create a package scanning error
    pub fn package_scanning(msg: impl Into<String>) -> Self {
        Self::PackageScanning(msg.into())
    }

    /// Create a repository error
    pub fn repository(msg: impl Into<String>) -> Self {
        Self::Repository(msg.into())
    }

    /// Create a compression error
    pub fn compression(msg: impl Into<String>) -> Self {
        Self::Compression(msg.into())
    }

    /// Create a resource limit error
    pub fn resource_limit(msg: impl Into<String>) -> Self {
        Self::ResourceLimit(msg.into())
    }

    /// Create an artifact error
    pub fn artifact(msg: impl Into<String>) -> Self {
        Self::Artifact(msg.into())
    }

    /// Check if the error is transient (worth retrying)
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Database(sqlx::Error::Io(_)) => true,
            Self::Database(sqlx::Error::PoolTimedOut) => true,
            Self::Http(e) => e.is_timeout() || e.is_connect(),
            Self::Redis(e) => e.is_connection_dropped() || e.is_io_error(),
            Self::RateLimit(_) => true,
            Self::Timeout(_) => true,
            Self::ExternalService { .. } => true, // Often network-related
            Self::Process { .. } => true,         // Process failures might be transient
            _ => false,
        }
    }

    /// Get the appropriate HTTP status code for this error
    pub fn http_status(&self) -> u16 {
        match self {
            Self::NotFound { .. } => 404,
            Self::AlreadyExists { .. } => 409,
            Self::PermissionDenied(_) => 403,
            Self::Auth(_) => 401,
            Self::Validation(_) => 400,
            Self::RateLimit(_) => 429,
            Self::Timeout(_) => 408,
            _ => 500,
        }
    }

    /// Get the error type category for API responses
    pub fn error_type(&self) -> &'static str {
        match self {
            Self::Database(_) => "database",
            Self::Io(_) => "io",
            Self::Json(_) => "parsing",
            Self::Http(_) => "http_client",
            Self::Config(_) => "configuration",
            Self::Auth(_) => "authentication",
            Self::Validation(_) => "validation",
            Self::ExternalService { .. } => "external_service",
            Self::RateLimit(_) => "rate_limit",
            Self::Timeout(_) => "timeout",
            Self::NotFound { .. } => "not_found",
            Self::AlreadyExists { .. } => "already_exists",
            Self::PermissionDenied(_) => "permission_denied",
            Self::Internal(_) => "internal",
            Self::Redis(_) => "redis",
            Self::Process { .. } => "process",
            Self::Archive(_) => "archive",
            Self::Upload(_) => "upload",
            Self::Gpg(_) => "gpg",
            Self::PackageScanning(_) => "package_scanning",
            Self::Repository(_) => "repository",
            Self::Compression(_) => "compression",
            Self::ResourceLimit(_) => "resource_limit",
            Self::Artifact(_) => "artifact",
        }
    }

    /// Get a specific error code for this error
    pub fn error_code(&self) -> String {
        match self {
            Self::Database(_) => "DATABASE_ERROR".to_string(),
            Self::Io(_) => "IO_ERROR".to_string(),
            Self::Json(_) => "JSON_PARSE_ERROR".to_string(),
            Self::Http(_) => "HTTP_CLIENT_ERROR".to_string(),
            Self::Config(_) => "CONFIG_ERROR".to_string(),
            Self::Auth(_) => "AUTH_ERROR".to_string(),
            Self::Validation(_) => "VALIDATION_ERROR".to_string(),
            Self::ExternalService { service, .. } => {
                format!("EXTERNAL_{}_ERROR", service.to_uppercase())
            }
            Self::RateLimit(_) => "RATE_LIMIT_EXCEEDED".to_string(),
            Self::Timeout(_) => "TIMEOUT".to_string(),
            Self::NotFound { resource, .. } => format!("{}_NOT_FOUND", resource.to_uppercase()),
            Self::AlreadyExists { resource, .. } => {
                format!("{}_ALREADY_EXISTS", resource.to_uppercase())
            }
            Self::PermissionDenied(_) => "PERMISSION_DENIED".to_string(),
            Self::Internal(_) => "INTERNAL_ERROR".to_string(),
            Self::Redis(_) => "REDIS_ERROR".to_string(),
            Self::Process { .. } => "PROCESS_ERROR".to_string(),
            Self::Archive(_) => "ARCHIVE_ERROR".to_string(),
            Self::Upload(_) => "UPLOAD_ERROR".to_string(),
            Self::Gpg(_) => "GPG_ERROR".to_string(),
            Self::PackageScanning(_) => "PACKAGE_SCANNING_ERROR".to_string(),
            Self::Repository(_) => "REPOSITORY_ERROR".to_string(),
            Self::Compression(_) => "COMPRESSION_ERROR".to_string(),
            Self::ResourceLimit(_) => "RESOURCE_LIMIT_EXCEEDED".to_string(),
            Self::Artifact(_) => "ARTIFACT_ERROR".to_string(),
        }
    }

    /// Get service-specific error details
    pub fn error_details(&self) -> Option<ErrorDetails> {
        match self {
            Self::NotFound { resource, id } | Self::AlreadyExists { resource, id } => {
                Some(ErrorDetails::Resource {
                    resource_type: resource.clone(),
                    resource_id: id.clone(),
                })
            }
            Self::ExternalService { service, .. } => Some(ErrorDetails::ExternalService {
                service: service.clone(),
            }),
            Self::Process { command, .. } => Some(ErrorDetails::Process {
                command: command.clone(),
                failure_reason: "process execution failed",
            }),
            _ => None,
        }
    }

    /// Get help URL for this error type
    pub fn help_url(&self) -> Option<String> {
        match self {
            Self::Config(_) => Some("https://docs.janitor.io/config".to_string()),
            Self::Auth(_) => Some("https://docs.janitor.io/auth".to_string()),
            Self::Validation(_) => Some("https://docs.janitor.io/api".to_string()),
            _ => None,
        }
    }
}

/// Result type alias using JanitorError
pub type Result<T> = std::result::Result<T, JanitorError>;

/// Implement Axum response conversion for JanitorError
impl axum::response::IntoResponse for JanitorError {
    fn into_response(self) -> axum::response::Response {
        use axum::http::StatusCode;
        use axum::response::Json;

        let status = match self.http_status() {
            400 => StatusCode::BAD_REQUEST,
            401 => StatusCode::UNAUTHORIZED,
            403 => StatusCode::FORBIDDEN,
            404 => StatusCode::NOT_FOUND,
            408 => StatusCode::REQUEST_TIMEOUT,
            409 => StatusCode::CONFLICT,
            429 => StatusCode::TOO_MANY_REQUESTS,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };

        let response = StandardErrorResponse::from_janitor_error(self);
        (status, Json(response)).into_response()
    }
}

/// Standardized error response structure for all Janitor services
#[derive(serde::Serialize)]
pub struct StandardErrorResponse {
    pub error: ErrorInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
}

/// Service-specific details attached to an error response.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(untagged)]
pub enum ErrorDetails {
    Resource {
        resource_type: String,
        resource_id: String,
    },
    ExternalService {
        service: String,
    },
    Process {
        command: String,
        failure_reason: &'static str,
    },
}

/// Detailed error information
#[derive(serde::Serialize)]
pub struct ErrorInfo {
    pub r#type: String,  // Error category (e.g., "not_found", "validation")
    pub code: String,    // Specific error code (e.g., "RUN_NOT_FOUND")
    pub message: String, // Human-readable message
    pub transient: bool, // Whether retry makes sense
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<ErrorDetails>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help_url: Option<String>, // Link to documentation
}

impl StandardErrorResponse {
    /// Create a StandardErrorResponse from a JanitorError
    pub fn from_janitor_error(error: JanitorError) -> Self {
        let error_info = ErrorInfo {
            r#type: error.error_type().to_string(),
            code: error.error_code(),
            message: error.to_string(),
            transient: error.is_transient(),
            details: error.error_details(),
            help_url: error.help_url(),
        };

        Self {
            error: error_info,
            request_id: None, // Will be set by middleware
            timestamp: chrono::Utc::now().to_rfc3339(),
            service: None, // Will be set by service-specific helper
        }
    }

    /// Set the request ID (typically called by middleware)
    pub fn with_request_id(mut self, request_id: String) -> Self {
        self.request_id = Some(request_id);
        self
    }

    /// Set the service name
    pub fn with_service(mut self, service: String) -> Self {
        self.service = Some(service);
        self
    }

    /// Create a standardized error response from scratch
    pub fn new(error_type: &str, code: &str, message: &str, transient: bool) -> Self {
        let error_info = ErrorInfo {
            r#type: error_type.to_string(),
            code: code.to_string(),
            message: message.to_string(),
            transient,
            details: None,
            help_url: None,
        };

        Self {
            error: error_info,
            request_id: None,
            timestamp: chrono::Utc::now().to_rfc3339(),
            service: None,
        }
    }

    /// Add service-specific error details
    pub fn with_details(mut self, details: ErrorDetails) -> Self {
        self.error.details = Some(details);
        self
    }

    /// Add a help URL for this error
    pub fn with_help_url(mut self, url: String) -> Self {
        self.error.help_url = Some(url);
        self
    }
}

/// Convert common error types to JanitorError
impl From<url::ParseError> for JanitorError {
    fn from(e: url::ParseError) -> Self {
        Self::Validation(format!("Invalid URL: {}", e))
    }
}

/// Error conversion utilities
pub trait ErrorContext<T> {
    /// Add context to an error
    fn with_context<F>(self, f: F) -> Result<T>
    where
        F: FnOnce() -> String;

    /// Add simple string context to an error
    fn context(self, msg: &str) -> Result<T>;
}

impl<T, E> ErrorContext<T> for std::result::Result<T, E>
where
    E: Into<JanitorError>,
{
    fn with_context<F>(self, f: F) -> Result<T>
    where
        F: FnOnce() -> String,
    {
        self.map_err(|e| {
            let base_error = e.into();
            match base_error {
                JanitorError::Internal(msg) => JanitorError::Internal(format!("{}: {}", f(), msg)),
                other => JanitorError::Internal(format!("{}: {}", f(), other)),
            }
        })
    }

    fn context(self, msg: &str) -> Result<T> {
        self.with_context(|| msg.to_string())
    }
}

/// Helper for creating validation errors
pub fn validation_error(msg: impl Into<String>) -> JanitorError {
    JanitorError::Validation(msg.into())
}

/// Helper for creating not found errors
pub fn not_found(resource: impl Into<String>, id: impl Into<String>) -> JanitorError {
    JanitorError::not_found(resource, id)
}

/// Helper for creating internal errors
pub fn internal_error(msg: impl Into<String>) -> JanitorError {
    JanitorError::Internal(msg.into())
}

/// Extension trait for Option to convert None to JanitorError
pub trait OptionExt<T> {
    /// Convert None to a not found error
    fn ok_or_not_found(self, resource: &str, id: &str) -> Result<T>;

    /// Convert None to an internal error
    fn ok_or_internal(self, msg: &str) -> Result<T>;

    /// Convert None to a validation error
    fn ok_or_validation(self, msg: &str) -> Result<T>;
}

impl<T> OptionExt<T> for Option<T> {
    fn ok_or_not_found(self, resource: &str, id: &str) -> Result<T> {
        self.ok_or_else(|| JanitorError::not_found(resource, id))
    }

    fn ok_or_internal(self, msg: &str) -> Result<T> {
        self.ok_or_else(|| JanitorError::internal(msg))
    }

    fn ok_or_validation(self, msg: &str) -> Result<T> {
        self.ok_or_else(|| JanitorError::validation(msg))
    }
}

/// Extension trait for Result to add more context helpers
pub trait ResultExt<T> {
    /// Add not found context
    fn not_found_context(self, resource: &str, id: &str) -> Result<T>;

    /// Add validation context
    fn validation_context(self, msg: &str) -> Result<T>;

    /// Add internal context
    fn internal_context(self, msg: &str) -> Result<T>;
}

impl<T, E> ResultExt<T> for std::result::Result<T, E>
where
    E: Into<JanitorError>,
{
    fn not_found_context(self, resource: &str, id: &str) -> Result<T> {
        self.map_err(|_| JanitorError::not_found(resource, id))
    }

    fn validation_context(self, msg: &str) -> Result<T> {
        self.map_err(|_| JanitorError::validation(msg))
    }

    fn internal_context(self, msg: &str) -> Result<T> {
        self.map_err(|e| {
            let base_error = e.into();
            JanitorError::internal(format!("{}: {}", msg, base_error))
        })
    }
}

/// Convenience functions for creating standardized error responses
pub mod responses {
    use super::*;
    use axum::response::IntoResponse;

    /// Create a standardized not found response
    pub fn not_found(resource: &str, id: &str) -> impl IntoResponse {
        JanitorError::not_found(resource, id)
    }

    /// Create a standardized validation error response
    pub fn validation_error(message: &str) -> impl IntoResponse {
        JanitorError::validation(message)
    }

    /// Create a standardized internal error response
    pub fn internal_error(message: &str) -> impl IntoResponse {
        JanitorError::internal(message)
    }

    /// Create a standardized permission denied response
    pub fn permission_denied(message: &str) -> impl IntoResponse {
        JanitorError::permission_denied(message)
    }

    /// Create a standardized already exists response
    pub fn already_exists(resource: &str, id: &str) -> impl IntoResponse {
        JanitorError::already_exists(resource, id)
    }

    /// Create a standardized external service error response
    pub fn external_service_error(service: &str, message: &str) -> impl IntoResponse {
        JanitorError::external_service(service, message)
    }

    /// Create a standardized rate limit error response
    pub fn rate_limited(message: &str) -> impl IntoResponse {
        JanitorError::RateLimit(message.to_string())
    }

    /// Create a standardized timeout error response
    pub fn timeout_error(message: &str) -> impl IntoResponse {
        JanitorError::Timeout(message.to_string())
    }
}

/// Extension trait for Result to easily convert to standardized responses
pub trait IntoStandardResponse<T> {
    /// Convert result to a standardized HTTP response, mapping errors to JanitorError
    fn into_response(self) -> impl IntoResponse;

    /// Convert result to a standardized HTTP response with custom error mapping
    fn into_response_with<F>(self, error_mapper: F) -> impl IntoResponse
    where
        F: FnOnce() -> JanitorError;
}

impl<T, E> IntoStandardResponse<T> for std::result::Result<T, E>
where
    T: IntoResponse,
    E: Into<JanitorError>,
{
    fn into_response(self) -> impl IntoResponse {
        match self {
            Ok(response) => response.into_response(),
            Err(error) => error.into().into_response(),
        }
    }

    fn into_response_with<F>(self, error_mapper: F) -> impl IntoResponse
    where
        F: FnOnce() -> JanitorError,
    {
        match self {
            Ok(response) => response.into_response(),
            Err(_) => error_mapper().into_response(),
        }
    }
}

// Service-specific type aliases for easy migration
pub type ArchiveError = JanitorError;
pub type ArchiveResult<T> = Result<T>;

pub type GitStoreError = JanitorError;
pub type GitStoreResult<T> = Result<T>;

pub type UploadError = JanitorError;
pub type UploadResult<T> = Result<T>;

pub type WorkerError = JanitorError;
pub type WorkerResult<T> = Result<T>;

pub type PublisherError = JanitorError;
pub type PublisherResult<T> = Result<T>;

pub type SiteError = JanitorError;
pub type SiteResult<T> = Result<T>;

pub type RunnerError = JanitorError;
pub type RunnerResult<T> = Result<T>;

/// Conversion helpers for migrating from service-specific errors
pub trait IntoJanitorError {
    fn into_janitor_error(self) -> JanitorError;
}

// Provide blanket implementations for common error patterns
impl IntoJanitorError for String {
    fn into_janitor_error(self) -> JanitorError {
        JanitorError::internal(self)
    }
}

impl IntoJanitorError for &str {
    fn into_janitor_error(self) -> JanitorError {
        JanitorError::internal(self.to_string())
    }
}

/// Helper for creating common error patterns
pub mod errors {
    use super::JanitorError;

    pub fn archive_error(msg: impl Into<String>) -> JanitorError {
        JanitorError::archive(msg)
    }

    pub fn upload_error(msg: impl Into<String>) -> JanitorError {
        JanitorError::upload(msg)
    }

    pub fn git_error<E: std::fmt::Display>(e: E) -> JanitorError {
        JanitorError::internal(format!("Git operation failed: {}", e))
    }

    pub fn package_scanning_error(msg: impl Into<String>) -> JanitorError {
        JanitorError::package_scanning(msg)
    }

    pub fn repository_error(msg: impl Into<String>) -> JanitorError {
        JanitorError::repository(msg)
    }

    pub fn artifacts_missing(build_id: impl Into<String>) -> JanitorError {
        JanitorError::artifact(format!("Missing artifacts for build {}", build_id.into()))
    }

    pub fn gpg_operation_failed(msg: impl Into<String>) -> JanitorError {
        JanitorError::gpg(msg)
    }

    pub fn compression_failed(msg: impl Into<String>) -> JanitorError {
        JanitorError::compression(msg)
    }

    pub fn resource_limit_exceeded(msg: impl Into<String>) -> JanitorError {
        JanitorError::resource_limit(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_creation() {
        let err = JanitorError::not_found("run", "test-123");
        assert_eq!(err.to_string(), "Not found: run 'test-123'");
        assert_eq!(err.http_status(), 404);
    }

    #[test]
    fn test_transient_errors() {
        assert_eq!(
            JanitorError::RateLimit("test".to_string()).is_transient(),
            true
        );
        assert_eq!(
            JanitorError::Timeout("test".to_string()).is_transient(),
            true
        );
        assert_eq!(
            JanitorError::ExternalService {
                service: "git".to_string(),
                message: "fail".to_string()
            }
            .is_transient(),
            true
        );
        assert_eq!(
            JanitorError::Process {
                command: "cmd".to_string(),
                reason: "fail".to_string()
            }
            .is_transient(),
            true
        );
    }

    #[test]
    fn test_non_transient_errors() {
        assert_eq!(
            JanitorError::NotFound {
                resource: "test".to_string(),
                id: "123".to_string()
            }
            .is_transient(),
            false
        );
        assert_eq!(
            JanitorError::Validation("bad input".to_string()).is_transient(),
            false
        );
        assert_eq!(
            JanitorError::Auth("denied".to_string()).is_transient(),
            false
        );
        assert_eq!(
            JanitorError::Config("bad".to_string()).is_transient(),
            false
        );
    }

    #[test]
    fn test_http_status_codes() {
        assert_eq!(JanitorError::not_found("test", "123").http_status(), 404);
        assert_eq!(JanitorError::validation("test").http_status(), 400);
        assert_eq!(JanitorError::permission_denied("test").http_status(), 403);
        assert_eq!(JanitorError::auth("denied").http_status(), 401);
        assert_eq!(
            JanitorError::already_exists("test", "123").http_status(),
            409
        );
        assert_eq!(
            JanitorError::RateLimit("slow down".to_string()).http_status(),
            429
        );
        assert_eq!(
            JanitorError::Timeout("too slow".to_string()).http_status(),
            408
        );
        assert_eq!(JanitorError::internal("oops").http_status(), 500);
        assert_eq!(JanitorError::Config("bad".to_string()).http_status(), 500);
    }

    #[test]
    fn test_error_context() {
        let result: std::result::Result<(), std::io::Error> = Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "file not found",
        ));

        let err = result.context("Failed to read config").unwrap_err();
        assert_eq!(
            err.to_string(),
            "Internal error: Failed to read config: I/O error: file not found"
        );
    }

    #[test]
    fn test_error_codes() {
        assert_eq!(
            JanitorError::not_found("run", "123").error_code(),
            "RUN_NOT_FOUND"
        );
        assert_eq!(
            JanitorError::validation("test").error_code(),
            "VALIDATION_ERROR"
        );
        assert_eq!(
            JanitorError::external_service("git", "failed").error_code(),
            "EXTERNAL_GIT_ERROR"
        );
    }

    #[test]
    fn test_error_type() {
        assert_eq!(
            JanitorError::not_found("run", "123").error_type(),
            "not_found"
        );
        assert_eq!(JanitorError::validation("test").error_type(), "validation");
        assert_eq!(JanitorError::auth("denied").error_type(), "authentication");
        assert_eq!(JanitorError::internal("oops").error_type(), "internal");
        assert_eq!(
            JanitorError::RateLimit("slow".to_string()).error_type(),
            "rate_limit"
        );
        assert_eq!(
            JanitorError::Archive("bad".to_string()).error_type(),
            "archive"
        );
    }

    #[test]
    fn test_error_details() {
        let err = JanitorError::not_found("run", "test-123");
        assert_eq!(
            err.error_details(),
            Some(ErrorDetails::Resource {
                resource_type: "run".to_string(),
                resource_id: "test-123".to_string(),
            })
        );

        let process_err = JanitorError::process("cargo build", "compilation failed");
        assert_eq!(
            process_err.error_details(),
            Some(ErrorDetails::Process {
                command: "cargo build".to_string(),
                failure_reason: "process execution failed",
            })
        );
    }

    #[test]
    fn test_error_details_serialization() {
        let details = JanitorError::not_found("run", "test-123").error_details();
        assert_eq!(
            serde_json::to_string(&details).unwrap(),
            r#"{"resource_type":"run","resource_id":"test-123"}"#
        );
        let details = JanitorError::process("make", "boom").error_details();
        assert_eq!(
            serde_json::to_string(&details).unwrap(),
            r#"{"command":"make","failure_reason":"process execution failed"}"#
        );
    }

    #[test]
    fn test_error_details_none() {
        assert_eq!(JanitorError::validation("bad").error_details(), None);
        assert_eq!(JanitorError::internal("oops").error_details(), None);
        assert_eq!(
            JanitorError::Config("bad".to_string()).error_details(),
            None
        );
    }

    #[test]
    fn test_help_urls() {
        assert_eq!(
            JanitorError::Config("bad".to_string()).help_url(),
            Some("https://docs.janitor.io/config".to_string())
        );
        assert_eq!(
            JanitorError::Auth("denied".to_string()).help_url(),
            Some("https://docs.janitor.io/auth".to_string())
        );
        assert_eq!(
            JanitorError::Validation("bad".to_string()).help_url(),
            Some("https://docs.janitor.io/api".to_string())
        );
        assert_eq!(JanitorError::internal("oops").help_url(), None);
    }

    #[test]
    fn test_display_messages() {
        assert_eq!(
            JanitorError::not_found("run", "abc").to_string(),
            "Not found: run 'abc'"
        );
        assert_eq!(
            JanitorError::already_exists("user", "john").to_string(),
            "Already exists: user 'john'"
        );
        assert_eq!(
            JanitorError::external_service("git", "clone failed").to_string(),
            "External service error: git: clone failed"
        );
        assert_eq!(
            JanitorError::process("cargo build", "exit code 1").to_string(),
            "Process error: 'cargo build' failed: exit code 1"
        );
    }

    #[test]
    fn test_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err: JanitorError = io_err.into();
        assert_eq!(err.error_type(), "io");
        assert_eq!(err.http_status(), 500);
    }

    #[test]
    fn test_from_serde_json_error() {
        let json_err = serde_json::from_str::<serde_json::Value>("invalid json").unwrap_err();
        let err: JanitorError = json_err.into();
        assert_eq!(err.error_type(), "parsing");
    }

    #[test]
    fn test_from_url_parse_error() {
        let url_err = url::Url::parse("not a url %%%").unwrap_err();
        let err: JanitorError = url_err.into();
        assert_eq!(err.error_type(), "validation");
    }

    #[test]
    fn test_option_ext_ok_or_not_found() {
        let some: Option<i32> = Some(42);
        assert_eq!(some.ok_or_not_found("item", "1").unwrap(), 42);

        let none: Option<i32> = None;
        let err = none.ok_or_not_found("item", "1").unwrap_err();
        assert_eq!(err.http_status(), 404);
        assert_eq!(err.to_string(), "Not found: item '1'");
    }

    #[test]
    fn test_option_ext_ok_or_internal() {
        let none: Option<i32> = None;
        let err = none.ok_or_internal("missing value").unwrap_err();
        assert_eq!(err.http_status(), 500);
        assert_eq!(err.to_string(), "Internal error: missing value");
    }

    #[test]
    fn test_option_ext_ok_or_validation() {
        let none: Option<i32> = None;
        let err = none.ok_or_validation("required field").unwrap_err();
        assert_eq!(err.http_status(), 400);
        assert_eq!(err.to_string(), "Validation error: required field");
    }

    #[test]
    fn test_standard_error_response() {
        let err = JanitorError::not_found("run", "test-123");
        let response = StandardErrorResponse::from_janitor_error(err);

        assert_eq!(response.error.r#type, "not_found");
        assert_eq!(response.error.code, "RUN_NOT_FOUND");
        assert_eq!(response.error.message, "Not found: run 'test-123'");
        assert_eq!(response.error.transient, false);
        assert!(response.error.details.is_some());
        assert_eq!(response.error.help_url, None);
        assert!(!response.timestamp.is_empty());
    }

    #[test]
    fn test_standard_error_response_with_metadata() {
        let response =
            StandardErrorResponse::new("validation", "INVALID_INPUT", "Input is required", false)
                .with_request_id("req-123".to_string())
                .with_service("runner".to_string())
                .with_details(ErrorDetails::ExternalService {
                    service: "vcs".to_string(),
                })
                .with_help_url("https://docs.example.com/validation".to_string());

        assert_eq!(response.request_id, Some("req-123".to_string()));
        assert_eq!(response.service, Some("runner".to_string()));
        assert_eq!(
            response.error.details,
            Some(ErrorDetails::ExternalService {
                service: "vcs".to_string()
            })
        );
        assert_eq!(
            response.error.help_url,
            Some("https://docs.example.com/validation".to_string())
        );
    }

    #[test]
    fn test_standard_error_response_serialization() {
        let err = JanitorError::validation("bad input");
        let response = StandardErrorResponse::from_janitor_error(err)
            .with_request_id("req-abc".to_string())
            .with_service("publisher".to_string());

        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["error"]["type"], "validation");
        assert_eq!(json["error"]["code"], "VALIDATION_ERROR");
        assert_eq!(json["error"]["message"], "Validation error: bad input");
        assert_eq!(json["error"]["transient"], false);
        assert_eq!(json["request_id"], "req-abc");
        assert_eq!(json["service"], "publisher");
    }

    #[test]
    fn test_into_janitor_error_string() {
        let err: JanitorError = "something failed".into_janitor_error();
        assert_eq!(err.to_string(), "Internal error: something failed");
    }

    #[test]
    fn test_convenience_responses() {
        use crate::error::responses::*;

        // These should compile and return IntoResponse types
        let _not_found_resp = not_found("run", "123");
        let _validation_resp = validation_error("invalid input");
        let _internal_resp = internal_error("something went wrong");
        let _permission_resp = permission_denied("access denied");
        let _exists_resp = already_exists("user", "john");
        let _service_resp = external_service_error("git", "clone failed");
        let _rate_resp = rate_limited("too many requests");
        let _timeout_resp = timeout_error("operation timed out");
    }

    #[test]
    fn test_error_context_with_closure() {
        let result: std::result::Result<(), std::io::Error> = Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "access denied",
        ));

        let err = result
            .with_context(|| format!("reading config for {}", "myservice"))
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Internal error: reading config for myservice: I/O error: access denied"
        );
    }

    #[test]
    fn test_result_ext_not_found_context() {
        let result: std::result::Result<(), std::io::Error> =
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "missing"));
        let err = result.not_found_context("run", "abc").unwrap_err();
        assert_eq!(err.to_string(), "Not found: run 'abc'");
        assert_eq!(err.http_status(), 404);
    }

    #[test]
    fn test_result_ext_validation_context() {
        let result: std::result::Result<(), std::io::Error> =
            Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "bad"));
        let err = result.validation_context("invalid input").unwrap_err();
        assert_eq!(err.to_string(), "Validation error: invalid input");
        assert_eq!(err.http_status(), 400);
    }

    #[test]
    fn test_errors_module_helpers() {
        use crate::error::errors::*;

        assert_eq!(
            archive_error("corrupt").to_string(),
            "Archive error: corrupt"
        );
        assert_eq!(upload_error("failed").to_string(), "Upload error: failed");
        assert_eq!(
            package_scanning_error("no packages").to_string(),
            "Package scanning error: no packages"
        );
        assert_eq!(
            repository_error("bad repo").to_string(),
            "Repository error: bad repo"
        );
        assert_eq!(
            gpg_operation_failed("no key").to_string(),
            "GPG error: no key"
        );
        assert_eq!(
            compression_failed("corrupt archive").to_string(),
            "Compression error: corrupt archive"
        );
        assert_eq!(
            resource_limit_exceeded("out of disk").to_string(),
            "Resource limit exceeded: out of disk"
        );
        assert_eq!(
            artifacts_missing("run-123").to_string(),
            "Artifact error: Missing artifacts for build run-123"
        );
    }
}
