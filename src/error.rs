//! Shared error type for the Janitor platform

/// Error returned by the shared Redis helpers and their message handlers.
#[derive(Debug)]
pub enum JanitorError {
    /// Redis errors
    Redis(redis::RedisError),

    /// External service errors (VCS, build tools, etc.)
    ExternalService { service: String, message: String },
}

impl std::fmt::Display for JanitorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Redis(e) => write!(f, "Redis error: {}", e),
            Self::ExternalService { service, message } => {
                write!(f, "External service error: {}: {}", service, message)
            }
        }
    }
}

impl std::error::Error for JanitorError {}

impl From<redis::RedisError> for JanitorError {
    fn from(e: redis::RedisError) -> Self {
        Self::Redis(e)
    }
}

impl JanitorError {
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
}
