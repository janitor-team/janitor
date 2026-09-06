//! Postgres queries over `debian_build` + `run` for repository generation.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use tracing::{debug, warn};

// Import shared database utilities
use janitor::database::{Database as SharedDatabase, DatabaseError};

use crate::error::{ArchiveError, ArchiveResult};
use crate::scanner::BuildInfo;

/// One row from the `debian_build` + `run` join used by
/// [`ArchiveDatabase::get_builds_for_suite`] and friends.
///
/// `architecture`, `component`, `binary_files`, `source_files` are
/// projected as SQL constants (`'amd64'`, `'main'`, `'[]'::jsonb`)
/// because the underlying schema doesn't carry them. The scanner
/// fills in the real values from the artifact manager at generation
/// time; consumers must not use them as filters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildRecord {
    /// Synthetic `<run_id>/<source>` key -- unique across the table.
    pub id: String,
    /// `debian_build.run_id`, used for artifact retrieval.
    pub run_id: String,
    /// `run.codebase`.
    pub codebase: String,
    /// `debian_build.distribution` (the build target).
    pub suite: String,
    /// `debian_build.source` (used as `package` in the projection too).
    pub package: String,
    /// `debian_build.source`.
    pub source_package: String,
    /// Constant `"amd64"` (see struct-level note).
    pub architecture: String,
    /// Constant `"main"` (see struct-level note).
    pub component: String,
    /// `debian_build.version` as text.
    pub version: String,
    /// Constant `"success"` -- the query filters on this.
    pub status: String,
    /// `run.finish_time` cast to `TIMESTAMPTZ` for sqlx decode.
    pub finish_time: Option<DateTime<Utc>>,
    /// Empty (see struct-level note).
    pub binary_files: Vec<String>,
    /// Empty (see struct-level note).
    pub source_files: Vec<String>,
}

/// Database manager for archive operations.
pub struct ArchiveDatabase {
    shared_db: SharedDatabase,
}

impl ArchiveDatabase {
    /// Wrap an existing `sqlx::PgPool`.
    pub fn new(pool: PgPool) -> Self {
        Self {
            shared_db: SharedDatabase::from_pool(pool),
        }
    }

    pub(crate) fn pool(&self) -> &PgPool {
        self.shared_db.pool()
    }

    /// `SELECT 1` roundtrip. Used by `/ready`/`/health` handlers.
    pub async fn health_check(&self) -> Result<bool, DatabaseError> {
        self.shared_db.health_check().await
    }

    /// Get all successful builds for a given `build_distribution`.
    ///
    /// The argument is the `debian_build.distribution` value, sourced
    /// from `campaign_config.debian_build.build_distribution`. Callers
    /// that only know the apt_repository name should pass that -- for
    /// simple deployments the two coincide, but multi-campaign
    /// apt_repositories should resolve the build_distribution via
    /// [`ArchiveConfig::runtime_config`] first.
    pub async fn get_builds_for_suite(
        &self,
        build_distribution: &str,
    ) -> ArchiveResult<Vec<BuildRecord>> {
        debug!(
            "Querying builds for build_distribution: {}",
            build_distribution
        );

        // The `debian_build` table stores (run_id, version,
        // distribution, source, binary_packages, lintian_result).
        // Select on `debian_build.distribution` (the build-target
        // distribution, usually the campaign's `build_distribution`),
        // not on `run.suite`. This is the query that decides which
        // .deb files are considered for an apt_repository, so getting
        // it wrong silently ships stale binaries into the wrong suite.
        //
        // `DISTINCT ON (source) ... ORDER BY source, version DESC`
        // picks the newest version per source. Without it, multiple
        // versions of the same source land in Packages/Sources and
        // apt picks arbitrarily between them.
        //
        // Component / architecture / binary_files / source_files are
        // still projected as synthetic constants (the scanner fills
        // in the real file lists from the artifact manager at
        // generation time); consumers must not use them as filters.
        let query = r#"
            SELECT DISTINCT ON (debian_build.source)
                (debian_build.run_id || '/' || debian_build.source) AS id,
                debian_build.run_id,
                r.codebase,
                debian_build.distribution AS suite,
                debian_build.source AS package,
                debian_build.source AS source_package,
                'amd64' AS architecture,
                'main' AS component,
                debian_build.version::text AS version,
                'success' AS status,
                (r.finish_time AT TIME ZONE 'UTC') AS finish_time,
                '[]'::jsonb AS binary_files,
                '[]'::jsonb AS source_files
            FROM debian_build
            JOIN run r ON debian_build.run_id = r.id
            WHERE debian_build.distribution = $1
              AND r.publish_status != 'rejected'
            ORDER BY debian_build.source, debian_build.version DESC
        "#;

        let rows = sqlx::query(query)
            .bind(build_distribution)
            .fetch_all(self.pool())
            .await
            .map_err(ArchiveError::Database)?;

        let builds = self.parse_build_records(rows).await?;
        debug!(
            "Found {} builds for build_distribution {}",
            builds.len(),
            build_distribution
        );
        Ok(builds)
    }

