use janitor_types::LogFilename;
use janitor_types::{CodebaseName, RunId};
use object_store::ObjectStoreScheme;
use std::io;

mod store;
pub use store::{LogFileManager, LogStream};

// Re-export common error types
pub use self::Error as LogError;

#[derive(Debug, Clone)]
pub enum Error {
    NotFound,
    ServiceUnavailable,
    PermissionDenied,
    Io(String), // Store string representation for Clone
    LogRetrieval(String),
    Timeout,
    InvalidName(String),
    Other(String),
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        // Writers into an object store report the store's errors as I/O
        // errors; keep them distinguishable.
        if err
            .get_ref()
            .is_some_and(|inner| inner.is::<object_store::Error>())
        {
            let inner = err
                .into_inner()
                .expect("checked above")
                .downcast::<object_store::Error>()
                .expect("checked above");
            return (*inner).into();
        }
        // Python's managers raised PermissionError for these, which
        // import_log handles separately.
        if err.kind() == io::ErrorKind::PermissionDenied {
            Error::PermissionDenied
        } else {
            Error::Io(err.to_string())
        }
    }
}

impl From<object_store::Error> for Error {
    fn from(err: object_store::Error) -> Self {
        match err {
            object_store::Error::NotFound { .. } => Error::NotFound,
            object_store::Error::PermissionDenied { .. } => Error::PermissionDenied,
            err => Error::Other(err.to_string()),
        }
    }
}

