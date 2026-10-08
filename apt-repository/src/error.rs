//! Error types for the APT repository library.

/// Result type for APT repository operations.
pub type Result<T> = std::result::Result<T, AptRepositoryError>;

/// Errors that can occur when working with APT repositories.
#[derive(Debug, thiserror::Error)]
pub enum AptRepositoryError {
    /// I/O error occurred during file operations.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Invalid repository configuration.
    #[error("Invalid repository configuration: {0}")]
    InvalidConfiguration(String),

    /// Invalid package control data.
    #[error("Invalid package data: {0}")]
    InvalidPackageData(String),

    /// Invalid source control data.
    #[error("Invalid source data: {0}")]
    InvalidSourceData(String),

    /// Compression error.
    #[error("Compression error: {0}")]
    Compression(String),

    /// Hash calculation error.
    #[error("Hash calculation error: {0}")]
    Hash(String),

    /// Missing required field.
    #[error("Missing required field: {0}")]
    MissingField(String),

    /// Invalid field value.
    #[error("Invalid field value for '{field}': {value}")]
    InvalidField { field: String, value: String },

    /// File not found.
    #[error("File not found: {0}")]
    FileNotFound(String),

    /// Directory creation failed.
    #[error("Failed to create directory: {0}")]
    DirectoryCreation(String),

    /// A package or source provider failed.
    #[error("Provider error: {0}")]
    Provider(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl AptRepositoryError {
    /// Create a new invalid configuration error.
    pub fn invalid_config<S: Into<String>>(msg: S) -> Self {
        Self::InvalidConfiguration(msg.into())
    }

    /// Create a new invalid package data error.
    pub fn invalid_package<S: Into<String>>(msg: S) -> Self {
        Self::InvalidPackageData(msg.into())
    }

    /// Create a new invalid source data error.
    pub fn invalid_source<S: Into<String>>(msg: S) -> Self {
        Self::InvalidSourceData(msg.into())
    }

    /// Create a new missing field error.
    pub fn missing_field<S: Into<String>>(field: S) -> Self {
        Self::MissingField(field.into())
    }

    /// Create a new invalid field error.
    pub fn invalid_field<S: Into<String>>(field: S, value: S) -> Self {
        Self::InvalidField {
            field: field.into(),
            value: value.into(),
        }
    }
}
