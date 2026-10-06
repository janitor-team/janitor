//! Application initialization and orchestration for the runner.

use crate::{
    database::RunnerDatabase,
    error_tracking::{ErrorSeverity, ErrorTracker, ErrorTrackingConfig},
    metrics::MetricsCollector,
    vcs::RunnerVcsManager,
    AppState,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// Application builder for configuring and initializing the runner.
pub struct ApplicationBuilder {
    config: janitor::config::Config,
    debug: bool,
    gcp_logging: bool,
    run_timeout_minutes: u64,
    avoid_hosts: Vec<String>,
    public_vcs_location: Option<String>,
    backup_directory: Option<PathBuf>,
    public_apt_archive_location: Option<String>,
    public_dep_server_url: Option<String>,
}

impl ApplicationBuilder {
    /// Builder seeded with the janitor configuration.
    pub fn new(config: janitor::config::Config) -> Self {
        Self {
            config,
            debug: false,
            gcp_logging: false,
            run_timeout_minutes: 60,
            avoid_hosts: Vec::new(),
            public_vcs_location: None,
            backup_directory: None,
            public_apt_archive_location: None,
            public_dep_server_url: None,
        }
    }

    /// Load the janitor configuration file and use it to seed the builder.
    pub fn from_file<P: AsRef<std::path::Path>>(path: P) -> Result<Self, ApplicationError> {
        let config = janitor::config::read_file(path.as_ref()).map_err(|e| {
            ApplicationError::Configuration(format!(
                "Failed to load config from {}: {}",
                path.as_ref().display(),
                e
            ))
        })?;
        Ok(Self::new(config))
    }

    /// Set the backup directory. Its `logs` and `artifacts`
    /// subdirectories are used when the main log or artifact manager
    /// fails, and the artifacts are uploaded to the main artifact
    /// manager at startup.
    pub fn with_backup_directory(mut self, path: Option<PathBuf>) -> Self {
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

    /// Set the dependency server URL handed to workers. Corresponds to
    /// the `--public-dep-server-url` CLI flag.
    pub fn with_public_dep_server_url(mut self, url: Option<String>) -> Self {
        self.public_dep_server_url = url;
        self
    }

    /// Set the public VCS location used for URLs handed to workers:
    /// a base URL serving `git/` and `bzr/`, or `git=URL,bzr=URL`.
    /// Corresponds to the required `--public-vcs-location` CLI flag.
    pub fn with_public_vcs_location(mut self, location: String) -> Self {
        self.public_vcs_location = Some(location);
        self
    }

    /// Set the watchdog run timeout (in minutes). Corresponds to the
    /// `--run-timeout` CLI flag.
    pub fn with_run_timeout_minutes(mut self, minutes: u64) -> Self {
        self.run_timeout_minutes = minutes;
        self
    }

    /// Set the hosts to avoid when assigning work. Corresponds to the
    /// `--avoid-host` CLI flag (repeatable).
    pub fn with_avoid_hosts(mut self, hosts: Vec<String>) -> Self {
        self.avoid_hosts = hosts;
        self
    }

    /// Enable debug logging.
    pub fn with_debug(mut self, debug: bool) -> Self {
        self.debug = debug;
        self
    }

    /// Log to Google Cloud Logging. Corresponds to the
    /// `--gcp-logging` CLI flag.
    pub fn with_gcp_logging(mut self, gcp_logging: bool) -> Self {
        self.gcp_logging = gcp_logging;
        self
    }

    fn validate(&self) -> Result<(), ApplicationError> {
        // Both stores are hard requirements: the DB backs every
        // handler, and the ActiveRunStore is Redis-backed so worker
        // /finish uploads survive runner restarts.
        if self.config.database_location.is_none() {
            return Err(ApplicationError::Configuration(
                "database_location must be set".to_string(),
            ));
        }
        if self.config.redis_location.is_none() {
            return Err(ApplicationError::Configuration(
                "redis_location must be set".to_string(),
            ));
        }
        if self.run_timeout_minutes == 0 {
            return Err(ApplicationError::Configuration(
                "run timeout must be greater than 0".to_string(),
            ));
        }
        Ok(())
    }

    fn tracing_config(&self) -> crate::tracing::TracingConfig {
        crate::tracing::TracingConfig {
            log_level: if self.debug { "debug" } else { "info" }.to_string(),
            ..Default::default()
        }
    }

    fn error_tracking_config() -> ErrorTrackingConfig {
        ErrorTrackingConfig {
            max_errors_in_memory: 1000,
            log_to_file: false,
            error_log_path: None,
            enable_stack_traces: true,
            min_severity: ErrorSeverity::Warning,
            enable_correlation: true,
        }
    }

    /// Build and initialize the application.
    pub async fn build(self) -> Result<Application, ApplicationError> {
        // Initialize tracing and logging first. Like Python, spans are
        // only exported when zipkin_address is set.
        let zipkin_address = self.config.zipkin_address.as_deref();
        let tracer_guard = if self.gcp_logging {
            janitor::logging::init_logging(true, self.debug);
            zipkin_address
                .map(crate::tracing::init_span_export)
                .transpose()
        } else {
            crate::tracing::init_tracing(&self.tracing_config(), zipkin_address)
        }
        .map_err(|e| {
            ApplicationError::Configuration(format!("Failed to initialize tracing: {}", e))
        })?;

        log::info!("Initializing Janitor Runner application...");

        self.validate()?;

        let public_vcs_location = self.public_vcs_location.as_deref().ok_or_else(|| {
            ApplicationError::Configuration("public VCS location must be set".to_string())
        })?;
        let public_vcs_managers =
            janitor::vcs::get_vcs_managers(public_vcs_location).map_err(|e| {
                ApplicationError::Configuration(format!(
                    "Invalid public VCS location {}: {}",
                    public_vcs_location, e
                ))
            })?;

        // Initialize metrics first so other systems can use them
        log::info!("Initializing metrics collection...");
        let metrics = Arc::new(MetricsCollector {});
        crate::metrics::init_metrics();

        // Initialize error tracking
        log::info!("Initializing error tracking...");
        let error_tracking_config = Self::error_tracking_config();
        let error_tracker = Arc::new(ErrorTracker::new(error_tracking_config));

        log::info!("Initializing database connection...");
        let janitor_config = self.config;
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
                janitor_config.redis_location.clone(),
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

        let (backup_log_manager, backup_artifact_manager) = match &self.backup_directory {
            Some(dir) => {
                let (logs, artifacts) = open_backup_managers(dir)?;
                (Some(logs), Some(artifacts))
            }
            None => (None, None),
        };

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
                "Redis is required for active-runs persistence; configure redis_location"
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
            backup_log_manager,
            backup_artifact_manager,
            error_tracker,
            metrics,
            config: Arc::new(janitor_config),
            upload_processor,
            auth_service,
            security_service,
            resume_service,
            health_checker,
            public_apt_archive_location: self.public_apt_archive_location,
            public_vcs_managers: Arc::new(public_vcs_managers),
            public_dep_server_url: self.public_dep_server_url,
            avoid_hosts: self.avoid_hosts,
        });

        log::info!("Janitor Runner application initialized successfully");

        Ok(Application {
            state: app_state,
            _tracer_guard: tracer_guard,
            run_timeout_minutes: self.run_timeout_minutes,
        })
    }
}

