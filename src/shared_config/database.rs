//! Shared database configuration for all Janitor services

use serde::{Deserialize, Serialize};
use sqlx::{postgres::PgPoolOptions, PgPool, Postgres, Transaction};
use std::time::Duration;
use tracing::info;

use crate::shared_config::{defaults::*, env::EnvParser, ConfigError, FromEnv, ValidationError};

/// Database configuration used across all Janitor services
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    /// PostgreSQL connection URL
    pub url: String,

    /// Maximum number of connections in the pool
    #[serde(default = "default_db_max_connections")]
    pub max_connections: u32,

    /// Connection timeout in seconds
    #[serde(default = "default_db_connection_timeout")]
    pub connection_timeout_seconds: u64,

    /// Query timeout in seconds
    #[serde(default = "default_db_query_timeout")]
    pub query_timeout_seconds: u64,

    /// Enable SQL query logging (for debugging)
    #[serde(default = "default_false")]
    pub enable_sql_logging: bool,

    /// Minimum idle connections to maintain
    #[serde(default)]
    pub min_connections: Option<u32>,

    /// Maximum lifetime of a connection in seconds
    #[serde(default)]
    pub max_lifetime_seconds: Option<u64>,

    /// Idle timeout for connections in seconds
    #[serde(default)]
    pub idle_timeout_seconds: Option<u64>,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: "postgresql://localhost/janitor".to_string(),
            max_connections: default_db_max_connections(),
            connection_timeout_seconds: default_db_connection_timeout(),
            query_timeout_seconds: default_db_query_timeout(),
            enable_sql_logging: default_false(),
            min_connections: None,
            max_lifetime_seconds: None,
            idle_timeout_seconds: None,
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
                .unwrap_or_else(default_db_max_connections),
            connection_timeout_seconds: parser
                .get_u64("DATABASE_CONNECTION_TIMEOUT_SECONDS")?
                .unwrap_or_else(default_db_connection_timeout),
            query_timeout_seconds: parser
                .get_u64("DATABASE_QUERY_TIMEOUT_SECONDS")?
                .unwrap_or_else(default_db_query_timeout),
            enable_sql_logging: parser
                .get_bool("DATABASE_ENABLE_SQL_LOGGING")?
                .unwrap_or_else(default_false),
            min_connections: parser.get_u32("DATABASE_MIN_CONNECTIONS")?,
            max_lifetime_seconds: parser.get_u64("DATABASE_MAX_LIFETIME_SECONDS")?,
            idle_timeout_seconds: parser.get_u64("DATABASE_IDLE_TIMEOUT_SECONDS")?,
        })
    }
}

impl DatabaseConfig {
    /// Validate the database configuration
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.url.is_empty() {
            return Err(ValidationError::InvalidValue {
                field: "url".to_string(),
                message: "Database URL cannot be empty".to_string(),
            });
        }

        if !self.url.starts_with("postgresql://") && !self.url.starts_with("postgres://") {
            return Err(ValidationError::InvalidValue {
                field: "url".to_string(),
                message: "Database URL must be a PostgreSQL connection string".to_string(),
            });
        }

        if self.max_connections == 0 {
            return Err(ValidationError::InvalidValue {
                field: "max_connections".to_string(),
                message: "Maximum connections must be greater than 0".to_string(),
            });
        }

        if let Some(min_conn) = self.min_connections {
            if min_conn > self.max_connections {
                return Err(ValidationError::InvalidValue {
                    field: "min_connections".to_string(),
                    message: "Minimum connections cannot exceed maximum connections".to_string(),
                });
            }
        }

        Ok(())
    }

    /// Create a PostgreSQL connection pool from this configuration
    pub async fn create_pool(&self) -> Result<PgPool, sqlx::Error> {
        let mut options = PgPoolOptions::new()
            .max_connections(self.max_connections)
            .acquire_timeout(Duration::from_secs(self.connection_timeout_seconds));

        if let Some(min_conn) = self.min_connections {
            options = options.min_connections(min_conn);
        }

        if let Some(max_lifetime) = self.max_lifetime_seconds {
            options = options.max_lifetime(Duration::from_secs(max_lifetime));
        }

        if let Some(idle_timeout) = self.idle_timeout_seconds {
            options = options.idle_timeout(Duration::from_secs(idle_timeout));
        }

        options.connect(&self.url).await
    }

    /// Get connection timeout as Duration
    pub fn connection_timeout(&self) -> Duration {
        Duration::from_secs(self.connection_timeout_seconds)
    }

    /// Get query timeout as Duration
    pub fn query_timeout(&self) -> Duration {
        Duration::from_secs(self.query_timeout_seconds)
    }
}

/// Database manager with unified migration support across all services
#[derive(Clone)]
pub struct DatabaseManager {
    pool: PgPool,
    config: DatabaseConfig,
}

