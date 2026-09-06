use async_trait::async_trait;
use std::path::Path;

mod local;

pub use local::LocalArtifactManager;

#[cfg(feature = "gcs")]
mod gcs;

#[cfg(feature = "gcs")]
pub use gcs::GCSArtifactManager;

#[derive(Debug)]
pub enum Error {
    ServiceUnavailable,
    ArtifactsMissing,
    IoError(std::io::Error),
    InvalidPath,
    Other(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::ServiceUnavailable => write!(f, "Service unavailable"),
            Error::ArtifactsMissing => write!(f, "Artifacts missing"),
            Error::IoError(e) => write!(f, "IO error: {}", e),
            Error::InvalidPath => write!(f, "Invalid path"),
            Error::Other(e) => write!(f, "Error: {}", e),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::IoError(e)
    }
}

#[async_trait]
pub trait ArtifactManager: std::fmt::Debug + Send + Sync {
    /// Store artifacts from a local directory
    async fn store_artifacts(
        &self,
        run_id: &str,
        local_path: &Path,
        names: Option<&[String]>,
    ) -> Result<(), Error>;

    /// Get a single artifact as a reader
    async fn get_artifact(
        &self,
        run_id: &str,
        filename: &str,
    ) -> Result<Box<dyn std::io::Read + Sync + Send>, Error>;

    /// Get the public URL for an artifact
    fn public_artifact_url(&self, run_id: &str, filename: &str) -> url::Url;

    /// Retrieve artifacts to a local directory with optional filter
    async fn retrieve_artifacts(
        &self,
        run_id: &str,
        local_path: &Path,
        filter_fn: Option<&(dyn for<'a> Fn(&'a str) -> bool + Sync + Send)>,
    ) -> Result<(), Error>;

    /// List all artifact IDs
    async fn iter_ids(&self) -> Box<dyn Iterator<Item = String> + Send>;

    /// Delete all artifacts for a run
    async fn delete_artifacts(&self, run_id: &str) -> Result<(), Error>;
}

pub async fn list_ids(manager: &dyn ArtifactManager) -> Result<(), Error> {
    for id in manager.iter_ids().await {
        println!("{}", id);
    }

    Ok(())
}

pub async fn get_artifact_manager(location: &str) -> Result<Box<dyn ArtifactManager>, Error> {
    if location.starts_with("gs://") {
        #[cfg(feature = "gcs")]
        {
            Ok(Box::new(
                GCSArtifactManager::from_url(&location.parse().unwrap()).await?,
            ))
        }
        #[cfg(not(feature = "gcs"))]
        {
            Err(Error::ServiceUnavailable)
        }
    } else {
        Ok(Box::new(LocalArtifactManager::new(Path::new(location))?))
    }
}

/// Upload all backup artifacts to the main artifact manager and delete them from the backup
/// manager.
///
/// # Arguments
/// * `backup_artifact_manager` - The backup artifact manager to retrieve artifacts from.
/// * `artifact_manager` - The main artifact manager to store artifacts to.
///
/// # Returns
/// A list of run IDs for which the backup artifacts were successfully uploaded.
pub async fn upload_backup_artifacts(
    backup_artifact_manager: &dyn ArtifactManager,
    artifact_manager: &dyn ArtifactManager,
) -> Result<Vec<String>, Error> {
    use futures::stream::{self, StreamExt};

    // Process artifacts in parallel with a concurrency limit
    const MAX_CONCURRENT: usize = 3;

    let ids: Vec<_> = backup_artifact_manager.iter_ids().await.collect();

    let results = stream::iter(ids)
        .map(|id| async move {
            let result =
                process_single_backup_artifact(&id, backup_artifact_manager, artifact_manager)
                    .await;
            (id, result)
        })
        .buffer_unordered(MAX_CONCURRENT)
        .collect::<Vec<_>>()
        .await;

    let mut done = vec![];
    for (id, result) in results {
        match result {
            Ok(_) => done.push(id),
            Err(e) => log::warn!("Unable to upload backup artifacts for {}: {}", id, e),
        }
    }

    Ok(done)
}

async fn process_single_backup_artifact(
    id: &str,
    backup_artifact_manager: &dyn ArtifactManager,
    artifact_manager: &dyn ArtifactManager,
) -> Result<(), Error> {
    // Must be a directory: retrieve_artifacts writes each file as
    // `local_path/<filename>`, so `local_path` needs to already be
    // a directory. An earlier version used NamedTempFile, which
    // silently made every migration fail as a filesystem-level
    // "not a directory" error.
    let td = tempfile::tempdir()?;
    backup_artifact_manager
        .retrieve_artifacts(id, td.path(), None)
        .await?;

    match artifact_manager.store_artifacts(id, td.path(), None).await {
        Ok(_) => {
            backup_artifact_manager.delete_artifacts(id).await?;
            Ok(())
        }
        Err(Error::ArtifactsMissing) => unreachable!(),
        Err(e) => Err(e),
    }
}

