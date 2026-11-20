//! Fetch build artifacts from the configured artifact store.

use tempfile::TempDir;
use tracing::{debug, info, warn};

use crate::error::{Result, UploadError};
use janitor::artifacts::{get_artifact_manager, ArtifactManager, RETRIEVE_TIMEOUT_SECS};
use janitor_types::RunId;

/// Wraps the shared [`ArtifactManager`] and downloads runs into a fresh temp dir.
pub struct ArtifactProcessor {
    artifact_manager: ArtifactManager,
}

impl ArtifactProcessor {
    /// Open the artifact store at `artifact_location`.
    pub async fn new(artifact_location: &str) -> Result<Self> {
        let artifact_manager = get_artifact_manager(artifact_location).map_err(|e| {
            UploadError::Config(format!("Failed to create artifact manager: {}", e))
        })?;
        Ok(Self { artifact_manager })
    }

    /// Download the artifacts for `run_id` into a temporary directory.
    ///
    /// The `TempDir` is deleted when dropped, so callers must keep it alive
    /// for the lifetime of any paths derived from it.
    pub async fn retrieve_artifacts(&self, run_id: &RunId) -> Result<TempDir> {
        info!(%run_id, "retrieving artifacts");

        let temp_dir = TempDir::new().map_err(UploadError::Io)?;
        // Bound the download: an artifact store that accepts the
        // connection and then stalls would otherwise wedge this upload
        // indefinitely.
        let retrieval = self
            .artifact_manager
            .retrieve_artifacts(run_id, temp_dir.path(), None);
        let retrieval = tokio::time::timeout(
            std::time::Duration::from_secs(RETRIEVE_TIMEOUT_SECS),
            retrieval,
        );
        match retrieval.await {
            Err(_) => {
                warn!(%run_id, "timed out retrieving artifacts");
                Err(UploadError::ArtifactRetrievalTimeout(run_id.to_string()))
            }
            Ok(Ok(_)) => {
                debug!(%run_id, path = ?temp_dir.path(), "retrieved");
                Ok(temp_dir)
            }
            Ok(Err(janitor::artifacts::Error::Timeout(e))) => {
                warn!(%run_id, error = %e, "artifact store timed out");
                Err(UploadError::ArtifactRetrievalTimeout(run_id.to_string()))
            }
            Ok(Err(janitor::artifacts::Error::ArtifactsMissing)) => {
                warn!(%run_id, "artifacts missing");
                Err(UploadError::ArtifactsMissing(run_id.to_string()))
            }
            Ok(Err(e)) => {
                warn!(%run_id, error = ?e, "artifact retrieval failed");
                Err(UploadError::ArtifactRetrievalFailed {
                    run_id: run_id.to_string(),
                    reason: e.to_string(),
                })
            }
        }
    }
}
