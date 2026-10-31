//! Codebase and worker lookups against Postgres.

use crate::error::Result;
use janitor::CodebaseName;
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

    pub async fn codebase_exists(&self, codebase: &CodebaseName) -> Result<bool> {
        debug!("codebase_exists: {}", codebase);
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM codebase WHERE name = $1)")
                .bind(codebase)
                .fetch_one(&self.pool)
                .await?;
        Ok(exists)
    }
}
