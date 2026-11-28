//! Configuration for the git-store service.

use janitor::shared_config::DatabaseConfig;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<DatabaseConfig>,

    #[serde(flatten)]
    pub git: GitStoreConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GitStoreConfig {
    pub local_path: PathBuf,

    #[serde(default = "default_admin_port")]
    pub admin_port: u16,

    #[serde(default = "default_public_port")]
    pub public_port: u16,

    #[serde(default = "default_host")]
    pub host: String,

    pub templates_path: Option<PathBuf>,

    #[serde(default = "default_git_timeout")]
    pub git_timeout: u64,
}

impl Default for GitStoreConfig {
    fn default() -> Self {
        Self {
            local_path: PathBuf::from("/srv/git"),
            admin_port: default_admin_port(),
            public_port: default_public_port(),
            host: default_host(),
            templates_path: None,
            git_timeout: default_git_timeout(),
        }
    }
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}

fn default_admin_port() -> u16 {
    9421
}

fn default_public_port() -> u16 {
    9422
}

fn default_git_timeout() -> u64 {
    30
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();

        assert_eq!(config.git.host, "0.0.0.0");
        assert_eq!(config.git.git_timeout, 30);
        assert_eq!(config.git.admin_port, 9421);
        assert_eq!(config.git.public_port, 9422);
    }
}
