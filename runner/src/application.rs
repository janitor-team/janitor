//! Application initialization and orchestration for the runner.

use crate::{
    config::RunnerConfig,
    database::RunnerDatabase,
    error_tracking::{ErrorTracker, ErrorTrackingConfig},
    metrics::MetricsCollector,
    vcs::RunnerVcsManager,
    AppState,
};
use janitor::shared_config::ConfigLoader;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal;

/// Application configuration combining all subsystem configurations.
#[derive(Debug, Clone)]
pub struct ApplicationConfig {
    /// Core runner configuration using shared config modules
    pub runner_config: RunnerConfig,
    /// Error tracking configuration.
    pub error_tracking_config: ErrorTrackingConfig,
    /// Metrics collection interval.
    pub metrics_interval: Duration,
    /// Enable graceful shutdown handling.
    pub enable_graceful_shutdown: bool,
    /// Shutdown timeout.
    pub shutdown_timeout: Duration,
}

impl Default for ApplicationConfig {
    fn default() -> Self {
        Self {
            runner_config: RunnerConfig::default(),
            error_tracking_config: ErrorTrackingConfig::default(),
            metrics_interval: Duration::from_secs(30),
            enable_graceful_shutdown: true,
            shutdown_timeout: Duration::from_secs(30),
        }
    }
}

/// Application builder for configuring and initializing the runner.
pub struct ApplicationBuilder {
    config: RunnerConfig,
    backup_directory: Option<std::path::PathBuf>,
    public_apt_archive_location: Option<String>,
}

impl Default for ApplicationBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ApplicationBuilder {
    /// Empty builder using `RunnerConfig::default()`.
    pub fn new() -> Self {
        Self {
            config: RunnerConfig::default(),
            backup_directory: None,
            public_apt_archive_location: None,
        }
    }

    /// Builder seeded with an existing config.
    pub fn from_config(config: RunnerConfig) -> Self {
        Self {
            config,
            backup_directory: None,
            public_apt_archive_location: None,
        }
    }

    /// Load config from a file and use it to seed the builder.
    pub fn from_file<P: AsRef<std::path::Path>>(path: P) -> Result<Self, ApplicationError> {
        let config = RunnerConfig::from_file(path).map_err(|e| {
            ApplicationError::Configuration(format!("Failed to load config: {}", e))
        })?;
        Ok(Self::from_config(config))
    }

    /// Set the backup directory used when the main artifact manager is
    /// unreachable. When set, a periodic task drains the directory into
    /// the main artifact manager every 15 minutes.
    pub fn with_backup_directory(mut self, path: Option<std::path::PathBuf>) -> Self {
        self.backup_directory = path;
        self
    }

    /// Set the public base URL of the apt archive.
    ///
    /// Corresponds to the `--public-apt-archive-location` CLI flag.
    pub fn with_public_apt_archive_location(mut self, url: Option<String>) -> Self {
        self.public_apt_archive_location = url;
        self
    }

    /// Override the config file's `public_vcs_location` (used for URLs
    /// handed to workers). Corresponds to the `--public-vcs-location`
    /// CLI flag. A `None` leaves whatever the config file set.
    pub fn with_public_vcs_location(mut self, url: Option<String>) -> Self {
        if let Some(url) = url {
            self.config.vcs.public_vcs_location = Some(url);
        }
        self
    }

    /// Override the config file's watchdog run timeout (in minutes).
    /// Corresponds to the `--run-timeout` CLI flag.
    pub fn with_run_timeout_minutes(mut self, minutes: u64) -> Self {
        self.config.worker.run_timeout_minutes = minutes;
        self
    }

    /// Override the config file's `worker.avoid_hosts`. Corresponds to
    /// the `--avoid-host` CLI flag (repeatable). An empty vec leaves
    /// whatever the config file set.
    pub fn with_avoid_hosts(mut self, hosts: Vec<String>) -> Self {
        if !hosts.is_empty() {
            self.config.worker.avoid_hosts = hosts;
        }
        self
    }

    /// Set the database URL.
    pub fn with_database_url(mut self, url: String) -> Self {
        self.config.base.database = Some(janitor::shared_config::DatabaseConfig {
            url,
            ..Default::default()
        });
        self
    }

    /// Set the Redis URL for coordination.
    pub fn with_redis_url(mut self, url: Option<String>) -> Self {
        if let Some(url) = url {
            self.config.base.redis = Some(janitor::shared_config::RedisConfig {
                url,
                ..Default::default()
            });
        } else {
            self.config.base.redis = None;
        }
        self
    }

