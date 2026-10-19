//! Worker authentication and security for the runner.

use base64::{engine::general_purpose, Engine as _};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::database::RunnerDatabase;

/// Worker authentication information.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct WorkerAuth {
    /// Worker name/username.
    pub name: String,
    /// Optional worker link/URL.
    pub link: Option<String>,
}

/// Authentication errors.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// Invalid authorization header format.
    #[error("Invalid authorization header format")]
    InvalidFormat,

    /// Invalid credentials.
    #[error("Invalid credentials")]
    InvalidCredentials,

    /// Database error during authentication.
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    /// Base64 decode error.
    #[error("Base64 decode error: {0}")]
    Base64(#[from] base64::DecodeError),

    /// UTF-8 decode error.
    #[error("UTF-8 decode error: {0}")]
    Utf8(#[from] std::str::Utf8Error),
}

/// Worker authentication service.
pub struct WorkerAuthService {
    database: Arc<RunnerDatabase>,
}

impl WorkerAuthService {
    /// Create a new worker authentication service.
    pub fn new(database: Arc<RunnerDatabase>) -> Self {
        Self { database }
    }

    /// Authenticate a worker using HTTP Basic Auth.
    pub async fn authenticate_worker(&self, auth_header: &str) -> Result<WorkerAuth, AuthError> {
        // Parse Basic Auth header
        let (username, password) = self.parse_basic_auth(auth_header)?;

        // Verify credentials against database
        let worker_info = self.verify_worker_credentials(&username, &password).await?;

        match worker_info {
            Some(auth) => Ok(auth),
            None => Err(AuthError::InvalidCredentials),
        }
    }

    /// Parse HTTP Basic Auth header.
    fn parse_basic_auth(&self, auth_header: &str) -> Result<(String, String), AuthError> {
        // Remove "Basic " prefix
        let encoded = auth_header
            .strip_prefix("Basic ")
            .ok_or(AuthError::InvalidFormat)?;

        // Decode base64
        let decoded = general_purpose::STANDARD.decode(encoded)?;
        let decoded_str = std::str::from_utf8(&decoded)?;

        // Split username:password
        let parts: Vec<&str> = decoded_str.splitn(2, ':').collect();
        if parts.len() != 2 {
            return Err(AuthError::InvalidFormat);
        }

        Ok((parts[0].to_string(), parts[1].to_string()))
    }

    /// Verify worker credentials against the database.
    async fn verify_worker_credentials(
        &self,
        username: &str,
        password: &str,
    ) -> Result<Option<WorkerAuth>, AuthError> {
        // Query the worker table with encrypted password verification
        Ok(sqlx::query_as(
            "SELECT name, link FROM worker WHERE name = $1 AND password = crypt($2, password)",
        )
        .bind(username)
        .bind(password)
        .fetch_optional(self.database.pool())
        .await?)
    }

    /// Create a new worker account (for admin operations).
    pub async fn create_worker(
        &self,
        name: &str,
        password: &str,
        link: Option<&str>,
    ) -> Result<(), AuthError> {
        sqlx::query(
            "INSERT INTO worker (name, password, link) VALUES ($1, crypt($2, gen_salt('bf')), $3)",
        )
        .bind(name)
        .bind(password)
        .bind(link)
        .execute(self.database.pool())
        .await?;

        log::info!("Created worker account: {}", name);
        Ok(())
    }

    /// Delete worker account.
    pub async fn delete_worker(&self, name: &str) -> Result<bool, AuthError> {
        let result = sqlx::query("DELETE FROM worker WHERE name = $1")
            .bind(name)
            .execute(self.database.pool())
            .await?;

        if result.rows_affected() > 0 {
            log::info!("Deleted worker account: {}", name);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// List all worker accounts.
    pub async fn list_workers(&self) -> Result<Vec<WorkerAuth>, AuthError> {
        Ok(
            sqlx::query_as("SELECT name, link FROM worker ORDER BY name")
                .fetch_all(self.database.pool())
                .await?,
        )
    }

    /// Check if a worker exists.
    pub async fn worker_exists(&self, name: &str) -> Result<bool, AuthError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM worker WHERE name = $1")
            .bind(name)
            .fetch_one(self.database.pool())
            .await?;

        Ok(count > 0)
    }
}

/// Per-worker concurrent run limit reported by the security stats.
pub const MAX_CONCURRENT_RUNS_PER_WORKER: u32 = 5;

/// Security service for rate limiting and access control.
pub struct SecurityService {
    database: Arc<RunnerDatabase>,
    active_runs: crate::active_runs::ActiveRunStore,
}

impl SecurityService {
    /// Create a new security service.
    pub fn new(
        database: Arc<RunnerDatabase>,
        active_runs: crate::active_runs::ActiveRunStore,
    ) -> Self {
        Self {
            database,
            active_runs,
        }
    }

    /// Get security statistics.
    pub async fn get_security_stats(&self) -> Result<SecurityStats, AuthError> {
        let total_workers: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM worker")
            .fetch_one(self.database.pool())
            .await?;

        let active_workers = self.active_runs.distinct_worker_count().await as u64;
        let total_active_runs = self.active_runs.len().await as u64;

        Ok(SecurityStats {
            total_workers: total_workers as u64,
            active_workers,
            total_active_runs,
            max_concurrent_runs_per_worker: MAX_CONCURRENT_RUNS_PER_WORKER,
        })
    }
}

/// Security statistics.
#[derive(Debug, Serialize, Deserialize)]
pub struct SecurityStats {
    /// Total number of registered workers.
    pub total_workers: u64,
    /// Number of workers currently running jobs.
    pub active_workers: u64,
    /// Total number of active runs.
    pub total_active_runs: u64,
    /// Maximum concurrent runs allowed per worker.
    pub max_concurrent_runs_per_worker: u32,
}
