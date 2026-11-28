//! Small config structs shared by the git-store and bzr-store services.

use serde::{Deserialize, Serialize};

fn default_max_connections() -> u32 {
    10
}

/// PostgreSQL connection settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    pub url: String,

    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: "postgresql://localhost/janitor".to_string(),
            max_connections: default_max_connections(),
        }
    }
}

/// Settings for the standard middleware stack of a web service.
#[derive(Debug, Clone)]
pub struct WebConfig {
    pub request_timeout_seconds: u64,
    pub max_request_size_bytes: usize,
    pub enable_request_logging: bool,
    pub enable_cors: bool,
    pub enable_compression: bool,
}
