//! Configuration for the auto-upload service.
//!
//! Loads the janitor protobuf text-format configuration via the shared
//! [`janitor::config`] loader.

use std::path::Path;

use janitor::config as janitor_config;

/// Auto-upload configuration.
///
/// A thin wrapper around [`janitor_config::Config`] exposing only the fields
/// auto-upload actually needs (artifact/redis/database locations). Kept as a
/// wrapper rather than a re-export so the accessors can return `&str` and
/// callers don't have to know about protobuf option semantics.
#[derive(Debug, Clone)]
pub struct Config {
    inner: janitor_config::Config,
}

impl Config {
    /// Load the janitor configuration from a file path.
    ///
    /// Returns an error if the file does not exist or does not parse as a
    /// valid protobuf text-format config.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let inner = janitor_config::read_file(path).map_err(|e| ConfigError {
            path: path.display().to_string(),
            message: e.to_string(),
        })?;
        Ok(Self { inner })
    }

    /// Artifact storage location URL (e.g. `file:///…` or an object-store URL).
    ///
    /// `None` when the config does not set `artifact_location`.
    pub fn artifact_location(&self) -> Option<&str> {
        self.inner.artifact_location.as_deref()
    }

    /// Redis pub/sub URL.
    pub fn redis_location(&self) -> Option<&str> {
        self.inner.redis_location.as_deref()
    }

    /// PostgreSQL connection URL.
    pub fn database_location(&self) -> Option<&str> {
        self.inner.database_location.as_deref()
    }
}

/// Failure to load [`Config`] from disk.
#[derive(Debug, thiserror::Error)]
#[error("failed to load config from {path}: {message}")]
pub struct ConfigError {
    /// Path we tried to load.
    pub path: String,
    /// Rendered underlying error from the shared loader. Stored as a string
    /// because `janitor::config::read_file` returns a boxed error that is not
    /// `Send + Sync`.
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_example_config() {
        // Sanity check that the shared protobuf loader is what we're using.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("janitor.conf.example");
        let config = Config::from_file(&path).expect("example config should parse");
        assert_eq!(config.artifact_location(), Some("/home/janitor/artifacts"));
        assert_eq!(config.redis_location(), Some("redis://localhost"));
        assert_eq!(
            config.database_location(),
            Some("postgresql://janitor@example.com:5432/janitor")
        );
    }

    #[test]
    fn missing_file_is_an_error() {
        let err = Config::from_file("/nonexistent/janitor.conf").unwrap_err();
        assert!(err.to_string().contains("/nonexistent/janitor.conf"));
    }
}