impl From<tokio::time::error::Elapsed> for Error {
    fn from(_: tokio::time::error::Elapsed) -> Self {
        Error::Timeout
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::NotFound => write!(f, "Not found"),
            Error::ServiceUnavailable => write!(f, "Service unavailable"),
            Error::PermissionDenied => write!(f, "Permission denied"),
            Error::Io(err) => write!(f, "I/O error: {}", err),
            Error::LogRetrieval(msg) => write!(f, "Log retrieval error: {}", msg),
            Error::Timeout => write!(f, "Operation timed out"),
            Error::InvalidName(name) => write!(f, "Invalid name: {}", name),
            Error::Other(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for Error {}

// Metrics tracking
use prometheus::{register_int_counter, IntCounter, Result as PrometheusResult};
use std::sync::LazyLock;

static PRIMARY_LOGFILE_UPLOAD_FAILED_COUNT: LazyLock<PrometheusResult<IntCounter>> =
    LazyLock::new(|| {
        register_int_counter!(
            "primary_logfile_upload_failed",
            "Number of failed logs to primary logfile target"
        )
    });

static LOGFILE_UPLOADED_COUNT: LazyLock<PrometheusResult<IntCounter>> =
    LazyLock::new(|| register_int_counter!("logfile_uploads", "Number of uploaded log files"));

// Helper functions to safely increment counters
fn increment_upload_failed() {
    if let Ok(ref counter) = *PRIMARY_LOGFILE_UPLOAD_FAILED_COUNT {
        counter.inc();
    }
}

fn increment_upload_success() {
    if let Ok(ref counter) = *LOGFILE_UPLOADED_COUNT {
        counter.inc();
    }
}

/// Create a log file manager based on the location string.
///
/// Supported location formats:
/// - Local filesystem path (e.g., "/var/log/janitor")
/// - Google Cloud Storage URL (e.g., "gs://bucket-name")
/// - S3 URL (e.g., "s3://bucket-name", "s3://bucket-name/logs"), with the
///   endpoint in `AWS_ENDPOINT_URL` for servers other than AWS
/// - Any other URL that `object_store` knows (e.g., "memory:///")
/// - None/empty: Uses temporary directory
pub fn create_log_manager(location: Option<&str>) -> Result<LogFileManager, Error> {
    let loc = match location {
        None | Some("") => return LogFileManager::local(std::env::temp_dir()),
        Some(loc) => loc,
    };
    let url = match url::Url::parse(loc) {
        Ok(url) => url,
        Err(url::ParseError::RelativeUrlWithoutBase) => return LogFileManager::local(loc),
        Err(e) => {
            return Err(Error::Other(format!(
                "Invalid logs location {}: {}",
                loc, e
            )))
        }
    };
    match ObjectStoreScheme::parse(&url) {
        Ok((ObjectStoreScheme::Local, _)) => {
            let path = url
                .to_file_path()
                .map_err(|_| Error::Other(format!("Invalid logs location {}", loc)))?;
            LogFileManager::local(path)
        }
        Ok((ObjectStoreScheme::GoogleCloudStorage, _)) => {
            #[cfg(feature = "gcs")]
            {
                let bucket_name = url
                    .host_str()
                    .ok_or_else(|| Error::Other(format!("Missing bucket name in {}", loc)))?;
                LogFileManager::gcs(bucket_name)
            }
            #[cfg(not(feature = "gcs"))]
            {
                Err(Error::Other("GCS support not compiled in".to_string()))
            }
        }
        Ok((scheme, prefix)) => crate::open_object_store(&url, scheme, prefix)
            .map(LogFileManager::new)
            .map_err(|e| Error::Other(format!("Unable to use logs location {}: {}", loc, e))),
        Err(e) => Err(Error::Other(format!(
            "Unsupported logs location {}: {}",
            loc, e
        ))),
    }
}

/// Import a log with primary and backup log managers
///
/// Like the Python implementation, a permission error is retried under a
/// different name and then falls back to the backup manager; any other
/// error is returned as is.
pub async fn import_log(
    primary_log_manager: &LogFileManager,
    backup_log_manager: Option<&LogFileManager>,
    codebase: &CodebaseName,
    run_id: &RunId,
    path: &str,
    basename: Option<&LogFilename>,
) -> Result<(), Error> {
    match primary_log_manager
        .import_log(codebase, run_id, path, basename)
        .await
    {
        Ok(()) => {
            increment_upload_success();
            return Ok(());
        }
        Err(Error::PermissionDenied) => {}
        Err(e) => return Err(e),
    }

    log::warn!("Permission denied error while uploading logfile {}", path);
    // It may just be that the file already exists.
    let name = match basename {
        Some(basename) => basename.to_string(),
        None => std::path::Path::new(path)
            .file_name()
            .ok_or_else(|| Error::InvalidName(path.to_string()))?
            .to_string_lossy()
            .into_owned(),
    };
    let alternative_basename = LogFilename::new(format!(
        "{}.{}",
        name,
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S")
    ))
    .map_err(|_| Error::InvalidName(name.clone()))?;
    match primary_log_manager
        .import_log(codebase, run_id, path, Some(&alternative_basename))
        .await
    {
        Ok(()) => {
            increment_upload_success();
            return Ok(());
        }
        Err(Error::PermissionDenied) => increment_upload_failed(),
        Err(e) => return Err(e),
    }

    // Without a backup manager Python only logged the failure above.
    if let Some(backup) = backup_log_manager {
        backup.import_log(codebase, run_id, path, basename).await?;
    }
    Ok(())
}

/// Import multiple logs concurrently
///
/// This function provides batch import capabilities with concurrent processing
/// similar to the Python implementation using asyncio.gather().
pub async fn import_logs(
    primary_log_manager: &LogFileManager,
    backup_log_manager: Option<&LogFileManager>,
    logs: Vec<(CodebaseName, RunId, String, Option<LogFilename>)>, // (codebase, run_id, path, basename)
) -> Vec<Result<(), Error>> {
    use futures::future::join_all;

    let import_futures = logs
        .into_iter()
        .map(|(codebase, run_id, path, basename)| async move {
            import_log(
                primary_log_manager,
                backup_log_manager,
                &codebase,
                &run_id,
                &path,
                basename.as_ref(),
            )
            .await
        });

    join_all(import_futures).await
}

/// Log entry structure for Python compatibility
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub name: LogFilename,
    pub path: String,
}

/// Import logs from a list of log entries
pub async fn import_logs_from_entries(
    entries: Vec<LogEntry>,
    logfile_manager: &LogFileManager,
    pkg: &CodebaseName,
    log_id: &RunId,
    backup_logfile_manager: Option<&LogFileManager>,
) -> Vec<Result<(), Error>> {
    use futures::future::join_all;

    let import_futures = entries.into_iter().map(|entry| async move {
        import_log(
            logfile_manager,
            backup_logfile_manager,
            pkg,
            log_id,
            &entry.path,
            Some(&entry.name),
        )
        .await
    });

    join_all(import_futures).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::BoxStream;
    use object_store::path::Path as ObjectPath;
    use object_store::{
        CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
        PutMultipartOptions, PutOptions, PutPayload, PutResult,
    };
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone, Copy)]
    enum Failure {
        PermissionDenied,
        Generic,
    }

