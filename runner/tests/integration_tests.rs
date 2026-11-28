//! Integration tests for the Janitor Runner.

#[tokio::test]
async fn test_metrics_collection() {
    // Test that metrics collection works

    use janitor_runner::metrics::MetricsCollector;

    use janitor_service::testing::{capture, Snapshot, Value};

    let snapshot = capture(|| {
        MetricsCollector::record_http_request("GET", "/test", 200, 0.1);
        MetricsCollector::record_database_operation("select", true, 0.05);
        MetricsCollector::set_active_runs("test-worker", 1);
    });
    assert_eq!(
        snapshot,
        Snapshot::from([
            (
                "active_runs{worker=\"test-worker\"}".to_string(),
                Value::Gauge(1.0)
            ),
            (
                "janitor_runner_database_operation_duration_seconds{operation=\"select\"}"
                    .to_string(),
                Value::Histogram(vec![0.05])
            ),
            (
                "janitor_runner_database_operations_total{operation=\"select\",status=\"success\"}"
                    .to_string(),
                Value::Counter(1)
            ),
            (
                "janitor_runner_http_request_duration_seconds{method=\"GET\",endpoint=\"/test\"}"
                    .to_string(),
                Value::Histogram(vec![0.1])
            ),
            (
                "janitor_runner_http_requests_total{method=\"GET\",endpoint=\"/test\",status=\"200\"}"
                    .to_string(),
                Value::Counter(1)
            ),
        ])
    );
}
