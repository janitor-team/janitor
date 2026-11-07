//! Prometheus metrics for the runner.

use ::metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};
use janitor_service::metrics::BucketOverrides;

const HTTP_REQUESTS_TOTAL: &str = "janitor_runner_http_requests_total";
const HTTP_REQUEST_DURATION: &str = "janitor_runner_http_request_duration_seconds";
const ACTIVE_RUNS: &str = "active_runs";
const RUN_COUNT: &str = "run_count_total";
const ASSIGNMENT_COUNT: &str = "assignments_total";
const QUEUE_EMPTY_COUNT: &str = "queue_empty_total";
const RATE_LIMITED_COUNT: &str = "rate_limited_host_total";
const QUEUE_SIZE: &str = "janitor_runner_queue_size";
/// Matches the Python `last_success_gauge` exported via aiohttp_openmetrics
/// so existing dashboards keep working.
const LAST_SUCCESS_GAUGE: &str = "job_last_success_unixtime";
const RUN_RESULT_COUNT: &str = "result_total";
const BUILD_DURATION: &str = "build_duration";
const DATABASE_OPERATIONS_TOTAL: &str = "janitor_runner_database_operations_total";
const DATABASE_OPERATION_DURATION: &str = "janitor_runner_database_operation_duration_seconds";
const REDIS_OPERATIONS_TOTAL: &str = "janitor_runner_redis_operations_total";
const WORKER_HEALTH_CHECKS_TOTAL: &str = "janitor_runner_worker_health_checks_total";
const WATCHDOG_TERMINATED_RUNS_TOTAL: &str = "janitor_runner_watchdog_terminated_runs_total";
const RATE_LIMITED_HOSTS: &str = "janitor_runner_rate_limited_hosts";
const ARTIFACT_UPLOADS_TOTAL: &str = "janitor_runner_artifact_uploads_total";
const ARTIFACT_UPLOAD_SIZE_BYTES: &str = "janitor_runner_artifact_upload_size_bytes";
const VCS_OPERATIONS_TOTAL: &str = "janitor_runner_vcs_operations_total";
const VCS_OPERATION_DURATION: &str = "janitor_runner_vcs_operation_duration_seconds";
const VCS_ERRORS_TOTAL: &str = "janitor_runner_vcs_errors_total";
const ERROR_OCCURRENCES_TOTAL: &str = "janitor_runner_error_occurrences_total";
const MEMORY_USAGE_BYTES: &str = "janitor_runner_memory_usage_bytes";
const SYSTEM_INFO: &str = "janitor_runner_system_info";
const FORGE_LOGIN_REQUIRED_COUNT: &str = "forge_login_required";
const ARTIFACT_UPLOAD_FAILED_COUNT: &str = "artifact_upload_failed_total";

/// Histogram bucket overrides for the runner.
pub const BUCKETS: BucketOverrides = &[(
    VCS_OPERATION_DURATION,
    &[
        0.001, 0.002, 0.004, 0.008, 0.016, 0.032, 0.064, 0.128, 0.256, 0.512, 1.024, 2.048, 4.096,
        8.192, 16.384,
    ],
)];

