//! Configuration for the git-store service.

use janitor::shared_config::{ConfigError, FromEnv, ServiceConfig};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Config {
    #[serde(flatten)]
    pub base: ServiceConfig,

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

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let base = ServiceConfig::from_env_with_prefix("GIT_STORE")?;
        let git = GitStoreConfig::from_env_with_prefix("GIT_STORE")?;
        Ok(Self { base, git })
    }
}

impl FromEnv for GitStoreConfig {
    fn from_env() -> Result<Self, ConfigError> {
        Self::from_env_with_prefix("GIT_STORE")
    }

    fn from_env_with_prefix(prefix: &str) -> Result<Self, ConfigError> {
        use janitor::shared_config::env::EnvParser;

        let parser = EnvParser::with_prefix(prefix);

        Ok(Self {
            local_path: parser
                .get_string("LOCAL_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/srv/git")),
            admin_port: parser
                .get_u16("ADMIN_PORT")?
                .unwrap_or(default_admin_port()),
            public_port: parser
                .get_u16("PUBLIC_PORT")?
                .unwrap_or(default_public_port()),
            host: parser.get_string("HOST").unwrap_or_else(default_host),
            templates_path: parser.get_string("TEMPLATES_PATH").map(PathBuf::from),
            git_timeout: parser
                .get_u64("GIT_TIMEOUT")?
                .unwrap_or(default_git_timeout()),
        })
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

    #[test]
    fn test_from_env() {
        std::env::set_var("GIT_STORE_LOCAL_PATH", "/custom/git");
        std::env::set_var("GIT_STORE_ADMIN_PORT", "9000");

        let config = Config::from_env().unwrap();

        assert_eq!(config.git.local_path, PathBuf::from("/custom/git"));
        assert_eq!(config.git.admin_port, 9000);

        std::env::remove_var("GIT_STORE_LOCAL_PATH");
        std::env::remove_var("GIT_STORE_ADMIN_PORT");
    }
}
