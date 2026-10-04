//! Backfill: re-upload the latest build for each `(distribution, source)`.
//!
//! Fetches the rows once, then runs `upload_build_result` sequentially. Errors
//! on individual builds are logged and counted; the caller decides whether the
//! aggregate is a failure.

use tracing::{error, info};

use crate::artifacts::ArtifactProcessor;
use crate::database::DatabaseClient;
use crate::error::Result;
use crate::process::upload_build_result;
use crate::upload::UploadConfig;

/// Fetches the latest debian_build rows and re-runs the sign+upload pipeline.
pub struct BackfillProcessor {
    db_client: DatabaseClient,
    artifact_processor: ArtifactProcessor,
    upload_config: UploadConfig,
}

impl BackfillProcessor {
    /// Open connections to the database and artifact store.
    pub async fn new(
        database_url: &str,
        artifact_location: &str,
        upload_config: UploadConfig,
    ) -> Result<Self> {
        Ok(Self {
            db_client: DatabaseClient::new(database_url).await?,
            artifact_processor: ArtifactProcessor::new(artifact_location).await?,
            upload_config,
        })
    }

    /// Run backfill. Individual failures do not abort the run.
    pub async fn run_backfill(&self) -> Result<BackfillSummary> {
        info!("starting backfill");

        let dists = &self.upload_config.distributions;
        let builds = self
            .db_client
            .get_backfill_builds((!dists.is_empty()).then_some(dists.as_slice()))
            .await?;

        let mut summary = BackfillSummary {
            total_builds: builds.len() as u64,
            failed_uploads: 0,
        };

        for build in &builds {
            if let Err(e) =
                upload_build_result(&build.run_id, &self.artifact_processor, &self.upload_config)
                    .await
            {
                summary.failed_uploads += 1;
                error!(
                    run_id = %build.run_id,
                    distribution = %build.distribution,
                    source = %build.source,
                    error = %e,
                    "backfill upload failed",
                );
            }
        }

        info!("backfill complete: {}", summary);
        Ok(summary)
    }
}

/// Summary of a backfill run.
#[derive(Debug, Clone)]
pub struct BackfillSummary {
    /// Total builds returned by the query.
    pub total_builds: u64,
    /// Builds whose upload raised an error.
    pub failed_uploads: u64,
}

impl std::fmt::Display for BackfillSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} total, {} failed",
            self.total_builds, self.failed_uploads
        )
    }
}