fn describe() {
    describe_counter!(HTTP_REQUESTS_TOTAL, "Total number of HTTP requests");
    describe_histogram!(HTTP_REQUEST_DURATION, "HTTP request duration in seconds");
    describe_gauge!(ACTIVE_RUNS, "Number of active runs");
    describe_counter!(RUN_COUNT, "Number of runs executed.");
    describe_counter!(ASSIGNMENT_COUNT, "Number of assignments handed out");
    describe_counter!(
        QUEUE_EMPTY_COUNT,
        "Number of times the queue was empty when an assignment was requested"
    );
    describe_counter!(RATE_LIMITED_COUNT, "Rate limiting per host");
    describe_gauge!(QUEUE_SIZE, "Number of items in queue by campaign");
    describe_gauge!(
        LAST_SUCCESS_GAUGE,
        "Last time a batch job successfully finished"
    );
    describe_counter!(RUN_RESULT_COUNT, "Result counts");
    describe_histogram!(BUILD_DURATION, "Build duration");
    describe_counter!(
        DATABASE_OPERATIONS_TOTAL,
        "Total number of database operations"
    );
    describe_histogram!(
        DATABASE_OPERATION_DURATION,
        "Database operation duration in seconds"
    );
    describe_counter!(REDIS_OPERATIONS_TOTAL, "Total number of Redis operations");
    describe_counter!(
        WORKER_HEALTH_CHECKS_TOTAL,
        "Total number of worker health checks"
    );
    describe_counter!(
        WATCHDOG_TERMINATED_RUNS_TOTAL,
        "Total number of runs terminated by watchdog"
    );
    describe_gauge!(RATE_LIMITED_HOSTS, "Number of rate limited hosts");
    describe_counter!(ARTIFACT_UPLOADS_TOTAL, "Total number of artifact uploads");
    describe_counter!(
        ARTIFACT_UPLOAD_SIZE_BYTES,
        "Size of uploaded artifacts in bytes"
    );
    describe_counter!(VCS_OPERATIONS_TOTAL, "Total number of VCS operations");
    describe_histogram!(VCS_OPERATION_DURATION, "VCS operation duration in seconds");
    describe_counter!(
        VCS_ERRORS_TOTAL,
        "Total number of VCS errors by type and error code"
    );
    describe_counter!(
        ERROR_OCCURRENCES_TOTAL,
        "Total number of tracked errors by category, component, and severity"
    );
    describe_gauge!(MEMORY_USAGE_BYTES, "Memory usage in bytes");
    describe_gauge!(SYSTEM_INFO, "System information");
    describe_counter!(
        FORGE_LOGIN_REQUIRED_COUNT,
        "Number of times a resume-branch lookup failed for lack of forge credentials"
    );
    describe_counter!(
        ARTIFACT_UPLOAD_FAILED_COUNT,
        "Number of failed artifact uploads"
    );
}

/// Install the metrics recorder and register the runner's metrics.
pub fn install() {
    janitor_service::metrics::install(BUCKETS);
    register();
}

fn register() {
    describe();
    janitor::logs::describe_metrics();
    // Export these (as zero) before their first use, like the Python
    // runner did.
    counter!(RUN_COUNT).increment(0);
    counter!(QUEUE_EMPTY_COUNT).increment(0);
    gauge!(LAST_SUCCESS_GAUGE).increment(0.0);
}

/// Metrics collection helper functions
pub struct MetricsCollector;

impl MetricsCollector {
    /// Record HTTP request
    pub fn record_http_request(method: &str, endpoint: &str, status: u16, duration: f64) {
        counter!(
            HTTP_REQUESTS_TOTAL,
            "method" => method.to_string(),
            "endpoint" => endpoint.to_string(),
            "status" => status.to_string()
        )
        .increment(1);
        histogram!(
            HTTP_REQUEST_DURATION,
            "method" => method.to_string(),
            "endpoint" => endpoint.to_string()
        )
        .record(duration);
    }

    /// Record run completion
    pub fn record_run_completion(
        campaign: &janitor_types::Campaign,
        result_code: &str,
        duration: f64,
    ) {
        counter!(
            RUN_RESULT_COUNT,
            "campaign" => campaign.as_str().to_string(),
            "result_code" => result_code.to_string()
        )
        .increment(1);
        histogram!(BUILD_DURATION, "campaign" => campaign.as_str().to_string()).record(duration);
    }

    /// Record database operation
    pub fn record_database_operation(operation: &str, success: bool, duration: f64) {
        let status = if success { "success" } else { "error" };
        counter!(
            DATABASE_OPERATIONS_TOTAL,
            "operation" => operation.to_string(),
            "status" => status
        )
        .increment(1);
        histogram!(DATABASE_OPERATION_DURATION, "operation" => operation.to_string())
            .record(duration);
    }

    /// Record a failed Redis operation.
    pub fn record_redis_error(operation: &'static str) {
        counter!(REDIS_OPERATIONS_TOTAL, "operation" => operation, "status" => "error")
            .increment(1);
    }

    /// Record a run being handed out to a worker.
    pub fn record_run_assigned() {
        counter!(RUN_COUNT).increment(1);
    }

    /// Record an assignment request from a worker.
    pub fn record_assignment_request(worker: &str) {
        counter!(ASSIGNMENT_COUNT, "worker" => worker.to_string()).increment(1);
    }

    /// Record an assignment request that found the queue empty.
    pub fn record_queue_empty() {
        counter!(QUEUE_EMPTY_COUNT).increment(1);
    }

    /// Record a host that was found to be rate limiting us.
    pub fn record_rate_limited_host(host: &str) {
        counter!(RATE_LIMITED_COUNT, "host" => host.to_string()).increment(1);
    }

    /// Record that a run just finished successfully.
    pub fn record_last_success() {
        gauge!(LAST_SUCCESS_GAUGE).set(chrono::Utc::now().timestamp() as f64);
    }

