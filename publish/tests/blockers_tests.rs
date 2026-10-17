use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use janitor::{schema::setup_test_database, test_utils::TestDatabase, test_with_database};
use janitor_publish::{AppState, PublishWorker};
use sqlx::PgPool;
use tower::ServiceExt;

async fn seed_codebase_and_run(pool: &PgPool, codebase: &str, campaign: &str) {
    sqlx::query(
        "INSERT INTO codebase (name, branch_url, url, vcs_type)
         VALUES ($1, $2, $2, 'git')",
    )
    .bind(codebase)
    .bind(format!("https://example.invalid/{}", codebase))
    .execute(pool)
    .await
    .unwrap();

    sqlx::query("INSERT INTO change_set (id, campaign) VALUES ($1, $2)")
        .bind("cs-1")
        .bind(campaign)
        .execute(pool)
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO run (
            id, codebase, suite, command, start_time, finish_time, description,
            result_code, branch_url, main_branch_revision, revision, value,
            change_set, logfilenames, vcs_type
         ) VALUES (
            $1, $2, $3, 'noop', NOW(), NOW(), 'ok', 'success', $4,
            'rev-base', 'rev-tip', 100, 'cs-1', ARRAY[]::text[], 'git'
         )",
    )
    .bind("run-1")
    .bind(codebase)
    .bind(campaign)
    .bind(format!("https://example.invalid/{}", codebase))
    .execute(pool)
    .await
    .unwrap();
}

async fn build_app(pool: PgPool) -> axum::Router {
    let config: &'static janitor::config::Config = Box::leak(Box::new(
        janitor::config::read_string("").expect("empty config parses"),
    ));
    let publish_worker = PublishWorker::new(
        None,
        None,
        url::Url::parse("http://differ.invalid").unwrap(),
        None,
        None,
        None,
    )
    .await;
    let health_checker = Arc::new(janitor_publish::health::BasicHealthChecker::with_info(
        "publish-test".to_string(),
        env!("CARGO_PKG_VERSION").to_string(),
    ));
    let state = Arc::new(AppState {
        conn: pool,
        bucket_rate_limiter: Arc::new(Mutex::new(Box::new(
            janitor_publish::rate_limiter::NonRateLimiter,
        ))),
        forge_rate_limiter: Arc::new(RwLock::new(HashMap::new())),
        push_limit: None,
        redis: None,
        redis_manager: None,
        config,
        publish_worker,
        vcs_managers: Arc::new(HashMap::new()),
        modify_mp_limit: None,
        unexpected_mp_limit: None,
        gpg: Arc::new(breezyshim::gpg::GPGContext::new()),
        require_binary_diff: false,
        health_checker,
        last_full_scan_at: tokio::sync::watch::channel(None).0,
    });
    janitor_publish::web::app(state)
}

/// Give `run-1` a candidate and a policy without a rate limit bucket.
async fn seed_policy_without_bucket(pool: &PgPool, codebase: &str, campaign: &str) {
    sqlx::query(
        "INSERT INTO named_publish_policy (name, per_branch_policy) \
         VALUES ('nobucket', ARRAY[ROW('main', 'propose', NULL)::branch_publish_policy])",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO candidate (suite, command, publish_policy, codebase, value) \
         VALUES ($1, 'noop', 'nobucket', $2, 100)",
    )
    .bind(campaign)
    .bind(codebase)
    .execute(pool)
    .await
    .unwrap();
}

async fn get_blockers(pool: &PgPool) -> (StatusCode, serde_json::Value) {
    let app = build_app(pool.clone()).await;
    let req = Request::builder()
        .uri("/blockers/run-1")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

test_with_database! {
    async fn blockers_without_a_rate_limit_bucket(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase_and_run(test_db.pool(), "cb-1", "lintian-fixes").await;
        seed_policy_without_bucket(test_db.pool(), "cb-1", "lintian-fixes").await;

        let (status, body) = get_blockers(test_db.pool()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["propose_rate_limit"]["result"], true, "{body}");
    }
}

test_with_database! {
    async fn blockers_with_a_review(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase_and_run(test_db.pool(), "cb-1", "lintian-fixes").await;
        seed_policy_without_bucket(test_db.pool(), "cb-1", "lintian-fixes").await;
        sqlx::query(
            "INSERT INTO review (run_id, reviewer, verdict, reviewed_at) \
             VALUES ('run-1', 'alice', 'approved', '2026-01-02 03:04:05')",
        )
        .execute(test_db.pool())
        .await
        .unwrap();

        let (status, body) = get_blockers(test_db.pool()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let review = &body["publish_status"]["details"]["reviews"]["alice"];
        assert_eq!(review["verdict"], "approved", "{body}");
        assert_eq!(review["timestamp"], "2026-01-02T03:04:05Z", "{body}");
    }
}

test_with_database! {
    async fn blockers_with_a_review_without_reviewer(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase_and_run(test_db.pool(), "cb-1", "lintian-fixes").await;
        seed_policy_without_bucket(test_db.pool(), "cb-1", "lintian-fixes").await;
        sqlx::query("INSERT INTO review (run_id, verdict) VALUES ('run-1', 'approved')")
            .execute(test_db.pool())
            .await
            .unwrap();

        let (status, body) = get_blockers(test_db.pool()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let reviews = &body["publish_status"]["details"]["reviews"];
        assert_eq!(reviews["null"]["verdict"], "approved", "{body}");
    }
}
