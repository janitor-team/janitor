//! Standardized error handling for web services

use axum::{
    response::{IntoResponse, Response},
    http::StatusCode,
    Json,
};
use serde::{Serialize, Deserialize};
use std::fmt;

/// Result type for web operations
pub type WebResult<T> = Result<T, WebError>;

/// Standard error response structure
#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// Error code for programmatic handling
    pub code: String,
    /// Human-readable error message
    pub message: String,
    /// Optional detailed error information
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    /// Request ID for tracing
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

/// Web service errors
#[derive(Debug)]
pub enum WebError {
    /// Bad request (400)
    BadRequest(String),

    /// Unauthorized (401)
    Unauthorized(String),

    /// Forbidden (403)
    Forbidden(String),

    /// Not found (404)
    NotFound(String),

    /// Conflict (409)
    Conflict(String),

    /// Unprocessable entity (422)
    ValidationError(String),

    /// Too many requests (429)
    RateLimitExceeded(String),

    /// Internal server error (500)
    InternalError(String),

    /// Service unavailable (503)
    ServiceUnavailable(String),

    /// Database error
    DatabaseError(sqlx::Error),

    /// Redis error
    RedisError(redis::RedisError),

    /// Serialization error
    SerializationError(serde_json::Error),

    /// Configuration error
    ConfigError(crate::shared_config::ConfigError),

    /// Generic error with custom status code
    Custom {
        status: StatusCode,
        code: String,
        message: String,
    },
}

impl fmt::Display for WebError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WebError::BadRequest(msg) => write!(f, "Bad request: {}", msg),
            WebError::Unauthorized(msg) => write!(f, "Unauthorized: {}", msg),
            WebError::Forbidden(msg) => write!(f, "Forbidden: {}", msg),
            WebError::NotFound(msg) => write!(f, "Not found: {}", msg),
            WebError::Conflict(msg) => write!(f, "Conflict: {}", msg),
            WebError::ValidationError(msg) => write!(f, "Validation error: {}", msg),
            WebError::RateLimitExceeded(msg) => write!(f, "Rate limit exceeded: {}", msg),
            WebError::InternalError(msg) => write!(f, "Internal server error: {}", msg),
            WebError::ServiceUnavailable(msg) => write!(f, "Service unavailable: {}", msg),
            WebError::DatabaseError(err) => write!(f, "Database error: {}", err),
            WebError::RedisError(err) => write!(f, "Redis error: {}", err),
            WebError::SerializationError(err) => write!(f, "Serialization error: {}", err),
            WebError::ConfigError(err) => write!(f, "Configuration error: {}", err),
            WebError::Custom { message, .. } => write!(f, "{}", message),
        }
    }
}

impl std::error::Error for WebError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WebError::DatabaseError(err) => Some(err),
            WebError::RedisError(err) => Some(err),
            WebError::SerializationError(err) => Some(err),
            WebError::ConfigError(err) => Some(err),
            _ => None,
        }
    }
}

impl From<sqlx::Error> for WebError {
    fn from(err: sqlx::Error) -> Self {
        WebError::DatabaseError(err)
    }
}

impl From<redis::RedisError> for WebError {
    fn from(err: redis::RedisError) -> Self {
        WebError::RedisError(err)
    }
}

impl From<serde_json::Error> for WebError {
    fn from(err: serde_json::Error) -> Self {
        WebError::SerializationError(err)
    }
}

impl From<crate::shared_config::ConfigError> for WebError {
    fn from(err: crate::shared_config::ConfigError) -> Self {
        WebError::ConfigError(err)
    }
}

impl WebError {
    /// Get the HTTP status code for this error
    pub fn status_code(&self) -> StatusCode {
        match self {
            WebError::BadRequest(_) => StatusCode::BAD_REQUEST,
            WebError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            WebError::Forbidden(_) => StatusCode::FORBIDDEN,
            WebError::NotFound(_) => StatusCode::NOT_FOUND,
            WebError::Conflict(_) => StatusCode::CONFLICT,
            WebError::ValidationError(_) => StatusCode::UNPROCESSABLE_ENTITY,
            WebError::RateLimitExceeded(_) => StatusCode::TOO_MANY_REQUESTS,
            WebError::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            WebError::ServiceUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            WebError::DatabaseError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            WebError::RedisError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            WebError::SerializationError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            WebError::ConfigError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            WebError::Custom { status, .. } => *status,
        }
    }
    
