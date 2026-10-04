//! HTTP-level tests for the runner's write endpoints.
//!
//! Each test drives a route through the real axum router, then checks
//! the response body AND the database state so a passing test proves
//! the endpoint actually did what it claims. Skipped cleanly when
//! there's no test database or Redis available.

use axum::body::{to_bytes, Body};
use axum::http::{Method, Request, StatusCode};
use chrono::Utc;
use janitor::queue::VcsInfo;
use janitor_runner::{test_utils, ActiveRun, AppState, Backchannel};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

/// Set up the test router + AppState, or skip if no DB/Redis.
/// `create_test_app_with_state_if_available` starts an ephemeral
/// Redis container itself when Docker is available, so tests need no
/// preinstalled Redis.
async fn setup() -> Option<(axum::Router, Arc<AppState>)> {
    test_utils::create_test_app_with_state_if_available()
        .await
        .expect("test app setup should either succeed or return None cleanly")
}

/// Like `setup()` but registers `test-campaign` (command: `true`) in
/// the config so the `POST /candidates` handler's campaign validation
/// accepts candidates targeting it.
async fn setup_with_campaign() -> Option<(axum::Router, Arc<AppState>)> {
    let builder = test_utils::TestConfigBuilder::new().with_campaign("test-campaign", "true");
    test_utils::create_test_app_with_state_with_config_if_available(builder)
        .await
        .expect("test app setup should either succeed or return None cleanly")
}

async fn get_body(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body should be readable");
    if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("response body should be JSON")
    }
}

/// Insert a codebase row directly against the test pool. Callers use
/// this to build the referenced state that a write endpoint expects.
///
/// The `codebase` table's check constraint requires
/// `(branch_url IS NULL) = (url IS NULL)`, so set both.
async fn insert_codebase(pool: &sqlx::PgPool, name: &str) {
    let url = format!("https://example.invalid/{name}");
    sqlx::query(
        "INSERT INTO codebase (name, branch_url, url, vcs_type)
         VALUES ($1, $2, $2, 'git')
         ON CONFLICT DO NOTHING",
    )
    .bind(name)
    .bind(&url)
    .execute(pool)
    .await
    .expect("codebase insert");
}

