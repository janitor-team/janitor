//! Configuration shared between the store services.

pub mod database;
pub mod env;
pub mod validation;
pub mod web;

pub use database::DatabaseConfig;
pub use env::{EnvParser, FromEnv};
pub use validation::{ConfigError, ValidationError};
pub use web::WebConfig;

/// Base configuration that service configs include.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ServiceConfig {
    /// Database configuration (optional for services that don't need database)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<DatabaseConfig>,
}

impl FromEnv for ServiceConfig {
    fn from_env() -> Result<Self, ConfigError> {
        Self::from_env_with_prefix("")
    }

    fn from_env_with_prefix(prefix: &str) -> Result<Self, ConfigError> {
        let parser = EnvParser::with_prefix(prefix);

        let database = if parser.get_string("DATABASE_URL").is_some() {
            Some(DatabaseConfig::from_env_with_prefix(prefix)?)
        } else {
            None
        };

        Ok(Self { database })
    }
}
