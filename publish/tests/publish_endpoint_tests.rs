use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use janitor::publish::MergeProposalStatus;
use janitor::{test_utils::TestDatabase, test_with_database};
use janitor_publish::rate_limiter::{FixedRateLimiter, RateLimiter};
use janitor_publish::web::{
    Blocker, BlockerBackoffDetails, BlockerChangeSetDetails, BlockerCommandDetails,
    BlockerInactiveDetails, BlockerInfo, BlockerPreviousMpDetails, BlockerProposeRateLimitDetails,
    BlockerPublishStatusDetails, BlockerRateLimitAllowedDetails, BlockerRateLimitExceededDetails,
    BlockerSuccessDetails, ReviewSummary,
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
    bucket_rate_limiter: Box<dyn RateLimiter>,
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

/// A rate limiter allowing at most `max_open` open merge proposals per
/// bucket, with `open` currently open in `bucket-a`.
fn rate_limiter(max_open: usize, open: usize) -> Box<dyn RateLimiter> {
    let mut limiter = FixedRateLimiter::new(max_open);
    limiter.set_mps_per_bucket(&HashMap::from([(
        MergeProposalStatus::Open,
        HashMap::from([("bucket-a".to_string(), open)]),
    )]));
    Box::new(limiter)
}

async fn get_blockers(app: axum::Router, run_id: &str) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .uri(format!("/blockers/{}", run_id))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
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

#[tokio::test]
async fn rate_limits_reports_max_open_of_a_fixed_limit() {
    let mut limiter: Box<dyn RateLimiter> = Box::new(FixedRateLimiter::new(3));
    limiter.set_mps_per_bucket(&maplit::hashmap! {
        MergeProposalStatus::Open => maplit::hashmap! { "lintian-fixes".to_string() => 1 },
    });
    let pool = PgPool::connect_lazy("postgres://localhost/unused").unwrap();
    let app = build_app_with_rate_limiter(pool, limiter).await;

    let mut bodies = Vec::new();
    for bucket in ["lintian-fixes", "never-seen"] {
        let req = Request::builder()
            .uri(format!("/rate-limits/{bucket}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        bodies.push(serde_json::from_slice::<serde_json::Value>(&bytes).unwrap());
    }
    assert_eq!(
        bodies,
        vec![
            serde_json::json!({"open": 1, "max_open": 3, "remaining": 2}),
            serde_json::json!({"open": null, "max_open": 3, "remaining": null}),
        ]
    );
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
                    details: BlockerSuccessDetails {
                        result_code: "success".to_string(),
                    },
                },
                inactive: Blocker {
                    result: true,
                    details: BlockerInactiveDetails { inactive: false },
                },
                command: Blocker {
                    result: true,
                    details: BlockerCommandDetails {
                        correct: "noop".to_string(),
                        actual: Some("noop".to_string()),
                    },
                },
                publish_status: Blocker {
                    result: false,
                    details: BlockerPublishStatusDetails {
                        status: "unknown".to_string(),
                        reviews: HashMap::new(),
                    },
                },
                backoff: Blocker {
                    result: true,
                    details: BlockerBackoffDetails {
                        attempt_count: 1,
                        next_try_time: "2026-01-02T05:04:05Z".parse().unwrap(),
                    },
                },
                propose_rate_limit: Blocker {
                    result: true,
                    details: BlockerProposeRateLimitDetails::Allowed(
                        BlockerRateLimitAllowedDetails { bucket: None },
                    ),
                },
                change_set: Blocker {
                    result: true,
                    details: BlockerChangeSetDetails {
                        change_set_id: ChangeSetId::new("cs-1").unwrap(),
                        change_set_state: "ready".to_string(),
                    },
                },
                previous_mp: Blocker {
                    result: true,
                    details: vec![],
                },
            }
        );
    }
}

