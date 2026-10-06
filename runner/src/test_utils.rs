//! Test utilities for the runner crate.
//!
//! The runner needs a small amount of shared testing infrastructure
//! (an ephemeral test database, mock artifact/log managers, a helper
//! for building a wired-up `AppState`). Frigg exposes these from the
//! parent janitor crate as `janitor::test_utils`; this checkout does
//! not, so the runner keeps its own copy -- narrower in scope than
//! frigg's, containing only what the runner tests actually reach for.

use crate::database::RunnerDatabase;
use crate::{
    auth::{SecurityService, WorkerAuthService},
    error_tracking::ErrorTracker,
    metrics::MetricsCollector,
    upload::UploadProcessor,
    vcs::RunnerVcsManager,
    AppState,
};
use async_trait::async_trait;
use janitor::database::{Database, DatabaseConfig};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// Configuration for `TestDatabase`.
#[derive(Debug, Clone)]
pub struct TestDatabaseConfig {
    /// Base database URL (without database name).
    pub base_url: String,
    /// Create a unique database per test.
    pub unique_per_test: bool,
    /// Run migrations after creating the database.
    pub run_migrations: bool,
}

impl Default for TestDatabaseConfig {
    fn default() -> Self {
        Self {
            base_url: std::env::var("TEST_DATABASE_URL")
                .unwrap_or_else(|_| "postgresql://localhost/postgres".to_string()),
            unique_per_test: true,
            run_migrations: false,
        }
    }
}

/// Ephemeral PostgreSQL database for a single test.
///
/// Dropping the value schedules deletion of the underlying database.
pub struct TestDatabase {
    /// Pool connected to the test database.
    pub pool: PgPool,
    /// Name of the created database.
    pub database_name: String,
    admin_pool: PgPool,
    should_cleanup: bool,
}

impl TestDatabase {
    /// Create a new test database using default configuration.
    pub async fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Self::with_config(TestDatabaseConfig::default()).await
    }

    /// Create a test database, or return `Ok(None)` if none is available.
    ///
    /// Handy for tests that should skip cleanly in CI without a
    /// database instead of failing.
    pub async fn new_optional() -> Result<Option<Self>, Box<dyn std::error::Error + Send + Sync>> {
        match Self::with_config(TestDatabaseConfig::default()).await {
            Ok(db) => Ok(Some(db)),
            Err(e) => {
                eprintln!(
                    "test database unavailable, skipping db-dependent test: {}",
                    e
                );
                Ok(None)
            }
        }
    }

    /// Create a new test database with the given configuration.
    pub async fn with_config(
        config: TestDatabaseConfig,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let admin_pool = PgPool::connect(&config.base_url).await?;

        let database_name = if config.unique_per_test {
            format!("janitor_test_{}", Uuid::new_v4().simple())
        } else {
            "janitor_test".to_string()
        };

        let create_sql = format!("CREATE DATABASE \"{}\"", database_name);
        sqlx::query(sqlx::AssertSqlSafe(&*create_sql))
            .execute(&admin_pool)
            .await?;

        let test_db_url = if config.base_url.contains('/') {
            let base = config.base_url.rsplit_once('/').unwrap().0;
            format!("{}/{}", base, database_name)
        } else {
            format!("{}/{}", config.base_url, database_name)
        };

        let pool = PgPool::connect(&test_db_url).await?;

        if config.run_migrations {
            // TODO: load schema.sql once the runner has a migration entry point.
        }

        Ok(Self {
            pool,
            database_name,
            admin_pool,
            should_cleanup: config.unique_per_test,
        })
    }

    /// Get the connection pool.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Turn this test database into a janitor `Database`.
    pub fn into_janitor_database(self) -> Database {
        let config = DatabaseConfig {
            url: format!("postgresql://localhost/{}", self.database_name),
            max_connections: 5,
            connect_timeout: Duration::from_secs(10),
            idle_timeout: Some(Duration::from_secs(600)),
            max_lifetime: Some(Duration::from_secs(3600)),
        };
        Database::from_pool_and_config(self.pool.clone(), config)
    }

    /// Execute a SQL statement against the test database.
    pub async fn execute(&self, query: &str) -> Result<(), sqlx::Error> {
        sqlx::query(sqlx::AssertSqlSafe(query))
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        if !self.should_cleanup {
            return;
        }
        let admin_pool = self.admin_pool.clone();
        let database_name = self.database_name.clone();
        tokio::spawn(async move {
            let sql = format!("DROP DATABASE IF EXISTS \"{}\"", database_name);
            let _ = sqlx::query(sqlx::AssertSqlSafe(&*sql))
                .execute(&admin_pool)
                .await;
        });
    }
}