    /// Set the web server port.
    pub fn with_port(mut self, port: u16) -> Self {
        if self.config.base.web.is_none() {
            self.config.base.web = Some(janitor::shared_config::WebConfig::default());
        }
        if let Some(ref mut web) = self.config.base.web {
            web.port = port;
        }
        self
    }

    /// Set the listen address.
    pub fn with_listen_address(mut self, address: String) -> Self {
        if self.config.base.web.is_none() {
            self.config.base.web = Some(janitor::shared_config::WebConfig::default());
        }
        if let Some(ref mut web) = self.config.base.web {
            web.listen_address = address;
        }
        self
    }

    /// Enable debug mode.
    pub fn with_debug(mut self, debug: bool) -> Self {
        // Add debug to application config - we need to add this field
        self.config.application.environment = if debug {
            "development".to_string()
        } else {
            "production".to_string()
        };
        self
    }

    /// Build and initialize the application.
    pub async fn build(self) -> Result<Application, ApplicationError> {
        // Initialize tracing and logging first
        let tracing_config = self.config.tracing_config();
        crate::tracing::init_tracing(&tracing_config).map_err(|e| {
            ApplicationError::Configuration(format!("Failed to initialize tracing: {}", e))
        })?;

        log::info!("Initializing Janitor Runner application...");

        self.config.validate_config().map_err(|e| {
            ApplicationError::Configuration(format!("Configuration validation failed: {}", e))
        })?;

        // Initialize metrics first so other systems can use them
        log::info!("Initializing metrics collection...");
        let metrics = Arc::new(MetricsCollector {});
        crate::metrics::init_metrics();

        // Initialize error tracking
        log::info!("Initializing error tracking...");
        let error_tracking_config = self.config.error_tracking_config();
        let error_tracker = Arc::new(ErrorTracker::new(error_tracking_config));

        log::info!("Initializing database connection...");
        let janitor_config = self.config.to_janitor_config();
        let database_pool = match janitor::state::create_pool(&janitor_config).await {
            Ok(pool) => pool,
            Err(e) => {
                let error =
                    ApplicationError::Database(format!("Failed to create database pool: {}", e));
                error_tracker
                    .track_error(error_tracker.create_tracked_error(
                        &error,
                        crate::error_tracking::ErrorCategory::Database,
                        "application",
                        "initialization",
                    ))
                    .await;
                return Err(error);
            }
        };

        let database = Arc::new(
            RunnerDatabase::new_with_redis_url(
                database_pool,
                self.config.redis().map(|r| r.url.clone()),
            )
            .await
            .map_err(|e| {
                ApplicationError::Database(format!("Failed to initialize database: {}", e))
            })?,
        );

        // Initialize VCS management
        log::info!("Initializing VCS management...");
        let vcs_manager =
            Arc::new(RunnerVcsManager::from_config(&janitor_config).map_err(|e| {
                ApplicationError::Configuration(format!("Failed to initialize VCS manager: {}", e))
            })?);

        // Initialize log management from textproto logs_location (e.g. gs:// or /path).
        log::info!("Initializing log management...");
        let log_manager: Arc<dyn janitor::logs::LogFileManager> = Arc::from(
            janitor::logs::create_log_manager(janitor_config.logs_location.as_deref())
                .await
                .map_err(|e| {
                    ApplicationError::LogManagement(format!(
                        "Failed to initialize log manager: {}",
                        e
                    ))
                })?,
        );

        // Initialize artifact management from textproto artifact_location.
        log::info!("Initializing artifact management...");
        let artifact_manager: Arc<dyn janitor::artifacts::ArtifactManager> = Arc::from(
            janitor::artifacts::get_artifact_manager(
                janitor_config
                    .artifact_location
                    .as_deref()
                    .unwrap_or("/var/lib/janitor/artifacts"),
            )
            .await
            .map_err(|e| {
                ApplicationError::ArtifactManagement(format!(
                    "Failed to initialize artifact manager: {}",
                    e
                ))
            })?,
        );

        // Initialize upload processor. The storage dir is shared with
        // the site pod (mounted via PV at the same path) so logs the
        // worker uploads here can be served straight off disk by the
        // site's FileSystemLogFileManager. Default to
        // `/var/log/janitor` to match the FS log manager default;
        // overridable via UPLOAD_STORAGE_DIR for unit tests / dev.
        log::info!("Initializing upload processor...");
        let upload_storage_dir = std::env::var("UPLOAD_STORAGE_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("/var/log/janitor"));
        // Fail startup if we can't materialise the upload dir --
        // continuing on a warn leaves the process healthy at
        // /health/live but every worker /finish upload will 500 with
        // an opaque write error.
        std::fs::create_dir_all(&upload_storage_dir).map_err(|e| {
            ApplicationError::Configuration(format!(
                "Could not create upload storage dir {:?}: {}",
                upload_storage_dir, e
            ))
        })?;
        let upload_processor = Arc::new(crate::upload::UploadProcessor::new(
            upload_storage_dir,
            100 * 1024 * 1024, // 100MB max file size
            500 * 1024 * 1024, // 500MB max total size
        ));

        // Initialize authentication and security services
        log::info!("Initializing authentication and security services...");
        let auth_service = Arc::new(crate::auth::WorkerAuthService::new(Arc::clone(&database)));

        let security_config = crate::auth::SecurityConfig {
            max_requests_per_minute: 100,
            enable_audit_logging: true,
            allowed_ip_ranges: vec![],
            max_concurrent_runs_per_worker: 5,
        };
        // ActiveRunStore is Redis-backed so worker /finish uploads
        // survive runner restarts and so multiple runner replicas share
        // a single source of truth for in-flight runs (see
        // salsa.debian.org/janitor-team/janitor.debian.net#117).
        let redis_client = database.redis().cloned().ok_or_else(|| {
            ApplicationError::Configuration(
                "Redis is required for active-runs persistence; configure base.redis.url"
                    .to_string(),
            )
        })?;
        let active_runs = crate::active_runs::ActiveRunStore::new(redis_client);
        let security_service = Arc::new(crate::auth::SecurityService::new(
            security_config,
            Arc::clone(&database),
            active_runs.clone(),
        ));

        // Initialize resume service
        log::info!("Initializing resume service...");
        let resume_service = Arc::new(crate::resume::ResumeService::new((*database).clone()));

        let health_checker = Arc::new(crate::HealthChecker::new(
            database.clone(),
            vcs_manager.clone(),
            log_manager.clone(),
            artifact_manager.clone(),
        ));

        let app_state = Arc::new(AppState {
            database,
            active_runs,
            vcs_manager,
            log_manager,
            artifact_manager,
            error_tracker,
            metrics,
            config: Arc::new(janitor_config),
            upload_processor,
            auth_service,
            security_service,
            resume_service,
            health_checker,
            public_apt_archive_location: self.public_apt_archive_location.clone(),
        });

        log::info!("Janitor Runner application initialized successfully");

        Ok(Application {
            state: app_state,
            config: self.config,
            backup_directory: self.backup_directory,
        })
    }
}

/// Main application struct that manages the runner lifecycle.
pub struct Application {
    /// Application state.
    pub state: Arc<AppState>,
    /// Application configuration.
    config: RunnerConfig,
    /// Optional backup artifact directory, polled by a periodic task
    /// that drains it into the main artifact manager.
    backup_directory: Option<std::path::PathBuf>,
}

impl Application {
    /// Create a new application builder.
    pub fn builder() -> ApplicationBuilder {
        ApplicationBuilder::new()
    }

