//! Test utilities: throwaway PostgreSQL databases and a `test_with_database!`
//! macro for tests that only run when a Postgres server is reachable.
//!
//! Only compiled with the `testing` feature.

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;
use std::sync::Once;
use tokio::sync::OnceCell;

static INIT: Once = Once::new();

fn init_test_env() {
    INIT.call_once(|| {
        let _ = env_logger::builder()
            .filter_level(log::LevelFilter::Debug)
            .is_test(true)
            .try_init();
    });
}

/// Which schema a test database should start with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestSchema {
    /// Empty database; the caller runs any schema setup it needs.
    Empty,
    /// Core janitor schema (`schema/state.sql`).
    Core,
    /// Core plus Debian extensions (`schema/debian/debian.sql`).
    Debian,
}

/// Configuration for [`TestDatabase`].
#[derive(Debug, Clone)]
pub struct TestDatabaseConfig {
    /// Base connection URL. The database name is replaced with a
    /// per-test unique name unless `unique_per_test` is false.
    pub base_url: String,
    /// Whether to create a unique database per test (default: true).
    pub unique_per_test: bool,
    /// Schema to preload into the new database. [`TestSchema::Core`] and
    /// [`TestSchema::Debian`] use a cached template database on the
    /// server so subsequent tests pay only a `CREATE DATABASE ...
    /// TEMPLATE` round-trip rather than re-executing the schema SQL.
    pub schema: TestSchema,
}

impl Default for TestDatabaseConfig {
    fn default() -> Self {
        Self {
            base_url: std::env::var("TEST_DATABASE_URL")
                .unwrap_or_else(|_| "postgresql://localhost/postgres".to_string()),
            unique_per_test: true,
            schema: TestSchema::Empty,
        }
    }
}

/// A throwaway PostgreSQL database created for a single test. Dropped
/// when the value is dropped.
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
        Self::new_optional_with_schema(TestSchema::Empty).await
    }

    /// Like [`Self::new_optional`] but preloads `schema` via the shared
    /// template database.
    pub async fn new_optional_with_schema(
        schema: TestSchema,
    ) -> Result<Option<Self>, Box<dyn std::error::Error + Send + Sync>> {
        let config = TestDatabaseConfig {
            schema,
            ..Default::default()
        };
        match Self::with_config(config).await {
            Ok(db) => Ok(Some(db)),
            Err(e) => {
                eprintln!("Warning: test database unavailable, skipping: {}", e);
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

        let template = match config.schema {
            TestSchema::Empty => None,
            TestSchema::Core => {
                Some(ensure_template(&admin_pool, &CORE_TEMPLATE, CORE_TEMPLATE_NAME, false).await?)
            }
            TestSchema::Debian => Some(
                ensure_template(&admin_pool, &DEBIAN_TEMPLATE, DEBIAN_TEMPLATE_NAME, true).await?,
            ),
        };

        let database_name = if config.unique_per_test {
            format!("janitor_test_{}", uuid_hex())
        } else {
            "janitor_test".to_string()
        };

        let create_query = match template.as_deref() {
            Some(t) => format!("CREATE DATABASE \"{}\" TEMPLATE \"{}\"", database_name, t),
            None => format!("CREATE DATABASE \"{}\"", database_name),
        };
        sqlx::query(sqlx::AssertSqlSafe(create_query.as_str()))
            .execute(&admin_pool)
            .await?;

        let test_db_url = replace_db(&config.base_url, &database_name);
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

    /// Hand the database over to a pool of its own, which drops the
    /// database once that pool and all its clones are gone. Use when the
    /// pool must outlive the [`TestDatabase`] guard, for example when
    /// handing it off to a long-lived `AppState`.
    pub fn into_pool(mut self) -> PgPool {
        self.should_cleanup = false;
        let guard = DropDatabaseGuard {
            admin: (*self.admin_pool.connect_options()).clone(),
            name: self.database_name.clone(),
        };
        PgPoolOptions::new()
            // The hook owns the guard, so the guard lives as long as the pool.
            .after_connect(move |_, _| {
                let _guard = &guard;
                Box::pin(async { Ok(()) })
            })
            .connect_lazy_with((*self.pool.connect_options()).clone())
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        if self.should_cleanup {
            drop_test_database(&self.admin_pool.connect_options(), &self.database_name);
        }
    }
}

struct DropDatabaseGuard {
    admin: PgConnectOptions,
    name: String,
}

impl Drop for DropDatabaseGuard {
    fn drop(&mut self) {
        drop_test_database(&self.admin, &self.name);
    }
}

/// Drop the test database `name`, even while connections to it are open.
pub fn drop_test_database(admin: &PgConnectOptions, name: &str) {
    use sqlx::Connection;
    let options = admin.clone();
    let drop_query = format!("DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)", name);
    // `Drop` can't be async, so drop the database on a thread of our own.
    let dropped = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build a runtime to drop the test database");
        runtime.block_on(async {
            let mut conn = sqlx::PgConnection::connect_with(&options)
                .await
                .expect("connect to drop the test database");
            sqlx::query(sqlx::AssertSqlSafe(drop_query.as_str()))
                .execute(&mut conn)
                .await
                .expect("drop the test database");
        })
    })
    .join();
    if dropped.is_err() && !std::thread::panicking() {
        panic!("could not drop test database {}", name);
    }
}

