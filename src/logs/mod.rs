use crate::{CodebaseName, RunId};
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
        Error::Io(err.to_string())
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
/// - None/empty: Uses temporary directory
pub fn create_log_manager(location: Option<&str>) -> Result<LogFileManager, Error> {
    match location {
        None | Some("") => LogFileManager::local(std::env::temp_dir()),
        Some(loc) if loc.starts_with("gs://") => {
            #[cfg(feature = "gcs")]
            {
                let url = url::Url::parse(loc)
                    .map_err(|e| Error::Other(format!("Invalid GCS URL: {}", e)))?;
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
        Some(loc) if loc.starts_with("s3://") => LogFileManager::s3(loc),
        Some(loc) if loc.contains("://") => Err(Error::Other(format!(
            "Unsupported logs location {}: use a local path, gs:// or s3://",
            loc
        ))),
        Some(loc) => LogFileManager::local(loc),
    }
}

/// Import a log with primary and backup log managers
///
/// This function provides sophisticated error handling and fallback mechanisms
/// matching the Python implementation. It will attempt to use the primary manager
/// first, then fall back to the backup manager on service unavailability.
pub async fn import_log(
    primary_log_manager: &LogFileManager,
    backup_log_manager: Option<&LogFileManager>,
    codebase: &CodebaseName,
    run_id: &RunId,
    path: &str,
    basename: Option<&str>,
) -> Result<(), Error> {
    // Validate input path (no slashes in components)
    if let Some(basename) = basename {
        if basename.contains('/') {
            return Err(Error::Other(
                "Basename cannot contain '/' characters".to_string(),
            ));
        }
    }

    // Attempt with primary log manager
    match primary_log_manager
        .import_log(codebase, run_id, path, basename)
        .await
    {
        Ok(()) => {
            log::info!("Successfully imported log {} to primary manager", path);
            increment_upload_success();
            return Ok(());
        }
        Err(Error::ServiceUnavailable) => {
            log::warn!("Unable to upload logfile {}: service unavailable", path);
            increment_upload_failed();
            if let Some(backup) = backup_log_manager {
                match backup.import_log(codebase, run_id, path, basename).await {
                    Ok(()) => {
                        increment_upload_success();
                        return Ok(());
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        Err(Error::Timeout) => {
            log::warn!("Timeout uploading logfile {}", path);
            increment_upload_failed();
            if let Some(backup) = backup_log_manager {
                match backup.import_log(codebase, run_id, path, basename).await {
                    Ok(()) => {
                        increment_upload_success();
                        return Ok(());
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        Err(Error::PermissionDenied) => {
            log::warn!("Permission denied error while uploading logfile {}", path);
            // Try with timestamp-based alternative basename
            let suffix = chrono::Utc::now().format("%Y%m%dT%H%M%S").to_string();
            let alt_basename = basename.map(|b| format!("{}.{}", b, suffix));

            match primary_log_manager
                .import_log(codebase, run_id, path, alt_basename.as_deref())
                .await
            {
                Ok(()) => {
                    increment_upload_success();
                    return Ok(());
                }
                Err(_) => {
                    increment_upload_failed();
                    // If alternative basename fails, try backup manager
                    if let Some(backup) = backup_log_manager {
                        match backup.import_log(codebase, run_id, path, basename).await {
                            Ok(()) => {
                                increment_upload_success();
                                return Ok(());
                            }
                            Err(e) => return Err(e),
                        }
                    }
                }
            }
        }
        Err(e) => {
            log::error!("Primary log manager failed: {}", e);
            increment_upload_failed();
            // For other errors, try backup if available
            if let Some(backup) = backup_log_manager {
                log::info!("Trying backup log manager");
                match backup.import_log(codebase, run_id, path, basename).await {
                    Ok(()) => {
                        increment_upload_success();
                        return Ok(());
                    }
                    Err(e) => return Err(e),
                }
            }
            return Err(e);
        }
    }

    Err(Error::Other("All log managers failed".to_string()))
}

/// Import multiple logs concurrently
///
/// This function provides batch import capabilities with concurrent processing
/// similar to the Python implementation using asyncio.gather().
pub async fn import_logs(
    primary_log_manager: &LogFileManager,
    backup_log_manager: Option<&LogFileManager>,
    logs: Vec<(CodebaseName, RunId, String, Option<String>)>, // (codebase, run_id, path, basename)
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

    #[test]
    fn test_create_log_manager_s3() {
        create_log_manager(Some("s3://janitor-logs")).unwrap();
        create_log_manager(Some("s3://janitor-logs/logs")).unwrap();
        for location in ["s3://", "https://s3.example.com/janitor-logs"] {
            let err = create_log_manager(Some(location)).unwrap_err();
            assert!(matches!(err, Error::Other(_)), "got {err:?}");
            assert!(err.to_string().contains(location), "got {err}");
        }
    }
}
