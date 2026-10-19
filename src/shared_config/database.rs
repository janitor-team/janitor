//! Shared database configuration for Janitor services

use serde::{Deserialize, Serialize};

use crate::shared_config::{env::EnvParser, ConfigError, FromEnv};

fn default_max_connections() -> u32 {
    10
}

/// Database configuration used across Janitor services
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    /// PostgreSQL connection URL
    pub url: String,

    /// Maximum number of connections in the pool
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

impl FromEnv for DatabaseConfig {
    fn from_env() -> Result<Self, ConfigError> {
        Self::from_env_with_prefix("")
    }

    fn from_env_with_prefix(prefix: &str) -> Result<Self, ConfigError> {
        let parser = EnvParser::with_prefix(prefix);

        let url = parser
            .get_string("DATABASE_URL")
            .ok_or_else(|| ConfigError::MissingRequired("DATABASE_URL".to_string()))?;

        Ok(Self {
            url,
            max_connections: parser
                .get_u32("DATABASE_MAX_CONNECTIONS")?
                .unwrap_or_else(default_max_connections),
        })
    }
}
