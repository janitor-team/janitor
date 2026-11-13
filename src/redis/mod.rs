//! Redis connection management and pub/sub helpers for the Janitor platform.

use crate::error::JanitorError;
use redis::{
    aio::{ConnectionManager, ConnectionManagerConfig},
    Client,
};
use std::{sync::Arc, time::Duration};
use tokio::sync::OnceCell;
use tracing::{error, info, warn};

pub mod pubsub;

pub use pubsub::{PubSubMessage, Publisher, Subscriber};

/// Configuration for Redis connections
#[derive(Clone)]
pub struct RedisConfig {
    /// Redis URL
    pub url: String,
    /// Password, kept out of the URL so it is never logged
    pub password: Option<String>,
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
            password: None,
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

    /// Set the password to authenticate with
    pub fn with_password(mut self, password: Option<String>) -> Self {
        self.password = password;
        self
    }

    /// Connection info for the URL with the password added
    pub fn connection_info(&self) -> redis::RedisResult<redis::ConnectionInfo> {
        let info = redis::IntoConnectionInfo::into_connection_info(self.url.as_str())?;
        Ok(match &self.password {
            Some(password) => {
                let settings = info.redis_settings().clone().set_password(password);
                info.set_redis_settings(settings)
            }
            None => info,
        })
    }

    /// The URL with the password added, for clients that only take a URL
    pub fn url_with_password(&self) -> Result<String, url::ParseError> {
        let mut url = url::Url::parse(&self.url)?;
        if let Some(password) = &self.password {
            if url.set_password(Some(password)).is_err() {
                // A unix socket URL has no host to put it on.
                url.query_pairs_mut().append_pair("pass", password);
            }
        }
        Ok(url.into())
    }
}

impl std::fmt::Debug for RedisConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisConfig")
            .field("url", &self.url)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field(
                "connection_timeout_seconds",
                &self.connection_timeout_seconds,
            )
            .field("retry_attempts", &self.retry_attempts)
            .field("retry_delay_ms", &self.retry_delay_ms)
            .finish()
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
        let client = Client::open(config.connection_info()?)?;

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
            .get_or_try_init(|| async {
                info!("Attempting to connect to Redis at {}", self.config.url);
                let config = ConnectionManagerConfig::new().set_connection_timeout(Some(
                    Duration::from_secs(self.config.connection_timeout_seconds),
                ));
                match ConnectionManager::new_with_config((*self.client).clone(), config).await {
                    Ok(conn) => {
                        info!("Successfully connected to Redis");
                        Ok(conn)
                    }
                    Err(e) => {
                        error!("Failed to connect to Redis: {}", e);
                        Err(e)
                    }
                }
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
        let mut last_error = None;
        for attempt in 1..=self.config.retry_attempts {
            if attempt > 1 {
                tokio::time::sleep(Duration::from_millis(self.config.retry_delay_ms)).await;
            }
            let mut conn = match self.get_connection_manager().await {
                Ok(conn) => conn,
                Err(e) => {
                    last_error = Some(redis::RedisError::from((
                        redis::ErrorKind::Io,
                        "Connection pool error",
                        e.to_string(),
                    )));
                    continue;
                }
            };
            match cmd.query_async(&mut conn).await {
                Ok(result) => return Ok(result),
                Err(e) if Self::is_connection_error(&e) => {
                    warn!("Redis connection error, will retry: {}", e);
                    last_error = Some(e);
                }
                Err(e) => return Err(e.into()),
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
    fn test_password_stays_out_of_url() {
        let config = RedisConfig::new("redis://redis:6379/0").with_password(Some("s3cret".into()));
        let info = config.connection_info().unwrap();
        assert_eq!(info.redis_settings().password(), Some("s3cret"));
        assert_eq!(
            config.url_with_password().unwrap(),
            "redis://:s3cret@redis:6379/0"
        );
        assert!(!format!("{:?}", config).contains("s3cret"));

        let unix =
            RedisConfig::new("redis+unix:///run/redis.sock").with_password(Some("s3cret".into()));
        assert_eq!(
            unix.connection_info().unwrap().redis_settings().password(),
            Some("s3cret")
        );
        assert_eq!(
            unix.url_with_password().unwrap(),
            "redis+unix:///run/redis.sock?pass=s3cret"
        );
    }

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
