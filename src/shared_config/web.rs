//! Shared web server configuration for Janitor services

/// Settings for the standard middleware stack of a web service.
#[derive(Debug, Clone)]
pub struct WebConfig {
    /// Request timeout in seconds
    pub request_timeout_seconds: u64,

    /// Maximum request size in bytes
    pub max_request_size_bytes: usize,

    /// Enable request logging
    pub enable_request_logging: bool,

    /// Enable CORS headers
    pub enable_cors: bool,

    /// Enable compression
    pub enable_compression: bool,
}
