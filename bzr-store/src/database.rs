//! Database operations for the BZR Store service.
//!
//! Schema-of-record: `schema/state.sql::worker` — `(name text, password
//! text, link text)`. The `password` column stores a pgcrypto-style hash;
//! authentication is done entirely inside Postgres with `crypt($2, password)`
//! to avoid a Rust-side bcrypt/crypt divergence. Matches
//! `py/janitor/worker_creds.py::is_worker`.

use janitor::database::{Database as SharedDatabase, DatabaseConfig};
use sqlx::PgPool;
use tracing::{debug, info};

use crate::config::Config;
use crate::error::{BzrError, Result};

/// Database manager for BZR Store operations
#[derive(Debug, Clone)]
pub struct DatabaseManager {
    /// Shared database instance
    shared_db: SharedDatabase,
}

impl DatabaseManager {
    /// Create a new database manager
    pub async fn new(config: &Config) -> Result<Self> {
        info!("Connecting to database: {}", config.database_url());

        let db_config = DatabaseConfig::new(config.database_url())
            .with_max_connections(config.max_connections());

        let shared_db = SharedDatabase::connect_with_config(db_config)
            .await
            .map_err(|e| BzrError::Database(sqlx::Error::Configuration(e.to_string().into())))?;

        // Test the connection
        shared_db
            .health_check()
            .await
            .map_err(|e| BzrError::Database(sqlx::Error::Configuration(e.to_string().into())))?;

        debug!("Database connection test successful");

        Ok(Self { shared_db })
    }

    /// Build a `DatabaseManager` directly from an existing `sqlx::PgPool`,
    /// bypassing `Config` loading. Intended for integration tests where the
    /// pool comes from `janitor::test_utils::TestDatabase`.
    pub fn from_pool(pool: PgPool) -> Self {
        Self {
            shared_db: SharedDatabase::from_pool(pool),
        }
    }

    /// Get the database pool
    pub fn pool(&self) -> &PgPool {
        self.shared_db.pool()
    }

    /// Validate that a codebase exists in the database
    pub async fn validate_codebase(&self, codebase: &str) -> Result<bool> {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM codebase WHERE name = $1)")
                .bind(codebase)
                .fetch_one(self.pool())
                .await?;

        Ok(exists)
    }

    /// Return the worker name if `(username, password)` matches a row in the
    /// `worker` table, otherwise `None`. Mirrors
    /// `py/janitor/worker_creds.py::is_worker`: the password check is done
    /// inside Postgres with `crypt($2, password)` so hash algorithm matches
    /// whatever pgcrypto settings the Python code uses.
    pub async fn is_worker(&self, username: &str, password: &str) -> Result<Option<String>> {
        let row: Option<String> = sqlx::query_scalar(
            "SELECT name FROM worker WHERE name = $1 AND password = crypt($2, password)",
        )
        .bind(username)
        .bind(password)
        .fetch_optional(self.pool())
        .await?;

        Ok(row)
    }

    /// Check database health
    pub async fn health_check(&self) -> Result<()> {
        self.shared_db
            .health_check()
            .await
            .map_err(|e| BzrError::Database(sqlx::Error::Configuration(e.to_string().into())))?;
        Ok(())
    }
}
