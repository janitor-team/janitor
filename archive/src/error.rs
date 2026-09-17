//! Typed errors returned by every fallible operation in the crate.

use thiserror::Error;

#[derive(Error, Debug)]
#[allow(missing_docs)]
pub enum ArchiveError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Package scanning failed: {0}")]
    PackageScanning(String),

    #[error("Source scanning failed: {0}")]
    SourceScanning(String),

    #[error("Artifact retrieval failed: {0}")]
    ArtifactRetrieval(String),

    #[error("Missing artifacts for build {build_id}: {message}")]
    ArtifactsMissing { build_id: String, message: String },

    #[error("Failed to create temporary directory: {0}")]
    TempDir(#[from] tempfile::PersistError),

    #[error("Repository generation failed: {0}")]
    RepositoryGeneration(String),

    #[error("GPG operation failed: {0}")]
    Gpg(String),

    #[error("Compression failed: {0}")]
    Compression(String),

    #[error("Invalid archive configuration: {0}")]
    InvalidConfiguration(String),

    #[error("Redis error: {0}")]
    Redis(String),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Resource limit exceeded: {0}")]
    ResourceLimit(String),

    #[error("Resource not found: {0}")]
    NotFound(String),

    /// Wraps `shared_config::ConfigError` directly rather than
    /// stringifying, so callers can match on `ParseError`/`IoError`/etc.
    #[error("Configuration error: {0}")]
    Config(#[from] janitor::shared_config::ConfigError),
}

#[allow(missing_docs)]
pub type ArchiveResult<T> = Result<T, ArchiveError>;
