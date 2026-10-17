//! Tests for `refresh_bucket_mp_counts` against the production schema.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use janitor::{
    schema::setup_test_database,
    test_utils::{TestDatabase, TestDatabaseConfig},
    test_with_database,
};
use janitor_publish::rate_limiter::{FixedRateLimiter, RateLimiter};
use janitor_publish::{refresh_bucket_mp_counts, AppState, PublishWorker};
use maplit::hashmap;
use sqlx::PgPool;

async fn make_state(pool: PgPool) -> Arc<AppState> {
    let limiter: Box<dyn RateLimiter> = Box::new(FixedRateLimiter::new(5));
    let publish_worker = PublishWorker::new(
        None,
        None,
        "http://localhost/".parse().unwrap(),
        None,
        None,
        None,
    )
    .await;
    Arc::new(AppState {
        conn: pool,
        bucket_rate_limiter: Arc::new(Mutex::new(limiter)),
        forge_rate_limiter: Arc::new(RwLock::new(HashMap::new())),
        push_limit: None,
        redis: None,
        redis_manager: None,
        config: Box::leak(Box::new(janitor::config::Config::new())),
        publish_worker,
        vcs_managers: Arc::new(HashMap::new()),
        modify_mp_limit: None,
        unexpected_mp_limit: None,
        gpg: Arc::new(breezyshim::gpg::GPGContext::new()),
        require_binary_diff: false,
        health_checker: Arc::new(janitor_publish::health::BasicHealthChecker::with_info(
            "publish".to_string(),
            "0".to_string(),
        )),
        last_full_scan_at: tokio::sync::watch::channel(None).0,
    })
}

async fn seed_merge_proposal(pool: &PgPool, url: &str, status: Option<&str>, bucket: Option<&str>) {
    sqlx::query(
        "INSERT INTO merge_proposal (url, status, rate_limit_bucket)
         VALUES ($1, $2::merge_proposal_status, $3)",
    )
    .bind(url)
    .bind(status)
    .bind(bucket)
    .execute(pool)
    .await
    .unwrap();
}

/// Refresh the limiter from the database and return its open counts.
async fn refreshed_open_counts(pool: &PgPool) -> Option<HashMap<String, usize>> {
    let state = make_state(pool.clone()).await;
    refresh_bucket_mp_counts(state.clone()).await.unwrap();
    let limiter = state.bucket_rate_limiter.lock().unwrap();
    limiter.get_stats().map(|stats| stats.per_bucket)
}

test_with_database! {
    async fn test_refresh_bucket_mp_counts_counts_open_per_bucket(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        let pool = test_db.pool();
        seed_merge_proposal(pool, "https://example.invalid/mp/1", Some("open"), Some("a")).await;
        seed_merge_proposal(pool, "https://example.invalid/mp/2", Some("open"), Some("a")).await;
        seed_merge_proposal(pool, "https://example.invalid/mp/3", Some("open"), Some("b")).await;
        seed_merge_proposal(pool, "https://example.invalid/mp/4", Some("merged"), Some("a")).await;

        assert_eq!(
            refreshed_open_counts(pool).await,
            Some(hashmap! {"a".to_string() => 2, "b".to_string() => 1})
        );
    }
}

test_with_database! {
    async fn test_refresh_bucket_mp_counts_skips_null_bucket(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        let pool = test_db.pool();
        seed_merge_proposal(pool, "https://example.invalid/mp/1", Some("open"), Some("a")).await;
        seed_merge_proposal(pool, "https://example.invalid/mp/2", Some("open"), None).await;

        assert_eq!(
            refreshed_open_counts(pool).await,
            Some(hashmap! {"a".to_string() => 1})
        );
    }
}

test_with_database! {
    async fn test_refresh_bucket_mp_counts_skips_null_status(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        let pool = test_db.pool();
        seed_merge_proposal(pool, "https://example.invalid/mp/1", Some("open"), Some("a")).await;
        seed_merge_proposal(pool, "https://example.invalid/mp/2", None, Some("b")).await;

        assert_eq!(
            refreshed_open_counts(pool).await,
            Some(hashmap! {"a".to_string() => 1})
        );
    }
}