test_with_database! {
    schema = Core;
    async fn blockers_for_publishable_run(test_db: TestDatabase) {
        seed_codebase_and_run(test_db.pool(), "cb-1", "lintian-fixes").await;
        sqlx::raw_sql(
            "INSERT INTO named_publish_policy (name, per_branch_policy, rate_limit_bucket)
                 VALUES ('policy', ARRAY[
                     ROW('main', 'propose'::publish_mode, NULL::int)::branch_publish_policy],
                     'bucket-a');
             INSERT INTO candidate (suite, value, command, publish_policy, codebase)
                 VALUES ('lintian-fixes', 100, 'noop', 'policy', 'cb-1');
             UPDATE run SET start_time = '2026-01-02 02:04:05',
                            finish_time = '2026-01-02 03:04:05',
                            publish_status = 'approved';
             UPDATE change_set SET state = 'publishing';
             INSERT INTO merge_proposal (url, status, revision)
                 VALUES ('https://forge.invalid/cb-1/pulls/1', 'merged', 'rev-tip');
             INSERT INTO review (run_id, comment, reviewer, verdict, reviewed_at) VALUES
                 ('run-1', 'looks good', 'alice', 'approved', '2026-01-02 04:00:00'),
                 ('run-1', NULL, 'bob', 'abstained', '2026-01-02 05:00:00')",
        )
        .execute(test_db.pool())
        .await
        .unwrap();

        let app = build_app_with_rate_limiter(test_db.pool().clone(), rate_limiter(5, 1)).await;
        let (status, body) = get_blockers(app, "run-1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            serde_json::from_value::<BlockerInfo>(body).unwrap(),
            BlockerInfo {
                success: Blocker {
                    result: true,
                    details: BlockerSuccessDetails {
                        result_code: "success".to_string(),
                    },
                },
                inactive: Blocker {
                    result: true,
                    details: BlockerInactiveDetails { inactive: false },
                },
                command: Blocker {
                    result: true,
                    details: BlockerCommandDetails {
                        correct: "noop".to_string(),
                        actual: Some("noop".to_string()),
                    },
                },
                publish_status: Blocker {
                    result: true,
                    details: BlockerPublishStatusDetails {
                        status: "approved".to_string(),
                        reviews: HashMap::from([
                            (
                                "alice".to_string(),
                                ReviewSummary {
                                    timestamp: "2026-01-02T04:00:00Z".parse().unwrap(),
                                    comment: Some("looks good".to_string()),
                                    verdict: "approved".to_string(),
                                },
                            ),
                            (
                                "bob".to_string(),
                                ReviewSummary {
                                    timestamp: "2026-01-02T05:00:00Z".parse().unwrap(),
                                    comment: None,
                                    verdict: "abstained".to_string(),
                                },
                            ),
                        ]),
                    },
                },
                backoff: Blocker {
                    result: true,
                    details: BlockerBackoffDetails {
                        attempt_count: 0,
                        next_try_time: "2026-01-02T03:04:05Z".parse().unwrap(),
                    },
                },
                propose_rate_limit: Blocker {
                    result: true,
                    details: BlockerProposeRateLimitDetails::Allowed(
                        BlockerRateLimitAllowedDetails {
                            bucket: Some("bucket-a".to_string()),
                        },
                    ),
                },
                change_set: Blocker {
                    result: true,
                    details: BlockerChangeSetDetails {
                        change_set_id: ChangeSetId::new("cs-1").unwrap(),
                        change_set_state: "publishing".to_string(),
                    },
                },
                previous_mp: Blocker {
                    result: true,
                    details: vec![BlockerPreviousMpDetails {
                        url: "https://forge.invalid/cb-1/pulls/1".to_string(),
                        status: MergeProposalStatus::Merged,
                    }],
                },
            }
        );
    }
}