/// Mock artifact manager for use in tests.
#[derive(Debug, Clone)]
pub struct MockArtifactManager;

#[async_trait]
impl janitor::artifacts::ArtifactManager for MockArtifactManager {
    async fn store_artifacts(
        &self,
        _run_id: &str,
        _local_path: &std::path::Path,
        _names: Option<&[String]>,
    ) -> Result<(), janitor::artifacts::Error> {
        Ok(())
    }

    async fn get_artifact(
        &self,
        _run_id: &str,
        _filename: &str,
    ) -> Result<Box<dyn std::io::Read + Sync + Send>, janitor::artifacts::Error> {
        Ok(Box::new(std::io::Cursor::new(b"mock artifact data")))
    }

    fn public_artifact_url(&self, run_id: &str, filename: &str) -> url::Url {
        format!("mock://artifacts/{}/{}", run_id, filename)
            .parse()
            .unwrap()
    }

    async fn retrieve_artifacts(
        &self,
        _run_id: &str,
        _local_path: &std::path::Path,
        _filter_fn: Option<&(dyn for<'a> Fn(&'a str) -> bool + Sync + Send)>,
    ) -> Result<(), janitor::artifacts::Error> {
        Ok(())
    }

    async fn iter_ids(&self) -> Box<dyn Iterator<Item = String> + Send> {
        Box::new(Vec::<String>::new().into_iter())
    }

    async fn delete_artifacts(&self, _run_id: &str) -> Result<(), janitor::artifacts::Error> {
        Ok(())
    }
}

/// Mock log manager for use in tests.
#[derive(Debug, Clone)]
pub struct MockLogFileManager;

#[async_trait]
impl crate::logs::LogFileManager for MockLogFileManager {
    async fn has_log(
        &self,
        _codebase: &str,
        _run_id: &str,
        _name: &str,
    ) -> Result<bool, crate::logs::Error> {
        Ok(true)
    }

    async fn get_log(
        &self,
        _codebase: &str,
        _run_id: &str,
        _name: &str,
    ) -> Result<Box<dyn std::io::Read + Send + Sync>, crate::logs::Error> {
        Ok(Box::new(std::io::Cursor::new(b"mock log content")))
    }

    async fn import_log(
        &self,
        _codebase: &str,
        _run_id: &str,
        _orig_path: &str,
        _mtime: Option<chrono::DateTime<chrono::Utc>>,
        _basename: Option<&str>,
    ) -> Result<(), crate::logs::Error> {
        Ok(())
    }

    async fn delete_log(
        &self,
        _codebase: &str,
        _run_id: &str,
        _name: &str,
    ) -> Result<(), crate::logs::Error> {
        Ok(())
    }

    async fn iter_logs(&self) -> Box<dyn Iterator<Item = (String, String, Vec<String>)>> {
        Box::new(Vec::new().into_iter())
    }

    async fn get_ctime(
        &self,
        _codebase: &str,
        _run_id: &str,
        _name: &str,
    ) -> Result<chrono::DateTime<chrono::Utc>, crate::logs::Error> {
        Ok(chrono::Utc::now())
    }

    async fn health_check(&self) -> Result<(), crate::logs::Error> {
        Ok(())
    }
}

/// Build a test-flavoured `janitor::config::Config` with sensible
/// temp-directory defaults.
#[derive(Default)]
pub struct TestConfigBuilder {
    campaigns: Vec<janitor::config::Campaign>,
}