    /// Create a new application builder from configuration.
    pub fn builder_from_config(config: RunnerConfig) -> ApplicationBuilder {
        ApplicationBuilder::from_config(config)
    }

    /// Create a new application builder from config file.
    pub fn builder_from_file<P: AsRef<std::path::Path>>(
        path: P,
    ) -> Result<ApplicationBuilder, ApplicationError> {
        ApplicationBuilder::from_file(path)
    }

    /// Get the application state.
    pub fn state(&self) -> &Arc<AppState> {
        &self.state
    }

    /// Run the application with graceful shutdown handling.
    pub async fn run_with_graceful_shutdown<F, Fut>(
        self,
        server_factory: F,
    ) -> Result<(), ApplicationError>
    where
        F: FnOnce(Arc<AppState>) -> Fut,
        Fut: std::future::Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>>
            + Send
            + 'static,
    {
        // Spawn the watchdog loop before the HTTP server so stalled or
        // hung runs get aborted even during startup races. The task is
        // abandoned on process exit.
        let watchdog_state = self.state.clone();
        let watchdog_config =
            crate::watchdog::WatchdogConfig::from_worker_config(&self.config.worker);
        tokio::spawn(async move {
            let mut watchdog = crate::watchdog::Watchdog::new(
                watchdog_state.database.clone(),
                watchdog_state.active_runs.clone(),
                watchdog_config,
            );
            watchdog.start().await;
        });

        // If --backup-directory was supplied, spawn a periodic task
        // that drains the local backup tree into the main artifact
        // manager every 15 minutes.
        if let Some(backup_dir) = self.backup_directory.clone() {
            let artifact_manager = self.state.artifact_manager.clone();
            tokio::spawn(async move {
                let backup_manager =
                    match janitor::artifacts::LocalArtifactManager::new(&backup_dir) {
                        Ok(m) => m,
                        Err(e) => {
                            log::error!(
                                "Failed to open backup artifact directory {:?}: {}",
                                backup_dir,
                                e
                            );
                            return;
                        }
                    };
                let mut ticker = tokio::time::interval(std::time::Duration::from_secs(15 * 60));
                // First tick fires immediately; skip it so we don't
                // race with the rest of startup.
                ticker.tick().await;
                loop {
                    ticker.tick().await;
                    match janitor::artifacts::upload_backup_artifacts(
                        &backup_manager,
                        artifact_manager.as_ref(),
                    )
                    .await
                    {
                        Ok(done) if !done.is_empty() => {
                            log::info!(
                                "Uploaded {} backup artifact sets from {:?}",
                                done.len(),
                                backup_dir
                            );
                        }
                        Ok(_) => {}
                        Err(e) => {
                            log::warn!(
                                "Failed to drain backup artifact directory {:?}: {}",
                                backup_dir,
                                e
                            );
                        }
                    }
                }
            });
        }

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            let mut sigterm = match signal::unix::signal(signal::unix::SignalKind::terminate()) {
                Ok(signal) => signal,
                Err(e) => {
                    log::error!("Failed to install SIGTERM handler: {}", e);
                    return;
                }
            };
            let mut sigint = match signal::unix::signal(signal::unix::SignalKind::interrupt()) {
                Ok(signal) => signal,
                Err(e) => {
                    log::error!("Failed to install SIGINT handler: {}", e);
                    return;
                }
            };

            tokio::select! {
                _ = sigterm.recv() => {
                    log::info!("Received SIGTERM, initiating graceful shutdown");
                }
                _ = sigint.recv() => {
                    log::info!("Received SIGINT, initiating graceful shutdown");
                }
            }

            let _ = shutdown_tx.send(());
        });