    /// Record a run whose artifacts could not be stored.
    pub fn record_artifact_upload_failed() {
        counter!(ARTIFACT_UPLOAD_FAILED_COUNT).increment(1);
    }

    /// Record a resume-branch lookup that failed for lack of forge
    /// credentials.
    pub fn record_forge_login_required() {
        counter!(FORGE_LOGIN_REQUIRED_COUNT).increment(1);
    }

    /// Record a tracked error.
    pub fn record_error_occurrence(category: &str, component: &str, severity: &str) {
        counter!(
            ERROR_OCCURRENCES_TOTAL,
            "category" => category.to_string(),
            "component" => component.to_string(),
            "severity" => severity.to_string()
        )
        .increment(1);
    }

    /// Update active runs count
    pub fn set_active_runs(worker: &str, count: i64) {
        gauge!(ACTIVE_RUNS, "worker" => worker.to_string()).set(count as f64);
    }

    /// Update queue size
    pub fn set_queue_size(campaign: &janitor_types::Campaign, bucket: &str, size: i64) {
        gauge!(
            QUEUE_SIZE,
            "campaign" => campaign.as_str().to_string(),
            "bucket" => bucket.to_string()
        )
        .set(size as f64);
    }

    /// Record worker health check
    pub fn record_worker_health_check(worker: &str, healthy: bool) {
        let status = if healthy { "healthy" } else { "unhealthy" };
        counter!(
            WORKER_HEALTH_CHECKS_TOTAL,
            "worker" => worker.to_string(),
            "status" => status
        )
        .increment(1);
    }

    /// Record watchdog termination
    pub fn record_watchdog_termination(reason: &str) {
        counter!(WATCHDOG_TERMINATED_RUNS_TOTAL, "reason" => reason.to_string()).increment(1);
    }

    /// Update rate limited hosts
    pub fn set_rate_limited_hosts(host: &str, active: bool) {
        gauge!(RATE_LIMITED_HOSTS, "host" => host.to_string()).set(if active { 1.0 } else { 0.0 });
    }

    /// Record artifact upload
    pub fn record_artifact_upload(storage_type: &str, success: bool, size_bytes: u64) {
        let status = if success { "success" } else { "error" };
        counter!(
            ARTIFACT_UPLOADS_TOTAL,
            "storage_type" => storage_type.to_string(),
            "status" => status
        )
        .increment(1);
        if success {
            counter!(ARTIFACT_UPLOAD_SIZE_BYTES, "storage_type" => storage_type.to_string())
                .increment(size_bytes);
        }
    }

    /// Record VCS operation
    pub fn record_vcs_operation(operation: &str, vcs_type: &str, success: bool, duration: f64) {
        let status = if success { "success" } else { "error" };
        counter!(
            VCS_OPERATIONS_TOTAL,
            "operation" => operation.to_string(),
            "vcs_type" => vcs_type.to_string(),
            "status" => status
        )
        .increment(1);
        histogram!(
            VCS_OPERATION_DURATION,
            "vcs_type" => vcs_type.to_string(),
            "operation" => operation.to_string(),
            "status" => status
        )
        .record(duration);
    }

    /// Record the duration of a VCS operation.
    pub fn record_vcs_operation_duration(
        vcs_type: &str,
        operation: &str,
        status: &str,
        duration: std::time::Duration,
    ) {
        histogram!(
            VCS_OPERATION_DURATION,
            "vcs_type" => vcs_type.to_string(),
            "operation" => operation.to_string(),
            "status" => status.to_string()
        )
        .record(duration);
    }

    /// Record a VCS error.
    pub fn record_vcs_error(vcs_type: &str, error_code: &str) {
        counter!(
            VCS_ERRORS_TOTAL,
            "vcs_type" => vcs_type.to_string(),
            "error_code" => error_code.to_string()
        )
        .increment(1);
    }

    /// Update memory usage
    pub fn set_memory_usage(memory_type: &str, bytes: i64) {
        gauge!(MEMORY_USAGE_BYTES, "type" => memory_type.to_string()).set(bytes as f64);
    }

    /// Initialize system info metrics
    pub fn init_system_info() {
        let version = env!("CARGO_PKG_VERSION");
        let build_time = std::env::var("BUILD_TIME").unwrap_or_else(|_| "unknown".to_string());
        let rust_version = std::env::var("RUST_VERSION").unwrap_or_else(|_| "unknown".to_string());

        gauge!(
            SYSTEM_INFO,
            "version" => version,
            "build_time" => build_time,
            "rust_version" => rust_version
        )
        .set(1.0);
    }
}