/// Replace the database component of `base_url` with `database`.
fn replace_db(base_url: &str, database: &str) -> String {
    if base_url.contains('/') {
        let base = base_url.rsplit_once('/').unwrap().0;
        format!("{}/{}", base, database)
    } else {
        format!("{}/{}", base_url, database)
    }
}

static CORE_TEMPLATE: OnceCell<String> = OnceCell::const_new();
static DEBIAN_TEMPLATE: OnceCell<String> = OnceCell::const_new();

const CORE_TEMPLATE_NAME: &str = "janitor_test_template_core";
const DEBIAN_TEMPLATE_NAME: &str = "janitor_test_template_debian";

/// Ensure a template database exists on the server, loading it once
/// per process. Returns the template database name.
async fn ensure_template(
    admin_pool: &PgPool,
    cell: &'static OnceCell<String>,
    name: &'static str,
    with_debian: bool,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    cell.get_or_try_init(|| async move {
        create_template(admin_pool, name, with_debian).await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(name.to_string())
    })
    .await
    .cloned()
}

async fn create_template(
    admin_pool: &PgPool,
    name: &str,
    with_debian: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use sqlx::Executor;

    // Drop any leftover from a previous crashed run so we always start
    // from a known-good schema.
    let drop_query = format!("DROP DATABASE IF EXISTS \"{}\"", name);
    sqlx::query(sqlx::AssertSqlSafe(drop_query.as_str()))
        .execute(admin_pool)
        .await?;

    let create_query = format!("CREATE DATABASE \"{}\"", name);
    sqlx::query(sqlx::AssertSqlSafe(create_query.as_str()))
        .execute(admin_pool)
        .await?;

    let template_opts = (*admin_pool.connect_options()).clone().database(name);
    let template_pool = PgPool::connect_with(template_opts).await?;

    template_pool
        .execute(sqlx::raw_sql(crate::schema::CORE_SCHEMA))
        .await?;
    if with_debian {
        template_pool
            .execute(sqlx::raw_sql(crate::schema::DEBIAN_SCHEMA))
            .await?;
    }

    // Postgres refuses to use a template that has open connections, so
    // close ours before any test tries to clone it.
    template_pool.close().await;

    Ok(())
}

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn uuid_hex() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{:x}_{:x}", nanos, counter)
}

/// Convenience: define a `#[tokio::test]` that receives a fresh
/// [`TestDatabase`], skipping when Postgres isn't reachable.
///
/// Skips when `SKIP_DATABASE_TESTS` is set in the environment.
///
/// The second form takes a [`TestSchema`] variant to preload a schema
/// via the shared template database, so repeated tests avoid re-running
/// the full schema SQL:
///
/// ```ignore
/// test_with_database! {
///     schema = Core;
///     async fn my_test(test_db: TestDatabase) { ... }
/// }
/// ```
#[macro_export]
macro_rules! test_with_database {
    (async fn $name:ident($db:ident: TestDatabase) $body:block) => {
        $crate::test_with_database! {
            schema = Empty;
            async fn $name($db: TestDatabase) $body
        }
    };
    (schema = $schema:ident; async fn $name:ident($db:ident: TestDatabase) $body:block) => {
        #[tokio::test]
        #[serial_test::serial]
        async fn $name() {
            #[allow(unused_imports)]
            use $crate::test_utils::{TestDatabase, TestDatabaseConfig, TestSchema};
            if std::env::var("SKIP_DATABASE_TESTS").is_ok() {
                eprintln!("SKIP_DATABASE_TESTS set, skipping {}", stringify!($name));
                return;
            }
            let config = TestDatabaseConfig {
                schema: TestSchema::$schema,
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn table_count(pool: &PgPool) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM information_schema.tables \
             WHERE table_schema = 'public'",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn test_core_schema_via_template() {
        let config = TestDatabaseConfig {
            schema: TestSchema::Core,
            ..Default::default()
        };
        let Ok(db) = TestDatabase::with_config(config).await else {
            return;
        };
        assert!(table_count(db.pool()).await > 0);
        let has_debian_build: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM information_schema.tables \
             WHERE table_schema = 'public' AND table_name = 'debian_build'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(has_debian_build, 0);
    }

    #[tokio::test]
    async fn test_debian_schema_via_template() {
        let config = TestDatabaseConfig {
            schema: TestSchema::Debian,
            ..Default::default()
        };
        let Ok(db) = TestDatabase::with_config(config).await else {
            return;
        };
        let has_debian_build: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM information_schema.tables \
             WHERE table_schema = 'public' AND table_name = 'debian_build'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(has_debian_build, 1);
    }

    #[tokio::test]
    async fn test_template_cached_across_calls() {
        let config = TestDatabaseConfig {
            schema: TestSchema::Core,
            ..Default::default()
        };
        for _ in 0..3 {
            let Ok(db) = TestDatabase::with_config(config.clone()).await else {
                return;
            };
            assert!(table_count(db.pool()).await > 0);
        }
    }
}
