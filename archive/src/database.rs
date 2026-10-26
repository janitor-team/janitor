//! Postgres queries over `debian_build` + `run` for repository generation.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tracing::debug;

// Import shared database utilities
use janitor::database::{Database as SharedDatabase, DatabaseError};
use janitor::{ChangeSetId, CodebaseName, RunId};

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
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct BuildRecord {
    /// Synthetic `<run_id>/<source>` key -- unique across the table.
    pub id: String,
    /// `debian_build.run_id`, used for artifact retrieval.
    pub run_id: RunId,
    /// `run.codebase`.
    pub codebase: CodebaseName,
    /// `debian_build.distribution` (the build target).
    pub suite: String,
    /// `run.suite`, the campaign that made the build.
    pub campaign: String,
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
    #[sqlx(json)]
    pub binary_files: Vec<String>,
    /// Empty (see struct-level note).
    #[sqlx(json)]
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
        // `run.suite` (the campaign) only sets the pool path.
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
                r.suite AS campaign,
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

        let builds: Vec<BuildRecord> = sqlx::query_as(query)
            .bind(build_distribution)
            .fetch_all(self.pool())
            .await
            .map_err(ArchiveError::Database)?;

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
        changeset_id: &ChangeSetId,
    ) -> ArchiveResult<Vec<BuildRecord>> {
        debug!("Querying builds for changeset: {}", changeset_id);

        // See `get_builds_for_suite` for the schema rationale.
        let query = r#"
            SELECT DISTINCT ON (debian_build.source)
                (debian_build.run_id || '/' || debian_build.source) AS id,
                debian_build.run_id,
                r.codebase,
                debian_build.distribution AS suite,
                r.suite AS campaign,
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

        let builds: Vec<BuildRecord> = sqlx::query_as(query)
            .bind(changeset_id)
            .fetch_all(self.pool())
            .await
            .map_err(ArchiveError::Database)?;

        Ok(builds)
    }

    /// Get builds for a specific run.
    pub async fn get_builds_for_run(&self, run_id: &RunId) -> ArchiveResult<Vec<BuildRecord>> {
        debug!("Querying builds for run: {}", run_id);

        // See `get_builds_for_suite` for the schema rationale.
        let query = r#"
            SELECT DISTINCT ON (debian_build.source)
                (debian_build.run_id || '/' || debian_build.source) AS id,
                debian_build.run_id,
                r.codebase,
                debian_build.distribution AS suite,
                r.suite AS campaign,
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

        let builds: Vec<BuildRecord> = sqlx::query_as(query)
            .bind(run_id)
            .fetch_all(self.pool())
            .await
            .map_err(ArchiveError::Database)?;

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
            campaign: record.campaign,
            architecture: record.architecture,
            component: record.component,
            binary_files: record.binary_files,
            source_files: record.source_files,
        }
    }
}