/// Initialize metrics system.
pub fn init_metrics() {
    install();
    MetricsCollector::init_system_info();
    log::info!("Metrics system initialized");
}

/// Middleware for automatic HTTP metrics collection
pub async fn metrics_middleware(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let start = std::time::Instant::now();
    let method = request.method().to_string();
    let path = request.uri().path().to_string();

    let response = next.run(request).await;

    let duration = start.elapsed().as_secs_f64();
    let status = response.status().as_u16();

    MetricsCollector::record_http_request(&method, &path, status, duration);

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use janitor_service::testing::{capture, Snapshot, Value};

    #[test]
    fn unlabelled_metrics_exported_at_zero() {
        assert_eq!(
            capture(register),
            Snapshot::from([
                ("job_last_success_unixtime".to_string(), Value::Gauge(0.0)),
                ("queue_empty_total".to_string(), Value::Counter(0)),
                ("run_count_total".to_string(), Value::Counter(0)),
            ])
        );
    }

    #[test]
    fn record_vcs_operation() {
        assert_eq!(
            capture(|| MetricsCollector::record_vcs_operation("fetch", "git", false, 0.003)),
            Snapshot::from([
                (
                    "janitor_runner_vcs_operation_duration_seconds{vcs_type=\"git\",operation=\"fetch\",status=\"error\"}".to_string(),
                    Value::Histogram(vec![0.003])
                ),
                (
                    "janitor_runner_vcs_operations_total{operation=\"fetch\",vcs_type=\"git\",status=\"error\"}".to_string(),
                    Value::Counter(1)
                ),
            ])
        );
    }

    #[test]
    fn vcs_operation_duration_uses_exponential_buckets() {
        let recorder = janitor_service::metrics::builder(BUCKETS).build_recorder();
        ::metrics::with_local_recorder(&recorder, || {
            MetricsCollector::record_vcs_operation_duration(
                "git",
                "fetch",
                "success",
                std::time::Duration::from_millis(3),
            )
        });
        assert_eq!(
            recorder.handle().render(),
            r#"# TYPE janitor_runner_vcs_operation_duration_seconds histogram
janitor_runner_vcs_operation_duration_seconds_bucket{vcs_type="git",operation="fetch",status="success",le="0.001"} 0
janitor_runner_vcs_operation_duration_seconds_bucket{vcs_type="git",operation="fetch",status="success",le="0.002"} 0
janitor_runner_vcs_operation_duration_seconds_bucket{vcs_type="git",operation="fetch",status="success",le="0.004"} 1
janitor_runner_vcs_operation_duration_seconds_bucket{vcs_type="git",operation="fetch",status="success",le="0.008"} 1
janitor_runner_vcs_operation_duration_seconds_bucket{vcs_type="git",operation="fetch",status="success",le="0.016"} 1
janitor_runner_vcs_operation_duration_seconds_bucket{vcs_type="git",operation="fetch",status="success",le="0.032"} 1
janitor_runner_vcs_operation_duration_seconds_bucket{vcs_type="git",operation="fetch",status="success",le="0.064"} 1
janitor_runner_vcs_operation_duration_seconds_bucket{vcs_type="git",operation="fetch",status="success",le="0.128"} 1
janitor_runner_vcs_operation_duration_seconds_bucket{vcs_type="git",operation="fetch",status="success",le="0.256"} 1
janitor_runner_vcs_operation_duration_seconds_bucket{vcs_type="git",operation="fetch",status="success",le="0.512"} 1
janitor_runner_vcs_operation_duration_seconds_bucket{vcs_type="git",operation="fetch",status="success",le="1.024"} 1
janitor_runner_vcs_operation_duration_seconds_bucket{vcs_type="git",operation="fetch",status="success",le="2.048"} 1
janitor_runner_vcs_operation_duration_seconds_bucket{vcs_type="git",operation="fetch",status="success",le="4.096"} 1
janitor_runner_vcs_operation_duration_seconds_bucket{vcs_type="git",operation="fetch",status="success",le="8.192"} 1
janitor_runner_vcs_operation_duration_seconds_bucket{vcs_type="git",operation="fetch",status="success",le="16.384"} 1
janitor_runner_vcs_operation_duration_seconds_bucket{vcs_type="git",operation="fetch",status="success",le="+Inf"} 1
janitor_runner_vcs_operation_duration_seconds_sum{vcs_type="git",operation="fetch",status="success"} 0.003
janitor_runner_vcs_operation_duration_seconds_count{vcs_type="git",operation="fetch",status="success"} 1

"#
        );
    }
}
