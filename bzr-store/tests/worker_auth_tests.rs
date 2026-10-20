//! Integration tests for `janitor::web::authenticated_worker`.
//!
//! These load the production `schema/state.sql` schema via
//! `janitor::schema::setup_test_database` so pgcrypto and the exact
//! `worker(name text, password text, link text)` column set are in play.
//! The point is to verify that authentication goes through the same
//! pgcrypto path as `py/janitor/worker_creds.py::is_worker`, which was
//! the divergence that motivated rewriting this module.

mod common;

use axum::http::{header, HeaderMap};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use common::*;
use janitor::{schema::setup_test_database, test_with_database};
use sqlx::PgPool;

async fn is_worker(pool: &PgPool, username: &str, password: &str) -> Option<String> {
    let mut headers = HeaderMap::new();
    let credentials = BASE64.encode(format!("{}:{}", username, password));
    headers.insert(
        header::AUTHORIZATION,
        format!("Basic {}", credentials).parse().unwrap(),
    );
    janitor::web::authenticated_worker(pool, &headers)
        .await
        .unwrap()
}

test_with_database! {
    async fn test_is_worker_accepts_correct_password(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_worker(test_db.pool(), "alice", "s3cret").await;

        let result = is_worker(test_db.pool(), "alice", "s3cret").await;
        assert_eq!(result, Some("alice".to_string()));
    }
}

test_with_database! {
    async fn test_is_worker_rejects_wrong_password(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_worker(test_db.pool(), "alice", "s3cret").await;

        let result = is_worker(test_db.pool(), "alice", "wrong").await;
        assert_eq!(result, None);
    }
}

test_with_database! {
    async fn test_is_worker_rejects_unknown_user(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();

        let result = is_worker(test_db.pool(), "mallory", "anything").await;
        assert_eq!(result, None);
    }
}