type BackupManagers = (
    Arc<dyn janitor::logs::LogFileManager>,
    Arc<dyn janitor::artifacts::ArtifactManager>,
);

/// Create the `logs` and `artifacts` subdirectories of the
/// `--backup-directory` and open the backup managers on them.
fn open_backup_managers(dir: &std::path::Path) -> Result<BackupManagers, ApplicationError> {
    let logs_dir = dir.join("logs");
    let artifacts_dir = dir.join("artifacts");
    for subdir in [&logs_dir, &artifacts_dir] {
        if !subdir.is_dir() {
            std::fs::create_dir(subdir).map_err(|e| {
                ApplicationError::Configuration(format!(
                    "Could not create backup directory {:?}: {}",
                    subdir, e
                ))
            })?;
        }
    }
    let log_manager = janitor::logs::FileSystemLogFileManager::new(&logs_dir).map_err(|e| {
        ApplicationError::LogManagement(format!(
            "Failed to open backup log directory {:?}: {}",
            logs_dir, e
        ))
    })?;
    let artifact_manager =
        janitor::artifacts::LocalArtifactManager::new(&artifacts_dir).map_err(|e| {
            ApplicationError::ArtifactManagement(format!(
                "Failed to open backup artifact directory {:?}: {}",
                artifacts_dir, e
            ))
        })?;
    Ok((Arc::new(log_manager), Arc::new(artifact_manager)))
}

