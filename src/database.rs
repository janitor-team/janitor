//! Shared database utilities and connection management

use sqlx::PgPool;
use sqlx_postgres::PgPoolOptions;
use std::time::Duration;

/// Database connection error
#[derive(Debug)]
pub enum DatabaseError {
    Connection(sqlx::Error),
    Config(String),
}

impl std::fmt::Display for DatabaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connection(e) => write!(f, "Database connection error: {}", e),
            Self::Config(msg) => write!(f, "Configuration error: {}", msg),
        }
    }
}

impl std::error::Error for DatabaseError {}

impl From<sqlx::Error> for DatabaseError {
    fn from(e: sqlx::Error) -> Self {
        Self::Connection(e)
    }
}

/// Shared database connection pool configuration
#[derive(Debug, Clone)]
pub struct DatabaseConfig {
    /// Database URL
    pub url: String,
    /// Maximum number of connections in the pool
    pub max_connections: u32,
    /// Connection timeout
    pub connect_timeout: Duration,
    /// Idle timeout for connections
    pub idle_timeout: Option<Duration>,
    /// Maximum lifetime for connections
    pub max_lifetime: Option<Duration>,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: "postgresql://localhost/janitor".to_string(),
            max_connections: 10,
            connect_timeout: Duration::from_secs(30),
            idle_timeout: Some(Duration::from_secs(600)), // 10 minutes
            max_lifetime: Some(Duration::from_secs(1800)), // 30 minutes
        }
    }
}

impl DatabaseConfig {
    /// Create a new database configuration
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Default::default()
        }
    }

    /// Set maximum connections
    pub fn with_max_connections(mut self, max_connections: u32) -> Self {
        self.max_connections = max_connections;
        self
    }
}

/// Database connection manager with optional Redis support
#[derive(Debug, Clone)]
pub struct Database {
    pool: PgPool,
    redis: Option<redis::Client>,
}

impl Database {
    /// Create a database instance from an existing pool
    ///
    /// This is useful for compatibility with existing code that creates pools manually.
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool, redis: None }
    }

    /// Create a database instance from an existing pool with Redis support
    pub fn from_pool_with_redis(pool: PgPool, redis: redis::Client) -> Self {
        Self {
            pool,
            redis: Some(redis),
        }
    }

    /// Create a database instance from an existing pool and configuration
    ///
    /// This is useful for test scenarios where you have an existing pool
    /// but want to preserve configuration information.
    pub fn from_pool_and_config(pool: PgPool, _config: DatabaseConfig) -> Self {
        // For now, we don't need to store the config, just use the pool
        Self { pool, redis: None }
    }

    /// Create a new database connection with custom configuration
    pub async fn connect_with_config(config: DatabaseConfig) -> Result<Self, DatabaseError> {
        Self::connect_with_config_and_redis(config, None).await
    }

    /// Create a new database connection with custom configuration and Redis
    pub async fn connect_with_config_and_redis(
        config: DatabaseConfig,
        redis_url: Option<&str>,
    ) -> Result<Self, DatabaseError> {
        let mut options = PgPoolOptions::new()
            .max_connections(config.max_connections)
            .acquire_timeout(config.connect_timeout);

        if let Some(idle_timeout) = config.idle_timeout {
            options = options.idle_timeout(idle_timeout);
        }

        if let Some(max_lifetime) = config.max_lifetime {
            options = options.max_lifetime(max_lifetime);
        }

        let pool = options.connect(&config.url).await?;

        let redis =
            if let Some(url) = redis_url {
                Some(redis::Client::open(url).map_err(|e| {
                    DatabaseError::Config(format!("Failed to connect to Redis: {}", e))
                })?)
            } else {
                None
            };

        Ok(Self { pool, redis })
    }

    /// Get a reference to the connection pool
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Get a reference to the Redis client if available
    pub fn redis(&self) -> Option<&redis::Client> {
        self.redis.as_ref()
    }

    /// Test the database connection
    pub async fn test_connection(&self) -> Result<(), DatabaseError> {
        sqlx::query("SELECT 1").fetch_one(&self.pool).await?;
        Ok(())
    }

    /// Execute a health check query
    pub async fn health_check(&self) -> Result<bool, DatabaseError> {
        match sqlx::query_scalar::<_, i32>("SELECT 1 as health")
            .fetch_one(&self.pool)
            .await
        {
            Ok(health) => Ok(health == 1),
            Err(_) => Ok(false),
        }
    }
}