        // Run the server
        let server_handle = tokio::spawn(server_factory(self.state.clone()));

        // Wait for shutdown signal or server completion
        tokio::select! {
            result = server_handle => {
                match result {
                    Ok(Ok(())) => {
                        log::info!("Server completed successfully");
                        Ok(())
                    }
                    Ok(Err(e)) => {
                        log::error!("Server error: {}", e);
                        Err(ApplicationError::Runtime(format!("Server error: {}", e)))
                    }
                    Err(e) => {
                        log::error!("Server task failed: {}", e);
                        Err(ApplicationError::Runtime(format!("Server task failed: {}", e)))
                    }
                }
            }
            _ = &mut shutdown_rx => {
                log::info!("Initiating graceful shutdown...");
                self.shutdown().await?;
                Ok(())
            }
        }
    }

    /// Perform graceful shutdown of all systems.
    async fn shutdown(&self) -> Result<(), ApplicationError> {
        log::info!("Starting graceful shutdown sequence...");

        let shutdown_future = async {
            // 1. Stop accepting new work (this would be handled by the HTTP server)
            log::info!("Stopping new work acceptance...");

            // 2. Wait for active runs to complete or timeout
            log::info!("Waiting for active runs to complete...");
            if let Err(e) = self.wait_for_active_runs().await {
                log::warn!("Error waiting for active runs: {}", e);
            }

            // 3. Artifact manager doesn't have flush_all in janitor crate
            log::info!("Artifact flush not needed for janitor artifact manager");

            // 5. Clean up error tracking
            log::info!("Cleaning up error tracking...");
            self.state
                .error_tracker
                .cleanup_old_errors(chrono::Duration::hours(24))
                .await;

            // 6. Close database connections (handled by connection pool drop)
            log::info!("Closing database connections...");

            log::info!("Graceful shutdown completed successfully");
            Ok::<(), ApplicationError>(())
        };

        // Apply timeout to shutdown process
        tokio::time::timeout(self.config.shutdown_timeout(), shutdown_future)
            .await
            .map_err(|_| ApplicationError::ShutdownTimeout)?
    }

    /// Wait for active runs to complete.
    async fn wait_for_active_runs(&self) -> Result<(), ApplicationError> {
        let mut attempts = 0;
        const MAX_ATTEMPTS: u32 = 30; // 30 seconds with 1-second intervals

        while attempts < MAX_ATTEMPTS {
            let active_runs = self.state.active_runs.list().await;
            if active_runs.is_empty() {
                log::info!("All active runs completed");
                return Ok(());
            }
            log::info!(
                "Waiting for {} active runs to complete...",
                active_runs.len()
            );

            tokio::time::sleep(Duration::from_secs(1)).await;
            attempts += 1;
        }

        log::warn!("Timeout waiting for active runs to complete");
        Ok(())
    }

    /// Perform health checks on all systems.
    pub async fn health_check(&self) -> HealthCheckResult {
        let mut result = HealthCheckResult {
            overall_healthy: true,
            checks: Vec::new(),
        };

        // Database health check
        let db_health = match self.state.database.health_check().await {
            Ok(()) => ComponentHealth {
                component: "database".to_string(),
                healthy: true,
                message: "Database connection healthy".to_string(),
            },
            Err(e) => {
                result.overall_healthy = false;
                ComponentHealth {
                    component: "database".to_string(),
                    healthy: false,
                    message: format!("Database error: {}", e),
                }
            }
        };
        result.checks.push(db_health);

        // VCS health check
        let vcs_health = self.state.vcs_manager.health_check().await;
        if !vcs_health.overall_healthy {
            result.overall_healthy = false;
        }
        for (vcs_type, health) in vcs_health.vcs_statuses {
            result.checks.push(ComponentHealth {
                component: format!("vcs_{}", vcs_type),
                healthy: matches!(health, crate::vcs::VcsHealth::Healthy),
                message: match health {
                    crate::vcs::VcsHealth::Healthy => "VCS healthy".to_string(),
                    crate::vcs::VcsHealth::Warning(msg) => format!("VCS warning: {}", msg),
                    crate::vcs::VcsHealth::Error(msg) => format!("VCS error: {}", msg),
                },
            });
        }

        // Log manager health check
        let log_health = match self.state.log_manager.health_check().await {
            Ok(()) => ComponentHealth {
                component: "log_manager".to_string(),
                healthy: true,
                message: "Log manager healthy".to_string(),
            },
            Err(e) => {
                result.overall_healthy = false;
                ComponentHealth {
                    component: "log_manager".to_string(),
                    healthy: false,
                    message: format!("Log manager error: {}", e),
                }
            }
        };
        result.checks.push(log_health);

        // Artifact manager doesn't have health_check in janitor crate
        let artifact_health = ComponentHealth {
            component: "artifact_manager".to_string(),
            healthy: true,
            message: "Artifact manager assumed healthy".to_string(),
        };
        result.checks.push(artifact_health);

        result
    }
}

