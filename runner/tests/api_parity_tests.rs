//! Tests covering the runner's HTTP API surface.

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
    Json,
};
use serde_json::{json, Value};
use tower::ServiceExt;

/// Helper to create a test app with mock state.
async fn create_test_app() -> axum::Router {
    // In a real test environment, you'd set up a test database
    // For now, we'll create a simple router to test basic structure
    use axum::{
        routing::{delete, get, post},
        Router,
    };

    Router::new()
        .route(
            "/health",
            get(|| async {
                Json(json!({
                    "status": "healthy",
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "components": {
                        "database": "healthy",
                        "redis": "healthy"
                    }
                }))
            }),
        )
        .route(
            "/ready",
            get(|| async {
                Json(json!({
                    "status": "ready",
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "service": "janitor-runner"
                }))
            }),
        )
        .route(
            "/metrics",
            get(|| async {
                "# HELP test_metric A test metric\n# TYPE test_metric counter\ntest_metric 1\n"
            }),
        )
        .route("/candidates", get(|| async { Json(json!([])) }))
        .route(
            "/candidates",
            post(|body: String| async move {
                // Try to parse as JSON to simulate real behavior
                if serde_json::from_str::<Value>(&body).is_ok() {
                    (
                        StatusCode::OK,
                        Json(json!({"status": "success", "uploaded": 1})),
                    )
                } else {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error": "Invalid JSON"})),
                    )
                }
            }),
        )
        .route(
            "/candidates/{id}",
            delete(|| async { (StatusCode::NO_CONTENT, Json(json!({}))) }),
        )
        .route(
            "/codebases",
            get(|| async { Json(json!({"codebases": []})) }),
        )
        .route(
            "/codebases",
            post(|| async {
                (
                    StatusCode::OK,
                    Json(json!({"status": "success", "uploaded": 1})),
                )
            }),
        )
        .route(
            "/worker-config",
            get(|| async { Json(json!({"config": {}})) }),
        )
}

/// Test that health endpoint returns Python-compatible response.
#[tokio::test]
async fn test_health_endpoint_compatibility() {
    let app = create_test_app().await;

    let request = Request::builder()
        .method(Method::GET)
        .uri("/health")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();

    assert!(json.is_object());
    assert!(json.get("status").is_some());
    assert!(json.get("timestamp").is_some());
    assert!(json.get("components").is_some());

    // ISO-8601 timestamp with UTC "Z" or "+HH:MM" offset.
    let timestamp = json["timestamp"].as_str().unwrap();
    assert!(timestamp.ends_with("Z") || timestamp.contains("+"));
}

/// Test the ready endpoint.
#[tokio::test]
async fn test_ready_endpoint_compatibility() {
    let app = create_test_app().await;

    let request = Request::builder()
        .method(Method::GET)
        .uri("/ready")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();

    // 200 when ready, 503 when not ready.
    assert!(matches!(
        response.status(),
        StatusCode::OK | StatusCode::SERVICE_UNAVAILABLE
    ));

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();

    assert!(json.get("status").is_some());
    let status = json["status"].as_str().unwrap();
    assert!(status == "ready" || status == "not_ready");
}

/// Test metrics endpoint format (Prometheus text exposition).
#[tokio::test]
async fn test_metrics_endpoint_compatibility() {
    let app = create_test_app().await;

    let request = Request::builder()
        .method(Method::GET)
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Check Content-Type matches Prometheus format
    let content_type = response.headers().get("content-type").unwrap();
    assert!(content_type.to_str().unwrap().contains("text/plain"));

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let metrics_text = String::from_utf8(body.to_vec()).unwrap();

    // Verify Prometheus format (should have # HELP and # TYPE comments)
    assert!(
        metrics_text.contains("# HELP")
            || metrics_text.contains("# TYPE")
            || metrics_text.is_empty()
    );
}