    /// Get builds for a specific changeset.
    pub async fn get_builds_for_changeset(
        &self,
        changeset_id: &str,
    ) -> ArchiveResult<Vec<BuildRecord>> {
        debug!("Querying builds for changeset: {}", changeset_id);

        // See `get_builds_for_suite` for the schema rationale.
        let query = r#"
            SELECT DISTINCT ON (debian_build.source)
                (debian_build.run_id || '/' || debian_build.source) AS id,
                debian_build.run_id,
                r.codebase,
                debian_build.distribution AS suite,
                debian_build.source AS package,
                debian_build.source AS source_package,
                'amd64' AS architecture,
                'main' AS component,
                debian_build.version::text AS version,
                'success' AS status,
                (r.finish_time AT TIME ZONE 'UTC') AS finish_time,
                '[]'::jsonb AS binary_files,
                '[]'::jsonb AS source_files
            FROM debian_build
            JOIN run r ON debian_build.run_id = r.id
            WHERE r.change_set = $1
              AND r.publish_status != 'rejected'
            ORDER BY debian_build.source, debian_build.version DESC
        "#;

        let rows = sqlx::query(query)
            .bind(changeset_id)
            .fetch_all(self.pool())
            .await
            .map_err(ArchiveError::Database)?;

        self.parse_build_records(rows).await
    }

    /// Get builds for a specific run.
    pub async fn get_builds_for_run(&self, run_id: &str) -> ArchiveResult<Vec<BuildRecord>> {
        debug!("Querying builds for run: {}", run_id);

        // See `get_builds_for_suite` for the schema rationale.
        let query = r#"
            SELECT DISTINCT ON (debian_build.source)
                (debian_build.run_id || '/' || debian_build.source) AS id,
                debian_build.run_id,
                r.codebase,
                debian_build.distribution AS suite,
                debian_build.source AS package,
                debian_build.source AS source_package,
                'amd64' AS architecture,
                'main' AS component,
                debian_build.version::text AS version,
                'success' AS status,
                (r.finish_time AT TIME ZONE 'UTC') AS finish_time,
                '[]'::jsonb AS binary_files,
                '[]'::jsonb AS source_files
            FROM debian_build
            JOIN run r ON debian_build.run_id = r.id
            WHERE r.id = $1
              AND r.publish_status != 'rejected'
            ORDER BY debian_build.source, debian_build.version DESC
        "#;

        let rows = sqlx::query(query)
            .bind(run_id)
            .fetch_all(self.pool())
            .await
            .map_err(ArchiveError::Database)?;

        self.parse_build_records(rows).await
    }

    async fn parse_build_records(
        &self,
        rows: Vec<sqlx::postgres::PgRow>,
    ) -> ArchiveResult<Vec<BuildRecord>> {
        let mut builds = Vec::new();

        for row in rows {
            let binary_files: serde_json::Value = row.get("binary_files");
            let source_files: serde_json::Value = row.get("source_files");

            let binary_files: Vec<String> =
                serde_json::from_value(binary_files).unwrap_or_else(|e| {
                    warn!("Failed to parse binary_files: {}", e);
                    Vec::new()
                });

            let source_files: Vec<String> =
                serde_json::from_value(source_files).unwrap_or_else(|e| {
                    warn!("Failed to parse source_files: {}", e);
                    Vec::new()
                });

            builds.push(BuildRecord {
                id: row.get("id"),
                run_id: row.get("run_id"),
                codebase: row.get("codebase"),
                suite: row.get("suite"),
                package: row.get("package"),
                source_package: row.get("source_package"),
                architecture: row.get("architecture"),
                component: row.get("component"),
                version: row.get("version"),
                status: row.get("status"),
                finish_time: row.get("finish_time"),
                binary_files,
                source_files,
            });
        }

        Ok(builds)
    }
}

/// Type alias for compatibility with repository module.
pub type BuildManager = ArchiveDatabase;

impl From<BuildRecord> for BuildInfo {
    fn from(record: BuildRecord) -> Self {
        Self {
            id: record.id,
            run_id: record.run_id,
            codebase: record.codebase,
            source_package: record.source_package,
            suite: record.suite,
            architecture: record.architecture,
            component: record.component,
            binary_files: record.binary_files,
            source_files: record.source_files,
        }
    }
}
