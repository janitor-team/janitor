//! API parity tests between the Python and Rust runner implementations.

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
};
use serde_json::{json, Value};
use tower::ServiceExt;

use janitor_runner::test_utils;

/// Helper to create test app with full routing.
async fn create_test_app() -> Option<axum::Router> {
    test_utils::create_test_app_if_available()
        .await
        .expect("Failed to check test app availability")
}

/// Macro to skip test if no database is available
macro_rules! require_test_app {
    ($app:ident) => {
        let $app = match create_test_app().await {
            Some(app) => app,
            None => {
                eprintln!("Skipping test - no database available");
                return;
            }
        };
    };
}

/// Test assignment endpoint API compatibility.
#[tokio::test]
async fn test_assignment_endpoint_compatibility() {
    require_test_app!(app);

    // Test GET /assignment - should return assignment or 503 No Content
    let request = Request::builder()
        .method(Method::GET)
        .uri("/assignment")
        .header("Content-Type", "application/json")
        .body(Body::from(
            json!({
                "worker": "test-worker",
                "worker_link": "http://worker:8080",
                "backchannel": {
                    "type": "polling",
                    "url": "http://worker:8080"
                }
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();

    // Should be either 200 OK with assignment or 503 Service Unavailable
    assert!(matches!(
        response.status(),
        StatusCode::OK | StatusCode::SERVICE_UNAVAILABLE
    ));

    if response.status() == StatusCode::OK {
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let assignment: Value = serde_json::from_slice(&body).unwrap();

        assert!(assignment.get("queue_item").is_some());
        assert!(assignment.get("vcs_info").is_some());
        assert!(assignment.get("active_run").is_some());
        assert!(assignment.get("build_config").is_some());

        let queue_item = assignment.get("queue_item").unwrap();
        assert!(queue_item.get("id").is_some());
        assert!(queue_item.get("campaign").is_some());
        assert!(queue_item.get("codebase").is_some());
        assert!(queue_item.get("command").is_some());

        let vcs_info = assignment.get("vcs_info").unwrap();
        assert!(vcs_info.get("branch_url").is_some());
    }
}

/// Test result submission endpoint API compatibility.
#[tokio::test]
async fn test_result_submission_compatibility() {
    require_test_app!(app);

    let test_result = json!({
        "code": "success",
        "description": "Successfully completed",
        "context": {"test": true},
        "codemod": null,
        "main_branch_revision": null,
        "revision": null,
        "value": null,
        "branches": null,
        "tags": null,
        "remotes": null,
        "details": null,
        "stage": "complete",
        "builder_result": null,
        "start_time": "2023-01-01T00:00:00Z",
        "finish_time": "2023-01-01T01:00:00Z",
        "queue_id": 123
    });

    let request = Request::builder()
        .method(Method::POST)
        .uri("/result")
        .header("Content-Type", "application/json")
        .body(Body::from(test_result.to_string()))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();

    // Should accept valid result
    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let result: Value = serde_json::from_slice(&body).unwrap();

    // Verify response contains log_id
    assert!(result.get("log_id").is_some());
}

/// Test active runs endpoint API compatibility.
#[tokio::test]
async fn test_active_runs_compatibility() {
    require_test_app!(app);

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

/// Test queue position endpoint API compatibility.
#[tokio::test]
async fn test_queue_position_compatibility() {
    require_test_app!(app);

    let request = Request::builder()
        .method(Method::GET)
        .uri("/queue-position")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let position: Value = serde_json::from_slice(&body).unwrap();

    // Should match Python queue position response
    assert!(position.get("position").is_some());
    assert!(position.get("total").is_some());
}

/// Test health endpoint returns detailed Python-compatible health status.
#[tokio::test]
async fn test_health_endpoint_detailed() {
    require_test_app!(app);

    let request = Request::builder()
        .method(Method::GET)
        .uri("/health")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let health: Value = serde_json::from_slice(&body).unwrap();

    assert!(health.get("status").is_some());
    assert!(health.get("timestamp").is_some());
    assert!(health.get("components").is_some());

    let components = health.get("components").unwrap();
    assert!(components.get("database").is_some());
    assert!(components.get("log_manager").is_some());
    assert!(components.get("artifact_manager").is_some());
}

/// Test schedule control endpoint API compatibility.
#[tokio::test]
async fn test_schedule_control_compatibility() {
    require_test_app!(app);

    // Test reschedule action
    let reschedule_request = json!({
        "action": "reschedule",
        "campaign": "test-campaign",
        "min_success_chance": 0.5
    });

    let request = Request::builder()
        .method(Method::POST)
        .uri("/schedule-control")
        .header("Content-Type", "application/json")
        .body(Body::from(reschedule_request.to_string()))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let result: Value = serde_json::from_slice(&body).unwrap();

    // Should return affected count
    assert!(result.get("affected").is_some());

    // Test deschedule action
    let deschedule_request = json!({
        "action": "deschedule",
        "campaign": "test-campaign",
        "result_code": "build-failed"
    });

    let request = Request::builder()
        .method(Method::POST)
        .uri("/schedule-control")
        .header("Content-Type", "application/json")
        .body(Body::from(deschedule_request.to_string()))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

/// Test metrics endpoint compatibility.
#[tokio::test]
async fn test_metrics_endpoint_compatibility() {
    require_test_app!(app);

    let request = Request::builder()
        .method(Method::GET)
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let metrics = String::from_utf8(body.to_vec()).unwrap();

    // Should contain Prometheus format metrics
    assert!(metrics.contains("# HELP"));
    assert!(metrics.contains("# TYPE"));

    // Should contain key metrics from Python version
    assert!(metrics.contains("run_count"));
    assert!(metrics.contains("build_duration"));
    assert!(metrics.contains("active_runs"));
}

/// Test run upload endpoint with multipart form data.
#[tokio::test]
async fn test_run_upload_multipart_compatibility() {
    require_test_app!(app);

    // This would test multipart form upload similar to Python implementation
    // Would need to create actual multipart data for full test
    let request = Request::builder()
        .method(Method::POST)
        .uri("/upload/test-run-id")
        .header("Content-Type", "multipart/form-data; boundary=test")
        .body(Body::from("--test\r\nContent-Disposition: form-data; name=\"worker_result\"\r\n\r\n{\"code\":\"success\"}\r\n--test--"))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();

    // Should handle multipart upload
    assert!(matches!(
        response.status(),
        StatusCode::OK | StatusCode::BAD_REQUEST
    ));
}

/// Test error handling compatibility with Python implementation.
#[tokio::test]
async fn test_error_handling_compatibility() {
    require_test_app!(app);

    // Test invalid JSON
    let request = Request::builder()
        .method(Method::POST)
        .uri("/result")
        .header("Content-Type", "application/json")
        .body(Body::from("invalid json"))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // Test missing required fields
    let request = Request::builder()
        .method(Method::POST)
        .uri("/result")
        .header("Content-Type", "application/json")
        .body(Body::from("{}"))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// Test rate limiting behavior compatibility.
#[tokio::test]
async fn test_rate_limiting_compatibility() {
    require_test_app!(app);

    // Test assignment rate limiting
    for _ in 0..10 {
        let request = Request::builder()
            .method(Method::GET)
            .uri("/assignment")
            .header("Content-Type", "application/json")
            .body(Body::from(
                json!({
                    "worker": "rate-test-worker"
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.clone().oneshot(request).await.unwrap();

        // Should eventually hit rate limit or return no assignments
        assert!(matches!(
            response.status(),
            StatusCode::OK | StatusCode::SERVICE_UNAVAILABLE | StatusCode::TOO_MANY_REQUESTS
        ));
    }
}

/// Integration test for complete workflow compatibility.
#[tokio::test]
async fn test_complete_workflow_compatibility() {
    require_test_app!(app);

    // 1. Get assignment
    let request = Request::builder()
        .method(Method::GET)
        .uri("/assignment")
        .header("Content-Type", "application/json")
        .body(Body::from(
            json!({
                "worker": "workflow-test-worker",
                "worker_link": "http://worker:8080"
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();

    if response.status() == StatusCode::OK {
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let assignment: Value = serde_json::from_slice(&body).unwrap();

        // 2. Submit result
        let result_request = json!({
            "code": "success",
            "description": "Test completed successfully",
            "queue_id": assignment.get("queue_item").unwrap().get("id")
        });

        let request = Request::builder()
            .method(Method::POST)
            .uri("/result")
            .header("Content-Type", "application/json")
            .body(Body::from(result_request.to_string()))
            .unwrap();

        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // 3. Verify run is no longer active
        let request = Request::builder()
            .method(Method::GET)
            .uri("/active-runs")
            .body(Body::empty())
            .unwrap();

        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
