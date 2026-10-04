//! Main upload processing logic

use tracing::{error, info, warn};

use crate::artifacts::ArtifactProcessor;
use crate::error::{Result, UploadError};
use crate::upload::{sign_package, upload_package, UploadConfig};
use crate::utils::{find_changes_files, fix_file_permissions};

/// Retrieve, sign, and upload a build result.
pub async fn upload_build_result(
    run_id: &str,
    artifact_processor: &ArtifactProcessor,
    upload_config: &UploadConfig,
) -> Result<()> {
    info!(
        run_id,
        dput_host = upload_config.dput_host.as_deref().unwrap_or("<default>"),
        "processing upload"
    );

    let temp_dir = artifact_processor.retrieve_artifacts(run_id).await?;
    let artifacts_path = temp_dir.path();

    fix_file_permissions(artifacts_path).await?;

    let changes_files = find_changes_files(artifacts_path, upload_config.source_only).await?;
    info!(run_id, count = changes_files.len(), "found changes files");

    let mut had_failures = false;
    let mut successful_uploads = 0;

    for changes_path in &changes_files {
        let changes_file = changes_path.display();

        if let Err(e) = sign_package(changes_path, upload_config.debsign_keyid.as_deref()).await {
            error!(run_id, %changes_file, error = %e, "debsign failed");
            had_failures = true;
            continue;
        }
        info!(run_id, %changes_file, "signed");

        match upload_package(changes_path, upload_config.dput_host.as_deref()).await {
            Ok(_) => {
                info!(run_id, %changes_file, "uploaded");
                successful_uploads += 1;
            }
            Err(e) => {
                error!(run_id, %changes_file, error = %e, "dput failed");
                had_failures = true;
            }
        }
    }

    let total = changes_files.len();
    if !had_failures {
        info!(run_id, successful_uploads, "all packages uploaded");
        Ok(())
    } else if successful_uploads > 0 {
        warn!(run_id, successful_uploads, total, "partial upload");
        Err(UploadError::PartialUpload {
            successful: successful_uploads,
            total,
        })
    } else {
        error!(run_id, "no packages uploaded");
        Err(UploadError::NoPackagesUploaded)
    }
}