/// Health check result for the entire application.
#[derive(Debug, Clone)]
pub struct HealthCheckResult {
    /// Whether the application is overall healthy.
    pub overall_healthy: bool,
    /// Individual component health checks.
    pub checks: Vec<ComponentHealth>,
}

/// Health check result for a single component.
#[derive(Debug, Clone)]
pub struct ComponentHealth {
    /// Name of the component.
    pub component: String,
    /// Whether the component is healthy.
    pub healthy: bool,
    /// Human-readable status message.
    pub message: String,
}

/// Application initialization and runtime errors.
#[derive(Debug, thiserror::Error)]
pub enum ApplicationError {
    /// Database-related errors.
    #[error("Database error: {0}")]
    Database(String),

    /// Log management errors.
    #[error("Log management error: {0}")]
    LogManagement(String),

    /// Artifact management errors.
    #[error("Artifact management error: {0}")]
    ArtifactManagement(String),

    /// VCS management errors.
    #[error("VCS management error: {0}")]
    VcsManagement(String),

    /// Configuration errors.
    #[error("Configuration error: {0}")]
    Configuration(String),

    /// Runtime errors.
    #[error("Runtime error: {0}")]
    Runtime(String),

    /// Shutdown timeout error.
    #[error("Shutdown timeout")]
    ShutdownTimeout,
}

/// Initialize global metrics.
pub fn init_metrics() {
    crate::metrics::MetricsCollector::init_system_info();
    log::info!("Metrics system initialized");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_application_config_default() {
        let config = ApplicationConfig::default();
        // Default config may not have database configured
        assert!(config.enable_graceful_shutdown);
        assert_eq!(config.shutdown_timeout, Duration::from_secs(30));
    }

    #[tokio::test]
    async fn test_application_builder() {
        let builder = Application::builder()
            .with_database_url("postgresql://test/janitor".to_string())
            .with_redis_url(Some("redis://localhost:6379".to_string()));

        // We can't actually build without a real database, but we can test the builder pattern
        assert_eq!(
            builder.config.database().unwrap().url,
            "postgresql://test/janitor"
        );
        assert_eq!(
            builder.config.redis().as_ref().map(|r| &r.url),
            Some(&"redis://localhost:6379".to_string())
        );
    }
}
