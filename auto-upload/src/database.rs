//! Database access for backfill.
//!
//! One `SELECT DISTINCT ON` over `debian_build`, optionally filtered by
//! distribution.

use janitor_types::RunId;
use sqlx::PgPool;
use tracing::{debug, info};

use crate::error::Result;

/// Handle onto the shared janitor database pool.
pub struct DatabaseClient {
    shared_db: janitor::database::Database,
}

/// One row of the backfill query result.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DebianBuild {
    /// Debian suite the build targeted.
    pub distribution: String,
    /// Source package name.
    pub source: String,
    /// Run ID; used to look up the artifacts.
    pub run_id: RunId,
}

impl DatabaseClient {
    /// Connect with a small pool (backfill is single-threaded).
    pub async fn new(database_url: &str) -> Result<Self> {
        info!(
            "connecting to database: {}",
            database_url.split('@').next_back().unwrap_or("***")
        );

        let config = janitor::database::DatabaseConfig::new(database_url).with_max_connections(5);
        let shared_db = janitor::database::Database::connect_with_config(config).await?;

        Ok(Self { shared_db })
    }

    fn pool(&self) -> &PgPool {
        self.shared_db.pool()
    }

    /// Latest run for each `(distribution, source)`, filtered if the caller supplied a list.
    pub async fn get_backfill_builds(
        &self,
        distributions: Option<&[String]>,
    ) -> Result<Vec<DebianBuild>> {
        info!("querying backfill builds");
        if let Some(distributions) = distributions {
            debug!("filtering by distributions: {:?}", distributions);
        }

        let builds: Vec<DebianBuild> = sqlx::query_as(
            "SELECT DISTINCT ON (distribution, source) distribution, source, run_id
             FROM debian_build
             WHERE $1::text[] IS NULL OR distribution = ANY($1::text[])
             ORDER BY distribution, source, version DESC",
        )
        .bind(distributions)
        .fetch_all(self.pool())
        .await?;

        info!("found {} builds for backfill", builds.len());
        Ok(builds)
    }
}
