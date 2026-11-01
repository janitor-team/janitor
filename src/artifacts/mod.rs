use crate::RunId;
use futures::stream::{BoxStream, TryStreamExt};
use object_store::ObjectStoreScheme;
use std::path::Path;

mod store;

pub use store::ArtifactManager;

/// The contents of an artifact, as a stream of chunks.
pub type ArtifactStream = BoxStream<'static, Result<bytes::Bytes, Error>>;

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

impl From<object_store::Error> for Error {
    fn from(e: object_store::Error) -> Self {
        match e {
            object_store::Error::NotFound { .. } => Error::ArtifactsMissing,
            e => Error::Other(e.to_string()),
        }
    }
}

pub async fn list_ids(manager: &ArtifactManager) -> Result<(), Error> {
    let mut ids = manager.iter_ids();
    while let Some(id) = ids.try_next().await? {
        println!("{}", id);
    }

    Ok(())
}

pub fn get_artifact_manager(location: &str) -> Result<ArtifactManager, Error> {
    let url = match url::Url::parse(location) {
        Ok(url) => url,
        Err(url::ParseError::RelativeUrlWithoutBase) => {
            return Ok(ArtifactManager::local(Path::new(location))?);
        }
        Err(e) => {
            return Err(Error::Other(format!(
                "invalid artifact location {}: {}",
                location, e
            )))
        }
    };
    match ObjectStoreScheme::parse(&url) {
        Ok((ObjectStoreScheme::Local, _)) => {
            let path = url
                .to_file_path()
                .map_err(|_| Error::Other(format!("invalid artifact location {}", location)))?;
            Ok(ArtifactManager::local(&path)?)
        }
        Ok((ObjectStoreScheme::GoogleCloudStorage, _)) => {
            #[cfg(feature = "gcs")]
            {
                let bucket_name = url
                    .host_str()
                    .ok_or_else(|| Error::Other(format!("missing bucket name in {}", location)))?;
                ArtifactManager::gcs(bucket_name)
            }
            #[cfg(not(feature = "gcs"))]
            {
                Err(Error::ServiceUnavailable)
            }
        }
        Ok((scheme, prefix)) => {
            let store = crate::open_object_store(&url, scheme, prefix).map_err(|e| {
                Error::Other(format!(
                    "unable to use artifact location {}: {}",
                    location, e
                ))
            })?;
            let mut public_base = url.clone();
            if !public_base.path().ends_with('/') {
                public_base.set_path(&format!("{}/", url.path()));
            }
            Ok(ArtifactManager::new(store, public_base))
        }
        Err(e) => Err(Error::Other(format!(
            "unsupported artifact location {}: {}",
            location, e
        ))),
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
    backup_artifact_manager: &ArtifactManager,
    artifact_manager: &ArtifactManager,
) -> Result<Vec<String>, Error> {
    use futures::stream::{self, StreamExt};

    // Process artifacts in parallel with a concurrency limit
    const MAX_CONCURRENT: usize = 3;

    // Collect up front, since processing deletes from the store being listed.
    let ids: Vec<String> = backup_artifact_manager.iter_ids().try_collect().await?;

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
    backup_artifact_manager: &ArtifactManager,
    artifact_manager: &ArtifactManager,
) -> Result<(), Error> {
    // Must be a directory: retrieve_artifacts writes each file as
    // `local_path/<filename>`, so `local_path` needs to already be
    // a directory. An earlier version used NamedTempFile, which
    // silently made every migration fail as a filesystem-level
    // "not a directory" error.
    let id: RunId = id.parse().map_err(|e| Error::Other(format!("{}", e)))?;
    let td = tempfile::tempdir()?;
    backup_artifact_manager
        .retrieve_artifacts(&id, td.path(), None)
        .await?;

    match artifact_manager.store_artifacts(&id, td.path(), None).await {
        Ok(_) => {
            backup_artifact_manager.delete_artifacts(&id).await?;
            Ok(())
        }
        Err(Error::ArtifactsMissing) => unreachable!(),
        Err(e) => Err(e),
    }
}

pub async fn store_artifacts_with_backup(
    manager: &ArtifactManager,
    backup_manager: Option<&ArtifactManager>,
    from_dir: &Path,
    run_id: &RunId,
    names: Option<&[String]>,
) -> Result<(), Error> {
    match manager.store_artifacts(run_id, from_dir, names).await {
        Ok(_) => Ok(()),
        Err(Error::ArtifactsMissing) => unreachable!(),
        Err(e) => {
            log::warn!("Unable to upload artifacts for {}: {}", run_id, e);
            let Some(backup_manager) = backup_manager else {
                log::warn!("No backup artifact manager set.");
                return Err(e);
            };
            backup_manager
                .store_artifacts(run_id, from_dir, names)
                .await?;
            log::info!(
                "Uploading results to backup artifact location {:?}",
                backup_manager
            );
            Ok(())
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

    /// Bare filesystem path routes to a local store.
    #[tokio::test]
    async fn get_artifact_manager_bare_path_yields_local_manager() {
        let dir = tempfile::tempdir().unwrap();
        let mgr =
            get_artifact_manager(dir.path().to_str().unwrap()).expect("bare path must succeed");
        // Can't assert the concrete type, but we can exercise the
        // trait API to prove the construction is correct.
        let ids: Vec<String> = mgr.iter_ids().try_collect().await.unwrap();
        assert_eq!(ids, Vec::<String>::new());
    }

    /// `gs://` without the gcs feature compiled in returns
    /// ServiceUnavailable, NOT a silent fallthrough to a
    /// filesystem manager rooted at "gs://bucket".
    #[cfg(not(feature = "gcs"))]
    #[tokio::test]
    async fn get_artifact_manager_gs_without_feature_returns_service_unavailable() {
        let err = get_artifact_manager("gs://some-bucket").unwrap_err();
        assert!(matches!(err, Error::ServiceUnavailable));
    }

    #[tokio::test]
    async fn get_artifact_manager_object_store_url() {
        let mgr = get_artifact_manager("s3://janitor-artifacts/prefix").unwrap();
        assert_eq!(
            mgr.public_artifact_url(&RunId::new("run-1").unwrap(), "out.txt")
                .as_str(),
            "s3://janitor-artifacts/prefix/run-1/out.txt"
        );

        let mgr = get_artifact_manager("memory:///prefix").unwrap();
        let run_id = RunId::new("run-1").unwrap();
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("out.txt"), b"body").unwrap();
        mgr.store_artifacts(&run_id, src.path(), None)
            .await
            .unwrap();
        let ids: Vec<String> = mgr.iter_ids().try_collect().await.unwrap();
        assert_eq!(ids, vec!["run-1".to_string()]);
        let body: Vec<u8> = mgr
            .get_artifact(&run_id, "out.txt")
            .await
            .unwrap()
            .map_ok(|chunk| chunk.to_vec())
            .try_concat()
            .await
            .unwrap();
        assert_eq!(body, b"body");

        let err = get_artifact_manager("unknown://bucket/prefix").unwrap_err();
        assert!(matches!(err, Error::Other(_)), "got {err:?}");
        assert!(
            err.to_string().contains("unknown://bucket/prefix"),
            "got {err}"
        );
    }

    /// `store_artifacts_with_backup` happy path: primary succeeds,
    /// backup isn't touched.
    #[tokio::test]
    async fn store_with_backup_primary_success_skips_backup() {
        let primary_dir = tempfile::tempdir().unwrap();
        let primary = ArtifactManager::local(primary_dir.path()).unwrap();
        let backup_dir = tempfile::tempdir().unwrap();
        let backup = ArtifactManager::local(backup_dir.path()).unwrap();

        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("out.txt"), b"body").unwrap();

        store_artifacts_with_backup(
            &primary,
            Some(&backup),
            src.path(),
            &RunId::new("run-1").unwrap(),
            None,
        )
        .await
        .unwrap();

        // Primary has the artifact; backup does not.
        let primary_contents: Vec<u8> = primary
            .get_artifact(&RunId::new("run-1").unwrap(), "out.txt")
            .await
            .unwrap()
            .map_ok(|chunk| chunk.to_vec())
            .try_concat()
            .await
            .unwrap();
        assert_eq!(primary_contents, b"body");

        let err = match backup
            .get_artifact(&RunId::new("run-1").unwrap(), "out.txt")
            .await
        {
            Ok(_) => panic!("backup must not have the artifact"),
            Err(e) => e,
        };
        assert!(matches!(err, Error::ArtifactsMissing), "got {err:?}");
    }

    /// An artifact manager whose root directory has been replaced by a
    /// regular file, so storing artifacts in it fails.
    fn broken_manager(dir: &Path) -> ArtifactManager {
        let root = dir.join("artifacts");
        let manager = ArtifactManager::local(&root).unwrap();
        std::fs::remove_dir(&root).unwrap();
        std::fs::write(&root, b"").unwrap();
        manager
    }

    /// When the primary manager fails, storing in the backup manager
    /// counts as success.
    #[tokio::test]
    async fn store_with_backup_primary_failure_uses_backup() {
        let primary_dir = tempfile::tempdir().unwrap();
        let primary = broken_manager(primary_dir.path());
        let backup_dir = tempfile::tempdir().unwrap();
        let backup = ArtifactManager::local(backup_dir.path()).unwrap();

        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("out.txt"), b"body").unwrap();

        store_artifacts_with_backup(
            &primary,
            Some(&backup),
            src.path(),
            &RunId::new("run-1").unwrap(),
            None,
        )
        .await
        .unwrap();

        let backup_contents: Vec<u8> = backup
            .get_artifact(&RunId::new("run-1").unwrap(), "out.txt")
            .await
            .unwrap()
            .map_ok(|chunk| chunk.to_vec())
            .try_concat()
            .await
            .unwrap();
        assert_eq!(backup_contents, b"body");
    }

    /// Without a backup manager the primary's error is returned.
    #[tokio::test]
    async fn store_with_backup_primary_failure_without_backup_errors() {
        let primary_dir = tempfile::tempdir().unwrap();
        let primary = broken_manager(primary_dir.path());

        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("out.txt"), b"body").unwrap();

        let err = store_artifacts_with_backup(
            &primary,
            None,
            src.path(),
            &RunId::new("run-1").unwrap(),
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::IoError(_)), "got {err:?}");
    }

    /// `upload_backup_artifacts` migrates everything from backup to
    /// primary and clears it from backup.
    #[tokio::test]
    async fn upload_backup_artifacts_moves_artifacts_and_clears_backup() {
        let primary_dir = tempfile::tempdir().unwrap();
        let primary = ArtifactManager::local(primary_dir.path()).unwrap();
        let backup_dir = tempfile::tempdir().unwrap();
        let backup = ArtifactManager::local(backup_dir.path()).unwrap();

        // Seed the backup with two runs.
        for run in ["run-a", "run-b"] {
            let src = tempfile::tempdir().unwrap();
            std::fs::write(src.path().join("out.txt"), run.as_bytes()).unwrap();
            backup
                .store_artifacts(&RunId::new(run).unwrap(), src.path(), None)
                .await
                .unwrap();
        }

        let done = upload_backup_artifacts(&backup, &primary).await.unwrap();
        let mut done_sorted = done;
        done_sorted.sort();
        assert_eq!(done_sorted, vec!["run-a".to_string(), "run-b".to_string()]);

        // Backup is empty now.
        let backup_ids: Vec<String> = backup.iter_ids().try_collect().await.unwrap();
        assert!(
            backup_ids.is_empty(),
            "backup should be empty after migration, still has: {:?}",
            backup_ids
        );

        // Primary now holds both runs with correct bodies.
        for run in ["run-a", "run-b"] {
            let buf: Vec<u8> = primary
                .get_artifact(&RunId::new(run).unwrap(), "out.txt")
                .await
                .unwrap()
                .map_ok(|chunk| chunk.to_vec())
                .try_concat()
                .await
                .unwrap();
            assert_eq!(buf, run.as_bytes());
        }
    }

    /// Empty backup → empty done list. Don't error on nothing-to-do.
    #[tokio::test]
    async fn upload_backup_artifacts_empty_backup_returns_empty_done_list() {
        let primary_dir = tempfile::tempdir().unwrap();
        let primary = ArtifactManager::local(primary_dir.path()).unwrap();
        let backup_dir = tempfile::tempdir().unwrap();
        let backup = ArtifactManager::local(backup_dir.path()).unwrap();

        let done = upload_backup_artifacts(&backup, &primary).await.unwrap();
        assert!(done.is_empty());
    }
}
