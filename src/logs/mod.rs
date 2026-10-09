use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::io::{self, Read};
use std::time::Duration;

mod filesystem;
pub use filesystem::FileSystemLogFileManager;

#[cfg(feature = "gcs")]
mod gcs;
#[cfg(feature = "gcs")]
pub use gcs::GCSLogFileManager;

mod s3;
pub use s3::S3LogFileManager;

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
    Other(String),
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        // Python's managers raised PermissionError for these, which
        // import_log handles separately.
        if err.kind() == io::ErrorKind::PermissionDenied {
            Error::PermissionDenied
        } else {
            Error::Io(err.to_string())
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

/// A trait for managing logs.
///
/// This trait is implemented by various log file managers, which
/// can be either local or remote.
#[async_trait]
pub trait LogFileManager: Send + Sync {
    /// Check if a log exists.
    async fn has_log(&self, codebase: &str, run_id: &str, name: &str) -> Result<bool, Error>;

    /// Check if a log exists with timeout.
    async fn has_log_with_timeout(
        &self,
        codebase: &str,
        run_id: &str,
        name: &str,
        _timeout: Option<Duration>,
    ) -> Result<bool, Error> {
        // Default implementation ignores timeout
        self.has_log(codebase, run_id, name).await
    }

    /// Get a log.
    ///
    /// # Arguments
    /// * `codebase` - The codebase name.
    /// * `run_id` - The run ID.
    /// * `name` - The log name.
    ///
    /// # Returns
    /// A reader for the log file.
    async fn get_log(
        &self,
        codebase: &str,
        run_id: &str,
        name: &str,
    ) -> Result<Box<dyn Read + Send + Sync>, Error>;

    /// Get a log with timeout.
    async fn get_log_with_timeout(
        &self,
        codebase: &str,
        run_id: &str,
        name: &str,
        _timeout: Option<Duration>,
    ) -> Result<Box<dyn Read + Send + Sync>, Error> {
        // Default implementation ignores timeout
        self.get_log(codebase, run_id, name).await
    }

    /// Import a log.
    ///
    /// # Arguments
    /// * `codebase` - The codebase name.
    /// * `run_id` - The run ID.
    /// * `orig_path` - The original path of the log.
    /// * `mtime` - The modification time of the log.
    /// * `basename` - The basename of the log.
    async fn import_log(
        &self,
        codebase: &str,
        run_id: &str,
        orig_path: &str,
        mtime: Option<DateTime<Utc>>,
        basename: Option<&str>,
    ) -> Result<(), Error>;

    /// Delete a log.
    ///
    /// # Arguments
    /// * `codebase` - The codebase name.
    /// * `run_id` - The run ID.
    /// * `name` - The log name.
    async fn delete_log(&self, codebase: &str, run_id: &str, name: &str) -> Result<(), Error>;

    /// List logs.
    async fn iter_logs(&self) -> Box<dyn Iterator<Item = (String, String, Vec<String>)>>;

    /// Get the creation time of a log.
    ///
    /// # Arguments
    /// * `codebase` - The codebase name.
    /// * `run_id` - The run ID.
    /// * `name` - The log name.
    async fn get_ctime(
        &self,
        codebase: &str,
        run_id: &str,
        name: &str,
    ) -> Result<DateTime<Utc>, Error>;

    /// Perform a health check on the log manager.
    ///
    /// This method should verify that the log storage backend is accessible
    /// and functioning properly.
    async fn health_check(&self) -> Result<(), Error>;
}

/// Create a log file manager based on the location string.
///
/// Supported location formats:
/// - Local filesystem path (e.g., "/var/log/janitor")
/// - Google Cloud Storage URL (e.g., "gs://bucket-name")
/// - S3/HTTP URL (e.g., "https://s3.amazonaws.com", "http://minio:9000")
/// - None/empty: Uses temporary directory
pub async fn create_log_manager(location: Option<&str>) -> Result<Box<dyn LogFileManager>, Error> {
    match location {
        None | Some("") => {
            // Use temporary directory
            let temp_dir = std::env::temp_dir();
            Ok(Box::new(FileSystemLogFileManager::new(temp_dir)?))
        }
        Some(loc) if loc.starts_with("gs://") => {
            #[cfg(feature = "gcs")]
            {
                let url = url::Url::parse(loc)
                    .map_err(|e| Error::Other(format!("Invalid GCS URL: {}", e)))?;
                Ok(Box::new(GCSLogFileManager::from_url(&url, None).await?))
            }
            #[cfg(not(feature = "gcs"))]
            {
                Err(Error::Other("GCS support not compiled in".to_string()))
            }
        }
        Some(loc) if loc.starts_with("http://") || loc.starts_with("https://") => {
            // S3-compatible storage
            Ok(Box::new(S3LogFileManager::new(loc, None)?))
        }
        Some(loc) => {
            // Default to filesystem
            Ok(Box::new(FileSystemLogFileManager::new(loc)?))
        }
    }
}

/// Get a log manager from a location string (Python compatibility wrapper)
pub async fn get_log_manager(location: Option<&str>) -> Result<Box<dyn LogFileManager>, Error> {
    create_log_manager(location).await
}

/// Import a log with primary and backup log managers
///
/// Like the Python implementation, this falls back to the backup manager
/// when the primary manager is unavailable, times out or denies
/// permission (after retrying under a different name).
pub async fn import_log(
    primary_log_manager: &dyn LogFileManager,
    backup_log_manager: Option<&dyn LogFileManager>,
    codebase: &str,
    run_id: &str,
    path: &str,
    basename: Option<&str>,
    mtime: Option<i64>, // Unix timestamp for Python compatibility
) -> Result<(), Error> {
    // Validate input path (no slashes in components)
    if let Some(basename) = basename {
        if basename.contains('/') {
            return Err(Error::Other(
                "Basename cannot contain '/' characters".to_string(),
            ));
        }
    }

    let mtime_dt = if let Some(mtime) = mtime {
        DateTime::from_timestamp(mtime, 0)
    } else {
        std::fs::metadata(path)
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .map(DateTime::<Utc>::from)
    };

    let err = match primary_log_manager
        .import_log(codebase, run_id, path, mtime_dt, basename)
        .await
    {
        Ok(()) => {
            increment_upload_success();
            return Ok(());
        }
        Err(e) => e,
    };

    // Like Python, only unavailability, timeouts and permission errors
    // fall back to the backup manager; anything else is returned as is.
    match err {
        Error::ServiceUnavailable | Error::Timeout => {
            log::warn!("Unable to upload logfile {}: {}", path, err);
            increment_upload_failed();
        }
        Error::PermissionDenied => {
            log::warn!("Permission denied error while uploading logfile {}", path);
            // It may just be that the file already exists.
            let name = match basename {
                Some(basename) => basename.to_string(),
                None => std::path::Path::new(path)
                    .file_name()
                    .ok_or_else(|| Error::Other(format!("No file name in {}", path)))?
                    .to_string_lossy()
                    .into_owned(),
            };
            let alternative_basename =
                format!("{}.{}", name, Utc::now().format("%Y-%m-%dT%H:%M:%S"));
            match primary_log_manager
                .import_log(
                    codebase,
                    run_id,
                    path,
                    mtime_dt,
                    Some(&alternative_basename),
                )
                .await
            {
                Ok(()) => {
                    increment_upload_success();
                    return Ok(());
                }
                Err(Error::ServiceUnavailable | Error::Timeout | Error::PermissionDenied) => {}
                Err(e) => return Err(e),
            }
            increment_upload_failed();
        }
        e => return Err(e),
    }

    // Without a backup manager Python only logged the failure above.
    if let Some(backup) = backup_log_manager {
        backup
            .import_log(codebase, run_id, path, mtime_dt, basename)
            .await?;
    }
    Ok(())
}

/// Import multiple logs concurrently
///
/// This function provides batch import capabilities with concurrent processing
/// similar to the Python implementation using asyncio.gather().
pub async fn import_logs(
    primary_log_manager: &dyn LogFileManager,
    backup_log_manager: Option<&dyn LogFileManager>,
    logs: Vec<(String, String, String, Option<String>)>, // (codebase, run_id, path, basename)
    mtime: Option<i64>,
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
                basename.as_deref(),
                mtime,
            )
            .await
        });

    join_all(import_futures).await
}

