/// Core janitor database schema
pub const CORE_SCHEMA: &str = include_str!("../schema/state.sql");

/// Debian-specific database schema extensions
pub const DEBIAN_SCHEMA: &str = include_str!("../schema/debian/debian.sql");

#[cfg(feature = "testing")]
/// Set up a test database with core janitor schema.
///
/// Uses the simple-query protocol via `raw_sql` so the multi-statement
/// schema file loads in one round-trip; the prepared-statement path
/// (`sqlx::query`) errors with "cannot insert multiple commands into
/// a prepared statement".
pub async fn setup_test_database(pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
    use sqlx::Executor;
    pool.execute(sqlx::raw_sql(CORE_SCHEMA)).await?;
    Ok(())
}

#[cfg(feature = "testing")]
/// Set up a test database with Debian extensions
pub async fn setup_debian_test_database(pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
    use sqlx::Executor;
    setup_test_database(pool).await?;
    pool.execute(sqlx::raw_sql(DEBIAN_SCHEMA)).await?;
    Ok(())
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    use super::*;
    use crate::test_utils::TestDatabase;

    async fn table_names(pool: &sqlx::PgPool, names: &[&str]) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT table_name::text FROM information_schema.tables \
             WHERE table_schema = 'public' AND table_name = ANY($1) \
             ORDER BY table_name",
        )
        .bind(names)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn test_core_schema_creates_tables() {
        let Some(db) = TestDatabase::new_optional().await.unwrap() else {
            return;
        };
        setup_test_database(db.pool()).await.unwrap();

        let expected = [
            "change_set",
            "codebase",
            "merge_proposal",
            "named_publish_policy",
            "queue",
            "run",
            "worker",
        ];
        assert_eq!(table_names(db.pool(), &expected).await, expected);
        assert_eq!(
            table_names(db.pool(), &["debian_build"]).await,
            Vec::<String>::new()
        );
    }

    #[tokio::test]
    async fn test_debian_schema_creates_debian_build() {
        let Some(db) = TestDatabase::new_optional().await.unwrap() else {
            return;
        };
        setup_debian_test_database(db.pool()).await.unwrap();

        let columns: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT column_name::text, udt_name::text, is_nullable::text \
             FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = 'debian_build' \
             ORDER BY ordinal_position",
        )
        .fetch_all(db.pool())
        .await
        .unwrap();
        let expected = [
            ("run_id", "text", "NO"),
            ("version", "debversion", "NO"),
            ("distribution", "text", "NO"),
            ("source", "text", "NO"),
            ("binary_packages", "_text", "YES"),
            ("changes_filenames", "_text", "YES"),
            ("lintian_result", "json", "YES"),
        ];
        assert_eq!(
            columns,
            expected
                .iter()
                .map(|(n, t, null)| (n.to_string(), t.to_string(), null.to_string()))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn test_debian_build_requires_existing_run() {
        let Some(db) = TestDatabase::new_optional().await.unwrap() else {
            return;
        };
        setup_debian_test_database(db.pool()).await.unwrap();

        let err = sqlx::query(
            "INSERT INTO debian_build (run_id, version, distribution, source) \
             VALUES ('no-such-run', '1.0-1', 'unstable', 'foo')",
        )
        .execute(db.pool())
        .await
        .unwrap_err();
        let db_err = err.as_database_error().unwrap();
        assert_eq!(db_err.code().as_deref(), Some("23503"));
    }
}
