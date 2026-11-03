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
    ArtifactsMissing {
        build_id: janitor::RunId,
        message: String,
    },

    #[error("Repository generation failed: {0}")]
    RepositoryGeneration(String),

    #[error("Invalid archive configuration: {0}")]
    InvalidConfiguration(String),

    #[error("Redis error: {0}")]
    Redis(String),

    #[error("Resource not found: {0}")]
    NotFound(String),
}

#[allow(missing_docs)]
pub type ArchiveResult<T> = Result<T, ArchiveError>;
