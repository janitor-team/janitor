//! Fetch build artifacts from the configured artifact store.

use tempfile::TempDir;
use tracing::{debug, info, warn};

use crate::error::{Result, UploadError};
use janitor::artifacts::{get_artifact_manager, ArtifactManager};

/// Wraps the shared [`ArtifactManager`] and downloads runs into a fresh temp dir.
pub struct ArtifactProcessor {
    artifact_manager: Box<dyn ArtifactManager>,
}

impl ArtifactProcessor {
    /// Open the artifact store at `artifact_location`.
    pub async fn new(artifact_location: &str) -> Result<Self> {
        let artifact_manager = get_artifact_manager(artifact_location).await.map_err(|e| {
            UploadError::Config(format!("Failed to create artifact manager: {}", e))
        })?;
        Ok(Self { artifact_manager })
    }

    /// Download the artifacts for `run_id` into a temporary directory.
    ///
    /// The `TempDir` is deleted when dropped, so callers must keep it alive
    /// for the lifetime of any paths derived from it.
    pub async fn retrieve_artifacts(&self, run_id: &str) -> Result<TempDir> {
        info!(run_id, "retrieving artifacts");

        let temp_dir = TempDir::new().map_err(UploadError::Io)?;
        match self
            .artifact_manager
            .retrieve_artifacts(run_id, temp_dir.path(), None)
            .await
        {
            Ok(_) => {
                debug!(run_id, path = ?temp_dir.path(), "retrieved");
                Ok(temp_dir)
            }
            Err(e) => {
                warn!(run_id, error = ?e, "artifacts missing");
                Err(UploadError::ArtifactsMissing(run_id.to_string()))
            }
        }
    }
}
