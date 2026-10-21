use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::io::{self, Read};

mod filesystem;
pub use filesystem::FileSystemLogFileManager;

#[cfg(feature = "gcs")]
mod gcs;
#[cfg(feature = "gcs")]
pub use gcs::GCSLogFileManager;

mod s3;
pub use s3::S3LogFileManager;

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
        Error::Io(err.to_string())
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

/// Import a log with primary and backup log managers
///
/// This function provides sophisticated error handling and fallback mechanisms
/// matching the Python implementation. It will attempt to use the primary manager
/// first, then fall back to the backup manager on service unavailability.
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

    // Fall back to the backup manager, or fail with `no_backup` if there is none.
    let fallback = |no_backup: Error| async move {
        let Some(backup) = backup_log_manager else {
            return Err(no_backup);
        };
        backup
            .import_log(codebase, run_id, path, mtime_dt, basename)
            .await?;
        increment_upload_success();
        Ok(())
    };
    let all_failed = || Error::Other("All log managers failed".to_string());

    match primary_log_manager
        .import_log(codebase, run_id, path, mtime_dt, basename)
        .await
    {
        Ok(()) => {
            log::info!("Successfully imported log {} to primary manager", path);
            increment_upload_success();
            Ok(())
        }
        Err(Error::ServiceUnavailable) => {
            log::warn!("Unable to upload logfile {}: service unavailable", path);
            increment_upload_failed();
            fallback(all_failed()).await
        }
        Err(Error::Timeout) => {
            log::warn!("Timeout uploading logfile {}", path);
            increment_upload_failed();
            fallback(all_failed()).await
        }
        Err(Error::PermissionDenied) => {
            log::warn!("Permission denied error while uploading logfile {}", path);
            // Try with timestamp-based alternative basename
            let suffix = chrono::Utc::now().format("%Y%m%dT%H%M%S").to_string();
            let alt_basename = basename.map(|b| format!("{}.{}", b, suffix));

            match primary_log_manager
                .import_log(codebase, run_id, path, mtime_dt, alt_basename.as_deref())
                .await
            {
                Ok(()) => {
                    increment_upload_success();
                    Ok(())
                }
                Err(_) => {
                    increment_upload_failed();
                    fallback(all_failed()).await
                }
            }
        }
        Err(e) => {
            log::error!("Primary log manager failed: {}", e);
            increment_upload_failed();
            if backup_log_manager.is_some() {
                log::info!("Trying backup log manager");
            }
            fallback(e).await
        }
    }
}