    /// Get the error code for this error
    pub fn error_code(&self) -> String {
        match self {
            WebError::BadRequest(_) => "BAD_REQUEST",
            WebError::Unauthorized(_) => "UNAUTHORIZED",
            WebError::Forbidden(_) => "FORBIDDEN",
            WebError::NotFound(_) => "NOT_FOUND",
            WebError::Conflict(_) => "CONFLICT",
            WebError::ValidationError(_) => "VALIDATION_ERROR",
            WebError::RateLimitExceeded(_) => "RATE_LIMIT_EXCEEDED",
            WebError::InternalError(_) => "INTERNAL_ERROR",
            WebError::ServiceUnavailable(_) => "SERVICE_UNAVAILABLE",
            WebError::DatabaseError(_) => "DATABASE_ERROR",
            WebError::RedisError(_) => "REDIS_ERROR",
            WebError::SerializationError(_) => "SERIALIZATION_ERROR",
            WebError::ConfigError(_) => "CONFIG_ERROR",
            WebError::Custom { code, .. } => code,
        }.to_string()
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        // `IntoResponse::into_response(self)` can't see request
        // extensions, so we can't read the request_id added by
        // `request_id_middleware` from here. The field stays on
        // ErrorResponse for callers that construct ErrorResponse
        // directly (e.g. from a handler that has access to the
        // request) — they can populate it themselves.
        let error_response = ErrorResponse {
            code: self.error_code(),
            message: self.to_string(),
            details: None,
            request_id: None,
        };
        
        // Log server errors
        if status.is_server_error() {
            tracing::error!(
                error = %self,
                status = %status,
                "Server error occurred"
            );
        }
        
        (status, Json(error_response)).into_response()
    }
}

/// Convert common errors to WebError
impl From<std::io::Error> for WebError {
    fn from(err: std::io::Error) -> Self {
        WebError::InternalError(format!("I/O error: {}", err))
    }
}

/// Extension trait for Results to add web error context
pub trait WebErrorExt<T> {
    /// Convert error to WebError with custom message
    fn web_context(self, msg: &str) -> WebResult<T>;
    
    /// Convert error to 404 Not Found
    fn or_not_found(self, msg: &str) -> WebResult<T>;
    
    /// Convert error to 400 Bad Request
    fn or_bad_request(self, msg: &str) -> WebResult<T>;
}

