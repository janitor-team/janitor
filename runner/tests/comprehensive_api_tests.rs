//! API parity tests between the Python and Rust runner implementations.

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

use janitor_runner::{test_utils, AppState};

async fn setup() -> Option<(axum::Router, Arc<AppState>)> {
    test_utils::create_test_app_with_state_if_available()
        .await
        .expect("Failed to check test app availability")
}

/// Macro to skip test if no database is available
macro_rules! require_test_app {
    ($app:ident, $state:ident) => {
        let ($app, $state) = match setup().await {
            Some(pair) => pair,
            None => {
                eprintln!("Skipping test - no database available");
                return;
            }
        };
    };
}

async fn get_json(response: axum::response::Response) -> Value {
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

async fn insert_codebase(pool: &sqlx::PgPool, name: &str) {
    let url = format!("https://example.invalid/{name}");
    sqlx::query(
        "INSERT INTO codebase (name, branch_url, url, vcs_type)
         VALUES ($1, $2, $2, 'git')",
    )
    .bind(name)
    .bind(&url)
    .execute(pool)
    .await
    .expect("codebase insert");
}

async fn assign(app: &axum::Router, worker: &str) -> axum::response::Response {
    let request = Request::builder()
        .method(Method::POST)
        .uri("/active-runs")
        .header("Content-Type", "application/json")
        .body(Body::from(json!({"worker": worker}).to_string()))
        .unwrap();
    app.clone().oneshot(request).await.unwrap()
}

/// `POST /active-runs` on an empty queue returns 503, like Python's
/// `QueueEmpty` handling.
#[tokio::test]
async fn test_assign_queue_empty_compatibility() {
    require_test_app!(app, state);
    state
        .auth_service
        .create_worker("empty-queue-worker", "pw", None)
        .await
        .expect("create worker");

    let response = assign(&app, "empty-queue-worker").await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(get_json(response).await, json!({"reason": "queue empty"}));
}

/// Test active runs endpoint API compatibility.
#[tokio::test]
async fn test_active_runs_compatibility() {
    require_test_app!(app, _state);

    let request = Request::builder()
        .method(Method::GET)
        .uri("/active-runs")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let runs: Value = serde_json::from_slice(&body).unwrap();

    // Should return array of active runs
    assert!(runs.is_array());

    // Each run should have Python-compatible structure
    if let Some(runs_array) = runs.as_array() {
        for run in runs_array {
            assert!(run.get("worker_name").is_some());
            assert!(run.get("log_id").is_some());
            assert!(run.get("start_time").is_some());
            assert!(run.get("campaign").is_some());
            assert!(run.get("codebase").is_some());
        }
    }
}

/// `GET /queue/position` for a codebase that isn't queued.
#[tokio::test]
async fn test_queue_position_not_queued() {
    require_test_app!(app, _state);

    let request = Request::builder()
        .method(Method::GET)
        .uri("/queue/position?codebase=not-queued&campaign=test-campaign")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        get_json(response).await,
        json!({"position": null, "wait_time": null, "cumulative_wait_time": null})
    );
}

/// Python only reports a per-run `wait_time` when there are active
/// runs to divide the cumulative wait time over.
#[tokio::test]
async fn test_queue_position_without_active_runs() {
    require_test_app!(app, state);
    let pool = state.database.pool().clone();
    insert_codebase(&pool, "position-cb").await;
    sqlx::query(
        "INSERT INTO queue (codebase, suite, command, estimated_duration)
         VALUES ('position-cb', 'test-campaign', 'true', interval '10 seconds')",
    )
    .execute(&pool)
    .await
    .expect("insert queue row");

    let request = Request::builder()
        .method(Method::GET)
        .uri("/queue/position?codebase=position-cb&campaign=test-campaign")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        get_json(response).await,
        json!({"position": 1, "wait_time": null, "cumulative_wait_time": 0.0})
    );
}

/// `POST /schedule-control` takes the same body as Python's handler.
#[tokio::test]
async fn test_schedule_control_compatibility() {
    require_test_app!(app, state);
    let pool = state.database.pool().clone();
    insert_codebase(&pool, "control-cb").await;

    let request = Request::builder()
        .method(Method::POST)
        .uri("/schedule-control")
        .header("Content-Type", "application/json")
        .body(Body::from(
            json!({
                "codebase": "control-cb",
                "main_branch_revision": "some-revid",
                "requester": "test",
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let result = get_json(response).await;
    assert_eq!(result["campaign"], "control");
    assert_eq!(result["codebase"], "control-cb");
    assert!(result["queue_id"].is_i64(), "got {result}");

    let request = Request::builder()
        .method(Method::POST)
        .uri("/schedule-control")
        .header("Content-Type", "application/json")
        .body(Body::from(
            json!({"run_id": "no-such-run", "requester": "test"}).to_string(),
        ))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(get_json(response).await, json!({"reason": "Run not found"}));
}

/// `POST /schedule-control` with the `run_id` of a run that has no
/// `main_branch_revision` returns 400 and queues nothing.
#[tokio::test]
async fn test_schedule_control_run_without_main_branch_revision() {
    require_test_app!(app, state);
    let pool = state.database.pool().clone();
    insert_codebase(&pool, "control-norev-cb").await;
    sqlx::query("INSERT INTO change_set (id, campaign) VALUES ('cs-norev', 'test-campaign')")
        .execute(&pool)
        .await
        .expect("insert change_set");
    sqlx::query(
        "INSERT INTO run (
             id, suite, codebase, result_code,
             start_time, finish_time, logfilenames, change_set
         )
         VALUES ('run-norev', 'test-campaign', 'control-norev-cb', 'success',
                 NOW() - INTERVAL '1 minute', NOW(), '{}', 'cs-norev')",
    )
    .execute(&pool)
    .await
    .expect("insert run");

    let request = Request::builder()
        .method(Method::POST)
        .uri("/schedule-control")
        .header("Content-Type", "application/json")
        .body(Body::from(json!({"run_id": "run-norev"}).to_string()))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        get_json(response).await,
        json!({"reason": "Run has no main branch revision"})
    );

    let queued: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM queue WHERE codebase = 'control-norev-cb'")
            .fetch_one(&pool)
            .await
            .expect("count queue rows");
    assert_eq!(queued, 0);
}

/// The metrics the Python runner exported keep their names.
#[tokio::test]
async fn test_metrics_endpoint_compatibility() {
    require_test_app!(app, state);
    state
        .auth_service
        .create_worker("metrics-worker", "pw", None)
        .await
        .expect("create worker");
    let response = assign(&app, "metrics-worker").await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    let request = Request::builder()
        .method(Method::GET)
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let metrics = String::from_utf8(body.to_vec()).unwrap();

    for line in [
        "# TYPE run_count_total counter",
        "# TYPE queue_empty_total counter",
        "# TYPE job_last_success_unixtime gauge",
        "# TYPE assignments_total counter",
    ] {
        assert!(metrics.contains(line), "missing {line:?} in:\n{metrics}");
    }
    assert!(
        metrics
            .lines()
            .any(|l| l.starts_with("assignments_total{worker=\"metrics-worker\"} ")),
        "no assignments_total sample for metrics-worker in:\n{metrics}"
    );
}
