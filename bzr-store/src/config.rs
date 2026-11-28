//! Configuration for the bzr-store service.

use janitor::shared_config::DatabaseConfig;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;

/// Configuration for the BZR Store service
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BzrStoreConfig {
    /// Database connection, if one has been configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<DatabaseConfig>,

    /// BZR-specific settings.
    pub bzr: BzrConfig,
}

/// BZR-specific configuration options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BzrConfig {
    /// On-disk directory holding per-codebase shared bzr repositories.
    pub repository_path: PathBuf,

    /// Admin interface bind address (full access).
    pub admin_bind: SocketAddr,

    /// Public interface bind address (read-only).
    pub public_bind: SocketAddr,

    /// Optional Python path for PyO3.
    pub python_path: Option<String>,

    /// Request timeout in seconds.
    #[serde(default = "default_request_timeout")]
    pub request_timeout: u64,
}

impl Default for BzrStoreConfig {
    fn default() -> Self {
        Self {
            database: None,
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

impl BzrStoreConfig {
    /// Postgres URL, falling back to a local default if no database
    /// was configured. Callers that require a real database must
    /// verify `self.database.is_some()` first.
    pub fn database_url(&self) -> String {
        self.database
            .as_ref()
            .map(|db| db.url.clone())
            .unwrap_or_else(|| "postgresql://localhost/janitor".to_string())
    }

    /// Max pool connections, defaulted to 10 when no database configured.
    pub fn max_connections(&self) -> u32 {
        self.database
            .as_ref()
            .map(|db| db.max_connections)
            .unwrap_or(10)
    }

    /// Reject configurations that would misbehave at runtime.
    pub fn validate(&self) -> Result<(), String> {
        if !self.bzr.repository_path.is_absolute() {
            return Err(format!(
                "Repository path must be absolute: {}",
                self.bzr.repository_path.display()
            ));
        }
        if self.bzr.admin_bind.port() == self.bzr.public_bind.port() {
            return Err("Admin and public interfaces cannot use the same port".to_string());
        }
        Ok(())
    }
}

impl std::ops::Deref for BzrStoreConfig {
    type Target = BzrConfig;

    fn deref(&self) -> &Self::Target {
        &self.bzr
    }
}

fn default_request_timeout() -> u64 {
    30
}

pub use BzrStoreConfig as Config;
