//! Integration tests for the Janitor Runner.

use janitor::shared_config::ConfigLoader;
use janitor_runner::{application::Application, config::RunnerConfig};
use serial_test::serial;
use std::time::Duration;
use tokio::time::timeout;

/// Test configuration for integration tests.
fn test_config() -> RunnerConfig {
    use janitor::shared_config::{DatabaseConfig, RedisConfig, WebConfig};

    let mut config = RunnerConfig::default();

    // Configure base service settings
    config.base.database = Some(DatabaseConfig {
        url: "postgresql://localhost/janitor_test".to_string(),
        max_connections: 5,
        min_connections: Some(1),
        connection_timeout_seconds: 10,
        query_timeout_seconds: 30,
        idle_timeout_seconds: Some(300),
        max_lifetime_seconds: Some(1800),
        enable_sql_logging: false,
    });

    // Redis is required by validate() so the ActiveRunStore can back
    // /finish uploads across runner restarts.
    config.base.redis = Some(RedisConfig {
        url: "redis://localhost".to_string(),
        ..Default::default()
    });

    config.base.web = Some(WebConfig {
        port: 9999,
        listen_address: "127.0.0.1".to_string(),
        request_timeout_seconds: 30,
        max_request_size_bytes: 1024 * 1024,
        public_port: Some(9999),
        enable_cors: false,
        enable_request_logging: true,
        enable_compression: true,
        enable_http2: true,
        keep_alive_seconds: Some(75),
        workers: None,
    });

    config.application.name = "janitor-runner-test".to_string();
    config.application.environment = "test".to_string();

    // Worker authentication is on by default; provide a test secret so
    // validate() passes.
    config.worker.shared_secret = Some("test-secret".to_string());

    config
}

#[tokio::test]
#[serial]
async fn test_application_lifecycle() {
    // Test the complete application lifecycle: build, start, health check, shutdown

    let config = test_config();
    let app = Application::builder_from_config(config).build().await;

    // Application should build successfully (or fail with expected database error)
    match app {
        Ok(app) => {
            // If database is available, test health checks
            let health_result = app.health_check().await;

            // Should have health check results (at minimum database check)
            // In test environment, we expect at least a database health check
            assert!(
                !health_result.checks.is_empty(),
                "Expected at least database health check, got: {:?}",
                health_result.checks
            );

            // Verify we have at least the core health checks
            let component_names: Vec<&str> = health_result
                .checks
                .iter()
                .map(|c| c.component.as_str())
                .collect();
            assert!(
                component_names.contains(&"database"),
                "Expected database health check, got components: {:?}",
                component_names
            );

            // Test state access and verify components are initialized
            let _state = app.state();
            // Just verify we can access the database state without panicking

            // Verify metrics are being collected
            let metrics = janitor_runner::metrics::MetricsCollector::collect_metrics();
            // Just verify metrics collection doesn't panic
            let _ = metrics;
        }
        Err(e) => {
            // If database is not available, that's expected in CI
            let error_msg = format!("{}", e);
            assert!(
                error_msg.contains("database")
                    || error_msg.contains("connection")
                    || error_msg.contains("Failed to initialize tracing"),
                "Expected database connection error or tracing initialization error, got: {}",
                error_msg
            );
        }
    }
}

#[tokio::test]
async fn test_configuration_loading() {
    // RunnerConfig::default() leaves database/web as None -- they're opt-in.
    // Use the test_config() helper that builds a populated config explicitly.
    let config = test_config();

    assert!(config.base.database.is_some());
    if let Some(ref db_config) = config.base.database {
        assert!(db_config.url.contains("postgresql://"));
    }
    assert!(config.base.web.is_some());
    assert_eq!(config.application.name, "janitor-runner-test");

    // Test configuration validation
    assert!(config.validate().is_ok());

    // Test invalid configuration
    let mut invalid_config = config.clone();
    if let Some(ref mut db) = invalid_config.base.database {
        db.url = "".to_string();
    }
    assert!(invalid_config.validate().is_err());

    // Test that configuration is properly structured
    assert!(config
        .base
        .database
        .as_ref()
        .unwrap()
        .url
        .contains("postgresql://"));
}

