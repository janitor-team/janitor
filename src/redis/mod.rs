//! Redis connection management and pub/sub helpers for the Janitor platform.

use crate::error::JanitorError;
use redis::{
    aio::{ConnectionManager, ConnectionManagerConfig},
    Client,
};
use std::{sync::Arc, time::Duration};
use tokio::sync::OnceCell;
use tracing::warn;

pub mod pubsub;

pub use pubsub::{PubSubMessage, Publisher, Subscriber};

/// Configuration for Redis connections
#[derive(Debug, Clone)]
pub struct RedisConfig {
    /// Redis URL
    pub url: String,
    /// Connection timeout in seconds
    pub connection_timeout_seconds: u64,
    /// Number of retry attempts for failed operations
    pub retry_attempts: u32,
    /// Delay between retries in milliseconds
    pub retry_delay_ms: u64,
}

impl Default for RedisConfig {
    fn default() -> Self {
        Self {
            url: "redis://127.0.0.1/".to_string(),
            connection_timeout_seconds: 30,
            retry_attempts: 3,
            retry_delay_ms: 1000,
        }
    }
}

impl RedisConfig {
    /// Create a new Redis configuration with the given URL
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Default::default()
        }
    }
}

/// Redis manager holding a single, self-reconnecting connection that is
/// shared by every clone.
#[derive(Clone)]
pub struct RedisManager {
    client: Arc<Client>,
    config: Arc<RedisConfig>,
    connection: Arc<OnceCell<ConnectionManager>>,
}

impl RedisManager {
    /// Create a new Redis manager from configuration
    pub fn new(config: RedisConfig) -> Result<Self, JanitorError> {
        let client = Client::open(config.url.as_str())?;

        Ok(Self {
            client: Arc::new(client),
            config: Arc::new(config),
            connection: Arc::new(OnceCell::new()),
        })
    }

    /// Get the shared connection manager, connecting on first use
    pub async fn get_connection_manager(&self) -> Result<ConnectionManager, JanitorError> {
        let conn = self
            .connection
            .get_or_try_init(|| {
                let config = ConnectionManagerConfig::new().set_connection_timeout(Some(
                    Duration::from_secs(self.config.connection_timeout_seconds),
                ));
                ConnectionManager::new_with_config((*self.client).clone(), config)
            })
            .await?;
        Ok(conn.clone())
    }

    /// Perform a health check
    pub async fn health_check(&self) -> Result<(), JanitorError> {
        let mut conn = self.get_connection_manager().await?;
        redis::cmd("PING").query_async::<String>(&mut conn).await?;
        Ok(())
    }

    /// Create a publisher for pub/sub operations
    pub fn publisher(&self) -> Publisher {
        Publisher::new(self.clone())
    }

    /// Create a subscriber for pub/sub operations
    pub fn subscriber(&self) -> Subscriber {
        Subscriber::new(self.clone())
    }

    /// Get the underlying client (for specialized operations)
    pub fn client(&self) -> &Arc<Client> {
        &self.client
    }

    /// Execute a Redis command, retrying on connection failure
    pub async fn execute<T>(&self, cmd: &mut redis::Cmd) -> Result<T, JanitorError>
    where
        T: redis::FromRedisValue,
    {
        let mut attempt = 1;
        loop {
            let mut conn = self.get_connection_manager().await?;
            match cmd.query_async(&mut conn).await {
                Ok(result) => return Ok(result),
                Err(e) if Self::is_connection_error(&e) && attempt < self.config.retry_attempts => {
                    warn!("Redis connection error, will retry: {}", e);
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(self.config.retry_delay_ms)).await;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Check if an error is a connection-related error that should trigger a retry
    fn is_connection_error(error: &redis::RedisError) -> bool {
        matches!(
            error.kind(),
            redis::ErrorKind::Io
                | redis::ErrorKind::Server(redis::ServerErrorKind::BusyLoading)
                | redis::ErrorKind::Server(redis::ServerErrorKind::TryAgain)
                | redis::ErrorKind::Server(redis::ServerErrorKind::ClusterDown)
                | redis::ErrorKind::Server(redis::ServerErrorKind::MasterDown)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_connection_error() {
        // Test connection error detection
        let io_error = redis::RedisError::from((redis::ErrorKind::Io, "Connection refused"));
        assert!(RedisManager::is_connection_error(&io_error));

        let parse_error =
            redis::RedisError::from((redis::ErrorKind::UnexpectedReturnType, "Invalid type"));
        assert!(!RedisManager::is_connection_error(&parse_error));
    }
}