/// Log entry structure for Python compatibility
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub name: String,
    pub path: String,
}

/// Import logs from a list of log entries
pub async fn import_logs_from_entries(
    entries: Vec<LogEntry>,
    logfile_manager: &dyn LogFileManager,
    pkg: &str,
    log_id: &str,
    backup_logfile_manager: Option<&dyn LogFileManager>,
    mtime: Option<i64>,
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
            mtime,
        )
        .await
    });

    join_all(import_futures).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Log manager that fails imports with queued errors and records
    /// the basenames it was asked to import.
    #[derive(Default)]
    struct FakeLogManager {
        errors: Mutex<Vec<Error>>,
        imported: Mutex<Vec<Option<String>>>,
    }

    impl FakeLogManager {
        fn failing(errors: Vec<Error>) -> Self {
            Self {
                errors: Mutex::new(errors),
                imported: Mutex::new(Vec::new()),
            }
        }

        fn imported(&self) -> Vec<Option<String>> {
            self.imported.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl LogFileManager for FakeLogManager {
        async fn has_log(&self, _: &str, _: &str, _: &str) -> Result<bool, Error> {
            unimplemented!()
        }

        async fn get_log(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<Box<dyn Read + Send + Sync>, Error> {
            unimplemented!()
        }

        async fn import_log(
            &self,
            _codebase: &str,
            _run_id: &str,
            _orig_path: &str,
            _mtime: Option<DateTime<Utc>>,
            basename: Option<&str>,
        ) -> Result<(), Error> {
            self.imported
                .lock()
                .unwrap()
                .push(basename.map(str::to_owned));
            let mut errors = self.errors.lock().unwrap();
            if errors.is_empty() {
                Ok(())
            } else {
                Err(errors.remove(0))
            }
        }

        async fn delete_log(&self, _: &str, _: &str, _: &str) -> Result<(), Error> {
            unimplemented!()
        }

        async fn iter_logs(&self) -> Box<dyn Iterator<Item = (String, String, Vec<String>)>> {
            unimplemented!()
        }

        async fn get_ctime(&self, _: &str, _: &str, _: &str) -> Result<DateTime<Utc>, Error> {
            unimplemented!()
        }

        async fn health_check(&self) -> Result<(), Error> {
            Ok(())
        }
    }

    async fn run_import(
        primary: &FakeLogManager,
        backup: Option<&FakeLogManager>,
    ) -> Result<(), Error> {
        import_log(
            primary,
            backup.map(|b| b as &dyn LogFileManager),
            "codebase",
            "run-id",
            "/nonexistent/build.log",
            Some("build.log"),
            Some(0),
        )
        .await
    }

    #[test]
    fn test_io_permission_denied() {
        let err = Error::from(io::Error::from(io::ErrorKind::PermissionDenied));
        assert!(matches!(err, Error::PermissionDenied));
        let err = Error::from(io::Error::other("disk full"));
        assert_eq!(err.to_string(), "I/O error: disk full");
    }

    #[tokio::test]
    async fn test_import_log_unexpected_error_does_not_use_backup() {
        let primary = FakeLogManager::failing(vec![Error::Io("disk full".to_string())]);
        let backup = FakeLogManager::default();
        let err = run_import(&primary, Some(&backup)).await.unwrap_err();
        assert_eq!(err.to_string(), "I/O error: disk full");
        assert_eq!(backup.imported(), Vec::<Option<String>>::new());
    }

    #[tokio::test]
    async fn test_import_log_unavailable_uses_backup() {
        let primary = FakeLogManager::failing(vec![Error::ServiceUnavailable]);
        let backup = FakeLogManager::default();
        run_import(&primary, Some(&backup)).await.unwrap();
        assert_eq!(backup.imported(), vec![Some("build.log".to_string())]);
    }

    #[tokio::test]
    async fn test_import_log_unavailable_without_backup() {
        // Python only logged a warning in this case.
        let primary = FakeLogManager::failing(vec![Error::Timeout]);
        run_import(&primary, None).await.unwrap();
    }

    #[tokio::test]
    async fn test_import_log_permission_denied_retries_with_new_name() {
        let primary = FakeLogManager::failing(vec![Error::PermissionDenied]);
        let backup = FakeLogManager::default();
        run_import(&primary, Some(&backup)).await.unwrap();
        let imported = primary.imported();
        assert_eq!(imported.len(), 2);
        let retried = imported[1].as_deref().unwrap();
        let suffix = retried.strip_prefix("build.log.").unwrap();
        chrono::NaiveDateTime::parse_from_str(suffix, "%Y-%m-%dT%H:%M:%S").unwrap();
        assert_eq!(backup.imported(), Vec::<Option<String>>::new());
    }

    #[tokio::test]
    async fn test_import_log_permission_denied_retry_unexpected_error() {
        let primary = FakeLogManager::failing(vec![
            Error::PermissionDenied,
            Error::Other("boom".to_string()),
        ]);
        let backup = FakeLogManager::default();
        let err = run_import(&primary, Some(&backup)).await.unwrap_err();
        assert_eq!(err.to_string(), "boom");
        assert_eq!(backup.imported(), Vec::<Option<String>>::new());
    }

    #[tokio::test]
    async fn test_import_log_permission_denied_retry_unavailable_uses_backup() {
        let primary =
            FakeLogManager::failing(vec![Error::PermissionDenied, Error::ServiceUnavailable]);
        let backup = FakeLogManager::default();
        run_import(&primary, Some(&backup)).await.unwrap();
        assert_eq!(backup.imported(), vec![Some("build.log".to_string())]);
    }
}
