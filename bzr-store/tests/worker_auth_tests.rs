//! Integration tests for `DatabaseManager::is_worker`.
//!
//! These load the production `schema/state.sql` schema via the
//! `test_with_database! { schema = Core; ... }` fast path so pgcrypto
//! and the exact `worker(name text, password text, link text)` column
//! set are in play.
//! The point is to verify that authentication goes through the same
//! pgcrypto path as `py/janitor/worker_creds.py::is_worker`, which was
//! the divergence that motivated rewriting this module.

mod common;

use common::*;
use janitor::test_with_database;
use janitor_bzr_store::database::DatabaseManager;

test_with_database! {
    schema = Core;
    async fn test_is_worker_accepts_correct_password(test_db: TestDatabase) {
        seed_worker(test_db.pool(), "alice", "s3cret").await;

        let dbm = DatabaseManager::from_pool(test_db.pool().clone());
        let result = dbm.is_worker("alice", "s3cret").await.unwrap();
        assert_eq!(result, Some("alice".to_string()));
    }
}

test_with_database! {
    schema = Core;
    async fn test_is_worker_rejects_wrong_password(test_db: TestDatabase) {
        seed_worker(test_db.pool(), "alice", "s3cret").await;

        let dbm = DatabaseManager::from_pool(test_db.pool().clone());
        let result = dbm.is_worker("alice", "wrong").await.unwrap();
        assert_eq!(result, None);
    }
}

test_with_database! {
    schema = Core;
    async fn test_is_worker_rejects_unknown_user(test_db: TestDatabase) {
        let dbm = DatabaseManager::from_pool(test_db.pool().clone());
        let result = dbm.is_worker("mallory", "anything").await.unwrap();
        assert_eq!(result, None);
    }
}
