//! Worker authentication and security for the runner.

use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::Response,
};
use base64::{engine::general_purpose, Engine as _};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::sync::Arc;

use crate::database::RunnerDatabase;

/// Worker authentication information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerAuth {
    /// Worker name/username.
    pub name: String,
    /// Optional worker link/URL.
    pub link: Option<String>,
}

/// Authentication errors.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// Missing authorization header.
    #[error("Missing authorization header")]
    MissingAuth,

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
        let row = sqlx::query(
            "SELECT name, link FROM worker WHERE name = $1 AND password = crypt($2, password)",
        )
        .bind(username)
        .bind(password)
        .fetch_optional(self.database.pool())
        .await?;

        if let Some(row) = row {
            let name: String = row.get("name");
            let link: Option<String> = row.get("link");

            Ok(Some(WorkerAuth { name, link }))
        } else {
            Ok(None)
        }
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

    /// Update worker password.
    pub async fn update_worker_password(
        &self,
        name: &str,
        new_password: &str,
    ) -> Result<bool, AuthError> {
        let result =
            sqlx::query("UPDATE worker SET password = crypt($2, gen_salt('bf')) WHERE name = $1")
                .bind(name)
                .bind(new_password)
                .execute(self.database.pool())
                .await?;

        Ok(result.rows_affected() > 0)
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
        let rows = sqlx::query("SELECT name, link FROM worker ORDER BY name")
            .fetch_all(self.database.pool())
            .await?;

        let mut workers = Vec::new();
        for row in rows {
            let name: String = row.get("name");
            let link: Option<String> = row.get("link");
            workers.push(WorkerAuth { name, link });
        }

        Ok(workers)
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

/// Axum middleware for worker authentication.
pub async fn require_worker_auth(
    State(database): State<Arc<RunnerDatabase>>,
    mut request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let auth_service = WorkerAuthService::new(database);

    // Extract Authorization header
    let auth_header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok())
        .ok_or_else(|| {
            log::warn!("Missing Authorization header in worker request");
            StatusCode::UNAUTHORIZED
        })?;

    // Authenticate worker
    let worker_auth = auth_service
        .authenticate_worker(auth_header)
        .await
        .map_err(|e| {
            log::warn!("Worker authentication failed: {}", e);
            match e {
                AuthError::MissingAuth | AuthError::InvalidCredentials => StatusCode::UNAUTHORIZED,
                AuthError::InvalidFormat => StatusCode::BAD_REQUEST,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            }
        })?;

    // Store worker info in request extensions for use by handlers
    request.extensions_mut().insert(worker_auth);

    log::debug!("Worker authenticated successfully");
    Ok(next.run(request).await)
}

/// Helper function to get authenticated worker from request extensions.
pub fn get_authenticated_worker(request: &Request) -> Option<&WorkerAuth> {
    request.extensions().get::<WorkerAuth>()
}

/// Rate limiting and security configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    /// Maximum requests per minute per worker.
    pub max_requests_per_minute: u32,
    /// Enable request logging for security auditing.
    pub enable_audit_logging: bool,
    /// List of allowed IP ranges (CIDR notation).
    pub allowed_ip_ranges: Vec<String>,
    /// Maximum concurrent active runs per worker.
    pub max_concurrent_runs_per_worker: u32,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            max_requests_per_minute: 60,
            enable_audit_logging: true,
            allowed_ip_ranges: vec!["0.0.0.0/0".to_string()], // Allow all by default
            max_concurrent_runs_per_worker: 5,
        }
    }
}

/// Security service for rate limiting and access control.
pub struct SecurityService {
    config: SecurityConfig,
    database: Arc<RunnerDatabase>,
    active_runs: crate::active_runs::ActiveRunStore,
}

impl SecurityService {
    /// Create a new security service.
    pub fn new(
        config: SecurityConfig,
        database: Arc<RunnerDatabase>,
        active_runs: crate::active_runs::ActiveRunStore,
    ) -> Self {
        Self {
            config,
            database,
            active_runs,
        }
    }

    /// Check if a worker can start a new run (concurrency limit).
    pub async fn can_start_run(&self, worker_name: &str) -> Result<bool, AuthError> {
        let active_count = self.active_runs.count_for_worker(worker_name).await;
        Ok(active_count < self.config.max_concurrent_runs_per_worker as usize)
    }

    /// Log security event for auditing.
    pub fn log_security_event(&self, event_type: &str, worker_name: &str, details: &str) {
        if self.config.enable_audit_logging {
            log::info!(
                "SECURITY_EVENT: {} worker={} details={}",
                event_type,
                worker_name,
                details
            );
        }
    }

    /// Check if IP address is in allowed ranges.
    pub fn is_ip_allowed(&self, ip: &str) -> bool {
        // For now, simple implementation - in production this would use proper CIDR matching
        if self
            .config
            .allowed_ip_ranges
            .contains(&"0.0.0.0/0".to_string())
        {
            return true;
        }

        // Basic IP validation - in a real implementation, use ipnet crate for CIDR matching
        self.config.allowed_ip_ranges.iter().any(|range| {
            if range == ip {
                true
            } else if range.ends_with("/0") {
                // Very basic subnet matching - use proper CIDR library in production
                true
            } else {
                false
            }
        })
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
            max_concurrent_runs_per_worker: self.config.max_concurrent_runs_per_worker,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_security_config_default() {
        let config = SecurityConfig::default();
        assert_eq!(config.max_requests_per_minute, 60);
        assert!(config.enable_audit_logging);
        assert_eq!(config.allowed_ip_ranges, vec!["0.0.0.0/0"]);
        assert_eq!(config.max_concurrent_runs_per_worker, 5);
    }
}
