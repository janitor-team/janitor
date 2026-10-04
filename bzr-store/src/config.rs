//! Configuration management for BZR Store service

use janitor::shared_config::{ConfigError, FromEnv, ServiceConfig, ValidationError};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;

/// Configuration for the BZR Store service
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BzrStoreConfig {
    /// Shared base configuration
    #[serde(flatten)]
    pub base: ServiceConfig,

    /// BZR-specific configuration
    pub bzr: BzrConfig,
}

/// BZR-specific configuration options
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BzrConfig {
    /// Base path for repository storage
    pub repository_path: PathBuf,

    /// Admin interface bind address (full access)
    pub admin_bind: SocketAddr,

    /// Public interface bind address (read-only)
    pub public_bind: SocketAddr,

    /// Optional Python path for PyO3
    pub python_path: Option<String>,

    /// Request timeout in seconds
    #[serde(default = "default_request_timeout")]
    pub request_timeout: u64,
}

impl Default for BzrStoreConfig {
    fn default() -> Self {
        Self {
            base: ServiceConfig::default(),
            bzr: BzrConfig {
                repository_path: PathBuf::from("/var/lib/janitor/bzr"),
                admin_bind: "127.0.0.1:9929".parse().unwrap(),
                public_bind: "127.0.0.1:9930".parse().unwrap(),
                python_path: None,
                request_timeout: default_request_timeout(),
            },
        }
    }
}

impl FromEnv for BzrStoreConfig {
    fn from_env() -> Result<Self, ConfigError> {
        Self::from_env_with_prefix("")
    }

    fn from_env_with_prefix(prefix: &str) -> Result<Self, ConfigError> {
        let parser = janitor::shared_config::env::EnvParser::new();

        // Load base configuration
        let base = ServiceConfig::from_env_with_prefix(prefix)?;

        // Load BZR-specific configuration
        let bzr = BzrConfig {
            repository_path: parser
                .get_string("BZR_REPOSITORY_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/var/lib/janitor/bzr")),
            admin_bind: parser
                .get_string("BZR_ADMIN_BIND")
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| "127.0.0.1:9929".parse().unwrap()),
            public_bind: parser
                .get_string("BZR_PUBLIC_BIND")
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| "127.0.0.1:9930".parse().unwrap()),
            python_path: parser.get_string("PYTHON_PATH"),
            request_timeout: parser
                .get_u64("BZR_REQUEST_TIMEOUT")?
                .unwrap_or_else(default_request_timeout),
        };

        Ok(Self { base, bzr })
    }
}

impl BzrStoreConfig {
    /// Load configuration from file
    pub fn from_file<P: AsRef<std::path::Path>>(path: P) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path.as_ref()).map_err(|e| ConfigError::IoError {
            path: path.as_ref().display().to_string(),
            message: e.to_string(),
        })?;

        toml::from_str(&content).map_err(|e| ConfigError::ParseError {
            field: "config".to_string(),
            message: e.to_string(),
        })
    }

    /// Validate the configuration
    pub fn validate(&self) -> Result<(), ValidationError> {
        // Validate BZR-specific configuration
        // Ensure repository path is absolute
        if !self.bzr.repository_path.is_absolute() {
            return Err(ValidationError::InvalidValue {
                field: "bzr.repository_path".to_string(),
                message: format!(
                    "Repository path must be absolute: {}",
                    self.bzr.repository_path.display()
                ),
            });
        }

        // Ensure admin and public ports are different
        if self.bzr.admin_bind.port() == self.bzr.public_bind.port() {
            return Err(ValidationError::InvalidValue {
                field: "bzr.bind_addresses".to_string(),
                message: "Admin and public interfaces cannot use the same port".to_string(),
            });
        }

        Ok(())
    }

    /// Load configuration from environment variables and config file
    pub async fn load() -> Result<Self, anyhow::Error> {
        // Try to load from config file if specified
        if let Ok(config_path) = std::env::var("BZR_CONFIG_PATH") {
            match Self::from_file(&config_path) {
                Ok(config) => {
                    config
                        .validate()
                        .map_err(|e| anyhow::anyhow!("Config validation failed: {}", e))?;
                    return Ok(config);
                }
                Err(e) => {
                    eprintln!("Failed to load config from file {}: {}", config_path, e);
                }
            }
        }

        // Fall back to environment variables
        let config = <Self as FromEnv>::from_env()
            .map_err(|e| anyhow::anyhow!("Failed to load config from env: {}", e))?;

        config
            .validate()
            .map_err(|e| anyhow::anyhow!("Config validation failed: {}", e))?;

        Ok(config)
    }

    /// Get database configuration
    pub fn database(&self) -> Option<&janitor::shared_config::DatabaseConfig> {
        self.base.database.as_ref()
    }

    /// Get database URL for compatibility
    pub fn database_url(&self) -> String {
        self.database()
            .map(|db| db.url.clone())
            .unwrap_or_else(|| "postgresql://localhost/janitor".to_string())
    }

    /// Get max connections for compatibility
    pub fn max_connections(&self) -> u32 {
        self.database().map(|db| db.max_connections).unwrap_or(10)
    }
}

// Compatibility accessors - delegate to nested fields
impl std::ops::Deref for BzrStoreConfig {
    type Target = BzrConfig;

    fn deref(&self) -> &Self::Target {
        &self.bzr
    }
}

fn default_request_timeout() -> u64 {
    30
}

// Re-export the main config type for compatibility
pub use BzrStoreConfig as Config;