test_with_database! {
    schema = Core;
    async fn blockers_for_blocked_run(test_db: TestDatabase) {
        seed_codebase_and_run(test_db.pool(), "cb-1", "lintian-fixes").await;
        // run-1 failed and comes after run-0, the last successful run,
        // whose merge proposal was rejected.
        sqlx::raw_sql(
            "INSERT INTO named_publish_policy (name, per_branch_policy, rate_limit_bucket)
                 VALUES ('policy', ARRAY[
                     ROW('main', 'propose'::publish_mode, NULL::int)::branch_publish_policy],
                     'bucket-a');
             INSERT INTO candidate (suite, value, command, publish_policy, codebase)
                 VALUES ('lintian-fixes', 100, 'other', 'policy', 'cb-1');
             UPDATE codebase SET inactive = true;
             UPDATE run SET start_time = '2100-01-01 02:04:05',
                            finish_time = '2100-01-01 03:04:05',
                            result_code = 'build-failed',
                            command = NULL,
                            publish_status = 'rejected';
             UPDATE change_set SET state = 'done';
             INSERT INTO run (id, codebase, suite, command, start_time, finish_time,
                              result_code, revision, change_set, logfilenames)
                 VALUES ('run-0', 'cb-1', 'lintian-fixes', 'noop', '2026-01-01 02:00:00',
                         '2026-01-01 03:00:00', 'success', 'rev-old', 'cs-1',
                         ARRAY[]::text[]);
             INSERT INTO merge_proposal (url, status, revision)
                 VALUES ('https://forge.invalid/cb-1/pulls/1', 'rejected', 'rev-old');
             INSERT INTO publish (id, change_set, target_branch_url, revision, mode,
                                  result_code) VALUES
                 ('p1', 'cs-1', 'https://example.invalid/cb-1', 'rev-tip', 'propose',
                  'publish-failed'),
                 ('p2', 'cs-1', 'https://example.invalid/cb-1', 'rev-tip', 'propose',
                  'publish-failed');
             INSERT INTO review (run_id, comment, reviewer, verdict, reviewed_at) VALUES
                 ('run-1', 'broken', 'carol', 'rejected', '2100-01-01 04:00:00')",
        )
        .execute(test_db.pool())
        .await
        .unwrap();

        let app = build_app_with_rate_limiter(test_db.pool().clone(), rate_limiter(1, 1)).await;
        let (status, body) = get_blockers(app, "run-1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            serde_json::from_value::<BlockerInfo>(body).unwrap(),
            BlockerInfo {
                success: Blocker {
                    result: false,
                    details: BlockerSuccessDetails {
                        result_code: "build-failed".to_string(),
                    },
                },
                inactive: Blocker {
                    result: false,
                    details: BlockerInactiveDetails { inactive: true },
                },
                command: Blocker {
                    result: false,
                    details: BlockerCommandDetails {
                        correct: "other".to_string(),
                        actual: None,
                    },
                },
                publish_status: Blocker {
                    result: false,
                    details: BlockerPublishStatusDetails {
                        status: "rejected".to_string(),
                        reviews: HashMap::from([(
                            "carol".to_string(),
                            ReviewSummary {
                                timestamp: "2100-01-01T04:00:00Z".parse().unwrap(),
                                comment: Some("broken".to_string()),
                                verdict: "rejected".to_string(),
                            },
                        )]),
                    },
                },
                backoff: Blocker {
                    result: false,
                    details: BlockerBackoffDetails {
                        attempt_count: 2,
                        next_try_time: "2100-01-01T07:04:05Z".parse().unwrap(),
                    },
                },
                propose_rate_limit: Blocker {
                    result: false,
                    details: BlockerProposeRateLimitDetails::Exceeded(
                        BlockerRateLimitExceededDetails {
                            open: Some(1),
                            max_open: Some(1),
                        },
                    ),
                },
                change_set: Blocker {
                    result: false,
                    details: BlockerChangeSetDetails {
                        change_set_id: ChangeSetId::new("cs-1").unwrap(),
                        change_set_state: "done".to_string(),
                    },
                },
                previous_mp: Blocker {
                    result: false,
                    details: vec![BlockerPreviousMpDetails {
                        url: "https://forge.invalid/cb-1/pulls/1".to_string(),
                        status: MergeProposalStatus::Rejected,
                    }],
                },
            }
        );
    }
}

