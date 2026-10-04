//! Integration tests for `DatabaseManager::is_worker`.
//!
//! These load the production `schema/state.sql` schema via
//! `janitor::schema::setup_test_database` so pgcrypto and the exact
//! `worker(name text, password text, link text)` column set are in play.
//! The point is to verify that authentication goes through the same
//! pgcrypto path as `py/janitor/worker_creds.py::is_worker`, which was
//! the divergence that motivated rewriting this module.

use janitor::{schema::setup_test_database, test_with_database};
use janitor_bzr_store::database::DatabaseManager;
use sqlx::PgPool;

/// Insert a worker row with a pgcrypto-hashed password, matching the way
/// Python/pgcrypto-based deployments store credentials.
async fn seed_worker(pool: &PgPool, name: &str, password: &str) {
    sqlx::query(
        "INSERT INTO worker (name, password, link)
         VALUES ($1, crypt($2, gen_salt('bf')), NULL)",
    )
    .bind(name)
    .bind(password)
    .execute(pool)
    .await
    .unwrap();
}

test_with_database! {
    async fn test_is_worker_accepts_correct_password(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_worker(test_db.pool(), "alice", "s3cret").await;

        let dbm = DatabaseManager::from_pool(test_db.pool().clone());
        let result = dbm.is_worker("alice", "s3cret").await.unwrap();
        assert_eq!(result, Some("alice".to_string()));
    }
}

test_with_database! {
    async fn test_is_worker_rejects_wrong_password(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_worker(test_db.pool(), "alice", "s3cret").await;

        let dbm = DatabaseManager::from_pool(test_db.pool().clone());
        let result = dbm.is_worker("alice", "wrong").await.unwrap();
        assert_eq!(result, None);
    }
}

test_with_database! {
    async fn test_is_worker_rejects_unknown_user(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();

        let dbm = DatabaseManager::from_pool(test_db.pool().clone());
        let result = dbm.is_worker("mallory", "anything").await.unwrap();
        assert_eq!(result, None);
    }
}
