//! Codebase and worker lookups against Postgres.

use crate::error::Result;
use sqlx::PgPool;
use tracing::debug;

pub struct DatabaseManager {
    pool: PgPool,
}

impl DatabaseManager {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn codebase_exists(&self, codebase: &str) -> Result<bool> {
        debug!("codebase_exists: {}", codebase);
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM codebase WHERE name = $1)")
                .bind(codebase)
                .fetch_one(&self.pool)
                .await?;
        Ok(exists)
    }

    /// Return the worker name if `(username, password)` matches. The
    /// password comparison runs inside Postgres via `crypt($2, ...)`
    /// so the hash algorithm matches whatever pgcrypto is configured
    /// to use.
    pub async fn is_worker(&self, username: &str, password: &str) -> Result<Option<String>> {
        debug!("is_worker: {}", username);
        let row: Option<String> = sqlx::query_scalar(
            "SELECT name FROM worker WHERE name = $1 AND password = crypt($2, password)",
        )
        .bind(username)
        .bind(password)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }
}
