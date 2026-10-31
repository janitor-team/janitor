//! Redis connection management and utilities for the Janitor platform.
//!
//! This module provides a centralized Redis connection manager that handles:
//! - Connection pooling and management
//! - Automatic reconnection with backoff
//! - Pub/Sub messaging
//! - Health checking
//! - Standardized error handling

use crate::error::JanitorError;
use redis::{aio::ConnectionManager, Client};
use std::{sync::Arc, time::Duration};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

pub mod connection;
pub mod pubsub;

pub use connection::ConnectionPool;
pub use pubsub::{PubSubMessage, Publisher, Subscriber};

/// Configuration for Redis connections
#[derive(Debug, Clone)]
pub struct RedisConfig {
    /// Redis URL
    pub url: String,
    /// Maximum number of connections in the pool
    pub max_connections: u32,
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
            max_connections: 10,
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

/// Redis manager for centralized connection and operation management
#[derive(Clone)]
pub struct RedisManager {
    /// Redis client
    client: Arc<Client>,
    /// Configuration
    config: Arc<RedisConfig>,
    /// Connection pool for async operations
    connection_pool: Arc<ConnectionPool>,
    /// Current connection state
    connection_state: Arc<RwLock<ConnectionState>>,
}

/// Connection state tracking
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConnectionState {
    /// Not connected
    Disconnected,
    /// Currently connecting
    Connecting,
    /// Successfully connected
    Connected,
    /// Connection failed
    Failed,
}

impl RedisManager {
    /// Create a new Redis manager from configuration
    pub fn new(config: RedisConfig) -> Result<Self, JanitorError> {
        let client = Client::open(config.url.as_str()).map_err(|e| JanitorError::Redis(e))?;

        let connection_pool = ConnectionPool::new(
            Arc::new(client.clone()),
            config.max_connections as usize,
            config.connection_timeout_seconds * 1000, // Convert to ms
        );

        Ok(Self {
            client: Arc::new(client),
            config: Arc::new(config),
            connection_pool: Arc::new(connection_pool),
            connection_state: Arc::new(RwLock::new(ConnectionState::Disconnected)),
        })
    }

    /// Get a connection from the pool
    pub async fn get_connection(&self) -> Result<ConnectionManager, JanitorError> {
        self.ensure_connected().await?;
        self.connection_pool.get().await
    }

    /// Get a connection manager (single connection shared across tasks)
    pub async fn get_connection_manager(&self) -> Result<ConnectionManager, JanitorError> {
        self.ensure_connected().await?;

        ConnectionManager::new((*self.client).clone())
            .await
            .map_err(|e| JanitorError::Redis(e))
    }

    /// Ensure we have a valid connection, attempting to reconnect if necessary
    async fn ensure_connected(&self) -> Result<(), JanitorError> {
        let state = *self.connection_state.read().await;
        match state {
            ConnectionState::Connected => return Ok(()),
            ConnectionState::Connecting => {
                // Wait a bit for ongoing connection attempt
                tokio::time::sleep(Duration::from_millis(100)).await;
                if *self.connection_state.read().await == ConnectionState::Connected {
                    return Ok(());
                }
            }
            _ => {}
        }

        // Try to connect
        self.connect().await
    }

    /// Attempt to establish a connection
    async fn connect(&self) -> Result<(), JanitorError> {
        let mut state = self.connection_state.write().await;

        // Check if another task already connected
        if *state == ConnectionState::Connected {
            return Ok(());
        }

        *state = ConnectionState::Connecting;
        drop(state);

        info!("Attempting to connect to Redis at {}", self.config.url);

        match self.test_connection().await {
            Ok(()) => {
                info!("Successfully connected to Redis");
                *self.connection_state.write().await = ConnectionState::Connected;
                Ok(())
            }
            Err(e) => {
                error!("Failed to connect to Redis: {}", e);
                *self.connection_state.write().await = ConnectionState::Failed;
                Err(e)
            }
        }
    }

    /// Test the connection with a PING command
    async fn test_connection(&self) -> Result<(), JanitorError> {
        let mut conn = self
            .client
            .get_connection_manager()
            .await
            .map_err(JanitorError::Redis)?;

        redis::cmd("PING")
            .query_async::<String>(&mut conn)
            .await
            .map_err(JanitorError::Redis)?;

        Ok(())
    }

    /// Perform a health check
    pub async fn health_check(&self) -> Result<(), JanitorError> {
        self.test_connection().await
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

    /// Execute a Redis command with automatic retry on connection failure
    pub async fn execute<T>(&self, cmd: &mut redis::Cmd) -> Result<T, JanitorError>
    where
        T: redis::FromRedisValue,
    {
        let mut retries = self.config.retry_attempts;
        let mut last_error = None;

        while retries > 0 {
            match self.get_connection().await {
                Ok(mut conn) => match cmd.query_async(&mut conn).await {
                    Ok(result) => return Ok(result),
                    Err(e) => {
                        if Self::is_connection_error(&e) {
                            warn!("Redis connection error, will retry: {}", e);
                            *self.connection_state.write().await = ConnectionState::Disconnected;
                            last_error = Some(e);
                            retries -= 1;

                            if retries > 0 {
                                tokio::time::sleep(Duration::from_millis(
                                    self.config.retry_delay_ms,
                                ))
                                .await;
                            }
                        } else {
                            return Err(JanitorError::Redis(e));
                        }
                    }
                },
                Err(e) => {
                    last_error = Some(redis::RedisError::from((
                        redis::ErrorKind::Io,
                        "Connection pool error",
                        e.to_string(),
                    )));
                    retries -= 1;

                    if retries > 0 {
                        tokio::time::sleep(Duration::from_millis(self.config.retry_delay_ms)).await;
                    }
                }
            }
        }

        Err(JanitorError::redis_msg(format!(
            "Command failed after {} retries: {}",
            self.config.retry_attempts,
            last_error
                .map(|e| e.to_string())
                .unwrap_or_else(|| "Unknown error".to_string())
        )))
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

    #[tokio::test]
    async fn test_get_connection_manager_does_not_hang() {
        // Nothing listens on this port; redis-rs retries the connect for about 9 s.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let manager =
            RedisManager::new(RedisConfig::new(format!("redis://127.0.0.1:{}/", port))).unwrap();
        let result =
            tokio::time::timeout(Duration::from_secs(30), manager.get_connection_manager()).await;
        assert!(matches!(result, Ok(Err(_))));
        assert_eq!(
            *manager.connection_state.read().await,
            ConnectionState::Failed
        );
    }
}
