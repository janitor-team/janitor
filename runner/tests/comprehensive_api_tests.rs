//! API parity tests between the Python and Rust runner implementations.

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
};
use serde::de::DeserializeOwned;
use std::sync::Arc;
use tower::ServiceExt;

use janitor_runner::api_types::{
    AssignRequest, QueuePositionEta, ReasonResponse, ScheduleControlRequest, ScheduledRun,
};
use janitor_runner::{test_utils, ActiveRunJson, AppState};

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

async fn get_json<T: DeserializeOwned>(response: axum::response::Response) -> T {
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
        .body(Body::from(
            serde_json::to_string(&AssignRequest {
                worker: Some(worker.to_string()),
                ..Default::default()
            })
            .unwrap(),
        ))
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
    assert_eq!(
        get_json::<ReasonResponse>(response).await,
        ReasonResponse {
            reason: "queue empty".to_string()
        }
    );
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

    // Should return an array of active runs.
    let _runs: Vec<ActiveRunJson> = get_json(response).await;
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
        get_json::<QueuePositionEta>(response).await,
        QueuePositionEta {
            position: None,
            wait_time: None,
            cumulative_wait_time: None,
        }
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
        get_json::<QueuePositionEta>(response).await,
        QueuePositionEta {
            position: Some(1),
            wait_time: None,
            cumulative_wait_time: Some(0.0),
        }
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
            serde_json::to_string(&ScheduleControlRequest {
                codebase: Some(janitor::CodebaseName::new("control-cb").unwrap()),
                main_branch_revision: Some("some-revid".to_string()),
                requester: Some("test".to_string()),
                ..Default::default()
            })
            .unwrap(),
        ))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let result: ScheduledRun = get_json(response).await;
    assert_eq!(result.campaign, "control");
    assert_eq!(result.codebase, "control-cb");

    let request = Request::builder()
        .method(Method::POST)
        .uri("/schedule-control")
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&ScheduleControlRequest {
                run_id: Some(janitor::RunId::new("no-such-run").unwrap()),
                requester: Some("test".to_string()),
                ..Default::default()
            })
            .unwrap(),
        ))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        get_json::<ReasonResponse>(response).await,
        ReasonResponse {
            reason: "Run not found".to_string()
        }
    );
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
        .body(Body::from(
            serde_json::to_string(&ScheduleControlRequest {
                run_id: Some(janitor::RunId::new("run-norev").unwrap()),
                ..Default::default()
            })
            .unwrap(),
        ))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        get_json::<ReasonResponse>(response).await,
        ReasonResponse {
            reason: "Run has no main branch revision".to_string()
        }
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