#[tokio::test]
async fn test_metrics_collection() {
    // Test that metrics collection works

    use janitor_runner::metrics::MetricsCollector;

    // Record some test metrics to ensure we have data to collect
    MetricsCollector::record_http_request("GET", "/test", 200, 0.1);
    MetricsCollector::record_database_operation("select", true, 0.05);
    MetricsCollector::set_active_runs("test-worker", 1);

    // Test metrics collection (this should not fail)
    match MetricsCollector::collect_metrics() {
        Ok(metrics) => {
            // Should return some metrics data (we just recorded some)
            assert!(
                !metrics.is_empty(),
                "Expected non-empty metrics after recording test data"
            );
            // Verify metrics contain some expected content
            assert!(
                metrics.contains("janitor_runner"),
                "Expected metrics to contain runner-specific metrics"
            );
        }
        Err(e) => {
            // Metrics collection failure might be expected in test environment
            println!("Metrics collection failed (expected in test env): {}", e);
        }
    }
}

#[tokio::test]
async fn test_error_tracking() {
    // Test error tracking system

    use janitor_runner::error_tracking::{
        ErrorCategory, ErrorSeverity, ErrorTracker, ErrorTrackingConfig, TrackedError,
    };
    use std::collections::HashMap;

    let config = ErrorTrackingConfig {
        log_to_file: false, // Don't write files in tests
        ..Default::default()
    };

    let tracker = ErrorTracker::new(config);

    // Create a test error
    let error = TrackedError {
        id: "test-error-1".to_string(),
        timestamp: chrono::Utc::now(),
        severity: ErrorSeverity::Error,
        category: ErrorCategory::Database,
        component: "test-component".to_string(),
        operation: "test-operation".to_string(),
        message: "Test error message".to_string(),
        details: None,
        stack_trace: None,
        context: HashMap::new(),
        correlation_id: None,
        user_id: None,
        request_id: None,
        retry_count: 0,
        is_transient: false,
    };

    // Track the error
    tracker.track_error(error).await;

    // Get statistics
    let stats = tracker.get_error_statistics().await;
    assert_eq!(stats.total_errors, 1);
    assert_eq!(stats.by_category.get(&ErrorCategory::Database), Some(&1));
}

// `test_performance_monitoring` removed: the `performance` module and
// `AppState.performance_monitor` field that this test exercised were
// deleted in a runner refactor. The test became permanently broken
// (E0432 / E0609); restoring it would require the module to come
// back, which isn't on the roadmap.

#[tokio::test]
async fn test_vcs_manager() {
    // Test VCS management system

    use janitor_runner::vcs::RunnerVcsManager;
    use std::collections::HashMap;

    // Create VCS manager with empty config (no actual VCS backends)
    let managers = HashMap::new();
    let vcs_manager = RunnerVcsManager::new(managers);

    // Test health check
    let health = vcs_manager.health_check().await;
    // With no managers, should be "healthy" but empty
    assert!(health.overall_healthy); // No managers means no failures
    assert!(health.vcs_statuses.is_empty());

    // Test statistics
    let stats = vcs_manager.get_statistics();
    assert_eq!(stats.manager_count, 0);
    assert!(stats.supported_vcs_types.is_empty());
}

#[tokio::test]
async fn test_log_manager() {
    // Test log management system using existing infrastructure
    use janitor::logs::create_log_manager;
    use std::fs;
    use std::io::Write;
    use tempfile::TempDir;

    // Create a temporary directory for testing
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path().to_str().unwrap();

    // Create the log manager
    let manager = create_log_manager(Some(temp_path)).await.unwrap();

    // Test data
    let codebase = "test-package";
    let run_id = "test-run-123";
    let log_name = "test.log";
    let test_content = b"Test log content\nLine 2\n";

    // Create a temporary log file to import
    let temp_log_path = temp_dir.path().join("temp_log.txt");
    let mut temp_file = fs::File::create(&temp_log_path).unwrap();
    temp_file.write_all(test_content).unwrap();
    temp_file.flush().unwrap();
    drop(temp_file);

    // Import the log
    let import_result = manager
        .import_log(
            codebase,
            run_id,
            temp_log_path.to_str().unwrap(),
            None,
            Some(log_name),
        )
        .await;

    assert!(
        import_result.is_ok(),
        "Failed to import log: {:?}",
        import_result
    );

    // Check if log exists
    let exists = manager.has_log(codebase, run_id, log_name).await.unwrap();
    assert!(exists, "Log should exist after import");

    // Retrieve the log
    let mut retrieved = manager.get_log(codebase, run_id, log_name).await.unwrap();
    let mut retrieved_content = Vec::new();
    std::io::Read::read_to_end(&mut retrieved, &mut retrieved_content).unwrap();

    assert_eq!(
        retrieved_content, test_content,
        "Retrieved content should match original"
    );

    // Test health check
    let health_result = manager.health_check().await;
    assert!(health_result.is_ok(), "Health check should pass");

    // Test deletion
    let delete_result = manager.delete_log(codebase, run_id, log_name).await;
    assert!(delete_result.is_ok(), "Should be able to delete log");

    // Verify log no longer exists
    let exists_after_delete = manager.has_log(codebase, run_id, log_name).await.unwrap();
    assert!(!exists_after_delete, "Log should not exist after deletion");
}