    impl Failure {
        fn error(self, location: &ObjectPath) -> object_store::Error {
            match self {
                Failure::PermissionDenied => object_store::Error::PermissionDenied {
                    path: location.to_string(),
                    source: "denied".into(),
                },
                Failure::Generic => object_store::Error::Generic {
                    store: "failing",
                    source: "disk full".into(),
                },
            }
        }
    }

    /// In-memory store whose writes fail with queued errors.
    #[derive(Debug, Default)]
    struct FailingStore {
        inner: object_store::memory::InMemory,
        failures: Mutex<Vec<Failure>>,
    }

    impl FailingStore {
        fn failing(failures: Vec<Failure>) -> Self {
            Self {
                inner: object_store::memory::InMemory::new(),
                failures: Mutex::new(failures),
            }
        }

        fn next_failure(&self, location: &ObjectPath) -> object_store::Result<()> {
            let mut failures = self.failures.lock().unwrap();
            if failures.is_empty() {
                Ok(())
            } else {
                Err(failures.remove(0).error(location))
            }
        }
    }

    impl std::fmt::Display for FailingStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "FailingStore")
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for FailingStore {
        async fn put_opts(
            &self,
            location: &ObjectPath,
            payload: PutPayload,
            opts: PutOptions,
        ) -> object_store::Result<PutResult> {
            self.next_failure(location)?;
            self.inner.put_opts(location, payload, opts).await
        }

        async fn put_multipart_opts(
            &self,
            location: &ObjectPath,
            opts: PutMultipartOptions,
        ) -> object_store::Result<Box<dyn MultipartUpload>> {
            self.next_failure(location)?;
            self.inner.put_multipart_opts(location, opts).await
        }

        async fn get_opts(
            &self,
            location: &ObjectPath,
            options: GetOptions,
        ) -> object_store::Result<GetResult> {
            self.inner.get_opts(location, options).await
        }

        fn delete_stream(
            &self,
            locations: BoxStream<'static, object_store::Result<ObjectPath>>,
        ) -> BoxStream<'static, object_store::Result<ObjectPath>> {
            self.inner.delete_stream(locations)
        }

        fn list(
            &self,
            prefix: Option<&ObjectPath>,
        ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&ObjectPath>,
        ) -> object_store::Result<ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &ObjectPath,
            to: &ObjectPath,
            options: CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    /// A log manager over a store failing with `failures`, and the store
    /// itself to inspect what was written.
    fn failing_manager(failures: Vec<Failure>) -> (LogFileManager, Arc<FailingStore>) {
        let store = Arc::new(FailingStore::failing(failures));
        (LogFileManager::new(store.clone()), store)
    }

    async fn stored(store: &FailingStore) -> Vec<String> {
        use futures::TryStreamExt;
        let mut paths: Vec<String> = store
            .list(None)
            .map_ok(|meta| meta.location.to_string())
            .try_collect()
            .await
            .unwrap();
        paths.sort();
        paths
    }

    async fn run_import(
        primary: &LogFileManager,
        backup: Option<&LogFileManager>,
    ) -> Result<(), Error> {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("build.log");
        std::fs::write(&path, b"log contents").unwrap();
        import_log(
            primary,
            backup,
            &CodebaseName::new("codebase").unwrap(),
            &RunId::new("run-id").unwrap(),
            path.to_str().unwrap(),
            Some(&LogFilename::new("build.log").unwrap()),
        )
        .await
    }

    #[test]
    fn test_io_error_conversion() {
        let err = Error::from(io::Error::from(io::ErrorKind::PermissionDenied));
        assert!(matches!(err, Error::PermissionDenied), "got {err:?}");
        let err = Error::from(io::Error::other("disk full"));
        assert_eq!(err.to_string(), "I/O error: disk full");
        let err = Error::from(io::Error::from(
            Failure::PermissionDenied.error(&ObjectPath::from("x")),
        ));
        assert!(matches!(err, Error::PermissionDenied), "got {err:?}");
    }

    #[tokio::test]
    async fn test_import_log_unexpected_error_does_not_use_backup() {
        let (primary, _) = failing_manager(vec![Failure::Generic]);
        let (backup, backup_store) = failing_manager(vec![]);
        let err = run_import(&primary, Some(&backup)).await.unwrap_err();
        assert!(matches!(err, Error::Other(_)), "got {err:?}");
        assert_eq!(stored(&backup_store).await, Vec::<String>::new());
    }

    #[tokio::test]
    async fn test_import_log_permission_denied_retries_with_new_name() {
        let (primary, primary_store) = failing_manager(vec![Failure::PermissionDenied]);
        let (backup, backup_store) = failing_manager(vec![]);
        run_import(&primary, Some(&backup)).await.unwrap();
        let stored_paths = stored(&primary_store).await;
        assert_eq!(stored_paths.len(), 1);
        let suffix = stored_paths[0]
            .strip_prefix("codebase/run-id/build.log.")
            .and_then(|rest| rest.strip_suffix(".gz"))
            .unwrap();
        chrono::NaiveDateTime::parse_from_str(suffix, "%Y-%m-%dT%H:%M:%S").unwrap();
        assert_eq!(stored(&backup_store).await, Vec::<String>::new());
    }

    #[tokio::test]
    async fn test_import_log_permission_denied_retry_unexpected_error() {
        let (primary, _) = failing_manager(vec![Failure::PermissionDenied, Failure::Generic]);
        let (backup, backup_store) = failing_manager(vec![]);
        let err = run_import(&primary, Some(&backup)).await.unwrap_err();
        assert!(matches!(err, Error::Other(_)), "got {err:?}");
        assert_eq!(stored(&backup_store).await, Vec::<String>::new());
    }

    #[tokio::test]
    async fn test_import_log_permission_denied_twice_uses_backup() {
        let (primary, _) =
            failing_manager(vec![Failure::PermissionDenied, Failure::PermissionDenied]);
        let (backup, backup_store) = failing_manager(vec![]);
        run_import(&primary, Some(&backup)).await.unwrap();
        assert_eq!(
            stored(&backup_store).await,
            vec!["codebase/run-id/build.log.gz".to_string()]
        );
    }

    #[tokio::test]
    async fn test_import_log_permission_denied_twice_without_backup() {
        // Python only logged a warning in this case.
        let (primary, primary_store) =
            failing_manager(vec![Failure::PermissionDenied, Failure::PermissionDenied]);
        run_import(&primary, None).await.unwrap();
        assert_eq!(stored(&primary_store).await, Vec::<String>::new());
    }

    #[test]
    fn test_create_log_manager_s3() {
        create_log_manager(Some("s3://janitor-logs")).unwrap();
        create_log_manager(Some("s3://janitor-logs/logs")).unwrap();
        create_log_manager(Some("memory:///logs")).unwrap();
        for location in ["s3://", "https://s3.example.com/janitor-logs"] {
            let err = create_log_manager(Some(location)).unwrap_err();
            assert!(matches!(err, Error::Other(_)), "got {err:?}");
            assert!(err.to_string().contains(location), "got {err}");
        }
    }
}
