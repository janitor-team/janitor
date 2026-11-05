use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use janitor::{test_utils::TestDatabase, test_with_database};
use janitor_publish::web::{
    Blocker, BlockerBackoffDetails, BlockerChangeSetDetails, BlockerCommandDetails,
    BlockerInactiveDetails, BlockerInfo, BlockerPublishStatusDetails, BlockerSuccessDetails,
};
use janitor_publish::{AppState, PublishWorker};
use janitor_types::{Campaign, ChangeSetId, CodebaseName};
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
    build_app_with_rate_limiter(
        pool,
        Box::new(janitor_publish::rate_limiter::NonRateLimiter),
    )
    .await
}

async fn build_app_with_rate_limiter(
    pool: PgPool,
    bucket_rate_limiter: Box<dyn janitor_publish::rate_limiter::RateLimiter>,
) -> axum::Router {
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
        bucket_rate_limiter: Arc::new(Mutex::new(bucket_rate_limiter)),
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
    schema = Core;
    async fn merge_proposal_status_transitions(test_db: TestDatabase) {
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
    schema = Core;
    async fn publish_endpoint_returns_404_when_no_policy_and_no_mode(test_db: TestDatabase) {
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
    schema = Core;
    async fn publish_endpoint_rejects_invalid_mode_with_400(test_db: TestDatabase) {
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
    schema = Core;
    async fn blockers_after_failed_publish_without_rate_limit_bucket(test_db: TestDatabase) {
        seed_codebase_and_run(test_db.pool(), "cb-1", "lintian-fixes").await;
        sqlx::raw_sql(
            "INSERT INTO named_publish_policy (name, per_branch_policy) VALUES ('policy', ARRAY[
                 ROW('main', 'propose'::publish_mode, NULL::int)::branch_publish_policy]);
             INSERT INTO candidate (suite, value, command, publish_policy, codebase)
                 VALUES ('lintian-fixes', 100, 'noop', 'policy', 'cb-1');
             UPDATE run SET start_time = '2026-01-02 02:04:05', finish_time = '2026-01-02 03:04:05';
             INSERT INTO publish (id, change_set, target_branch_url, revision, mode, result_code)
                 VALUES ('p1', 'cs-1', 'https://example.invalid/cb-1', 'rev-tip', 'propose',
                         'publish-failed')",
        )
        .execute(test_db.pool())
        .await
        .unwrap();

        let app = build_app(test_db.pool().clone()).await;
        let req = Request::builder()
            .uri("/blockers/run-1")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let blockers: BlockerInfo = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            blockers,
            BlockerInfo {
                success: Blocker {
                    result: true,
                    waiting: false,
                    details: BlockerSuccessDetails {
                        result_code: "success".to_string(),
                    },
                },
                inactive: Blocker {
                    result: true,
                    waiting: false,
                    details: BlockerInactiveDetails { inactive: false },
                },
                command: Blocker {
                    result: true,
                    waiting: false,
                    details: BlockerCommandDetails {
                        correct: "noop".to_string(),
                        actual: "noop".to_string(),
                    },
                },
                publish_status: Blocker {
                    result: false,
                    waiting: true,
                    details: BlockerPublishStatusDetails {
                        status: "unknown".to_string(),
                        reviews: HashMap::new(),
                    },
                },
                backoff: Blocker {
                    result: true,
                    waiting: false,
                    details: BlockerBackoffDetails {
                        attempt_count: 1,
                        next_try_time: "2026-01-02T05:04:05Z".parse().unwrap(),
                    },
                },
                propose_rate_limit: Blocker {
                    result: true,
                    waiting: false,
                    details: serde_json::json!({"bucket": null}),
                },
                change_set: Blocker {
                    result: true,
                    waiting: false,
                    details: BlockerChangeSetDetails {
                        change_set_id: ChangeSetId::new("cs-1").unwrap(),
                        change_set_state: "ready".to_string(),
                    },
                },
                previous_mp: Blocker {
                    result: true,
                    waiting: false,
                    details: vec![],
                },
            }
        );
    }
}

test_with_database! {
    schema = Core;
    async fn blockers_waiting_flags(test_db: TestDatabase) {
        use janitor_publish::rate_limiter::{FixedRateLimiter, RateLimiter};
        seed_codebase_and_run(test_db.pool(), "cb-1", "lintian-fixes").await;
        sqlx::raw_sql(
            "INSERT INTO named_publish_policy (name, per_branch_policy, rate_limit_bucket)
                 VALUES ('policy', ARRAY[
                 ROW('main', 'propose'::publish_mode, NULL::int)::branch_publish_policy],
                 'bucket-1');
             INSERT INTO candidate (suite, value, command, publish_policy, codebase)
                 VALUES ('lintian-fixes', 100, 'noop', 'policy', 'cb-1');
             INSERT INTO publish (id, change_set, target_branch_url, revision, mode, result_code)
                 VALUES ('p1', 'cs-1', 'https://example.invalid/cb-1', 'rev-tip', 'propose',
                         'publish-failed');
             UPDATE change_set SET state = 'working'",
        )
        .execute(test_db.pool())
        .await
        .unwrap();
        let mut limiter = FixedRateLimiter::new(1);
        limiter.set_mps_per_bucket(&HashMap::from([(
            janitor::publish::MergeProposalStatus::Open,
            HashMap::from([("bucket-1".to_string(), 1)]),
        )]));

        let app = build_app_with_rate_limiter(test_db.pool().clone(), Box::new(limiter)).await;
        let req = Request::builder()
            .uri("/blockers/run-1")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let blockers: BlockerInfo = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            [
                (blockers.success.result, blockers.success.waiting),
                (blockers.publish_status.result, blockers.publish_status.waiting),
                (blockers.backoff.result, blockers.backoff.waiting),
                (
                    blockers.propose_rate_limit.result,
                    blockers.propose_rate_limit.waiting,
                ),
                (blockers.change_set.result, blockers.change_set.waiting),
            ],
            [
                (true, false),
                (false, true),
                (false, true),
                (false, true),
                (false, true)
            ]
        );

        // A failed run does not get reviewed, and its change set stays
        // working until a later run succeeds.
        sqlx::query("UPDATE run SET result_code = 'build-failed'")
            .execute(test_db.pool())
            .await
            .unwrap();
        let req = Request::builder()
            .uri("/blockers/run-1")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let blockers: BlockerInfo = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            [
                (blockers.success.result, blockers.success.waiting),
                (blockers.publish_status.result, blockers.publish_status.waiting),
                (blockers.change_set.result, blockers.change_set.waiting),
            ],
            [(false, false), (false, false), (false, false)]
        );
    }
}

test_with_database! {
    schema = Core;
    async fn invalid_identifiers_in_path_are_rejected(test_db: TestDatabase) {
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

test_with_database! {
    schema = Core;
    async fn previous_mp_status_reads_a_closed_merge_proposal(test_db: TestDatabase) {
        seed_codebase_and_run(test_db.pool(), "cb-1", "lintian-fixes").await;
        sqlx::query(
            "INSERT INTO merge_proposal (codebase, url, status, revision) \
             VALUES ('cb-1', 'https://example.invalid/mp/1', 'closed', 'rev-tip')",
        )
        .execute(test_db.pool())
        .await
        .unwrap();

        let status = janitor_publish::state::get_previous_mp_status(
            test_db.pool(),
            &CodebaseName::new("cb-1").unwrap(),
            &Campaign::new("lintian-fixes").unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            status,
            vec![(
                "https://example.invalid/mp/1".to_string(),
                janitor::publish::MergeProposalStatus::Closed
            )]
        );
    }
}