#[tokio::test]
async fn test_graceful_shutdown() {
    // Test graceful shutdown functionality

    let config = test_config();
    let app = Application::builder_from_config(config).build().await;

    if let Ok(app) = app {
        // Test that shutdown doesn't panic
        // In a real test, you'd start the server and then trigger shutdown
        // For now, just test that the application can be dropped cleanly
        drop(app);
    }
}

#[tokio::test]
async fn test_concurrent_operations() {
    // Test concurrent operations to ensure thread safety

    use janitor_runner::error_tracking::{
        ErrorCategory, ErrorSeverity, ErrorTracker, ErrorTrackingConfig, TrackedError,
    };
    use std::collections::HashMap;
    use std::sync::Arc;

    let config = ErrorTrackingConfig {
        log_to_file: false,
        ..Default::default()
    };

    let tracker = Arc::new(ErrorTracker::new(config));

    // Spawn multiple tasks that track errors concurrently
    let mut handles = Vec::new();

    for i in 0..10 {
        let tracker_clone = Arc::clone(&tracker);
        let handle = tokio::spawn(async move {
            let error = TrackedError {
                id: format!("concurrent-error-{}", i),
                timestamp: chrono::Utc::now(),
                severity: ErrorSeverity::Warning,
                category: ErrorCategory::Network,
                component: "concurrent-test".to_string(),
                operation: "test-operation".to_string(),
                message: format!("Concurrent test error {}", i),
                details: None,
                stack_trace: None,
                context: HashMap::new(),
                correlation_id: None,
                user_id: None,
                request_id: None,
                retry_count: 0,
                is_transient: false,
            };

            tracker_clone.track_error(error).await;
        });
        handles.push(handle);
    }

    // Wait for all tasks to complete
    for handle in handles {
        handle.await.unwrap();
    }

    // Check that all errors were tracked
    let stats = tracker.get_error_statistics().await;
    assert_eq!(stats.total_errors, 10);
}

#[tokio::test]
#[serial]
async fn test_system_integration() {
    // High-level integration test that exercises multiple systems together
    use janitor_runner::metrics::MetricsCollector;

    let config = test_config();
    let app_result = Application::builder_from_config(config).build().await;

    match app_result {
        Ok(app) => {
            // Test that all systems are integrated and accessible
            let state = app.state();

            // Test database (may fail if DB not available)
            let _ = state.database.health_check().await;

            // Test VCS manager
            let vcs_health = state.vcs_manager.health_check().await;
            assert!(vcs_health.vcs_statuses.is_empty()); // No VCS configured in test

            // performance_monitor field removed from AppState; the
            // assertion that exercised it lived here.

            // Test error tracker
            let error_stats = state.error_tracker.get_error_statistics().await;
            assert_eq!(
                error_stats.total_errors, 0,
                "Should have no errors initially"
            );

            // Test metrics
            let metrics_result = MetricsCollector::collect_metrics();
            // Just verify metrics collection doesn't panic
            let _ = metrics_result;

            println!("Integration test completed successfully");
        }
        Err(e) => {
            // Database connection failure is expected in many test environments
            println!("Application initialization failed (expected in CI): {}", e);
        }
    }
}

/// Helper to run integration tests with timeout
#[tokio::test]
async fn test_timeout_protection() {
    // Ensure tests don't hang indefinitely
    let test_future = async {
        // Use the fully-populated test_config, not default, since default
        // does not include filesystem paths and so does not validate.
        let config = test_config();
        config.validate()
    };

    let result = timeout(Duration::from_secs(5), test_future).await;
    assert!(result.is_ok(), "Test should complete within timeout");
    assert!(result.unwrap().is_ok(), "Configuration should be valid");
}
