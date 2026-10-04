//! Runner configuration using shared configuration modules.

use janitor::shared_config::{
    ConfigError, ConfigLoader, ConfigSource, DatabaseConfig, ExternalServiceConfig, FromEnv,
    LoggingConfig, RedisConfig, ServiceConfig, ValidationError, WebConfig,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Complete runner configuration that extends ServiceConfig.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerConfig {
    /// Base service configuration (database, redis, web, logging, external services)
    #[serde(flatten)]
    pub base: ServiceConfig,

    /// Runner-specific configuration
    #[serde(default)]
    pub runner: RunnerSpecificConfig,

    /// VCS management configuration
    #[serde(default)]
    pub vcs: VcsConfig,

    /// Worker coordination configuration
    #[serde(default)]
    pub worker: WorkerConfig,

    /// Application-specific configuration
    #[serde(default)]
    pub application: ApplicationConfig,
}

/// Runner-specific configuration not covered by ServiceConfig
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunnerSpecificConfig {
    /// Enable error tracking
    #[serde(default)]
    pub enable_error_tracking: bool,

    /// Error tracking DSN (e.g., Sentry)
    pub error_tracking_dsn: Option<String>,
}

/// VCS management configuration
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VcsConfig {
    /// Git repository base URL
    pub git_location: Option<String>,

    /// Bazaar repository base URL
    pub bzr_location: Option<String>,

    /// Public VCS location for workers
    pub public_vcs_location: Option<String>,

    /// Enable VCS caching
    #[serde(default = "default_true")]
    pub enable_caching: bool,

    /// VCS operation timeout in seconds
    #[serde(default = "default_vcs_timeout")]
    pub operation_timeout_seconds: u64,

    /// Hosts to avoid for VCS operations
    #[serde(default)]
    pub avoid_hosts: Vec<String>,
}

/// Worker coordination configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerConfig {
    /// Run timeout in minutes -- the watchdog aborts a run that has
    /// been running longer than this (or longer than the scheduler's
    /// per-codebase estimate, whichever is larger; see
    /// `runner::watchdog::Watchdog::check_timeout`).
    #[serde(default = "default_run_timeout")]
    pub run_timeout_minutes: u64,

    /// Worker health check interval in seconds
    #[serde(default = "default_worker_health_check")]
    pub health_check_interval_seconds: u64,

    /// Maximum parallel runs per worker
    #[serde(default = "default_max_parallel_runs")]
    pub max_parallel_runs: u32,

    /// Enable worker authentication
    #[serde(default = "default_true")]
    pub enable_authentication: bool,

    /// Shared secret for worker authentication
    pub shared_secret: Option<String>,

    /// Hosts to avoid when scheduling runs
    #[serde(default)]
    pub avoid_hosts: Vec<String>,
}

/// Application-specific configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplicationConfig {
    /// Application name for identification
    #[serde(default = "default_app_name")]
    pub name: String,

    /// Application environment (development, staging, production)
    #[serde(default = "default_environment")]
    pub environment: String,

    /// Enable dry-run mode
    #[serde(default)]
    pub dry_run: bool,

    /// Configuration file path for legacy config
    pub legacy_config_path: Option<PathBuf>,
}

impl Default for ApplicationConfig {
    fn default() -> Self {
        Self {
            name: default_app_name(),
            environment: default_environment(),
            dry_run: false,
            legacy_config_path: None,
        }
    }
}

#[allow(clippy::derivable_impls)]
impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            base: ServiceConfig::default(),
            runner: RunnerSpecificConfig::default(),
            vcs: VcsConfig::default(),
            worker: WorkerConfig::default(),
            application: ApplicationConfig::default(),
        }
    }
}

impl FromEnv for RunnerConfig {
    fn from_env() -> Result<Self, ConfigError> {
        Self::from_env_with_prefix("")
    }