impl DatabaseManager {
    /// Create a new database manager from configuration
    pub async fn new(config: DatabaseConfig) -> Result<Self, ConfigError> {
        Self::new_with_migrations(config, true).await
    }

    /// Create a new database manager with optional migration running
    pub async fn new_with_migrations(
        config: DatabaseConfig,
        run_migrations: bool,
    ) -> Result<Self, ConfigError> {
        let pool = config
            .create_pool()
            .await
            .map_err(|e| ConfigError::ParseError {
                field: "database.url".to_string(),
                message: format!("Failed to create database pool: {}", e),
            })?;

        if run_migrations {
            Self::run_migrations_on_pool(&pool).await?;
        }

        Ok(Self { pool, config })
    }

    /// Run database migrations.
    ///
    /// Migrations are service-local: each service bundles its own
    /// `migrations/` directory and runs them from its own
    /// `DatabaseManager`. The shared pool helper has no migrations
    /// attached, so a request to run them here is a configuration
    /// error rather than a silent no-op.
    async fn run_migrations_on_pool(_pool: &PgPool) -> Result<(), ConfigError> {
        Err(ValidationError::InvalidValue {
            field: "database.run_migrations".to_string(),
            message: "shared_config does not bundle migrations; run them from the service's own DatabaseManager".to_string(),
        }
        .into())
    }

    /// Get the database connection pool
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Get a database transaction
    pub async fn begin(&self) -> Result<Transaction<'_, Postgres>, sqlx::Error> {
        self.pool.begin().await
    }

    /// Perform a health check on the database connection
    pub async fn health_check(&self) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    /// Run database migrations manually
    pub async fn run_migrations(&self) -> Result<(), ConfigError> {
        Self::run_migrations_on_pool(&self.pool).await
    }

    /// Get database configuration
    pub fn config(&self) -> &DatabaseConfig {
        &self.config
    }

    /// Close the database connection pool
    pub async fn close(&self) {
        self.pool.close().await;
        info!("Database connection pool closed");
    }
}

/// Create a database manager from environment variables
/// This is a convenience function for services that need quick setup
pub async fn create_database_manager_from_env() -> Result<DatabaseManager, ConfigError> {
    let config = DatabaseConfig::from_env()?;
    DatabaseManager::new(config).await
}

/// Create a database pool using the legacy janitor::state::create_pool pattern
/// This maintains backward compatibility while encouraging migration to DatabaseManager
pub async fn create_pool_legacy(
    database_url: Option<String>,
) -> Result<PgPool, Box<dyn std::error::Error + Send + Sync>> {
    let url = database_url.ok_or("No database URL provided")?;

    let config = DatabaseConfig {
        url,
        max_connections: 10,
        connection_timeout_seconds: 30,
        query_timeout_seconds: 60,
        enable_sql_logging: false,
        min_connections: None,
        max_lifetime_seconds: Some(1800),
        idle_timeout_seconds: Some(600),
    };

    let manager = DatabaseManager::new_with_migrations(config, false).await?;
    Ok(manager.pool().clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_database_config_validation() {
        let mut config = DatabaseConfig::default();
        assert!(config.validate().is_ok());

        // Test empty URL
        config.url = "".to_string();
        assert!(config.validate().is_err());

        // Test invalid URL
        config.url = "mysql://localhost/test".to_string();
        assert!(config.validate().is_err());

        // Test zero max connections
        config.url = "postgresql://localhost/test".to_string();
        config.max_connections = 0;
        assert!(config.validate().is_err());

        // Test min > max connections
        config.max_connections = 10;
        config.min_connections = Some(15);
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_database_config_default() {
        let config = DatabaseConfig::default();
        assert_eq!(config.max_connections, 10);
        assert_eq!(config.connection_timeout_seconds, 30);
        assert_eq!(config.query_timeout_seconds, 60);
        assert!(!config.enable_sql_logging);
    }

    #[tokio::test]
    async fn test_database_manager_without_migrations() {
        // Test database manager creation without running migrations
        let config = DatabaseConfig {
            url: "postgres://test:test@localhost/test_db".to_string(),
            max_connections: 5,
            connection_timeout_seconds: 30,
            query_timeout_seconds: 60,
            enable_sql_logging: false,
            min_connections: None,
            max_lifetime_seconds: Some(3600),
            idle_timeout_seconds: Some(600),
        };

        // This will fail if no database is available, which is expected in CI
        let _result = DatabaseManager::new_with_migrations(config, false).await;
        // We don't assert success since this requires a real database
    }

    #[test]
    fn test_create_pool_legacy() {
        // Test the legacy function compiles and has correct signature
        let url = Some("postgres://test:test@localhost/test_db".to_string());
        let _future = create_pool_legacy(url);
        // We don't execute it since it requires a real database
    }
}
