//! Shared Redis configuration for all Janitor services

use redis::Client as RedisClient;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use url::Url;

use crate::shared_config::{env::EnvParser, ConfigError, FromEnv, ValidationError};

/// Redis configuration used across all Janitor services that need Redis coordination
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedisConfig {
    /// Redis connection URL
    pub url: String,

    /// Connection timeout in seconds
    #[serde(default = "default_redis_connection_timeout")]
    pub connection_timeout_seconds: u64,

    /// Command timeout in seconds
    #[serde(default = "default_redis_command_timeout")]
    pub command_timeout_seconds: u64,

    /// Maximum number of connections in pool
    #[serde(default = "default_redis_max_connections")]
    pub max_connections: u32,

    /// Connection retry attempts
    #[serde(default = "default_redis_retry_attempts")]
    pub retry_attempts: u32,

    /// Retry delay in milliseconds
    #[serde(default = "default_redis_retry_delay")]
    pub retry_delay_ms: u64,

    /// Database number to use (default 0)
    #[serde(default)]
    pub database: Option<i64>,

    /// Username for authentication
    #[serde(default)]
    pub username: Option<String>,

    /// Password for authentication  
    #[serde(default)]
    pub password: Option<String>,
}

impl Default for RedisConfig {
    fn default() -> Self {
        Self {
            url: "redis://localhost:6379".to_string(),
            connection_timeout_seconds: default_redis_connection_timeout(),
            command_timeout_seconds: default_redis_command_timeout(),
            max_connections: default_redis_max_connections(),
            retry_attempts: default_redis_retry_attempts(),
            retry_delay_ms: default_redis_retry_delay(),
            database: None,
            username: None,
            password: None,
        }
    }
}

impl FromEnv for RedisConfig {
    fn from_env() -> Result<Self, ConfigError> {
        Self::from_env_with_prefix("")
    }

    fn from_env_with_prefix(prefix: &str) -> Result<Self, ConfigError> {
        let parser = EnvParser::with_prefix(prefix);

        let url = parser
            .get_string("REDIS_URL")
            .unwrap_or_else(|| "redis://localhost:6379".to_string());

        let config = Self {
            url,
            connection_timeout_seconds: parser
                .get_u64("REDIS_CONNECTION_TIMEOUT_SECONDS")?
                .unwrap_or_else(default_redis_connection_timeout),
            command_timeout_seconds: parser
                .get_u64("REDIS_COMMAND_TIMEOUT_SECONDS")?
                .unwrap_or_else(default_redis_command_timeout),
            max_connections: parser
                .get_u32("REDIS_MAX_CONNECTIONS")?
                .unwrap_or_else(default_redis_max_connections),
            retry_attempts: parser
                .get_u32("REDIS_RETRY_ATTEMPTS")?
                .unwrap_or_else(default_redis_retry_attempts),
            retry_delay_ms: parser
                .get_u64("REDIS_RETRY_DELAY_MS")?
                .unwrap_or_else(default_redis_retry_delay),
            database: parser.get_i64("REDIS_DATABASE")?,
            username: parser.get_string("REDIS_USERNAME"),
            password: parser.get_string("REDIS_PASSWORD"),
        };
        config.validate()?;
        Ok(config)
    }
}

impl RedisConfig {
    /// Validate the Redis configuration
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.url.is_empty() {
            return Err(ValidationError::InvalidValue {
                field: "url".to_string(),
                message: "Redis URL cannot be empty".to_string(),
            });
        }

        // Validate URL format
        if !self.url.starts_with("redis://")
            && !self.url.starts_with("rediss://")
            && !self.url.starts_with("redis+unix://")
        {
            return Err(ValidationError::InvalidValue {
                field: "url".to_string(),
                message: "Redis URL must start with redis://, rediss://, or redis+unix://"
                    .to_string(),
            });
        }

        if let Err(message) = crate::utils::check_url_without_password(&self.url) {
            return Err(ValidationError::InvalidValue {
                field: "url".to_string(),
                message: format!("Redis URL {}, use the password setting", message),
            });
        }

        if self.max_connections == 0 {
            return Err(ValidationError::InvalidValue {
                field: "max_connections".to_string(),
                message: "Maximum connections must be greater than 0".to_string(),
            });
        }

        if let Some(db) = self.database {
            if db < 0 || db > 15 {
                return Err(ValidationError::InvalidValue {
                    field: "database".to_string(),
                    message: "Database number must be between 0 and 15".to_string(),
                });
            }
        }