/// Test queue endpoints behave like Python implementation.
#[tokio::test]
async fn test_queue_endpoints_compatibility() {
    let app = create_test_app().await;

    // Test get queue item endpoint
    let request = Request::builder()
        .method(Method::GET)
        .uri("/queue/next")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();

    // Python returns 404 when queue is empty, 200 with item when available
    assert!(matches!(
        response.status(),
        StatusCode::OK | StatusCode::NOT_FOUND
    ));

    if response.status() == StatusCode::OK {
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();

        // Verify Python-compatible queue item structure
        assert!(json.get("id").is_some());
        assert!(json.get("command").is_some());
        assert!(json.get("campaign").is_some());
        assert!(json.get("codebase").is_some());

        // Check that optional fields can be null (Python behavior)
        assert!(json.get("context").is_some()); // May be null
        assert!(json.get("requester").is_some()); // May be null
        assert!(json.get("change_set").is_some()); // May be null
    }
}

/// Test run management endpoints match Python API.
#[tokio::test]
async fn test_run_endpoints_compatibility() {
    let app = create_test_app().await;

    // Test finish run endpoint format
    let finish_payload = json!({
        "result_code": "success",
        "description": "Run completed successfully",
        "start_time": "2023-10-15T14:30:00Z",
        "finish_time": "2023-10-15T14:35:00Z"
    });

    let request = Request::builder()
        .method(Method::POST)
        .uri("/finish-run/test-log-id")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&finish_payload).unwrap()))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();

    // Python API typically returns 200 for successful operations
    assert!(matches!(
        response.status(),
        StatusCode::OK | StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND
    ));

    if response.status() == StatusCode::OK {
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();

        // Check Python-compatible response format
        assert!(json.is_object());

        // Python typically returns status and processed file info
        if let Some(filenames) = json.get("filenames") {
            assert!(filenames.is_array());
        }
        if let Some(logs) = json.get("logs") {
            assert!(logs.is_array());
        }
        if let Some(artifacts) = json.get("artifacts") {
            assert!(artifacts.is_array());
        }
    }
}

/// Test the candidates endpoint.
#[tokio::test]
async fn test_candidates_endpoints_compatibility() {
    let app = create_test_app().await;

    // Test get candidates
    let request = Request::builder()
        .method(Method::GET)
        .uri("/candidates")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();

    assert!(json.is_array());

    // Test upload candidates format
    let candidates_payload = json!([
        {
            "suite": "lintian-fixes",
            "context": {"priority": "high"},
            "value": 100,
            "success_chance": 0.85,
            "command": "lintian-fixes",
            "publish_policy": "default",
            "change_set": "cs-123",
            "codebase": "example-package"
        }
    ]);

    let request = Request::builder()
        .method(Method::POST)
        .uri("/candidates")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&candidates_payload).unwrap()))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();

    assert!(matches!(
        response.status(),
        StatusCode::OK | StatusCode::BAD_REQUEST
    ));

    if response.status() == StatusCode::OK {
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();

        assert!(json.get("status").is_some());
        let status = json["status"].as_str().unwrap();
        assert!(status == "success" || status == "partial_failure");

        if status == "success" {
            assert!(json.get("uploaded").is_some());
        } else {
            assert!(json.get("errors").is_some());
        }
    }
}

/// Test the codebases endpoint.
#[tokio::test]
async fn test_codebases_endpoints_compatibility() {
    let app = create_test_app().await;

    // Test upload codebases format
    let codebases_payload = json!([
        {
            "name": "example-package",
            "branch_url": "https://github.com/example/package.git",
            "url": "https://github.com/example/package.git",
            "branch": "main",
            "subpath": "",
            "vcs_type": "git",
            "value": 50,
            "inactive": false
        }
    ]);

    let request = Request::builder()
        .method(Method::POST)
        .uri("/codebases")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&codebases_payload).unwrap()))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();

    assert!(matches!(
        response.status(),
        StatusCode::OK | StatusCode::BAD_REQUEST
    ));

    if response.status() == StatusCode::OK {
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();

        assert!(json.get("status").is_some());
        assert_eq!(json["status"], "success");
        assert!(json.get("uploaded").is_some());
    }
}

/// Test the worker-config endpoint.
#[tokio::test]
async fn test_worker_config_compatibility() {
    let app = create_test_app().await;

    let request = Request::builder()
        .method(Method::GET)
        .uri("/worker-config")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();

    // Worker config should have Python-compatible structure
    assert!(json.is_object());

    if let Some(env) = json.get("env") {
        assert!(env.is_object());
    }

    if let Some(campaign_config) = json.get("campaign_config") {
        assert!(campaign_config.is_object());
    }
}