impl TestConfigBuilder {
    /// Create a builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a campaign with the given name and command so tests
    /// can hit the `POST /candidates` success path (which validates
    /// candidates against `config.campaign`).
    pub fn with_campaign(mut self, name: &str, command: &str) -> Self {
        let mut campaign = janitor::config::Campaign::default();
        campaign.name = Some(name.to_string());
        campaign.command = Some(command.to_string());
        self.campaigns.push(campaign);
        self
    }

    /// Register an arbitrary campaign.
    pub fn with_campaign_config(mut self, campaign: janitor::config::Campaign) -> Self {
        self.campaigns.push(campaign);
        self
    }

    /// Build a janitor `Config` for tests.
    pub fn build_janitor_config(self) -> janitor::config::Config {
        let mut config = janitor::config::Config::new();
        config.database_location = Some(
            std::env::var("TEST_DATABASE_URL")
                .unwrap_or_else(|_| "postgresql://localhost/janitor_test".to_string()),
        );
        config.logs_location = Some(
            std::env::temp_dir()
                .join("janitor_test_logs")
                .to_string_lossy()
                .to_string(),
        );
        config.artifact_location = Some(
            std::env::temp_dir()
                .join("janitor_test_artifacts")
                .to_string_lossy()
                .to_string(),
        );
        config.committer = Some("Test Runner <test@example.com>".to_string());
        config.campaign = self.campaigns;
        config
    }
}

/// Helper macro for database-dependent tests: builds a
/// `TestDatabase`, hands it to the test body, and skips cleanly if no
/// database is reachable.
#[macro_export]
macro_rules! test_with_database {
    (async fn $name:ident($db:ident: TestDatabase) $body:block) => {
        #[tokio::test]
        #[serial_test::serial]
        async fn $name() {
            use $crate::test_utils::{TestDatabase, TestDatabaseConfig};
            if std::env::var("SKIP_DATABASE_TESTS").is_ok() {
                eprintln!("SKIP_DATABASE_TESTS set, skipping {}", stringify!($name));
                return;
            }
            let config = TestDatabaseConfig {
                run_migrations: true,
                ..Default::default()
            };
            match TestDatabase::with_config(config).await {
                Ok($db) => $body,
                Err(e) => {
                    eprintln!("skipping {}: {}", stringify!($name), e);
                }
            }
        }
    };
}

/// Create a wired-up test `AppState`.
///
/// Errors if no database or Redis is available; use
/// `create_test_app_state_if_available` for the skip-on-missing form.
pub async fn create_test_app_state(
) -> Result<Arc<AppState>, Box<dyn std::error::Error + Send + Sync>> {
    create_test_app_state_with_config(TestConfigBuilder::new()).await
}