        Ok(())
    }

    /// Create a Redis client from this configuration
    pub fn create_client(&self) -> Result<RedisClient, redis::RedisError> {
        // Build the connection URL with authentication and database
        let mut url = self.url.clone();

        // If username or password or database is specified, we need to build a complete URL
        if self.username.is_some() || self.password.is_some() || self.database.is_some() {
            let parsed_url = Url::parse(&self.url).map_err(|_| {
                redis::RedisError::from((
                    redis::ErrorKind::InvalidClientConfig,
                    "Invalid URL format",
                ))
            })?;
            let mut new_url = parsed_url.clone();

            // Set authentication if provided
            if let Some(username) = &self.username {
                new_url.set_username(username).map_err(|_| {
                    redis::RedisError::from((
                        redis::ErrorKind::InvalidClientConfig,
                        "Failed to set username",
                    ))
                })?;
            }

            if let Some(password) = &self.password {
                new_url.set_password(Some(password)).map_err(|_| {
                    redis::RedisError::from((
                        redis::ErrorKind::InvalidClientConfig,
                        "Failed to set password",
                    ))
                })?;
            }

            // Set database if provided (append as path)
            if let Some(database) = self.database {
                new_url.set_path(&format!("/{}", database));
            }

            url = new_url.to_string();
        }

        RedisClient::open(url.as_str())
    }

    /// Get connection timeout as Duration
    pub fn connection_timeout(&self) -> Duration {
        Duration::from_secs(self.connection_timeout_seconds)
    }

    /// Get command timeout as Duration
    pub fn command_timeout(&self) -> Duration {
        Duration::from_secs(self.command_timeout_seconds)
    }

    /// Get retry delay as Duration
    pub fn retry_delay(&self) -> Duration {
        Duration::from_millis(self.retry_delay_ms)
    }
}

// Default value functions for Redis configuration
fn default_redis_connection_timeout() -> u64 {
    10
}

fn default_redis_command_timeout() -> u64 {
    5
}

fn default_redis_max_connections() -> u32 {
    10
}

fn default_redis_retry_attempts() -> u32 {
    3
}

fn default_redis_retry_delay() -> u64 {
    1000 // 1 second
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_redis_config_validation() {
        let mut config = RedisConfig::default();
        assert!(config.validate().is_ok());

        // Test empty URL
        config.url = "".to_string();
        assert!(config.validate().is_err());

        // Test invalid URL
        config.url = "mysql://localhost/test".to_string();
        assert!(config.validate().is_err());

        // Test valid URLs
        config.url = "redis://localhost:6379".to_string();
        assert!(config.validate().is_ok());

        config.url = "rediss://localhost:6380".to_string();
        assert!(config.validate().is_ok());

        config.url = "redis+unix:///var/run/redis.sock".to_string();
        assert!(config.validate().is_ok());

        // Test zero max connections
        config.url = "redis://localhost:6379".to_string();
        config.max_connections = 0;
        assert!(config.validate().is_err());

        // Test invalid database number
        config.max_connections = 10;
        config.database = Some(-1);
        assert!(config.validate().is_err());

        config.database = Some(16);
        assert!(config.validate().is_err());

        config.database = Some(0);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_redis_config_from_env_rejects_password() {
        std::env::set_var("PWTEST_REDIS_URL", "redis://:secret@localhost:6379");
        assert!(RedisConfig::from_env_with_prefix("PWTEST").is_err());
        std::env::set_var("PWTEST_REDIS_URL", "redis://localhost:6379");
        std::env::set_var("PWTEST_REDIS_PASSWORD", "secret");
        let config = RedisConfig::from_env_with_prefix("PWTEST").unwrap();
        assert_eq!(config.password.as_deref(), Some("secret"));
    }

    #[test]
    fn test_redis_config_default() {
        let config = RedisConfig::default();
        assert_eq!(config.url, "redis://localhost:6379");
        assert_eq!(config.connection_timeout_seconds, 10);
        assert_eq!(config.command_timeout_seconds, 5);
        assert_eq!(config.max_connections, 10);
        assert_eq!(config.retry_attempts, 3);
        assert_eq!(config.retry_delay_ms, 1000);
    }

    #[test]
    fn test_redis_client_creation() {
        let config = RedisConfig::default();
        assert!(config.create_client().is_ok());

        // Test with authentication
        let mut config_with_auth = config.clone();
        config_with_auth.username = Some("testuser".to_string());
        config_with_auth.password = Some("testpass".to_string());
        config_with_auth.database = Some(1);

        // This will fail to connect but should successfully create the client
        assert!(config_with_auth.create_client().is_ok());
    }
}