test_with_database! {
    schema = Core;
    async fn blockers_need_publish_ready_run(test_db: TestDatabase) {
        // run-1 has no candidate with a publish policy.
        seed_codebase_and_run(test_db.pool(), "cb-1", "lintian-fixes").await;

        let app = build_app(test_db.pool().clone()).await;
        let mut got = vec![];
        for run_id in ["run-1", "run-2"] {
            let (status, body) = get_blockers(app.clone(), run_id).await;
            got.push((status, body["reason"].clone()));
        }
        assert_eq!(
            got,
            vec![
                (
                    StatusCode::NOT_FOUND,
                    serde_json::json!("No such publish-ready run")
                ),
                (
                    StatusCode::NOT_FOUND,
                    serde_json::json!("No such publish-ready run")
                ),
            ]
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

async fn get_json(app: axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn get_absorbed(app: axum::Router, query: &str) -> (StatusCode, serde_json::Value) {
    let uri = if query.is_empty() {
        "/absorbed".to_string()
    } else {
        format!("/absorbed?{}", query)
    };
    get_json(app, &uri).await
}

/// Seed the full `merge_proposal + run(codebase, campaign)` fixture
/// used by the merge-proposals list tests.
#[allow(clippy::too_many_arguments)]
async fn seed_mp_with_run(
    pool: &PgPool,
    codebase: &str,
    campaign: &str,
    cs_id: &str,
    run_id: &str,
    revision: &str,
    mp_url: &str,
    status: &str,
) {
    sqlx::query(
        "INSERT INTO codebase (name, branch_url, url, vcs_type)
         VALUES ($1, $2, $2, 'git') ON CONFLICT DO NOTHING",
    )
    .bind(codebase)
    .bind(format!("https://example.invalid/{}", codebase))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO change_set (id, campaign) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(cs_id)
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
             $1, $2, $3, 'noop', NOW() - INTERVAL '1 minute', NOW(), 'ok',
             'success', $4, 'rev-base', $5, 100, $6, ARRAY[]::text[], 'git'
         )",
    )
    .bind(run_id)
    .bind(codebase)
    .bind(campaign)
    .bind(format!("https://example.invalid/{}", codebase))
    .bind(revision)
    .bind(cs_id)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO merge_proposal (url, status, revision)
         VALUES ($1, $2::merge_proposal_status, $3)",
    )
    .bind(mp_url)
    .bind(status)
    .bind(revision)
    .execute(pool)
    .await
    .unwrap();
}

/// Seed one absorbed run (via the `absorbed_runs` view's `merge_proposal`
/// branch) for `(codebase, campaign)`, merged at `merged_at`.
async fn seed_absorbed_run(
    pool: &PgPool,
    run_id: &str,
    codebase: &str,
    campaign: &str,
    revision: &str,
    merged_at: chrono::DateTime<chrono::Utc>,
) {
    sqlx::query(
        "INSERT INTO codebase (name, branch_url, url, vcs_type)
         VALUES ($1, $2, $2, 'git') ON CONFLICT DO NOTHING",
    )
    .bind(codebase)
    .bind(format!("https://example.invalid/{}", codebase))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO change_set (id, campaign) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind("cs-absorbed")
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
             $1, $2, $3, 'noop', NOW() - INTERVAL '1 minute', NOW(), 'ok',
             'success', $4, 'rev-base', $5, 100,
             'cs-absorbed', ARRAY[]::text[], 'git'
         )",
    )
    .bind(run_id)
    .bind(codebase)
    .bind(campaign)
    .bind(format!("https://example.invalid/{}", codebase))
    .bind(revision)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO merge_proposal (url, status, revision, merged_at, merged_by)
         VALUES ($1, 'merged', $2, $3, 'reviewer@example.invalid')",
    )
    .bind(format!(
        "https://forge.invalid/{}/pulls/{}",
        codebase, run_id
    ))
    .bind(revision)
    .bind(merged_at)
    .execute(pool)
    .await
    .unwrap();
}

