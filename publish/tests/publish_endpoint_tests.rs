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

async fn post_merge_proposal_status(
    app: &axum::Router,
    url: &str,
    status: &str,
) -> (StatusCode, serde_json::Value) {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("url", url)
        .append_pair("status", status)
        .finish();
    let req = Request::builder()
        .method("POST")
        .uri("/merge-proposal")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn merge_proposal_status(pool: &PgPool, url: &str) -> String {
    sqlx::query_scalar("SELECT status::text FROM merge_proposal WHERE url = $1")
        .bind(url)
        .fetch_one(pool)
        .await
        .unwrap()
}

test_with_database! {
    async fn merge_proposal_status_transitions(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        let open_url = "https://forge.invalid/pv/pulls/1";
        let closed_url = "https://forge.invalid/sl/pulls/2";
        sqlx::query(
            "INSERT INTO merge_proposal (url, status)
             VALUES ($1, 'open'), ($2, 'closed')",
        )
        .bind(open_url)
        .bind(closed_url)
        .execute(test_db.pool())
        .await
        .unwrap();

        let app = build_app(test_db.pool().clone()).await;

        let missing_url = "https://forge.invalid/hello/pulls/3";
        assert_eq!(
            post_merge_proposal_status(&app, missing_url, "closed").await,
            (
                StatusCode::NOT_FOUND,
                serde_json::json!({"reason": "No such merge proposal", "url": missing_url}),
            )
        );

        assert_eq!(
            post_merge_proposal_status(&app, open_url, "open").await,
            (
                StatusCode::BAD_REQUEST,
                serde_json::json!({"error": "no transition from open to open"}),
            )
        );
        assert_eq!(
            post_merge_proposal_status(&app, closed_url, "open").await,
            (
                StatusCode::BAD_REQUEST,
                serde_json::json!({"error": "no transition from closed to open"}),
            )
        );
        assert_eq!(merge_proposal_status(test_db.pool(), open_url).await, "open");

        assert_eq!(
            post_merge_proposal_status(&app, closed_url, "abandoned").await,
            (
                StatusCode::OK,
                serde_json::json!({"status": "success", "url": closed_url}),
            )
        );
        assert_eq!(
            merge_proposal_status(test_db.pool(), closed_url).await,
            "abandoned"
        );
    }
}

test_with_database! {
    async fn publish_endpoint_returns_404_when_no_policy_and_no_mode(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase_and_run(test_db.pool(), "cb-1", "lintian-fixes").await;

        let app = build_app(test_db.pool().clone()).await;
        let req = Request::builder()
            .method("POST")
            .uri("/lintian-fixes/cb-1/publish")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(""))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let reason = body["reason"].as_str().expect("reason field");
        assert!(
            reason.contains("cb-1") && reason.contains("lintian-fixes"),
            "reason should name the codebase/campaign: {reason}"
        );
    }
}

test_with_database! {
    async fn publish_endpoint_rejects_invalid_mode_with_400(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase_and_run(test_db.pool(), "cb-1", "lintian-fixes").await;

        let app = build_app(test_db.pool().clone()).await;
        let req = Request::builder()
            .method("POST")
            .uri("/lintian-fixes/cb-1/publish")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from("mode=bogus"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}

test_with_database! {
    async fn publish_endpoint_only_returns_ids_for_queued_roles(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase_and_run(test_db.pool(), "cb-1", "lintian-fixes").await;
        sqlx::query(
            "INSERT INTO new_result_branch (run_id, role, remote_name)
             VALUES ('run-1', 'main', 'main'), ('run-1', 'extra', 'extra')",
        )
        .execute(test_db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO named_publish_policy (name, per_branch_policy) VALUES ('policy', ARRAY[
                ROW('main', 'propose'::publish_mode, NULL::int)::branch_publish_policy,
                ROW('extra', 'skip'::publish_mode, NULL::int)::branch_publish_policy])",
        )
        .execute(test_db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO candidate (suite, value, command, publish_policy, codebase)
             VALUES ('lintian-fixes', 100, 'noop', 'policy', 'cb-1')",
        )
        .execute(test_db.pool())
        .await
        .unwrap();

        let app = build_app(test_db.pool().clone()).await;
        let req = Request::builder()
            .method("POST")
            .uri("/lintian-fixes/cb-1/publish")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(""))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let roles: Vec<&String> = body["publish_ids"].as_object().unwrap().keys().collect();
        assert_eq!(roles, ["main"]);
    }
}

test_with_database! {
    async fn invalid_identifiers_in_path_are_rejected(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();

        let app = build_app(test_db.pool().clone()).await;
        for (method, uri) in [
            ("POST", "/lintian-fixes/Foo/publish"),
            ("POST", "/lintian_fixes/cb-1/publish"),
            ("GET", "/-foo/merge-proposals"),
            ("GET", "/c/foo%2Fbar/merge-proposals"),
            ("POST", "/consider/a.b"),
            ("GET", "/blockers/a%2Fb"),
        ] {
            let req = Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(""))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{} {}", method, uri);
        }
    }
}