    fn from_env_with_prefix(prefix: &str) -> Result<Self, ConfigError> {
        // Load base service configuration
        let base = ServiceConfig::from_env_with_prefix(prefix)?;

        // Load runner-specific configuration
        let parser = janitor::shared_config::env::EnvParser::with_prefix(prefix);

        let runner = RunnerSpecificConfig {
            enable_error_tracking: parser
                .get_bool("ENABLE_ERROR_TRACKING")?
                .unwrap_or_default(),
            error_tracking_dsn: parser.get_string("ERROR_TRACKING_DSN"),
        };

        // VCS configuration
        let vcs = VcsConfig {
            git_location: parser.get_string("GIT_LOCATION"),
            bzr_location: parser.get_string("BZR_LOCATION"),
            public_vcs_location: parser.get_string("PUBLIC_VCS_LOCATION"),
            enable_caching: parser.get_bool("VCS_ENABLE_CACHING")?.unwrap_or(true),
            operation_timeout_seconds: parser.get_u64("VCS_OPERATION_TIMEOUT")?.unwrap_or(300),
            avoid_hosts: parser
                .get_string("VCS_AVOID_HOSTS")
                .map(|s| s.split(',').map(String::from).collect())
                .unwrap_or_default(),
        };

        // Worker configuration
        let worker = WorkerConfig {
            run_timeout_minutes: parser.get_u64("WORKER_RUN_TIMEOUT_MINUTES")?.unwrap_or(60),
            health_check_interval_seconds: parser
                .get_u64("WORKER_HEALTH_CHECK_INTERVAL")?
                .unwrap_or(30),
            max_parallel_runs: parser.get_u32("WORKER_MAX_PARALLEL_RUNS")?.unwrap_or(4),
            enable_authentication: parser.get_bool("WORKER_ENABLE_AUTH")?.unwrap_or(true),
            shared_secret: parser.get_string("WORKER_SHARED_SECRET"),
            avoid_hosts: parser
                .get_string("WORKER_AVOID_HOSTS")
                .map(|s| s.split(',').map(|h| h.trim().to_string()).collect())
                .unwrap_or_default(),
        };

        // Application configuration
        let application = ApplicationConfig {
            name: parser
                .get_string("APP_NAME")
                .unwrap_or_else(|| "janitor-runner".to_string()),
            environment: parser
                .get_string("APP_ENVIRONMENT")
                .unwrap_or_else(|| "development".to_string()),
            dry_run: parser.get_bool("DRY_RUN")?.unwrap_or_default(),
            legacy_config_path: parser.get_string("LEGACY_CONFIG_PATH").map(PathBuf::from),
        };

        Ok(Self {
            base,
            runner,
            vcs,
            worker,
            application,
        })
    }
}

impl ConfigLoader for RunnerConfig {
    fn from_file<P: AsRef<std::path::Path>>(path: P) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path.as_ref()).map_err(|e| ConfigError::IoError {
            path: path.as_ref().display().to_string(),
            message: e.to_string(),
        })?;

        // Try different formats based on file extension
        let config = match path.as_ref().extension().and_then(|ext| ext.to_str()) {
            Some("toml") => toml::from_str(&content).map_err(|e| ConfigError::ParseError {
                field: "root".to_string(),
                message: e.to_string(),
            })?,
            Some("yaml") | Some("yml") => {
                serde_yaml::from_str(&content).map_err(|e| ConfigError::ParseError {
                    field: "root".to_string(),
                    message: e.to_string(),
                })?
            }
            Some("json") => {
                serde_json::from_str(&content).map_err(|e| ConfigError::ParseError {
                    field: "root".to_string(),
                    message: e.to_string(),
                })?
            }
            _ => {
                // Default to TOML
                toml::from_str(&content).map_err(|e| ConfigError::ParseError {
                    field: "root".to_string(),
                    message: e.to_string(),
                })?
            }
        };

        Ok(config)
    }

    fn from_env() -> Result<Self, ConfigError> {
        <Self as FromEnv>::from_env()
    }

    fn from_sources(_sources: &[ConfigSource]) -> Result<Self, ConfigError> {
        // For now, just use environment
        <Self as ConfigLoader>::from_env()
    }

    fn validate(&self) -> Result<(), ValidationError> {
        self.validate_config()
    }
}

impl RunnerConfig {
    /// Validate the entire configuration
    pub fn validate_config(&self) -> Result<(), ValidationError> {
        use janitor::shared_config::validation::Validator;
        let mut validator = Validator::new();

        // Both stores are hard requirements: the DB backs every
        // handler, and the ActiveRunStore is Redis-backed so worker
        // /finish uploads survive runner restarts. Fail loudly at
        // startup instead of deferring to an opaque error deep in
        // build().
        match self.base.database.as_ref() {
            Some(db) => db.validate()?,
            None => {
                validator.add_missing_required("base.database.url");
            }
        }
        match self.base.redis.as_ref() {
            Some(redis) => redis.validate()?,
            None => {
                validator.add_missing_required("base.redis.url");
            }
        }
        self.base.logging.validate()?;

        if self.worker.enable_authentication && self.worker.shared_secret.is_none() {
            validator.add_missing_required("worker.shared_secret");
        }

        if self.worker.run_timeout_minutes == 0 {
            validator.add_invalid_value(
                "worker.run_timeout_minutes",
                "Timeout must be greater than 0",
            );
        }

        validator.finish()
    }

    /// Get database config if available
    pub fn database(&self) -> Option<&DatabaseConfig> {
        self.base.database.as_ref()
    }

    /// Get Redis config if available
    pub fn redis(&self) -> Option<&RedisConfig> {
        self.base.redis.as_ref()
    }

    /// Get web config if available
    pub fn web(&self) -> Option<&WebConfig> {
        self.base.web.as_ref()
    }

    /// Get logging config
    pub fn logging(&self) -> &LoggingConfig {
        &self.base.logging
    }

    /// Get external services config
    pub fn external_services(&self) -> &ExternalServiceConfig {
        &self.base.external_services
    }