/// Seed a codebase, change_set, and `publish` row with the given
/// scalar fields. The web_url is derived from the codebase name.
#[allow(clippy::too_many_arguments)]
async fn seed_publish(
    pool: &PgPool,
    publish_id: &str,
    codebase: &str,
    mp_url: Option<&str>,
    mode: &str,
    result_code: &str,
    description: &str,
    revision: Option<&str>,
    timestamp: chrono::DateTime<chrono::Utc>,
) {
    sqlx::query(
        "INSERT INTO codebase (name, branch_url, url, vcs_type, web_url)
         VALUES ($1, $2, $2, 'git', $3) ON CONFLICT DO NOTHING",
    )
    .bind(codebase)
    .bind(format!("https://example.invalid/{}", codebase))
    .bind(format!("https://browse.invalid/{}", codebase))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO change_set (id, campaign) VALUES ('cs-publishes', 'lintian-fixes') \
         ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await
    .unwrap();
    if let Some(url) = mp_url {
        sqlx::query(
            "INSERT INTO merge_proposal (url, status, revision) \
             VALUES ($1, 'open', $2) ON CONFLICT DO NOTHING",
        )
        .bind(url)
        .bind(revision)
        .execute(pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO publish \
         (id, codebase, change_set, mode, result_code, description, \
          merge_proposal_url, revision, timestamp) \
         VALUES ($1, $2, 'cs-publishes', $3::publish_mode, $4, $5, $6, $7, $8)",
    )
    .bind(publish_id)
    .bind(codebase)
    .bind(mode)
    .bind(result_code)
    .bind(description)
    .bind(mp_url)
    .bind(revision)
    .bind(timestamp)
    .execute(pool)
    .await
    .unwrap();
}

test_with_database! {
    schema = Core;
    async fn merge_proposals_list_includes_codebase_and_campaign(test_db: TestDatabase) {
        seed_mp_with_run(
            test_db.pool(),
            "alpha",
            "lintian-fixes",
            "cs-alpha",
            "run-alpha",
            "rev-alpha",
            "https://forge.invalid/alpha/pulls/1",
            "open",
        )
        .await;
        seed_mp_with_run(
            test_db.pool(),
            "beta",
            "other-campaign",
            "cs-beta",
            "run-beta",
            "rev-beta",
            "https://forge.invalid/beta/pulls/2",
            "merged",
        )
        .await;

        let app = build_app(test_db.pool().clone()).await;
        let (status, body) = get_json(app, "/merge-proposals").await;
        assert_eq!(status, StatusCode::OK);
        let mut entries: Vec<(String, String, String, String)> = body
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["url"].as_str().unwrap().to_string(),
                    e["status"].as_str().unwrap().to_string(),
                    e["codebase"].as_str().unwrap().to_string(),
                    e["campaign"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        entries.sort();
        assert_eq!(
            entries,
            vec![
                (
                    "https://forge.invalid/alpha/pulls/1".to_string(),
                    "open".to_string(),
                    "alpha".to_string(),
                    "lintian-fixes".to_string(),
                ),
                (
                    "https://forge.invalid/beta/pulls/2".to_string(),
                    "merged".to_string(),
                    "beta".to_string(),
                    "other-campaign".to_string(),
                ),
            ]
        );
    }
}

test_with_database! {
    schema = Core;
    async fn list_publishes_returns_newest_first_with_codebase_web_url(test_db: TestDatabase) {
        use chrono::{TimeZone, Utc};
        let early = Utc.with_ymd_and_hms(2026, 1, 10, 12, 0, 0).unwrap();
        let mid = Utc.with_ymd_and_hms(2026, 2, 10, 12, 0, 0).unwrap();
        let late = Utc.with_ymd_and_hms(2026, 3, 10, 12, 0, 0).unwrap();

        seed_publish(test_db.pool(), "p-early", "alpha", None, "push",
                     "success", "pushed", Some("rev-1"), early).await;
        seed_publish(test_db.pool(), "p-mid", "alpha",
                     Some("https://forge.invalid/alpha/pulls/1"),
                     "propose", "success", "proposed", Some("rev-2"), mid).await;
        seed_publish(test_db.pool(), "p-late", "beta", None, "push",
                     "publish-failed", "retry", Some("rev-3"), late).await;

        let app = build_app(test_db.pool().clone()).await;

        // Unfiltered: three rows, newest first.
        let (status, body) = get_json(app.clone(), "/publish").await;
        assert_eq!(status, StatusCode::OK);
        let publishes = body["publishes"].as_array().unwrap();
        let ids: Vec<&str> = publishes.iter().map(|p| p["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["p-late", "p-mid", "p-early"]);
        assert_eq!(publishes[0]["codebase"], "beta");
        assert_eq!(publishes[0]["codebase_web_url"], "https://browse.invalid/beta");
        assert_eq!(publishes[0]["result_code"], "publish-failed");

        // limit=1 returns only the newest row.
        let (status, body) = get_json(app.clone(), "/publish?limit=1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["publishes"].as_array().unwrap().len(), 1);
        assert_eq!(body["publishes"][0]["id"], "p-late");
    }
}

test_with_database! {
    schema = Core;
    async fn get_merge_proposal_returns_detail_joined_with_run(test_db: TestDatabase) {
        use chrono::{TimeZone, Utc};
        let pool = test_db.pool();

        sqlx::query(
            "INSERT INTO codebase (name, branch_url, url, vcs_type)
             VALUES ('alpha', 'https://example.invalid/alpha', 'https://example.invalid/alpha', 'git')",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO change_set (id, campaign) VALUES ('cs-detail', 'lintian-fixes')")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO run (
                 id, codebase, suite, command, start_time, finish_time, description,
                 result_code, branch_url, main_branch_revision, revision, value,
                 change_set, logfilenames, vcs_type
             ) VALUES (
                 'run-detail', 'alpha', 'lintian-fixes', 'noop',
                 NOW() - INTERVAL '1 minute', NOW(), 'ok',
                 'success', 'https://example.invalid/alpha', 'rev-base', 'rev-tip',
                 100, 'cs-detail', ARRAY[]::text[], 'git'
             )",
        )
        .execute(pool)
        .await
        .unwrap();
        let merged_at = Utc.with_ymd_and_hms(2026, 3, 1, 12, 0, 0).unwrap();
        let last_scanned = Utc.with_ymd_and_hms(2026, 3, 2, 9, 30, 0).unwrap();
        let mp_url = "https://forge.invalid/alpha/pulls/42";
        sqlx::query(
            "INSERT INTO merge_proposal \
             (url, status, revision, merged_at, merged_by, last_scanned, can_be_merged) \
             VALUES ($1, 'merged', 'rev-tip', $2, 'reviewer@example.invalid', $3, true)",
        )
        .bind(mp_url)
        .bind(merged_at)
        .bind(last_scanned)
        .execute(pool)
        .await
        .unwrap();

        let app = build_app(pool.clone()).await;
        let (status, body) = get_json(
            app,
            &format!(
                "/merge-proposal?url={}",
                url::form_urlencoded::byte_serialize(mp_url.as_bytes()).collect::<String>()
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["url"], mp_url);
        assert_eq!(body["status"], "merged");
        assert_eq!(body["codebase"], "alpha");
        assert_eq!(body["campaign"], "lintian-fixes");
        assert_eq!(body["merged_by"], "reviewer@example.invalid");
        assert_eq!(body["can_be_merged"], true);
        assert_eq!(
            body["merged_at"]
                .as_str()
                .unwrap()
                .parse::<chrono::DateTime<chrono::Utc>>()
                .unwrap(),
            merged_at
        );
        assert_eq!(
            body["last_scanned"]
                .as_str()
                .unwrap()
                .parse::<chrono::DateTime<chrono::Utc>>()
                .unwrap(),
            last_scanned
        );
    }
}

test_with_database! {
    schema = Core;
    async fn merge_proposals_by_codebase_filters_by_campaign(test_db: TestDatabase) {
        for (campaign, cs, run, rev, mp) in [
            ("lintian-fixes", "cs-a", "run-a", "rev-a", "https://forge.invalid/alpha/pulls/1"),
            ("other-campaign", "cs-b", "run-b", "rev-b", "https://forge.invalid/alpha/pulls/2"),
        ] {
            seed_mp_with_run(
                test_db.pool(),
                "alpha",
                campaign,
                cs,
                run,
                rev,
                mp,
                "open",
            )
            .await;
        }

        let app = build_app(test_db.pool().clone()).await;

        // No campaign filter: both proposals come back.
        let (status, body) = get_json(app.clone(), "/c/alpha/merge-proposals").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().unwrap().len(), 2);

        // Campaign filter narrows to one.
        let (status, body) =
            get_json(app, "/c/alpha/merge-proposals?campaign=lintian-fixes").await;
        assert_eq!(status, StatusCode::OK);
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["url"], "https://forge.invalid/alpha/pulls/1");
        assert_eq!(arr[0]["campaign"], "lintian-fixes");
    }
}

test_with_database! {
    schema = Core;
    async fn absorbed_filters_by_campaign_and_returns_oldest(test_db: TestDatabase) {
        use chrono::{TimeZone, Utc};
        let early = Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap();
        let mid = Utc.with_ymd_and_hms(2026, 2, 15, 12, 0, 0).unwrap();
        let late = Utc.with_ymd_and_hms(2026, 3, 15, 12, 0, 0).unwrap();

        seed_absorbed_run(test_db.pool(), "r-early-lf", "alpha", "lintian-fixes", "rev-early", early).await;
        seed_absorbed_run(test_db.pool(), "r-mid-lf", "beta", "lintian-fixes", "rev-mid", mid).await;
        seed_absorbed_run(test_db.pool(), "r-late-other", "gamma", "other-campaign", "rev-late", late).await;

        let app = build_app(test_db.pool().clone()).await;

        // Without a filter: all three rows, oldest from the full set.
        let (status, body) = get_absorbed(app.clone(), "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["runs"].as_array().unwrap().len(), 3);
        assert_eq!(
            body["oldest"]
                .as_str()
                .unwrap()
                .parse::<chrono::DateTime<Utc>>()
                .unwrap(),
            early
        );

        // Filter by campaign: only rows for that campaign, oldest scoped to it.
        let (status, body) = get_absorbed(app.clone(), "campaign=lintian-fixes").await;
        assert_eq!(status, StatusCode::OK);
        let runs = body["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 2);
        for run in runs {
            assert_eq!(run["campaign"], "lintian-fixes");
        }
        assert_eq!(
            body["oldest"]
                .as_str()
                .unwrap()
                .parse::<chrono::DateTime<Utc>>()
                .unwrap(),
            early
        );

        // `since` narrows the runs list, but `oldest` stays scoped by
        // campaign so the UI's min-date bound doesn't shift.
        let since_encoded = url::form_urlencoded::byte_serialize(mid.to_rfc3339().as_bytes())
            .collect::<String>();
        let (status, body) = get_absorbed(
            app.clone(),
            &format!("campaign=lintian-fixes&since={}", since_encoded),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let runs = body["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["id"], "r-mid-lf");
        assert_eq!(
            body["oldest"]
                .as_str()
                .unwrap()
                .parse::<chrono::DateTime<Utc>>()
                .unwrap(),
            early
        );

        // Campaign with no absorbed runs: empty list, null oldest.
        let (status, body) = get_absorbed(app, "campaign=no-such-campaign").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["runs"].as_array().unwrap().len(), 0);
        assert!(body["oldest"].is_null());
    }
}

test_with_database! {
    schema = Core;
    async fn list_publishes_filters_by_merge_proposal_url(test_db: TestDatabase) {
        use chrono::{TimeZone, Utc};
        let t0 = Utc.with_ymd_and_hms(2026, 2, 1, 12, 0, 0).unwrap();
        let t1 = Utc.with_ymd_and_hms(2026, 2, 2, 12, 0, 0).unwrap();

        let mp = "https://forge.invalid/alpha/pulls/1";
        seed_publish(test_db.pool(), "p-match-1", "alpha", Some(mp),
                     "propose", "success", "ok", Some("rev-1"), t0).await;
        seed_publish(test_db.pool(), "p-match-2", "alpha", Some(mp),
                     "propose", "success", "ok", Some("rev-2"), t1).await;
        seed_publish(test_db.pool(), "p-other", "alpha",
                     Some("https://forge.invalid/alpha/pulls/99"),
                     "propose", "success", "ok", Some("rev-3"), t0).await;

        let app = build_app(test_db.pool().clone()).await;
        let (status, body) = get_json(
            app,
            &format!(
                "/publish?merge_proposal_url={}",
                url::form_urlencoded::byte_serialize(mp.as_bytes()).collect::<String>()
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let publishes = body["publishes"].as_array().unwrap();
        assert_eq!(publishes.len(), 2);
        for row in publishes {
            assert_eq!(row["merge_proposal_url"], mp);
        }
        // Newest first.
        assert_eq!(publishes[0]["id"], "p-match-2");
        assert_eq!(publishes[1]["id"], "p-match-1");
    }
}

test_with_database! {
    schema = Core;
    async fn list_publishes_filters_by_revision(test_db: TestDatabase) {
        use chrono::{TimeZone, Utc};
        let t0 = Utc.with_ymd_and_hms(2026, 2, 1, 12, 0, 0).unwrap();
        let t1 = Utc.with_ymd_and_hms(2026, 2, 2, 12, 0, 0).unwrap();

        // Two publishes for rev-target (in different modes); one
        // publish for rev-other.
        seed_publish(test_db.pool(), "p-prop", "alpha",
                     Some("https://forge.invalid/alpha/pulls/1"),
                     "propose", "success", "proposed",
                     Some("rev-target"), t0).await;
        seed_publish(test_db.pool(), "p-push", "alpha", None,
                     "push", "success", "pushed",
                     Some("rev-target"), t1).await;
        seed_publish(test_db.pool(), "p-other", "alpha", None,
                     "push", "success", "pushed",
                     Some("rev-other"), t0).await;

        let app = build_app(test_db.pool().clone()).await;
        let (status, body) = get_json(app, "/publish?revision=rev-target").await;
        assert_eq!(status, StatusCode::OK);
        let publishes = body["publishes"].as_array().unwrap();
        assert_eq!(publishes.len(), 2);
        for row in publishes {
            assert_eq!(row["revision"], "rev-target");
        }
        // Newest first.
        assert_eq!(publishes[0]["id"], "p-push");
        assert_eq!(publishes[1]["id"], "p-prop");
    }
}

test_with_database! {
    schema = Core;
    async fn get_merge_proposal_missing_returns_404(test_db: TestDatabase) {
        let app = build_app(test_db.pool().clone()).await;
        let (status, body) = get_json(app, "/merge-proposal?url=https://none.invalid/x").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["reason"], "No such merge proposal");
        assert_eq!(body["url"], "https://none.invalid/x");
    }
}

test_with_database! {
    schema = Core;
    async fn openapi_json_is_served(test_db: TestDatabase) {
        let app = build_app(test_db.pool().clone()).await;
        let req = Request::builder()
            .method("GET")
            .uri("/openapi.json")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["info"]["title"], serde_json::json!("janitor-publish"));
        assert!(
            body["paths"]["/blockers/{id}"]["get"].is_object(),
            "{body}"
        );
    }
}