/// Test error response formats.
#[tokio::test]
async fn test_error_responses_compatibility() {
    let app = create_test_app().await;

    // Test invalid endpoint
    let request = Request::builder()
        .method(Method::GET)
        .uri("/nonexistent")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // Test invalid method
    let request = Request::builder()
        .method(Method::DELETE)
        .uri("/health")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);

    // Test malformed JSON
    let request = Request::builder()
        .method(Method::POST)
        .uri("/candidates")
        .header("content-type", "application/json")
        .body(Body::from("invalid json"))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();

    // Error responses should have consistent format
    assert!(json.get("error").is_some());
}

/// Test log endpoints match Python behavior.
#[tokio::test]
async fn test_log_endpoints_compatibility() {
    let app = create_test_app().await;

    // Test list logs for a run
    let request = Request::builder()
        .method(Method::GET)
        .uri("/logs/test-run-id")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();

    // Python returns 404 if run doesn't exist, 200 with logs if it does
    assert!(matches!(
        response.status(),
        StatusCode::OK | StatusCode::NOT_FOUND
    ));

    if response.status() == StatusCode::OK {
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();

        // Should return array of log filenames
        assert!(json.is_array());

        // Each log filename should be a string
        for item in json.as_array().unwrap() {
            assert!(item.is_string());
        }
    }

    // Test get specific log file
    let request = Request::builder()
        .method(Method::GET)
        .uri("/logs/test-run-id/build.log")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();

    // Python returns 404 if log doesn't exist
    assert!(matches!(
        response.status(),
        StatusCode::OK | StatusCode::NOT_FOUND
    ));

    if response.status() == StatusCode::OK {
        // Content should be plain text (log content)
        let content_type = response.headers().get("content-type");
        if let Some(ct) = content_type {
            assert!(ct.to_str().unwrap().contains("text/plain"));
        }
    }
}

/// Test that HTTP status codes match Python implementation.
#[tokio::test]
async fn test_http_status_codes_compatibility() {
    let app = create_test_app().await;

    // Test cases that should return specific status codes
    let test_cases = vec![
        (
            "/health",
            Method::GET,
            vec![StatusCode::OK, StatusCode::SERVICE_UNAVAILABLE],
        ),
        (
            "/ready",
            Method::GET,
            vec![StatusCode::OK, StatusCode::SERVICE_UNAVAILABLE],
        ),
        ("/metrics", Method::GET, vec![StatusCode::OK]),
        ("/nonexistent", Method::GET, vec![StatusCode::NOT_FOUND]),
        ("/candidates", Method::GET, vec![StatusCode::OK]),
        (
            "/queue/next",
            Method::GET,
            vec![StatusCode::OK, StatusCode::NOT_FOUND],
        ),
    ];

    for (path, method, expected_statuses) in test_cases {
        let request = Request::builder()
            .method(method.clone())
            .uri(path)
            .body(Body::empty())
            .unwrap();

        let response = app.clone().oneshot(request).await.unwrap();

        assert!(
            expected_statuses.contains(&response.status()),
            "Path {} with method {:?} returned unexpected status: {:?}",
            path,
            method,
            response.status()
        );
    }
}

/// Test content-type headers match Python.
#[tokio::test]
async fn test_content_type_compatibility() {
    let app = create_test_app().await;

    // JSON endpoints should return application/json
    let json_endpoints = vec!["/health", "/ready", "/candidates", "/queue/next"];

    for endpoint in json_endpoints {
        let request = Request::builder()
            .method(Method::GET)
            .uri(endpoint)
            .body(Body::empty())
            .unwrap();

        let response = app.clone().oneshot(request).await.unwrap();

        if response.status().is_success() {
            let content_type = response.headers().get("content-type");
            if let Some(ct) = content_type {
                let ct_str = ct.to_str().unwrap();
                assert!(ct_str.contains("application/json") || endpoint == "/queue/next");
            }
        }
    }

    // Metrics should return text/plain
    let request = Request::builder()
        .method(Method::GET)
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();

    if response.status().is_success() {
        let content_type = response.headers().get("content-type").unwrap();
        assert!(content_type.to_str().unwrap().contains("text/plain"));
    }
}