    /// Convert to legacy janitor config for compatibility
    pub fn to_janitor_config(&self) -> janitor::config::Config {
        // If the TOML specifies legacy_config_path, load the textproto and
        // layer the TOML-sourced overrides on top. This is how we surface
        // campaigns / bugtrackers / apt_repository entries from the
        // deployment's `janitor.conf` to the runner (the TOML config
        // doesn't model those, so without this runner's campaign list is
        // empty and `upload_candidates` rejects every candidate as
        // "unknown campaign").
        let mut config = match self.application.legacy_config_path.as_ref() {
            Some(path) => janitor::config::read_file(path).unwrap_or_else(|e| {
                tracing::warn!(
                    "Failed to load legacy config at {}: {}; using defaults",
                    path.display(),
                    e
                );
                janitor::config::Config::default()
            }),
            None => janitor::config::Config::default(),
        };

        if let Some(ref db) = self.base.database {
            config.database_location = Some(db.url.clone());
        }

        config
    }

    /// Get shutdown timeout duration
    pub fn shutdown_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(30) // Default timeout
    }

    /// Get tracing configuration (create a compatible one)
    pub fn tracing_config(&self) -> crate::tracing::TracingConfig {
        // Convert from shared logging config to tracing config
        crate::tracing::TracingConfig {
            log_level: self.base.logging.level.clone(),
            json_format: self.base.logging.json_format,
            console_output: self.base.logging.console_output,
            file_output: self.base.logging.file_output.as_ref().map(|f| {
                crate::tracing::FileOutputConfig {
                    path: f.path.clone(),
                    rotation_enabled: true,
                    max_file_size_mb: 100,
                    max_files: 10,
                    compress: false,
                }
            }),
            structured_logging: crate::tracing::StructuredLoggingConfig {
                include_source_location: self.base.logging.include_module_path,
                include_thread_info: false,
                include_process_info: true,
                include_hostname: true,
                static_fields: std::collections::HashMap::new(),
            },
            tracing: crate::tracing::TracingSpanConfig {
                enable_distributed_tracing: false,
                trace_http_requests: true,
                trace_database_operations: true,
                trace_vcs_operations: false,
                trace_queue_operations: true,
                sample_rate: 1.0,
            },
            performance: crate::tracing::PerformanceLoggingConfig {
                log_slow_operations: true,
                slow_operation_threshold_ms: 5000,
                log_performance_metrics: false,
                metrics_interval_seconds: 60,
            },
        }
    }

    /// Get error tracking configuration
    pub fn error_tracking_config(&self) -> crate::error_tracking::ErrorTrackingConfig {
        crate::error_tracking::ErrorTrackingConfig {
            max_errors_in_memory: 1000,
            log_to_file: self.runner.enable_error_tracking,
            error_log_path: self.runner.error_tracking_dsn.as_ref().map(|dsn| {
                std::path::PathBuf::from(format!("logs/errors-{}.log", dsn.replace('/', "-")))
            }),
            enable_stack_traces: true,
            min_severity: crate::error_tracking::ErrorSeverity::Warning,
            enable_correlation: true,
        }
    }
}

// Default value functions
fn default_true() -> bool {
    true
}

fn default_vcs_timeout() -> u64 {
    300
}

fn default_run_timeout() -> u64 {
    60
}

fn default_worker_health_check() -> u64 {
    30
}

fn default_max_parallel_runs() -> u32 {
    4
}

fn default_app_name() -> String {
    "janitor-runner".to_string()
}

fn default_environment() -> String {
    "development".to_string()
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            run_timeout_minutes: default_run_timeout(),
            health_check_interval_seconds: default_worker_health_check(),
            max_parallel_runs: default_max_parallel_runs(),
            enable_authentication: default_true(),
            shared_secret: None,
            avoid_hosts: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runner_config_default() {
        let config = RunnerConfig::default();
        assert!(config.base.database.is_none());
        assert!(config.base.redis.is_none());
        assert_eq!(config.worker.run_timeout_minutes, 60);
    }

    #[test]
    fn test_runner_config_validation() {
        // Start from a config that has every hard requirement filled
        // in, so we can flip individual fields to test each rule in
        // isolation.
        let mut config = RunnerConfig::default();
        config.base.database = Some(janitor::shared_config::DatabaseConfig {
            url: "postgresql://localhost/janitor".to_string(),
            ..Default::default()
        });
        config.base.redis = Some(janitor::shared_config::RedisConfig {
            url: "redis://localhost".to_string(),
            ..Default::default()
        });

        // Worker authentication is on by default and needs a secret.
        assert!(config.validate().is_err());

        config.worker.enable_authentication = false;
        assert!(config.validate().is_ok());

        // A zero run timeout is rejected too.
        config.worker.run_timeout_minutes = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_runner_config_validation_rejects_missing_database() {
        let mut config = RunnerConfig::default();
        config.base.redis = Some(janitor::shared_config::RedisConfig {
            url: "redis://localhost".to_string(),
            ..Default::default()
        });
        config.worker.enable_authentication = false;
        assert!(config.validate().is_err(), "runner requires a database URL");
    }

    #[test]
    fn test_runner_config_validation_rejects_missing_redis() {
        let mut config = RunnerConfig::default();
        config.base.database = Some(janitor::shared_config::DatabaseConfig {
            url: "postgresql://localhost/janitor".to_string(),
            ..Default::default()
        });
        config.worker.enable_authentication = false;
        assert!(
            config.validate().is_err(),
            "runner requires a redis URL for the ActiveRunStore"
        );
    }
}