/// Main application struct that manages the runner lifecycle.
pub struct Application {
    /// Application state.
    pub state: Arc<AppState>,
    /// Keeps span export to zipkin_address running.
    _tracer_guard: Option<janitor::otlp::TracerGuard>,
    /// Watchdog run timeout in minutes.
    run_timeout_minutes: u64,
}

impl Application {
    /// Create a new application builder from configuration.
    pub fn builder(config: janitor::config::Config) -> ApplicationBuilder {
        ApplicationBuilder::new(config)
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
        let run_timeout_minutes = self.run_timeout_minutes;
        tokio::spawn(async move {
            let watchdog = crate::watchdog::Watchdog::new(
                watchdog_state.database.clone(),
                watchdog_state.active_runs.clone(),
                run_timeout_minutes,
            );
            watchdog.start().await;
        });

        // Like Python, upload whatever ended up in the backup artifact
        // directory during a previous run once at startup.
        if let Some(backup_manager) = self.state.backup_artifact_manager.clone() {
            let artifact_manager = self.state.artifact_manager.clone();
            tokio::spawn(async move {
                // TODO: Python applied a 15 minute timeout to each
                // retrieve/store; the Rust artifact managers take no timeout.
                match janitor::artifacts::upload_backup_artifacts(
                    backup_manager.as_ref(),
                    artifact_manager.as_ref(),
                )
                .await
                {
                    Ok(done) => {
                        log::info!("Uploaded {} backup artifact sets", done.len());
                    }
                    Err(e) => {
                        log::error!("Failed to upload backup artifacts: {}", e);
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
        tokio::time::timeout(SHUTDOWN_TIMEOUT, shutdown_future)
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
    fn test_builder_from_textproto_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("janitor.conf");
        std::fs::write(
            &path,
            r#"
database_location: "postgresql://test/janitor"
redis_location: "redis://localhost:6379"
campaign {
  name: "lintian-fixes"
  branch_name: "lintian-fixes"
}
"#,
        )
        .unwrap();

        let builder = ApplicationBuilder::from_file(&path).unwrap();
        assert_eq!(
            builder.config.database_location.as_deref(),
            Some("postgresql://test/janitor")
        );
        assert_eq!(
            builder.config.redis_location.as_deref(),
            Some("redis://localhost:6379")
        );
        assert_eq!(builder.config.campaign.len(), 1);
        builder.validate().unwrap();
    }

    #[test]
    fn test_builder_from_file_rejects_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("janitor.conf");
        std::fs::write(&path, "[database]\nurl = \"postgresql://test/janitor\"\n").unwrap();

        assert!(matches!(
            ApplicationBuilder::from_file(&path),
            Err(ApplicationError::Configuration(_))
        ));
    }

    #[test]
    fn test_validate_requires_database_and_redis() {
        let mut config = janitor::config::Config::new();
        config.redis_location = Some("redis://localhost".to_string());
        assert!(ApplicationBuilder::new(config.clone()).validate().is_err());

        config.database_location = Some("postgresql://localhost/janitor".to_string());
        config.redis_location = None;
        assert!(ApplicationBuilder::new(config.clone()).validate().is_err());

        config.redis_location = Some("redis://localhost".to_string());
        ApplicationBuilder::new(config.clone()).validate().unwrap();

        assert!(ApplicationBuilder::new(config)
            .with_run_timeout_minutes(0)
            .validate()
            .is_err());
    }

    #[test]
    fn test_open_backup_managers_creates_subdirectories() {
        let dir = tempfile::tempdir().unwrap();
        open_backup_managers(dir.path()).unwrap();
        assert!(dir.path().join("logs").is_dir());
        assert!(dir.path().join("artifacts").is_dir());
        // Reusing an existing backup directory works too.
        open_backup_managers(dir.path()).unwrap();
    }

    #[test]
    fn test_open_backup_managers_requires_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(open_backup_managers(&dir.path().join("missing")).is_err());
    }
}
