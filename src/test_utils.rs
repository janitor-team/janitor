//! Test utilities: throwaway PostgreSQL databases and a `test_with_database!`
//! macro for tests that only run when a Postgres server is reachable.
//!
//! Only compiled with the `testing` feature.

use sqlx::PgPool;
use std::sync::Once;

static INIT: Once = Once::new();

fn init_test_env() {
    INIT.call_once(|| {
        let _ = env_logger::builder()
            .filter_level(log::LevelFilter::Debug)
            .is_test(true)
            .try_init();
    });
}

/// Configuration for [`TestDatabase`].
#[derive(Debug, Clone)]
pub struct TestDatabaseConfig {
    /// Base connection URL. The database name is replaced with a
    /// per-test unique name unless `unique_per_test` is false.
    pub base_url: String,
    /// Whether to create a unique database per test (default: true).
    pub unique_per_test: bool,
    /// Reserved for a future migration runner; currently a no-op.
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

/// A throwaway PostgreSQL database created for a single test. Dropped
/// asynchronously when the value is dropped.
pub struct TestDatabase {
    /// Pool connected to the freshly-created database.
    pub pool: PgPool,
    /// Name of the freshly-created database.
    pub database_name: String,
    admin_pool: PgPool,
    should_cleanup: bool,
}

impl TestDatabase {
    /// Create a new test database using [`TestDatabaseConfig::default`].
    pub async fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Self::with_config(TestDatabaseConfig::default()).await
    }

    /// Create a new test database, returning `Ok(None)` when no
    /// PostgreSQL server is reachable so tests can skip cleanly on
    /// developer machines without a running server.
    pub async fn new_optional() -> Result<Option<Self>, Box<dyn std::error::Error + Send + Sync>> {
        match Self::with_config(TestDatabaseConfig::default()).await {
            Ok(db) => Ok(Some(db)),
            Err(_) => {
                eprintln!(
                    "Warning: Could not connect to test database, skipping database-dependent tests"
                );
                Ok(None)
            }
        }
    }

    /// Create a new test database with the given configuration.
    pub async fn with_config(
        config: TestDatabaseConfig,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        init_test_env();

        let admin_pool = PgPool::connect(&config.base_url).await?;

        let database_name = if config.unique_per_test {
            format!("janitor_test_{}", uuid_hex())
        } else {
            "janitor_test".to_string()
        };

        let create_query = format!("CREATE DATABASE \"{}\"", database_name);
        sqlx::query(sqlx::AssertSqlSafe(&*create_query))
            .execute(&admin_pool)
            .await?;

        let test_db_url = if config.base_url.contains('/') {
            let base = config.base_url.rsplit_once('/').unwrap().0;
            format!("{}/{}", base, database_name)
        } else {
            format!("{}/{}", config.base_url, database_name)
        };

        let pool = PgPool::connect(&test_db_url).await?;

        Ok(Self {
            pool,
            database_name,
            admin_pool,
            should_cleanup: config.unique_per_test,
        })
    }

    /// Get a reference to the connection pool.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        if self.should_cleanup {
            let admin_pool = self.admin_pool.clone();
            let database_name = self.database_name.clone();
            // `Drop` can't be async; spawn onto whatever runtime is
            // driving this thread. If there's no runtime the drop
            // is best-effort and the leaked database will be
            // cleaned up next CI run.
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let drop_query = format!("DROP DATABASE IF EXISTS \"{}\"", database_name);
                    let _ = sqlx::query(sqlx::AssertSqlSafe(&*drop_query))
                        .execute(&admin_pool)
                        .await;
                });
            }
        }
    }
}

fn uuid_hex() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{:x}_{:x}", nanos, counter)
}

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Convenience: define a `#[tokio::test]` that receives a fresh
/// [`TestDatabase`], skipping when Postgres isn't reachable.
///
/// Skips when `SKIP_DATABASE_TESTS` is set in the environment.
#[macro_export]
macro_rules! test_with_database {
    (async fn $name:ident($db:ident: TestDatabase) $body:block) => {
        #[tokio::test]
        #[serial_test::serial]
        async fn $name() {
            #[allow(unused_imports)]
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
