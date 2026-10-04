//! Error type for the auto-upload service.

use thiserror::Error;

/// Errors returned by the auto-upload processing pipeline.
#[derive(Debug, Error)]
pub enum UploadError {
    /// `debsign` failed.
    #[error("Failed to sign package: {0}")]
    DebsignFailure(String),

    /// `dput` failed.
    #[error("Failed to upload package: {0}")]
    DputFailure(String),

    /// All packages for a run failed to upload.
    #[error("no packages uploaded for run")]
    NoPackagesUploaded,

    /// Some but not all packages for a run failed to upload.
    #[error("partial upload: {successful}/{total} packages uploaded")]
    PartialUpload {
        /// Packages that made it to `dput` successfully.
        successful: usize,
        /// Total packages the run produced.
        total: usize,
    },

    /// Artifacts are missing from the artifact store.
    #[error("Artifacts missing for run {0}")]
    ArtifactsMissing(String),

    /// No `.changes` files were found under the retrieved artifacts.
    #[error("No changes files found in artifacts")]
    NoChangesFiles,

    /// SQL query failed.
    #[error(transparent)]
    Database(#[from] sqlx::Error),

    /// Failed to open a connection pool.
    #[error(transparent)]
    DatabaseConnect(#[from] janitor::database::DatabaseError),

    /// Redis error.
    #[error("Redis error: {0}")]
    Redis(#[from] redis::RedisError),

    /// Error from the shared janitor libraries (redis manager, etc).
    #[error(transparent)]
    Janitor(#[from] janitor::error::JanitorError),

    /// I/O error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Configuration error.
    #[error("Configuration error: {0}")]
    Config(String),

    /// A required configuration value was not set.
    #[error("no {0} configured")]
    MissingConfig(&'static str),

    /// A background task panicked.
    #[error("{name} task panicked: {source}")]
    TaskPanic {
        /// Which task panicked (e.g. "web", "redis").
        name: &'static str,
        /// The join error carrying the panic payload.
        source: tokio::task::JoinError,
    },

    /// Failed to load the configuration file.
    #[error(transparent)]
    ConfigLoad(#[from] crate::config::ConfigError),
}

/// Result alias for auto-upload operations.
pub type Result<T> = std::result::Result<T, UploadError>;