pub async fn store_artifacts_with_backup(
    manager: &dyn ArtifactManager,
    backup_manager: Option<&dyn ArtifactManager>,
    from_dir: &Path,
    run_id: &str,
    names: Option<&[String]>,
) -> Result<(), Error> {
    match manager.store_artifacts(run_id, from_dir, names).await {
        Ok(_) => Ok(()),
        Err(Error::ArtifactsMissing) => unreachable!(),
        Err(e) => {
            log::warn!("Unable to upload artifacts for {}: {}", run_id, e);
            if let Some(backup_manager) = backup_manager {
                backup_manager
                    .store_artifacts(run_id, from_dir, names)
                    .await?;
                log::info!(
                    "Uploading results to backup artifact location {:?}",
                    backup_manager
                );
            } else {
                log::warn!("No backup artifact manager set.");
            }
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin every Display variant. These strings land in user-facing
    /// error responses and log messages.
    #[test]
    fn error_display_matrix() {
        assert_eq!(Error::ServiceUnavailable.to_string(), "Service unavailable");
        assert_eq!(Error::ArtifactsMissing.to_string(), "Artifacts missing");
        assert_eq!(Error::InvalidPath.to_string(), "Invalid path");
        assert_eq!(Error::Other("x".into()).to_string(), "Error: x");
        let io_err: Error = std::io::Error::new(std::io::ErrorKind::NotFound, "nope").into();
        assert!(
            io_err.to_string().starts_with("IO error:"),
            "IO error message should start with 'IO error:', got {}",
            io_err
        );
    }

    /// `From<io::Error>` must produce IoError regardless of kind —
    /// callers that need finer resolution pattern-match the wrapped
    /// error themselves.
    #[test]
    fn from_io_error_produces_io_variant() {
        let err: Error = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "x").into();
        assert!(matches!(err, Error::IoError(_)));
    }

    #[test]
    fn error_implements_std_error_trait() {
        fn _requires<E: std::error::Error>(_: E) {}
        _requires(Error::ArtifactsMissing);
        _requires(Error::Other("x".into()));
    }

    /// Bare filesystem path routes to LocalArtifactManager.
    #[tokio::test]
    async fn get_artifact_manager_bare_path_yields_local_manager() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = get_artifact_manager(dir.path().to_str().unwrap())
            .await
            .expect("bare path must succeed");
        // Can't assert the concrete type, but we can exercise the
        // trait API to prove the construction is correct.
        let _ = mgr.iter_ids().await;
    }

    /// `gs://` without the gcs feature compiled in returns
    /// ServiceUnavailable, NOT a silent fallthrough to a
    /// filesystem manager rooted at "gs://bucket".
    #[cfg(not(feature = "gcs"))]
    #[tokio::test]
    async fn get_artifact_manager_gs_without_feature_returns_service_unavailable() {
        let err = get_artifact_manager("gs://some-bucket").await.unwrap_err();
        assert!(matches!(err, Error::ServiceUnavailable));
    }

    /// `store_artifacts_with_backup` happy path: primary succeeds,
    /// backup isn't touched.
    #[tokio::test]
    async fn store_with_backup_primary_success_skips_backup() {
        let primary_dir = tempfile::tempdir().unwrap();
        let primary = LocalArtifactManager::new(primary_dir.path()).unwrap();
        let backup_dir = tempfile::tempdir().unwrap();
        let backup = LocalArtifactManager::new(backup_dir.path()).unwrap();

        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("out.txt"), b"body").unwrap();

        store_artifacts_with_backup(&primary, Some(&backup), src.path(), "run-1", None)
            .await
            .unwrap();

        // Primary has the artifact; backup does not.
        let mut primary_out = primary.get_artifact("run-1", "out.txt").await.unwrap();
        let mut primary_contents = Vec::new();
        primary_out.read_to_end(&mut primary_contents).unwrap();
        assert_eq!(primary_contents, b"body");

        let err = match backup.get_artifact("run-1", "out.txt").await {
            Ok(_) => panic!("backup must not have the artifact"),
            Err(e) => e,
        };
        assert!(matches!(err, Error::ArtifactsMissing | Error::IoError(_)));
    }

    /// `upload_backup_artifacts` migrates everything from backup to
    /// primary and clears it from backup.
    #[tokio::test]
    async fn upload_backup_artifacts_moves_artifacts_and_clears_backup() {
        let primary_dir = tempfile::tempdir().unwrap();
        let primary = LocalArtifactManager::new(primary_dir.path()).unwrap();
        let backup_dir = tempfile::tempdir().unwrap();
        let backup = LocalArtifactManager::new(backup_dir.path()).unwrap();

        // Seed the backup with two runs.
        for run in ["run-a", "run-b"] {
            let src = tempfile::tempdir().unwrap();
            std::fs::write(src.path().join("out.txt"), run.as_bytes()).unwrap();
            backup.store_artifacts(run, src.path(), None).await.unwrap();
        }

        let done = upload_backup_artifacts(&backup, &primary).await.unwrap();
        let mut done_sorted = done;
        done_sorted.sort();
        assert_eq!(done_sorted, vec!["run-a".to_string(), "run-b".to_string()]);

        // Backup is empty now.
        let backup_ids: Vec<_> = backup.iter_ids().await.collect();
        assert!(
            backup_ids.is_empty(),
            "backup should be empty after migration, still has: {:?}",
            backup_ids
        );

        // Primary now holds both runs with correct bodies.
        for run in ["run-a", "run-b"] {
            let mut reader = primary.get_artifact(run, "out.txt").await.unwrap();
            let mut buf = Vec::new();
            reader.read_to_end(&mut buf).unwrap();
            assert_eq!(buf, run.as_bytes());
        }
    }

    /// Empty backup → empty done list. Don't error on nothing-to-do.
    #[tokio::test]
    async fn upload_backup_artifacts_empty_backup_returns_empty_done_list() {
        let primary_dir = tempfile::tempdir().unwrap();
        let primary = LocalArtifactManager::new(primary_dir.path()).unwrap();
        let backup_dir = tempfile::tempdir().unwrap();
        let backup = LocalArtifactManager::new(backup_dir.path()).unwrap();

        let done = upload_backup_artifacts(&backup, &primary).await.unwrap();
        assert!(done.is_empty());
    }
}