/// `DELETE /candidates/{id}` on a non-numeric id must reject with
/// 400. Verifies the parse-guard before we ever touch the DB.
#[tokio::test]
async fn delete_candidate_rejects_non_numeric_id() {
    let Some((app, _state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let req = Request::builder()
        .method(Method::DELETE)
        .uri("/candidates/not-a-number")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = get_body(response).await;
    assert_eq!(body["reason"], "Invalid candidate ID");
}

/// `DELETE /candidates/{id}` on a missing id returns 404 with the
/// `{reason}` body.
#[tokio::test]
async fn delete_candidate_missing_returns_404() {
    let Some((app, _state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let req = Request::builder()
        .method(Method::DELETE)
        .uri("/candidates/999999")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = get_body(response).await;
    assert_eq!(body["reason"], "No such candidate");
}

/// `POST /codebases` upserts the given codebases and a subsequent
/// `GET /codebases` returns them. Verifies both the write and the
/// visible side-effect through the database.
#[tokio::test]
async fn post_codebases_upserts_and_is_visible_via_get() {
    let Some((app, state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let body = json!([
        {
            "name": "test-codebase-a",
            "branch_url": "https://example.invalid/a",
            "vcs_type": "git"
        },
        {
            "name": "test-codebase-b",
            "branch_url": "https://example.invalid/b",
            "vcs_type": "git"
        }
    ]);

    let req = Request::builder()
        .method(Method::POST)
        .uri("/codebases")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Confirm the rows landed via a direct query, not via
    // `GET /codebases` -- the latter would just re-run the same code
    // path we already exercised in the POST.
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM codebase WHERE name IN ('test-codebase-a', 'test-codebase-b')
         ORDER BY name",
    )
    .fetch_all(state.database.pool())
    .await
    .expect("query codebases");
    assert_eq!(
        names,
        vec!["test-codebase-a".to_string(), "test-codebase-b".to_string()]
    );
}

/// `POST /schedule` with a nonexistent `run_id` returns 404 with the
/// `{reason}` body.
#[tokio::test]
async fn post_schedule_with_missing_run_returns_404() {
    let Some((app, _state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let body = json!({"run_id": "no-such-run"});
    let req = Request::builder()
        .method(Method::POST)
        .uri("/schedule")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = get_body(response).await;
    assert_eq!(body["reason"], "Run not found");
}

/// `POST /schedule` without any of `run_id`, `campaign`, `codebase`
/// returns 400 with `{reason: "missing campaign"}` -- the handler
/// gates on `campaign` first when no `run_id` is provided.
#[tokio::test]
async fn post_schedule_missing_campaign_returns_400() {
    let Some((app, _state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let req = Request::builder()
        .method(Method::POST)
        .uri("/schedule")
        .header("content-type", "application/json")
        .body(Body::from(json!({}).to_string()))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = get_body(response).await;
    assert_eq!(body["reason"], "missing campaign");
}

/// `POST /runs/{id}` on an unknown run returns 404 with a
/// `{reason}` body.
#[tokio::test]
async fn post_update_run_missing_returns_404() {
    let Some((app, _state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let body = json!({"publish_status": "approved"});
    let req = Request::builder()
        .method(Method::POST)
        .uri("/runs/no-such-run")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = get_body(response).await;
    assert!(
        body["reason"]
            .as_str()
            .is_some_and(|s| s.contains("no such run")),
        "expected `no such run` in reason, got {}",
        body
    );
}

/// `POST /runs/{id}` on an existing run updates `publish_status` and
/// returns a payload echoing the change. Verifies through a direct DB
/// query that the row was actually mutated.
#[tokio::test]
async fn post_update_run_persists_publish_status() {
    let Some((app, state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let pool = state.database.pool().clone();

    // Seed: one codebase, one change_set, one run row.
    insert_codebase(&pool, "update-run-codebase").await;
    sqlx::query("INSERT INTO change_set (id, campaign) VALUES ($1, $2)")
        .bind("cs-upd")
        .bind("test-campaign")
        .execute(&pool)
        .await
        .expect("insert change_set");
    // `revision` must be set so publish_status='approved' satisfies
    // the run check `publish_status != 'approved' or revision is not null`.
    sqlx::query(
        "INSERT INTO run (
             id, suite, codebase, result_code, revision,
             start_time, finish_time, logfilenames, change_set
         )
         VALUES ($1, $2, $3, $4, $5,
                 NOW() - INTERVAL '1 minute', NOW(), '{}', $6)",
    )
    .bind("run-upd-1")
    .bind("test-campaign")
    .bind("update-run-codebase")
    .bind("success")
    .bind("rev-upd-1")
    .bind("cs-upd")
    .execute(&pool)
    .await
    .expect("insert run");

    let body = json!({"publish_status": "approved"});
    let req = Request::builder()
        .method(Method::POST)
        .uri("/runs/run-upd-1")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = get_body(response).await;
    assert_eq!(body["run_id"], "run-upd-1");
    assert_eq!(body["publish_status"], "approved");
    assert_eq!(body["campaign"], "test-campaign");
    assert_eq!(body["codebase"], "update-run-codebase");

    // publish_status is a Postgres ENUM; cast to text so
    // query_scalar<String> can decode it.
    let stored: Option<String> =
        sqlx::query_scalar("SELECT publish_status::text FROM run WHERE id = 'run-upd-1'")
            .fetch_one(&pool)
            .await
            .expect("fetch stored publish_status");
    assert_eq!(stored.as_deref(), Some("approved"));
}

/// `POST /kill/{id}` on an unknown run returns 404 with the
/// `{reason}` body.
#[tokio::test]
async fn post_kill_missing_returns_404() {
    let Some((app, _state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let req = Request::builder()
        .method(Method::POST)
        .uri("/kill/no-such-run")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = get_body(response).await;
    assert!(
        body["reason"]
            .as_str()
            .is_some_and(|s| s.contains("No such current run")),
        "expected `No such current run` in reason, got {}",
        body
    );
}

/// `POST /active-runs/{id}/finish` on an unknown run returns 404
/// with the `{reason}` shape Python emits.
#[tokio::test]
async fn post_finish_missing_returns_404() {
    let Some((app, _state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    // Real `/finish` expects multipart; a plain empty body reaches the
    // "no such run" branch first because the run-id lookup happens
    // before multipart parsing.
    let req = Request::builder()
        .method(Method::POST)
        .uri("/active-runs/no-such-run/finish")
        .header("content-type", "multipart/form-data; boundary=x")
        .body(Body::from("--x--\r\n"))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = get_body(response).await;
    assert!(
        body["reason"]
            .as_str()
            .is_some_and(|s| s.contains("no such run")),
        "expected `no such run` in reason, got {}",
        body
    );
}

/// `POST /candidates` accepts an empty list and returns a success
/// response with all buckets empty. Sanity check on the request
/// shape and the response envelope.
#[tokio::test]
async fn post_candidates_empty_list_returns_success_envelope() {
    let Some((app, _state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let req = Request::builder()
        .method(Method::POST)
        .uri("/candidates")
        .header("content-type", "application/json")
        .body(Body::from(json!([]).to_string()))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = get_body(response).await;
    // Empty list -> nothing succeeded, nothing rejected. All buckets
    // are present in the envelope even when empty so callers can
    // parse a fixed shape.
    assert_eq!(body["success"], json!([]));
    assert_eq!(body["unknown_campaigns"], json!([]));
    assert_eq!(body["unknown_codebases"], json!([]));
    assert_eq!(body["invalid_command"], json!([]));
    assert_eq!(body["invalid_value"], json!([]));
    assert_eq!(body["unknown_publish_policies"], json!([]));
}

/// `POST /candidates` with a candidate for an unknown campaign
/// records it in `unknown_campaigns` and skips it.
#[tokio::test]
async fn post_candidates_records_unknown_campaign() {
    let Some((app, state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let pool = state.database.pool().clone();
    insert_codebase(&pool, "cand-unk-campaign-cb").await;

    let body = json!([{
        "codebase": "cand-unk-campaign-cb",
        "campaign": "not-a-real-campaign",
        "command": "true",
    }]);
    let req = Request::builder()
        .method(Method::POST)
        .uri("/candidates")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = get_body(response).await;
    assert_eq!(body["success"], json!([]));
    assert_eq!(
        body["unknown_campaigns"],
        json!(["not-a-real-campaign"]),
        "candidate for unknown campaign should be listed in unknown_campaigns"
    );
    assert_eq!(body["unknown_codebases"], json!([]));
}

/// `POST /candidates` on a candidate whose codebase does not exist
/// returns 400. Ours diverges from Python here (Python collects it in
/// `unknown_codebases` via the FK-violation path); this test pins the
/// Rust-side gate so we do not regress silently.
#[tokio::test]
async fn post_candidates_missing_codebase_field_is_bad_request() {
    let Some((app, _state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let body = json!([{
        "campaign": "any-campaign",
        "command": "true",
    }]);
    let req = Request::builder()
        .method(Method::POST)
        .uri("/candidates")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// `GET /runs/{id}` on an existing run returns `{codebase, campaign,
/// publish_status}`.
#[tokio::test]
async fn get_run_returns_codebase_campaign_publish_status() {
    let Some((app, state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let pool = state.database.pool().clone();
    insert_codebase(&pool, "get-run-cb").await;
    sqlx::query("INSERT INTO change_set (id, campaign) VALUES ($1, $2)")
        .bind("cs-get")
        .bind("test-campaign")
        .execute(&pool)
        .await
        .expect("insert change_set");
    sqlx::query(
        "INSERT INTO run (
             id, suite, codebase, result_code,
             start_time, finish_time, logfilenames, change_set
         )
         VALUES ($1, $2, $3, $4,
                 NOW() - INTERVAL '1 minute', NOW(), '{}', $5)",
    )
    .bind("run-get-1")
    .bind("test-campaign")
    .bind("get-run-cb")
    .bind("success")
    .bind("cs-get")
    .execute(&pool)
    .await
    .expect("insert run");

    let req = Request::builder()
        .method(Method::GET)
        .uri("/runs/run-get-1")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = get_body(response).await;
    assert_eq!(body["codebase"], "get-run-cb");
    assert_eq!(body["campaign"], "test-campaign");
    // publish_status is nullable until set; either JSON null or a
    // string is acceptable, but the key must be present so clients
    // can rely on the shape.
    assert!(
        body.get("publish_status").is_some(),
        "publish_status key must be present, got {body}"
    );
}

/// `GET /runs/{id}` on an unknown run returns 404 with the
/// `{reason}` body.
#[tokio::test]
async fn get_run_missing_returns_404() {
    let Some((app, _state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let req = Request::builder()
        .method(Method::GET)
        .uri("/runs/no-such-run")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = get_body(response).await;
    assert!(
        body["reason"]
            .as_str()
            .is_some_and(|s| s.contains("no such run")),
        "expected `no such run` in reason, got {body}"
    );
}

/// `GET /status` returns the three fields the Python handler emits:
/// `processing`, `avoid_hosts`, `rate_limit_hosts`. With an empty
/// active-runs set and no env overrides, all three come back empty.
#[tokio::test]
async fn get_status_returns_python_shape() {
    let Some((app, _state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let req = Request::builder()
        .method(Method::GET)
        .uri("/status")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = get_body(response).await;
    assert!(body["processing"].is_array(), "processing must be an array");
    assert!(
        body["avoid_hosts"].is_array(),
        "avoid_hosts must be an array"
    );
    assert!(
        body["rate_limit_hosts"].is_object(),
        "rate_limit_hosts must be an object"
    );
}

/// `GET /queue` returns the entries currently in the queue table.
#[tokio::test]
async fn get_queue_returns_scheduled_entries() {
    let Some((app, state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let pool = state.database.pool().clone();
    insert_codebase(&pool, "queue-listing-cb").await;
    sqlx::query(
        "INSERT INTO queue (codebase, suite, command, context)
         VALUES ($1, $2, $3, $4)",
    )
    .bind("queue-listing-cb")
    .bind("test-campaign")
    .bind("true")
    .bind("some-context")
    .execute(&pool)
    .await
    .expect("insert queue row");

    let req = Request::builder()
        .method(Method::GET)
        .uri("/queue")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = get_body(response).await;
    let entries = body.as_array().expect("body should be an array");
    let found = entries
        .iter()
        .find(|e| e["codebase"] == "queue-listing-cb" && e["campaign"] == "test-campaign");
    let entry = found.expect("expected our seeded row to appear in /queue");
    assert_eq!(entry["command"], "true");
    assert_eq!(entry["context"], "some-context");
    assert!(entry["queue_id"].is_i64(), "queue_id must be a number");
}

/// `POST /kill/{id}` on a run whose backchannel does not support
/// killing returns 501. Jenkins backchannel always returns
/// `PingError::NotSupported`.
#[tokio::test]
async fn post_kill_jenkins_run_returns_501() {
    let Some((app, state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let active_run = ActiveRun {
        worker_name: "jenkins-worker".to_string(),
        worker_link: None,
        queue_id: 1,
        log_id: "run-kill-not-supported".to_string(),
        start_time: Utc::now(),
        finish_time: None,
        estimated_duration: None,
        campaign: "test-campaign".to_string(),
        change_set: None,
        command: "true".to_string(),
        codebase: "kill-cb".to_string(),
        backchannel: Backchannel::Jenkins {
            my_url: "http://jenkins.example.invalid/".to_string(),
            jenkins: Some(json!({})),
        },
        vcs_info: VcsInfo::default(),
        instigated_context: None,
        resume_from: None,
    };
    state.active_runs.store(active_run).await;

    let req = Request::builder()
        .method(Method::POST)
        .uri("/kill/run-kill-not-supported")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    let body = get_body(response).await;
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|s| s.contains("Jenkins")),
        "expected error to mention Jenkins, got {body}"
    );

    // Cleanup so the row does not leak across serial tests.
    let _ = state.active_runs.remove("run-kill-not-supported").await;
}

/// `GET /active-runs` returns the empty list when no run is active.
#[tokio::test]
async fn get_active_runs_returns_empty_array_when_idle() {
    let Some((app, _state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let req = Request::builder()
        .method(Method::GET)
        .uri("/active-runs")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = get_body(response).await;
    // Each test uses a UUID-namespaced ActiveRunStore key, so no
    // other test's runs are visible here.
    assert_eq!(body, json!([]));
}

/// `POST /schedule` for an existing candidate returns the four
/// scheduling-envelope fields plus `queue_position` and
/// `queue_wait_time`.
#[tokio::test]
async fn post_schedule_returns_queue_position() {
    let Some((app, state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let pool = state.database.pool().clone();
    insert_codebase(&pool, "sched-pos-cb").await;
    sqlx::query(
        "INSERT INTO candidate (codebase, suite, command)
         VALUES ($1, $2, $3)",
    )
    .bind("sched-pos-cb")
    .bind("test-campaign")
    .bind("true")
    .execute(&pool)
    .await
    .expect("insert candidate");

    let body = json!({
        "codebase": "sched-pos-cb",
        "campaign": "test-campaign",
    });
    let req = Request::builder()
        .method(Method::POST)
        .uri("/schedule")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = get_body(response).await;
    assert_eq!(body["campaign"], "test-campaign");
    assert_eq!(body["codebase"], "sched-pos-cb");
    for key in [
        "offset",
        "bucket",
        "queue_id",
        "estimated_duration_seconds",
        "queue_position",
        "queue_wait_time",
    ] {
        assert!(
            body.get(key).is_some(),
            "response must include `{key}`, got {body}"
        );
    }
}

/// `POST /candidates` for a known campaign with a known codebase
/// lands in the `success` bucket and writes a `queue` row.
#[tokio::test]
async fn post_candidates_success_path_writes_queue_row() {
    let Some((app, state)) = setup_with_campaign().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let pool = state.database.pool().clone();
    insert_codebase(&pool, "cand-ok-cb").await;

    let body = json!([{
        "codebase": "cand-ok-cb",
        "campaign": "test-campaign",
    }]);
    let req = Request::builder()
        .method(Method::POST)
        .uri("/candidates")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = get_body(response).await;
    let success = body["success"]
        .as_array()
        .expect("success must be an array");
    assert_eq!(success.len(), 1, "expected one success entry: {body}");
    let entry = &success[0];
    assert_eq!(entry["codebase"], "cand-ok-cb");
    assert_eq!(entry["campaign"], "test-campaign");
    for key in ["bucket", "change_set", "offset", "queue-id", "refresh"] {
        assert!(
            entry.get(key).is_some(),
            "success entry must include `{key}`, got {entry}"
        );
    }
    assert_eq!(body["unknown_campaigns"], json!([]));
    assert_eq!(body["unknown_codebases"], json!([]));
    assert_eq!(body["invalid_value"], json!([]));

    // The candidate row must be in the queue table now.
    let queue_row: Option<(String, String)> =
        sqlx::query_as("SELECT codebase, suite::text FROM queue WHERE codebase = 'cand-ok-cb'")
            .fetch_optional(&pool)
            .await
            .expect("queue lookup");
    let (codebase, suite) = queue_row.expect("expected queue row for cand-ok-cb");
    assert_eq!(codebase, "cand-ok-cb");
    assert_eq!(suite, "test-campaign");
}

/// `POST /candidates` with `value: 0` records the candidate in
/// `invalid_value` and does not queue it.
#[tokio::test]
async fn post_candidates_records_invalid_value() {
    let Some((app, state)) = setup_with_campaign().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let pool = state.database.pool().clone();
    insert_codebase(&pool, "cand-invalid-cb").await;

    let body = json!([{
        "codebase": "cand-invalid-cb",
        "campaign": "test-campaign",
        "value": 0,
    }]);
    let req = Request::builder()
        .method(Method::POST)
        .uri("/candidates")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = get_body(response).await;
    assert_eq!(body["success"], json!([]));
    assert_eq!(
        body["invalid_value"],
        json!([0]),
        "value=0 must land in invalid_value, got {body}"
    );

    // Nothing should have been queued.
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM queue WHERE codebase = 'cand-invalid-cb'")
            .fetch_one(&pool)
            .await
            .expect("queue count");
    assert_eq!(count, 0, "invalid-value candidate must not enter the queue");
}

/// `POST /candidates` referencing an unknown `publish-policy` records
/// the policy name in `unknown_publish_policies` (FK-violation
/// branch).
#[tokio::test]
async fn post_candidates_records_unknown_publish_policy() {
    let Some((app, state)) = setup_with_campaign().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let pool = state.database.pool().clone();
    insert_codebase(&pool, "cand-unk-pp-cb").await;

    let body = json!([{
        "codebase": "cand-unk-pp-cb",
        "campaign": "test-campaign",
        "publish-policy": "no-such-policy",
    }]);
    let req = Request::builder()
        .method(Method::POST)
        .uri("/candidates")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = get_body(response).await;
    assert_eq!(body["success"], json!([]));
    assert_eq!(
        body["unknown_publish_policies"],
        json!(["no-such-policy"]),
        "unknown publish policy must land in unknown_publish_policies, got {body}"
    );
}

/// `POST /candidates` for a known campaign but a codebase that does
/// not have a row in the `codebase` table records it in
/// `unknown_codebases` via the FK-violation branch.
#[tokio::test]
async fn post_candidates_records_unknown_codebase_via_fk_violation() {
    let Some((app, _state)) = setup_with_campaign().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    // Note: no insert_codebase() -- we WANT the FK to fail.
    let body = json!([{
        "codebase": "codebase-that-does-not-exist",
        "campaign": "test-campaign",
    }]);
    let req = Request::builder()
        .method(Method::POST)
        .uri("/candidates")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = get_body(response).await;
    assert_eq!(body["success"], json!([]));
    assert_eq!(
        body["unknown_codebases"],
        json!(["codebase-that-does-not-exist"]),
        "candidate for missing codebase FK must land in unknown_codebases, got {body}"
    );
}

/// `GET /health` returns the full JSON health report with the four
/// component checks (database, vcs, logs, artifacts) and an overall
/// `status`. Rust-side probe -- Python's /health returns plain "ok".
#[tokio::test]
async fn get_health_returns_component_report() {
    let Some((app, _state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let req = Request::builder()
        .method(Method::GET)
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = get_body(response).await;
    assert_eq!(body["service"], "janitor-runner");
    let checks = body["checks"].as_array().expect("checks must be an array");
    let names: Vec<&str> = checks.iter().filter_map(|c| c["name"].as_str()).collect();
    assert!(
        names.contains(&"database"),
        "expected database check, got {body}"
    );
    assert!(names.contains(&"logs"), "expected logs check, got {body}");
    assert!(names.contains(&"vcs"), "expected vcs check, got {body}");
    assert!(
        names.contains(&"artifacts"),
        "expected artifacts check, got {body}"
    );
}

/// `GET /health/live` is a cheap liveness probe: 200 + "alive" text.
#[tokio::test]
async fn get_health_live_returns_alive() {
    let Some((app, _state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let req = Request::builder()
        .method(Method::GET)
        .uri("/health/live")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(&bytes[..], b"alive");
}

/// `GET /health/ready` returns "ready" (200) when every component
/// health check reports healthy. The mock log manager always reports
/// healthy in tests, so this should be the expected state.
#[tokio::test]
async fn get_health_ready_returns_ready_when_healthy() {
    let Some((app, _state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let req = Request::builder()
        .method(Method::GET)
        .uri("/health/ready")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(&bytes[..], b"ready");
}

/// `GET /metrics` returns 200 with a Prometheus text-format response.
#[tokio::test]
async fn get_metrics_returns_prometheus_text() {
    let Some((app, _state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let req = Request::builder()
        .method(Method::GET)
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let ct = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .expect("content-type must be present")
        .to_string();
    assert!(
        ct.starts_with("text/plain"),
        "expected text/plain content-type for Prometheus, got {ct}"
    );
}

/// `GET /queue/stats` (public router) returns the four counters the
/// operator UI expects plus a `status` string.
#[tokio::test]
async fn get_public_queue_stats_returns_expected_fields() {
    let Some((app, _state)) = test_utils::create_public_test_app_with_state_if_available()
        .await
        .expect("public app setup should either succeed or return None cleanly")
    else {
        eprintln!("skipping: no test resources");
        return;
    };

    let req = Request::builder()
        .method(Method::GET)
        .uri("/queue/stats")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = get_body(response).await;
    for key in [
        "queue_length",
        "active_runs",
        "succeeded",
        "failed",
        "status",
    ] {
        assert!(
            body.get(key).is_some(),
            "expected `{key}` in /queue/stats response, got {body}"
        );
    }
}

/// `GET /watchdog/health` (public router) returns 200 + a
/// `health_statuses` array. With no active runs, the array is empty.
#[tokio::test]
async fn get_public_watchdog_health_returns_empty_when_idle() {
    let Some((app, _state)) = test_utils::create_public_test_app_with_state_if_available()
        .await
        .expect("public app setup should either succeed or return None cleanly")
    else {
        eprintln!("skipping: no test resources");
        return;
    };

    let req = Request::builder()
        .method(Method::GET)
        .uri("/watchdog/health")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = get_body(response).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["active_runs"], 0);
    assert_eq!(body["health_statuses"], json!([]));
}

/// A worker-authenticated route on the public router returns 401
/// when the client sends no `Authorization` header. Verifies the
/// `authenticate_worker` middleware refuses to fall through.
#[tokio::test]
async fn public_authed_route_without_auth_header_returns_401() {
    let Some((app, _state)) = test_utils::create_public_test_app_with_state_if_available()
        .await
        .expect("public app setup should either succeed or return None cleanly")
    else {
        eprintln!("skipping: no test resources");
        return;
    };

    let req = Request::builder()
        .method(Method::GET)
        .uri("/active-runs/any-id")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// A worker-authenticated route returns 401 when the client sends
/// credentials that don't match any row in the `worker` table.
#[tokio::test]
async fn public_authed_route_with_bad_credentials_returns_401() {
    let Some((app, _state)) = test_utils::create_public_test_app_with_state_if_available()
        .await
        .expect("public app setup should either succeed or return None cleanly")
    else {
        eprintln!("skipping: no test resources");
        return;
    };

    // "nosuch:wrong" base64-encoded.
    let auth = format!(
        "Basic {}",
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"nosuch:wrong")
    );
    let req = Request::builder()
        .method(Method::GET)
        .uri("/active-runs/any-id")
        .header("authorization", auth)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// `POST /active-runs/{id}/finish` on the public router returns 403
/// when the authenticated worker is different from the worker the
/// run is assigned to. Guards the cross-worker authorization branch.
#[tokio::test]
async fn public_finish_returns_403_when_worker_does_not_own_run() {
    let Some((app, state)) = test_utils::create_public_test_app_with_state_if_available()
        .await
        .expect("public app setup should either succeed or return None cleanly")
    else {
        eprintln!("skipping: no test resources");
        return;
    };

    // Seed two worker accounts; alice owns the run, bob will try to
    // finish it.
    state
        .auth_service
        .create_worker("alice", "alice-pw", None)
        .await
        .expect("create alice");
    state
        .auth_service
        .create_worker("bob", "bob-pw", None)
        .await
        .expect("create bob");

    let active_run = ActiveRun {
        worker_name: "alice".to_string(),
        worker_link: None,
        queue_id: 1,
        log_id: "run-owned-by-alice".to_string(),
        start_time: Utc::now(),
        finish_time: None,
        estimated_duration: None,
        campaign: "test-campaign".to_string(),
        change_set: None,
        command: "true".to_string(),
        codebase: "auth-test-cb".to_string(),
        backchannel: Backchannel::None {},
        vcs_info: VcsInfo::default(),
        instigated_context: None,
        resume_from: None,
    };
    state.active_runs.store(active_run).await;

    // Bob attempts the finish -- the mismatch check runs before any
    // multipart parsing, so an empty body still reaches the 403 branch.
    let auth = format!(
        "Basic {}",
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"bob:bob-pw")
    );
    let req = Request::builder()
        .method(Method::POST)
        .uri("/active-runs/run-owned-by-alice/finish")
        .header("authorization", auth)
        .header("content-type", "multipart/form-data; boundary=x")
        .body(Body::from("--x--\r\n"))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = get_body(response).await;
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|s| s.contains("Not authorized")),
        "expected `Not authorized` in error, got {body}"
    );

    let _ = state.active_runs.remove("run-owned-by-alice").await;
}

/// End-to-end lifecycle test: seed a codebase and campaign, enqueue
/// via POST /candidates, assign via POST /active-runs, then finish
/// via POST /active-runs/{id}/finish. Verifies the four handlers
/// agree on state shapes across the whole worker interaction.
///
/// This is the parity guard for the `test_submit_candidate` flow in
/// Python's `test_runner.py`.
#[tokio::test]
async fn end_to_end_assignment_lifecycle() {
    let Some((app, state)) = setup_with_campaign().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let pool = state.database.pool().clone();
    insert_codebase(&pool, "e2e-cb").await;
    state
        .auth_service
        .create_worker("e2e-worker", "e2e-pw", None)
        .await
        .expect("create worker");

    // Step 1: POST /candidates enqueues the candidate.
    let candidate_body = json!([{
        "codebase": "e2e-cb",
        "campaign": "test-campaign",
    }]);
    let req = Request::builder()
        .method(Method::POST)
        .uri("/candidates")
        .header("content-type", "application/json")
        .body(Body::from(candidate_body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let body = get_body(response).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "candidate upload should succeed, body: {body}"
    );

    // Step 2: POST /active-runs (private route) pulls the queue item
    // and hands the worker an assignment envelope.
    let assign_body = json!({"worker": "e2e-worker"});
    let req = Request::builder()
        .method(Method::POST)
        .uri("/active-runs")
        .header("content-type", "application/json")
        .body(Body::from(assign_body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "assignment should succeed after candidate upload; got {:?}",
        response.status()
    );
    let assignment = get_body(response).await;
    // Every field Python's assign response contract promises must
    // be present, so worker code that reads any of them will work
    // against this runner.
    for key in [
        "id",
        "queue_id",
        "campaign",
        "codebase",
        "branch",
        "resume",
        "target_repository",
        "codemod",
        "env",
        "build",
    ] {
        assert!(
            assignment.get(key).is_some(),
            "assignment must include `{key}`, got {assignment}"
        );
    }
    assert_eq!(assignment["campaign"], "test-campaign");
    assert_eq!(assignment["codebase"], "e2e-cb");
    let branch = &assignment["branch"];
    assert!(
        branch.get("default-empty").is_some(),
        "assignment.branch must include `default-empty`, got {branch}"
    );
    let run_id = assignment["id"].as_str().expect("id must be a string");

    // Step 3: POST /active-runs/{id}/finish with a minimal multipart
    // body containing just the JSON metadata. The upload processor
    // accepts a bare `metadata` field with the worker result JSON.
    let boundary = "e2e-boundary";
    let worker_result = r#"{"code":"success","description":"e2e"}"#;
    let multipart_body = format!(
        "--{boundary}\r\n\
         Content-Disposition: form-data; name=\"metadata\"; filename=\"result.json\"\r\n\
         Content-Type: application/json\r\n\r\n\
         {worker_result}\r\n\
         --{boundary}--\r\n"
    );
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/active-runs/{run_id}/finish"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(multipart_body))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    let status = response.status();
    let body = get_body(response).await;
    assert!(
        status == StatusCode::CREATED || status == StatusCode::OK,
        "finish should succeed, got {status} with body {body}"
    );

    // The run row is now in the database; verify the code we sent
    // in the worker result landed in `run.result_code`.
    let (result_code, codebase, campaign): (String, String, String) =
        sqlx::query_as("SELECT result_code, codebase, suite::text FROM run WHERE id = $1")
            .bind(run_id)
            .fetch_one(&pool)
            .await
            .expect("run row must exist after finish");
    assert_eq!(result_code, "success");
    assert_eq!(codebase, "e2e-cb");
    assert_eq!(campaign, "test-campaign");

    // ActiveRun should be gone from Redis after finish -- the run is
    // no longer in-flight, so /kill and /active-runs/{id} must 404.
    assert!(
        state.active_runs.get(run_id).await.is_none(),
        "active run should be dropped from Redis after finish"
    );
}

/// Seed a candidate ready to be assigned, then walk the setup that
/// `end_to_end_assignment_lifecycle` uses, and return the parsed
/// assignment body. Used by the tests below that assert on the wire
/// shape of the assign response without caring about the finish half.
async fn assign_one(app: axum::Router, state: &Arc<AppState>, codebase: &str) -> Value {
    let pool = state.database.pool().clone();
    insert_codebase(&pool, codebase).await;
    state
        .auth_service
        .create_worker("envelope-worker", "envelope-pw", None)
        .await
        .expect("create worker");

    let candidate_body = json!([{ "codebase": codebase, "campaign": "test-campaign" }]);
    let req = Request::builder()
        .method(Method::POST)
        .uri("/candidates")
        .header("content-type", "application/json")
        .body(Body::from(candidate_body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let assign_body = json!({"worker": "envelope-worker"});
    let req = Request::builder()
        .method(Method::POST)
        .uri("/active-runs")
        .header("content-type", "application/json")
        .body(Body::from(assign_body.to_string()))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    get_body(response).await
}

/// `POST /active-runs` response envelope contract: every documented
/// top-level field must be present with the right JSON type, and the
/// nested `branch` and `build` objects must have their subfields.
/// This is the parity guard for the highest-traffic route -- if a
/// field is renamed or dropped the worker breaks silently.
#[tokio::test]
async fn assign_response_envelope_has_all_documented_fields() {
    let Some((app, state)) = setup_with_campaign().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let assignment = assign_one(app, &state, "assign-envelope-cb").await;

    // Top-level fields.
    assert!(assignment["id"].is_string(), "id: {assignment}");
    assert!(assignment["queue_id"].is_number(), "queue_id: {assignment}");
    assert!(assignment["campaign"].is_string(), "campaign: {assignment}");
    assert!(assignment["codebase"].is_string(), "codebase: {assignment}");
    assert!(
        assignment["force-build"].is_boolean(),
        "force-build: {assignment}"
    );
    assert!(assignment["branch"].is_object(), "branch: {assignment}");
    assert!(
        assignment["resume"].is_object() || assignment["resume"].is_null(),
        "resume: {assignment}"
    );
    assert!(
        assignment["target_repository"].is_object(),
        "target_repository: {assignment}"
    );
    assert!(
        assignment["skip-setup-validation"].is_boolean(),
        "skip-setup-validation: {assignment}"
    );
    assert!(assignment["codemod"].is_object(), "codemod: {assignment}");
    assert!(assignment["env"].is_object(), "env: {assignment}");
    assert!(assignment["build"].is_object(), "build: {assignment}");

    // Legacy wrapper-shape keys we still emit for downstream code
    // that reads them (test fixtures, logs).
    for key in ["queue_item", "vcs_info", "active_run", "build_config"] {
        assert!(
            assignment.get(key).is_some(),
            "legacy `{key}` must remain in assign envelope, got {assignment}"
        );
    }

    // Branch subfields the worker's `DebianBuildConfig` reads. Types
    // may be null (branch_url can be absent for default_empty
    // campaigns) but the keys must be present.
    let branch = &assignment["branch"];
    for key in [
        "cached_url",
        "vcs_type",
        "url",
        "subpath",
        "additional_colocated_branches",
        "default-empty",
    ] {
        assert!(
            branch.get(key).is_some(),
            "branch.{key} must be present, got {branch}"
        );
    }
    assert!(
        branch["default-empty"].is_boolean(),
        "branch.default-empty must be a bool"
    );

    // Build subfields: target string, config object, environment map.
    let build = &assignment["build"];
    assert!(
        build["target"].as_str().is_some(),
        "build.target must be a string, got {build}"
    );
    assert!(
        build["config"].is_object(),
        "build.config must be an object, got {build}"
    );
    assert!(
        build["environment"].is_object(),
        "build.environment must be an object, got {build}"
    );

    // Codemod carries the command and per-execution env.
    let codemod = &assignment["codemod"];
    assert!(
        codemod["command"].is_array() || codemod["command"].is_string(),
        "codemod.command must be a string or array (Python emits string; we may split), got {codemod}"
    );
    assert!(
        codemod["environment"].is_object(),
        "codemod.environment must be an object"
    );
}

/// `GET /active-runs/+peek` returns 201 + the peek envelope when a
/// queue item is available, and 503 with `{reason: "queue empty"}`
/// when it isn't. The peek shape is
/// `{queue_item, vcs_info, build_config, estimated_duration}` --
/// smaller than a full assign response because peek doesn't reserve
/// or wire up a worker.
#[tokio::test]
async fn peek_returns_queue_item_shape_when_available() {
    let Some((app, state)) = setup_with_campaign().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let pool = state.database.pool().clone();
    insert_codebase(&pool, "peek-cb").await;
    // Enqueue via POST /candidates so the queue row + campaign
    // config are wired the same way an assign would see them.
    let body = json!([{ "codebase": "peek-cb", "campaign": "test-campaign" }]);
    let req = Request::builder()
        .method(Method::POST)
        .uri("/candidates")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::OK
    );

    let req = Request::builder()
        .method(Method::GET)
        .uri("/active-runs/+peek")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = get_body(response).await;
    for key in ["queue_item", "vcs_info", "build_config"] {
        assert!(
            body.get(key).is_some(),
            "peek envelope missing `{key}`, got {body}"
        );
    }
    // estimated_duration is optional (null when the queue row hasn't
    // been through a run yet), but the key must be present.
    assert!(
        body.get("estimated_duration").is_some(),
        "peek envelope missing `estimated_duration`, got {body}"
    );
    let queue_item = &body["queue_item"];
    assert_eq!(queue_item["codebase"], "peek-cb");
    assert_eq!(queue_item["campaign"], "test-campaign");
}

#[tokio::test]
async fn peek_returns_503_when_queue_empty() {
    let Some((app, _state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let req = Request::builder()
        .method(Method::GET)
        .uri("/active-runs/+peek")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = get_body(response).await;
    assert_eq!(body["reason"], "queue empty");
}

/// Helper: seed an ActiveRun so a `POST /finish` gets past the
/// run-lookup gate and into the multipart-parsing branch we want to
/// exercise. Returns the run_id.
async fn seed_active_run_for_finish(state: &Arc<AppState>, log_id: &str) {
    let run = ActiveRun {
        worker_name: "finish-worker".to_string(),
        worker_link: None,
        queue_id: 1,
        log_id: log_id.to_string(),
        start_time: Utc::now(),
        finish_time: None,
        estimated_duration: None,
        campaign: "test-campaign".to_string(),
        change_set: None,
        command: "true".to_string(),
        codebase: "finish-neg-cb".to_string(),
        backchannel: Backchannel::None {},
        vcs_info: VcsInfo::default(),
        instigated_context: None,
        resume_from: None,
    };
    state.active_runs.store(run).await;
}

/// `POST /finish` with a multipart body that has no `metadata` /
/// `worker_result` part must be rejected as 400. This is the
/// upload-processor `Missing worker_result field` validation branch.
#[tokio::test]
async fn finish_multipart_without_metadata_returns_400() {
    let Some((app, state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };
    let pool = state.database.pool().clone();
    insert_codebase(&pool, "finish-neg-cb").await;
    seed_active_run_for_finish(&state, "run-no-metadata").await;

    // Multipart with a single unrelated `file` part -- no `metadata`
    // or `worker_result` field.
    let boundary = "no-metadata-boundary";
    let body = format!(
        "--{boundary}\r\n\
         Content-Disposition: form-data; name=\"file\"; filename=\"stray.log\"\r\n\
         Content-Type: text/plain\r\n\r\n\
         hello\r\n\
         --{boundary}--\r\n"
    );
    let req = Request::builder()
        .method(Method::POST)
        .uri("/active-runs/run-no-metadata/finish")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = get_body(response).await;
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|s| s.contains("worker_result") || s.contains("Missing")),
        "expected `worker_result`/`Missing` in error, got {body}"
    );

    let _ = state.active_runs.remove("run-no-metadata").await;
}

/// `POST /finish` with malformed multipart (unterminated body) yields
/// a 400 with the multipart-parse error, not a 500 or a hang.
#[tokio::test]
async fn finish_malformed_multipart_returns_400() {
    let Some((app, state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };
    let pool = state.database.pool().clone();
    insert_codebase(&pool, "finish-neg-cb").await;
    seed_active_run_for_finish(&state, "run-malformed").await;

    // Boundary declared but body doesn't contain it -- axum's
    // multipart reader raises the parse error we translate to 400.
    let boundary = "malformed-boundary";
    let req = Request::builder()
        .method(Method::POST)
        .uri("/active-runs/run-malformed/finish")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from("this is not a valid multipart body"))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let _ = state.active_runs.remove("run-malformed").await;
}

/// `POST /finish` with a multipart part that has no `name=` in the
/// Content-Disposition header returns 400. Guards the upload
/// processor's `Field missing name` branch.
#[tokio::test]
async fn finish_multipart_field_without_name_returns_400() {
    let Some((app, state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };
    let pool = state.database.pool().clone();
    insert_codebase(&pool, "finish-neg-cb").await;
    seed_active_run_for_finish(&state, "run-no-field-name").await;

    // Content-Disposition without a `name=` attribute -- axum returns
    // None from field.name(), and upload_processor rejects it.
    let boundary = "no-name-boundary";
    let body = format!(
        "--{boundary}\r\n\
         Content-Disposition: form-data\r\n\r\n\
         orphan payload\r\n\
         --{boundary}--\r\n"
    );
    let req = Request::builder()
        .method(Method::POST)
        .uri("/active-runs/run-no-field-name/finish")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let _ = state.active_runs.remove("run-no-field-name").await;
}

/// Assignment scoring by priority: given three candidates with the
/// same success_chance but different `queue.priority`, the assign
/// call picks the LOWEST priority number first (`ORDER BY priority
/// ASC`). Guards the SQL scoring in `next_queue_item_with_scoring`.
#[tokio::test]
async fn assign_picks_lowest_priority_first() {
    let Some((app, state)) = setup_with_campaign().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let pool = state.database.pool().clone();
    insert_codebase(&pool, "prio-a").await;
    insert_codebase(&pool, "prio-b").await;
    insert_codebase(&pool, "prio-c").await;
    state
        .auth_service
        .create_worker("prio-worker", "pw", None)
        .await
        .expect("create worker");

    // Insert queue rows directly with explicit priorities. b has the
    // lowest priority so it should be picked first.
    for (cb, prio) in [("prio-a", 100i64), ("prio-b", 1), ("prio-c", 50)] {
        sqlx::query(
            "INSERT INTO queue (codebase, suite, command, priority)
             VALUES ($1, 'test-campaign', 'true', $2)",
        )
        .bind(cb)
        .bind(prio)
        .execute(&pool)
        .await
        .expect("insert queue row");
    }

    let assign_body = json!({"worker": "prio-worker"});
    let req = Request::builder()
        .method(Method::POST)
        .uri("/active-runs")
        .header("content-type", "application/json")
        .body(Body::from(assign_body.to_string()))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let assignment = get_body(response).await;
    assert_eq!(
        assignment["codebase"], "prio-b",
        "lowest-priority row (`prio-b`) should be picked first, got {assignment}"
    );
}

/// Assignment scoring by success_chance: three candidates with the
/// same priority but different `candidate.success_chance`. Score is
/// `success_chance * 100 + (100 - priority)`, so higher
/// success_chance wins. Guards the scoring join.
#[tokio::test]
async fn assign_prefers_higher_success_chance_within_same_priority() {
    let Some((app, state)) = setup_with_campaign().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let pool = state.database.pool().clone();
    for cb in ["score-a", "score-b", "score-c"] {
        insert_codebase(&pool, cb).await;
    }
    state
        .auth_service
        .create_worker("score-worker", "pw", None)
        .await
        .expect("create worker");

    // Candidates carry the success_chance the scoring SQL joins on.
    // score-b has the highest -> should win despite identical priority.
    for (cb, chance) in [("score-a", 0.1f64), ("score-b", 0.9), ("score-c", 0.5)] {
        sqlx::query(
            "INSERT INTO candidate (codebase, suite, command, success_chance)
             VALUES ($1, 'test-campaign', 'true', $2)",
        )
        .bind(cb)
        .bind(chance)
        .execute(&pool)
        .await
        .expect("insert candidate");
        sqlx::query(
            "INSERT INTO queue (codebase, suite, command, priority)
             VALUES ($1, 'test-campaign', 'true', 50)",
        )
        .bind(cb)
        .execute(&pool)
        .await
        .expect("insert queue row");
    }

    let assign_body = json!({"worker": "score-worker"});
    let req = Request::builder()
        .method(Method::POST)
        .uri("/active-runs")
        .header("content-type", "application/json")
        .body(Body::from(assign_body.to_string()))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let assignment = get_body(response).await;
    assert_eq!(
        assignment["codebase"], "score-b",
        "highest-success_chance row (`score-b`) should be picked first, got {assignment}"
    );
}

/// Redis HSETNX-backed claim on `assign_queue_item` is atomic: if
/// two callers race to claim the same queue_id, exactly one gets
/// Ok and the other gets an `already assigned` error. Exercises
/// the DB layer directly rather than the HTTP path, both because
/// production wiring includes Redis (test_utils' default does not,
/// to avoid contaminating the process-shared `assigned-queue-items`
/// hash across parallel tests) and because the invariant we care
/// about is at the storage boundary.
#[tokio::test]
async fn concurrent_assign_queue_item_never_double_claims() {
    // Redis is auto-provisioned by `ensure_redis` -- the same code
    // path `create_test_app_with_state_if_available` uses.
    test_utils::ensure_redis().await;
    let redis_url = match std::env::var("TEST_REDIS_URL") {
        Ok(url) => url,
        Err(_) => {
            eprintln!("skipping: TEST_REDIS_URL unset and no container");
            return;
        }
    };
    let Ok(redis_client) = redis::Client::open(redis_url.as_str()) else {
        eprintln!("skipping: cannot open redis at {redis_url}");
        return;
    };

    // Use a queue_id that is very unlikely to collide with anything
    // else parallel tests write to the shared `assigned-queue-items`
    // hash (production would namespace differently or reset between
    // deploys; tests share the hash so we pick a big prime).
    let queue_id: i64 = 99991;

    // Clean any leftover from a previous run so the first claim
    // actually observes an empty slot.
    if let Ok(mut conn) = redis_client.get_multiplexed_async_connection().await {
        use redis::AsyncCommands;
        let _: redis::RedisResult<i64> = conn
            .hdel("assigned-queue-items", queue_id.to_string())
            .await;
    } else {
        eprintln!("skipping: cannot reach redis to preclean");
        return;
    }

    // Build two DBs that share the underlying Redis so their HSETNX
    // targets collide (production wires one DB per process; the two
    // DBs here simulate two racing tasks in the same process).
    let pg_url =
        std::env::var("TEST_DATABASE_URL").unwrap_or_else(|_| "postgresql:///postgres".to_string());
    let Ok(pool) = sqlx::PgPool::connect(&pg_url).await else {
        eprintln!("skipping: no postgres");
        return;
    };
    let db1 = janitor_runner::database::RunnerDatabase::new_with_redis(
        pool.clone(),
        redis_client.clone(),
    );
    let db2 = janitor_runner::database::RunnerDatabase::new_with_redis(pool, redis_client);

    let (r1, r2) = tokio::join!(
        db1.assign_queue_item(queue_id, "race-w1", "log1"),
        db2.assign_queue_item(queue_id, "race-w2", "log2"),
    );

    let oks = [&r1, &r2].iter().filter(|r| r.is_ok()).count();
    let errs = [&r1, &r2].iter().filter(|r| r.is_err()).count();
    assert_eq!(
        oks, 1,
        "exactly one claim should succeed; r1={r1:?} r2={r2:?}"
    );
    assert_eq!(
        errs, 1,
        "exactly one claim should fail; r1={r1:?} r2={r2:?}"
    );
    let losing_err = if r1.is_err() { &r1 } else { &r2 };
    let msg = losing_err.as_ref().err().unwrap().to_string();
    assert!(
        msg.contains("already assigned"),
        "loser error should mention `already assigned`, got: {msg}"
    );
}

/// `POST /runs/{id}` publishes a `publish-status` message on Redis so
/// the publish service can react. Subscribe first, then POST, then
/// pull one message and assert its payload. Uses tokio::spawn for the
/// subscriber so it's listening before the publish fires.
#[tokio::test]
async fn post_update_run_publishes_publish_status_event() {
    let Some((app, state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let pool = state.database.pool().clone();
    insert_codebase(&pool, "pubsub-cb").await;
    sqlx::query("INSERT INTO change_set (id, campaign) VALUES ('cs-pubsub', 'test-campaign')")
        .execute(&pool)
        .await
        .expect("insert change_set");
    sqlx::query(
        "INSERT INTO run (
             id, suite, codebase, result_code, revision,
             start_time, finish_time, logfilenames, change_set
         )
         VALUES ('run-pubsub-1', 'test-campaign', 'pubsub-cb',
                 'success', 'rev-pubsub',
                 NOW() - INTERVAL '1 minute', NOW(), '{}', 'cs-pubsub')",
    )
    .execute(&pool)
    .await
    .expect("insert run");

    let Some(redis_client) = state.database.redis().cloned() else {
        eprintln!("skipping: no redis client on state");
        return;
    };

    // Subscribe on a dedicated connection BEFORE we publish. The
    // subscriber must be ready to see the message, so we drive it in
    // a spawned task and hand back a oneshot for the received payload.
    let (tx, rx) = tokio::sync::oneshot::channel::<Option<String>>();
    let sub_client = redis_client.clone();
    tokio::spawn(async move {
        use futures::StreamExt;
        let Ok(mut pubsub_conn) = sub_client.get_async_pubsub().await else {
            let _ = tx.send(None);
            return;
        };
        if pubsub_conn.subscribe("publish-status").await.is_err() {
            let _ = tx.send(None);
            return;
        }
        let mut stream = pubsub_conn.on_message();
        let payload = tokio::time::timeout(std::time::Duration::from_secs(3), stream.next())
            .await
            .ok()
            .flatten()
            .and_then(|msg| msg.get_payload::<String>().ok());
        let _ = tx.send(payload);
    });

    // Give the subscriber a moment to register with the redis server
    // before we fire the publish. Without this the SUBSCRIBE ack may
    // land after PUBLISH and the message is dropped.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let body = json!({"publish_status": "rejected"});
    let req = Request::builder()
        .method(Method::POST)
        .uri("/runs/run-pubsub-1")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let payload = rx
        .await
        .expect("subscriber task should send a result")
        .expect("expected a publish-status message within 3s");
    let parsed: Value = serde_json::from_str(&payload).expect("payload is JSON");
    assert_eq!(parsed["run_id"], "run-pubsub-1");
    assert_eq!(parsed["publish_status"], "rejected");
    assert_eq!(parsed["codebase"], "pubsub-cb");
    assert_eq!(parsed["campaign"], "test-campaign");
}

/// `GET /active-runs/{id}` on an unknown id returns 404 with
/// `{reason}`. The success path is covered indirectly by the assign
/// tests, which retrieve the newly-stored run via this endpoint.
#[tokio::test]
async fn get_active_run_returns_404_for_unknown_id() {
    let Some((app, _state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };
    let req = Request::builder()
        .method(Method::GET)
        .uri("/active-runs/does-not-exist")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = get_body(response).await;
    assert!(body["reason"]
        .as_str()
        .map(|s| s.contains("does-not-exist"))
        .unwrap_or(false));
}

/// `GET /active-runs/{id}/current-stage` on an unknown id returns 404
/// with `{reason}`. Rust-only endpoint (no Python parity) -- worth
/// nailing the shape now so future callers can rely on it.
#[tokio::test]
async fn current_stage_returns_404_for_unknown_id() {
    let Some((app, _state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };
    let req = Request::builder()
        .method(Method::GET)
        .uri("/active-runs/does-not-exist/current-stage")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = get_body(response).await;
    assert!(body["reason"]
        .as_str()
        .map(|s| s.contains("does-not-exist"))
        .unwrap_or(false));
}

/// When the active run exists but its backchannel is `None`, the
/// current-stage endpoint returns 200 with `{current_stage: null}`.
/// `Backchannel::None` succeeds with `Ok(None)` -- it doesn't
/// generate an error field. Contrast with the retriable/timeout paths
/// that only Polling backchannels can hit.
#[tokio::test]
async fn current_stage_returns_null_for_none_backchannel() {
    let Some((app, state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };
    let pool = state.database.pool().clone();
    insert_codebase(&pool, "finish-neg-cb").await;
    seed_active_run_for_finish(&state, "run-current-stage").await;

    let req = Request::builder()
        .method(Method::GET)
        .uri("/active-runs/run-current-stage/current-stage")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = get_body(response).await;
    assert!(body["current_stage"].is_null());
    assert!(
        body.get("error").is_none(),
        "None backchannel is a soft success, not an error"
    );
}

/// `GET /workers` returns the summary envelope even when the workers
/// table is empty: `{workers:[], total_workers:0, active_workers:0,
/// idle_workers:0, summary:{...}, timestamp}`.
#[tokio::test]
async fn get_workers_returns_summary_envelope_when_empty() {
    let Some((app, _state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };
    let req = Request::builder()
        .method(Method::GET)
        .uri("/workers")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = get_body(response).await;
    assert!(body["workers"].is_array());
    assert_eq!(body["total_workers"], 0);
    assert_eq!(body["active_workers"], 0);
    assert_eq!(body["idle_workers"], 0);
    assert_eq!(body["summary"]["total"], 0);
    assert_eq!(body["summary"]["active"], 0);
    assert_eq!(body["summary"]["idle"], 0);
    assert!(
        body["timestamp"].as_str().is_some(),
        "expected timestamp string"
    );
}

/// `POST /resume/check` for a nonexistent (campaign, revision) returns
/// `{resume_available: false, message: ...}`.
#[tokio::test]
async fn resume_check_returns_not_available_for_unknown_revision() {
    let Some((app, _state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };
    let body = json!({
        "campaign": "nonexistent-campaign",
        "resume_revision": "rev-does-not-exist",
        "codebase": "any",
    });
    let req = Request::builder()
        .method(Method::POST)
        .uri("/resume/check")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = get_body(response).await;
    assert_eq!(body["resume_available"], false);
    assert!(body["message"].as_str().is_some());
}

/// `GET /resume/validate` on a fresh empty DB reports `consistent`.
#[tokio::test]
async fn resume_validate_returns_consistent_on_empty_db() {
    let Some((app, _state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };
    let req = Request::builder()
        .method(Method::GET)
        .uri("/resume/validate")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = get_body(response).await;
    assert_eq!(body["status"], "consistent");
}

/// `GET /resume/chain/{run_id}` on an unknown run should return a
/// well-formed JSON object with `run_id` echoed back and either an
/// empty chain or an error field. This is a Rust-only endpoint so
/// there's no Python behavior to match -- pin the shape.
#[tokio::test]
async fn resume_chain_returns_shape_for_unknown_run() {
    let Some((app, _state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };
    let req = Request::builder()
        .method(Method::GET)
        .uri("/resume/chain/unknown-run")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = get_body(response).await;
    assert!(body.is_object());
    // Either the run_id echoes back with an (empty) chain, or the
    // handler reports an error field. Both are valid outputs.
    let has_run_id = body["run_id"].as_str() == Some("unknown-run");
    let has_error = body["error"].is_string();
    assert!(has_run_id || has_error, "unexpected body shape: {body}");
}

/// `POST /admin/runs/cleanup?result_code=X&dry_run=1` on an empty DB
/// returns a JSON envelope describing zero matches. Exercises the
/// dry-run query parameter parsing and the empty-result envelope
/// shape.
#[tokio::test]
async fn admin_cleanup_runs_dry_run_returns_empty_matches() {
    let Some((app, _state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };
    let req = Request::builder()
        .method(Method::POST)
        .uri("/admin/runs/cleanup?result_code=lintian&dry_run=1")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    // Handler should succeed (2xx). Exact shape is up to the impl,
    // but the response must be JSON.
    assert!(
        response.status().is_success(),
        "expected 2xx, got {}",
        response.status()
    );
    let body = get_body(response).await;
    assert!(body.is_object(), "expected JSON object, got {body}");
}

/// `POST /admin/runs/cleanup` without the required `result_code`
/// query param must be rejected as 4xx (axum's Query extractor
/// rejects when the required field is missing).
#[tokio::test]
async fn admin_cleanup_runs_missing_result_code_is_bad_request() {
    let Some((app, _state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };
    let req = Request::builder()
        .method(Method::POST)
        .uri("/admin/runs/cleanup")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert!(
        response.status().is_client_error(),
        "expected 4xx, got {}",
        response.status()
    );
}

/// The caller sends `exclude_hosts` containing the only candidate's
/// host; the queue filter must skip the candidate and the response
/// must be 503 (queue empty).
#[tokio::test]
async fn post_active_runs_honours_client_exclude_hosts() {
    let Some((app, state)) = setup_with_campaign().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let pool = state.database.pool().clone();
    insert_codebase(&pool, "excl-host-cb").await;
    state
        .auth_service
        .create_worker("excl-worker", "excl-pw", None)
        .await
        .expect("create worker");

    let candidate_body = json!([{
        "codebase": "excl-host-cb",
        "campaign": "test-campaign",
    }]);
    let req = Request::builder()
        .method(Method::POST)
        .uri("/candidates")
        .header("content-type", "application/json")
        .body(Body::from(candidate_body.to_string()))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::OK,
    );

    let assign_body = json!({
        "worker": "excl-worker",
        "exclude_hosts": ["example.invalid"],
    });
    let req = Request::builder()
        .method(Method::POST)
        .uri("/active-runs")
        .header("content-type", "application/json")
        .body(Body::from(assign_body.to_string()))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "queue should look empty when the only candidate's host is excluded"
    );
}