/// Like [`create_test_app_state`] but with a caller-provided
/// [`TestConfigBuilder`], so tests can register campaigns and other
/// config knobs the handlers read.
pub async fn create_test_app_state_with_config(
    config_builder: TestConfigBuilder,
) -> Result<Arc<AppState>, Box<dyn std::error::Error + Send + Sync>> {
    let test_db = TestDatabase::new().await?;
    // Load the production schema so write-path tests hit the same
    // tables (with the same constraints and types) as production.
    janitor::schema::setup_test_database(test_db.pool()).await?;
    let janitor_db = test_db.into_janitor_database();
    let runner_db = RunnerDatabase::from_database(janitor_db);
    let runner_db_arc = Arc::new(runner_db);

    let redis_url =
        std::env::var("TEST_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    let redis_client = redis::Client::open(redis_url.clone())
        .map_err(|e| format!("Failed to open test redis at {}: {}", redis_url, e))?;
    let mut conn = redis_client
        .get_multiplexed_async_connection()
        .await
        .map_err(|e| format!("Test redis at {} not reachable: {}", redis_url, e))?;
    let _: String = redis::cmd("PING")
        .query_async(&mut conn)
        .await
        .map_err(|e| format!("Test redis PING failed: {}", e))?;

    let log_manager = Arc::new(MockLogFileManager);
    let artifact_manager = Arc::new(MockArtifactManager);
    let config = Arc::new(config_builder.build_janitor_config());

    let vcs_manager = Arc::new(RunnerVcsManager::new(std::collections::HashMap::new()));
    let error_tracker = Arc::new(ErrorTracker::new(
        crate::error_tracking::ErrorTrackingConfig::default(),
    ));
    let metrics = Arc::new(MetricsCollector);

    let temp_dir = std::env::temp_dir().join("janitor_test_uploads");
    let upload_processor = Arc::new(UploadProcessor::new(
        temp_dir,
        1024 * 1024,
        10 * 1024 * 1024,
    ));

    let auth_service = Arc::new(WorkerAuthService::new(runner_db_arc.clone()));

    let active_runs = crate::active_runs::ActiveRunStore::with_key(
        redis_client,
        format!("runner:active-runs:test:{}", uuid::Uuid::new_v4().simple()),
    );

    let security_service = Arc::new(SecurityService::new(
        crate::auth::SecurityConfig::default(),
        runner_db_arc.clone(),
        active_runs.clone(),
    ));
    let resume_service = Arc::new(crate::resume::ResumeService::new((*runner_db_arc).clone()));

    let health_checker = Arc::new(crate::HealthChecker::new(
        runner_db_arc.clone(),
        vcs_manager.clone(),
        log_manager.clone(),
        artifact_manager.clone(),
    ));

    let public_vcs_managers =
        janitor::vcs::get_vcs_managers("http://localhost:9923/").expect("valid test VCS location");

    Ok(Arc::new(AppState {
        database: runner_db_arc,
        active_runs,
        vcs_manager,
        log_manager,
        artifact_manager,
        error_tracker,
        metrics,
        config,
        upload_processor,
        auth_service,
        security_service,
        resume_service,
        health_checker,
        public_apt_archive_location: None,
        public_vcs_managers: Arc::new(public_vcs_managers),
        public_dep_server_url: None,
        avoid_hosts: Vec::new(),
    }))
}

/// Build a test `AppState`, returning `Ok(None)` when the underlying
/// resources (database, redis) are unavailable.
pub async fn create_test_app_state_if_available(
) -> Result<Option<Arc<AppState>>, Box<dyn std::error::Error + Send + Sync>> {
    create_test_app_state_with_config_if_available(TestConfigBuilder::new()).await
}

/// Like [`create_test_app_state_if_available`] but with a
/// caller-provided [`TestConfigBuilder`].
pub async fn create_test_app_state_with_config_if_available(
    config_builder: TestConfigBuilder,
) -> Result<Option<Arc<AppState>>, Box<dyn std::error::Error + Send + Sync>> {
    match create_test_app_state_with_config(config_builder).await {
        Ok(state) => Ok(Some(state)),
        Err(e) => {
            eprintln!("skipping test: could not create app state: {}", e);
            Ok(None)
        }
    }
}

/// Build a test router around a real `AppState`.
pub async fn create_test_app() -> Result<axum::Router, Box<dyn std::error::Error + Send + Sync>> {
    let state = create_test_app_state().await?;
    Ok(crate::web::app(state))
}

/// Build a test router, returning `Ok(None)` when app state is unavailable.
pub async fn create_test_app_if_available(
) -> Result<Option<axum::Router>, Box<dyn std::error::Error + Send + Sync>> {
    match create_test_app_state_if_available().await? {
        Some(state) => Ok(Some(crate::web::app(state))),
        None => Ok(None),
    }
}

/// Router + `AppState` pair for tests that need to seed the database
/// through the same pool the handlers use. Returns `Ok(None)` when
/// no test resources are available.
pub async fn create_test_app_with_state_if_available(
) -> Result<Option<(axum::Router, Arc<AppState>)>, Box<dyn std::error::Error + Send + Sync>> {
    create_test_app_with_state_with_config_if_available(TestConfigBuilder::new()).await
}

/// Like [`create_test_app_with_state_if_available`] but with a
/// caller-provided [`TestConfigBuilder`], so tests can register
/// campaigns and other config knobs the handlers read.
pub async fn create_test_app_with_state_with_config_if_available(
    config_builder: TestConfigBuilder,
) -> Result<Option<(axum::Router, Arc<AppState>)>, Box<dyn std::error::Error + Send + Sync>> {
    ensure_redis().await;
    match create_test_app_state_with_config_if_available(config_builder).await? {
        Some(state) => {
            let router = crate::web::app(state.clone());
            Ok(Some((router, state)))
        }
        None => Ok(None),
    }
}

/// Public router + `AppState` pair, mirroring
/// [`create_test_app_with_state_if_available`] for the public
/// (worker-facing) side of the runner.
pub async fn create_public_test_app_with_state_if_available(
) -> Result<Option<(axum::Router, Arc<AppState>)>, Box<dyn std::error::Error + Send + Sync>> {
    ensure_redis().await;
    match create_test_app_state_if_available().await? {
        Some(state) => {
            let router = crate::web::public_app(state.clone()).with_state(state.clone());
            Ok(Some((router, state)))
        }
        None => Ok(None),
    }
}

#[cfg(feature = "testing")]
mod redis_container {
    use std::sync::OnceLock;
    use testcontainers::{runners::AsyncRunner, ContainerAsync};
    use testcontainers_modules::redis::Redis;
    use tokio::sync::Mutex;

    /// A running Redis container that survives for the process's
    /// lifetime. Held in a `Mutex<Option<...>>` so all tests share the
    /// same container and its port stays valid until the process
    /// exits.
    static REDIS_CONTAINER: OnceLock<Mutex<Option<ContainerAsync<Redis>>>> = OnceLock::new();

    /// Ensure a Redis container is running for this test process, and
    /// set `TEST_REDIS_URL` to its address.
    ///
    /// Idempotent: only the first call spins up a container.
    /// Respects `TEST_REDIS_URL` if the environment already sets it.
    /// If Docker is unavailable, returns quietly so `create_test_app`
    /// can skip cleanly -- unless `TEST_REQUIRE_REDIS=1` is set, in
    /// which case a failed container start panics so CI signals loud.
    pub async fn ensure_redis() {
        if std::env::var_os("TEST_REDIS_URL").is_some() {
            return;
        }

        let cell = REDIS_CONTAINER.get_or_init(|| Mutex::new(None));
        let mut guard = cell.lock().await;
        if guard.is_some() {
            return;
        }

        let container = match Redis::default().start().await {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("could not start Redis test container: {e}");
                if std::env::var_os("TEST_REQUIRE_REDIS").is_some() {
                    panic!("{msg}");
                }
                eprintln!("{msg}");
                return;
            }
        };
        let port = match container.get_host_port_ipv4(6379).await {
            Ok(p) => p,
            Err(e) => {
                let msg = format!("could not read Redis test container port: {e}");
                if std::env::var_os("TEST_REQUIRE_REDIS").is_some() {
                    panic!("{msg}");
                }
                eprintln!("{msg}");
                return;
            }
        };
        // env::set_var is safe on Rust 2021 (edition currently used
        // by this workspace) but becomes an unsafe op under 2024's
        // stricter concurrency rules. Wrap early so the crate keeps
        // compiling when the workspace upgrades.
        #[allow(unused_unsafe)]
        unsafe {
            std::env::set_var("TEST_REDIS_URL", format!("redis://127.0.0.1:{port}"));
        }
        *guard = Some(container);
    }
}

/// Ensure a Redis container is running for this test process, and set
/// `TEST_REDIS_URL` to its address. See [`redis_container::ensure_redis`].
///
/// This is the "testing" feature entry point; without the feature it
/// is a no-op so callers can be written uniformly.
#[cfg(feature = "testing")]
pub async fn ensure_redis() {
    redis_container::ensure_redis().await
}

/// No-op fallback for when the `testing` feature is not enabled.
#[cfg(not(feature = "testing"))]
pub async fn ensure_redis() {}