impl<T, E> WebErrorExt<T> for Result<T, E>
where
    E: fmt::Display,
{
    fn web_context(self, msg: &str) -> WebResult<T> {
        self.map_err(|e| WebError::InternalError(format!("{}: {}", msg, e)))
    }
    
    fn or_not_found(self, msg: &str) -> WebResult<T> {
        self.map_err(|_| WebError::NotFound(msg.to_string()))
    }
    
    fn or_bad_request(self, msg: &str) -> WebResult<T> {
        self.map_err(|_| WebError::BadRequest(msg.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_error_status_codes() {
        assert_eq!(WebError::BadRequest("test".into()).status_code(), StatusCode::BAD_REQUEST);
        assert_eq!(WebError::NotFound("test".into()).status_code(), StatusCode::NOT_FOUND);
        assert_eq!(WebError::InternalError("test".into()).status_code(), StatusCode::INTERNAL_SERVER_ERROR);
    }
    
    #[test]
    fn test_error_codes() {
        assert_eq!(WebError::BadRequest("test".into()).error_code(), "BAD_REQUEST");
        assert_eq!(WebError::NotFound("test".into()).error_code(), "NOT_FOUND");
        assert_eq!(WebError::InternalError("test".into()).error_code(), "INTERNAL_ERROR");
    }
    
    #[test]
    fn test_web_error_ext() {
        let result: Result<i32, &str> = Err("error");

        let web_err = result.web_context("Operation failed").unwrap_err();
        assert!(matches!(web_err, WebError::InternalError(_)));
        assert_eq!(web_err.to_string(), "Internal server error: Operation failed: error");

        let result: Result<i32, &str> = Err("error");
        let web_err = result.or_not_found("Resource not found").unwrap_err();
        assert!(matches!(web_err, WebError::NotFound(_)));
        assert_eq!(web_err.to_string(), "Not found: Resource not found");
    }

    /// Pin the complete status-code matrix. Pair each variant with
    /// both its status and its code string so a refactor can't drop
    /// one without noticing.
    #[test]
    fn status_and_code_matrix_for_every_variant() {
        let cases: Vec<(WebError, StatusCode, &str)> = vec![
            (
                WebError::BadRequest("x".into()),
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
            ),
            (
                WebError::Unauthorized("x".into()),
                StatusCode::UNAUTHORIZED,
                "UNAUTHORIZED",
            ),
            (
                WebError::Forbidden("x".into()),
                StatusCode::FORBIDDEN,
                "FORBIDDEN",
            ),
            (
                WebError::NotFound("x".into()),
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
            ),
            (
                WebError::Conflict("x".into()),
                StatusCode::CONFLICT,
                "CONFLICT",
            ),
            (
                WebError::ValidationError("x".into()),
                StatusCode::UNPROCESSABLE_ENTITY,
                "VALIDATION_ERROR",
            ),
            (
                WebError::RateLimitExceeded("x".into()),
                StatusCode::TOO_MANY_REQUESTS,
                "RATE_LIMIT_EXCEEDED",
            ),
            (
                WebError::InternalError("x".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
            ),
            (
                WebError::ServiceUnavailable("x".into()),
                StatusCode::SERVICE_UNAVAILABLE,
                "SERVICE_UNAVAILABLE",
            ),
        ];
        for (err, status, code) in cases {
            assert_eq!(err.status_code(), status, "wrong status for {}", err);
            assert_eq!(err.error_code(), code, "wrong code for {}", err);
        }
    }

    /// Wrapping variants (DatabaseError / RedisError / etc.) all map
    /// to 500 Internal Server Error to avoid leaking backend details
    /// over HTTP. Pin that policy.
    #[test]
    fn wrapping_variants_are_all_500() {
        // Use a sqlx error that doesn't need a real DB.
        let db_err: WebError = sqlx::Error::PoolTimedOut.into();
        assert_eq!(db_err.status_code(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(db_err.error_code(), "DATABASE_ERROR");

        let ser_err: WebError = serde_json::from_str::<serde_json::Value>("not json")
            .unwrap_err()
            .into();
        assert_eq!(ser_err.status_code(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(ser_err.error_code(), "SERIALIZATION_ERROR");
    }

    /// Custom variant: the status code and error code are passed
    /// through verbatim — that's the whole point.
    #[test]
    fn custom_variant_passes_through_status_and_code() {
        let err = WebError::Custom {
            status: StatusCode::IM_A_TEAPOT,
            code: "COFFEE_IN_TEAPOT".into(),
            message: "short and stout".into(),
        };
        assert_eq!(err.status_code(), StatusCode::IM_A_TEAPOT);
        assert_eq!(err.error_code(), "COFFEE_IN_TEAPOT");
        // Display uses the message only (no variant prefix).
        assert_eq!(err.to_string(), "short and stout");
    }

    /// `From<std::io::Error>` prefixes the message with "I/O error:"
    /// so the log line can identify the source at a glance.
    #[test]
    fn from_io_error_produces_internal_error_with_prefix() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let web: WebError = io.into();
        assert!(matches!(web, WebError::InternalError(_)));
        assert_eq!(web.error_code(), "INTERNAL_ERROR");
        assert!(
            web.to_string().contains("I/O error: missing"),
            "expected I/O prefix in message, got {}",
            web
        );
    }

    /// Error::source() must unwrap for wrapping variants so
    /// tracing/anyhow chains report the root cause.
    #[test]
    fn source_unwraps_for_wrapping_variants_and_is_none_for_others() {
        use std::error::Error as _;

        let db: WebError = sqlx::Error::PoolTimedOut.into();
        assert!(db.source().is_some());

        assert!(WebError::BadRequest("x".into()).source().is_none());
        assert!(WebError::NotFound("x".into()).source().is_none());
        assert!(
            WebError::Custom {
                status: StatusCode::BAD_GATEWAY,
                code: "X".into(),
                message: "y".into(),
            }
            .source()
            .is_none()
        );
    }

    /// `or_bad_request` untested in the existing suite — pin it.
    #[test]
    fn web_error_ext_or_bad_request() {
        let result: Result<i32, &str> = Err("parse failure");
        let web_err = result.or_bad_request("Body must be JSON").unwrap_err();
        assert!(matches!(web_err, WebError::BadRequest(_)));
        assert_eq!(web_err.to_string(), "Bad request: Body must be JSON");
        assert_eq!(web_err.error_code(), "BAD_REQUEST");
    }

    /// IntoResponse must emit the status + a JSON body with the
    /// documented `code` / `message` fields. Assert against the
    /// serialised body so the client-facing contract is pinned.
    #[tokio::test]
    async fn into_response_serialises_error_response_as_json() {
        let err = WebError::NotFound("codebase 'nope'".into());
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // Assert Content-Type: application/json (axum::Json sets this).
        assert_eq!(
            resp.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "application/json"
        );

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: ErrorResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed.code, "NOT_FOUND");
        assert_eq!(parsed.message, "Not found: codebase 'nope'");
        assert!(parsed.details.is_none());
        assert!(parsed.request_id.is_none());
    }

    /// ErrorResponse skips `details` and `request_id` when None —
    /// clients rely on absent-vs-present to distinguish "no detail"
    /// from "empty detail".
    #[test]
    fn error_response_omits_none_fields_in_json() {
        let e = ErrorResponse {
            code: "X".into(),
            message: "y".into(),
            details: None,
            request_id: None,
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(!s.contains("details"), "details should be omitted: {}", s);
        assert!(
            !s.contains("request_id"),
            "request_id should be omitted: {}",
            s
        );
    }
}