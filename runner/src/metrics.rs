//! Prometheus metrics for the runner.

use lazy_static::lazy_static;
use prometheus::{
    register_counter_vec, register_gauge, register_gauge_vec, register_histogram_vec,
    register_int_counter, register_int_counter_vec, register_int_gauge_vec, CounterVec, Gauge,
    GaugeVec, HistogramVec, IntCounter, IntCounterVec, IntGaugeVec, TextEncoder,
};

lazy_static! {
    /// HTTP request metrics
    pub static ref HTTP_REQUESTS_TOTAL: IntCounterVec = register_int_counter_vec!(
        "janitor_runner_http_requests_total",
        "Total number of HTTP requests",
        &["method", "endpoint", "status"]
    ).unwrap();

    /// HTTP request duration
    pub static ref HTTP_REQUEST_DURATION: HistogramVec = register_histogram_vec!(
        "janitor_runner_http_request_duration_seconds",
        "HTTP request duration in seconds",
        &["method", "endpoint"]
    ).unwrap();

    /// Active runs gauge
    pub static ref ACTIVE_RUNS: IntGaugeVec = register_int_gauge_vec!(
        "janitor_runner_active_runs",
        "Number of currently active runs",
        &["worker"]
    ).unwrap();

    /// Queue metrics
    pub static ref QUEUE_SIZE: IntGaugeVec = register_int_gauge_vec!(
        "janitor_runner_queue_size",
        "Number of items in queue by campaign",
        &["campaign", "bucket"]
    ).unwrap();

    /// Unix timestamp at which the runner last successfully finished a
    /// run. Matches the Python `last_success_gauge` /
    /// `job_last_success_unixtime` exported via aiohttp_openmetrics so
    /// existing dashboards keep working.
    pub static ref LAST_SUCCESS_GAUGE: Gauge = register_gauge!(
        "job_last_success_unixtime",
        "Last time a batch job successfully finished"
    ).unwrap();

    /// Run completion metrics
    pub static ref RUNS_COMPLETED_TOTAL: IntCounterVec = register_int_counter_vec!(
        "janitor_runner_runs_completed_total",
        "Total number of completed runs",
        &["campaign", "result_code", "worker"]
    ).unwrap();

    /// Run duration
    pub static ref RUN_DURATION: HistogramVec = register_histogram_vec!(
        "janitor_runner_run_duration_seconds",
        "Run duration in seconds",
        &["campaign", "result_code"]
    ).unwrap();

    /// Database operation metrics
    pub static ref DATABASE_OPERATIONS_TOTAL: IntCounterVec = register_int_counter_vec!(
        "janitor_runner_database_operations_total",
        "Total number of database operations",
        &["operation", "status"]
    ).unwrap();

    /// Database operation duration
    pub static ref DATABASE_OPERATION_DURATION: HistogramVec = register_histogram_vec!(
        "janitor_runner_database_operation_duration_seconds",
        "Database operation duration in seconds",
        &["operation"]
    ).unwrap();

    /// Redis operation metrics
    pub static ref REDIS_OPERATIONS_TOTAL: IntCounterVec = register_int_counter_vec!(
        "janitor_runner_redis_operations_total",
        "Total number of Redis operations",
        &["operation", "status"]
    ).unwrap();

    /// Worker health metrics
    pub static ref WORKER_HEALTH_CHECKS_TOTAL: IntCounterVec = register_int_counter_vec!(
        "janitor_runner_worker_health_checks_total",
        "Total number of worker health checks",
        &["worker", "status"]
    ).unwrap();

    /// Watchdog metrics
    pub static ref WATCHDOG_TERMINATED_RUNS_TOTAL: IntCounterVec = register_int_counter_vec!(
        "janitor_runner_watchdog_terminated_runs_total",
        "Total number of runs terminated by watchdog",
        &["reason"]
    ).unwrap();

    /// Rate limiting metrics
    pub static ref RATE_LIMITED_HOSTS: IntGaugeVec = register_int_gauge_vec!(
        "janitor_runner_rate_limited_hosts",
        "Number of rate limited hosts",
        &["host"]
    ).unwrap();

    /// Artifact upload metrics
    pub static ref ARTIFACT_UPLOADS_TOTAL: IntCounterVec = register_int_counter_vec!(
        "janitor_runner_artifact_uploads_total",
        "Total number of artifact uploads",
        &["storage_type", "status"]
    ).unwrap();

    /// Artifact upload size
    pub static ref ARTIFACT_UPLOAD_SIZE_BYTES: CounterVec = register_counter_vec!(
        "janitor_runner_artifact_upload_size_bytes",
        "Size of uploaded artifacts in bytes",
        &["storage_type"]
    ).unwrap();

    /// Log file metrics
    pub static ref LOG_FILE_OPERATIONS_TOTAL: IntCounterVec = register_int_counter_vec!(
        "janitor_runner_log_file_operations_total",
        "Total number of log file operations",
        &["operation", "status"]
    ).unwrap();

    /// VCS operation metrics
    pub static ref VCS_OPERATIONS_TOTAL: IntCounterVec = register_int_counter_vec!(
        "janitor_runner_vcs_operations_total",
        "Total number of VCS operations",
        &["operation", "vcs_type", "status"]
    ).unwrap();

    /// VCS operation duration
    pub static ref VCS_OPERATION_DURATION: HistogramVec = register_histogram_vec!(
        "janitor_runner_vcs_operation_duration_seconds",
        "VCS operation duration in seconds",
        &["vcs_type", "operation", "status"],
        prometheus::exponential_buckets(0.001, 2.0, 15).unwrap()
    ).unwrap();

    /// VCS errors
    pub static ref VCS_ERRORS_TOTAL: IntCounterVec = register_int_counter_vec!(
        "janitor_runner_vcs_errors_total",
        "Total number of VCS errors by type and error code",
        &["vcs_type", "error_code"]
    ).unwrap();

    /// VCS branch cache hits
    pub static ref VCS_BRANCH_CACHE_HITS: IntCounterVec = register_int_counter_vec!(
        "janitor_runner_vcs_branch_cache_hits_total",
        "Total number of VCS branch cache hits",
        &["vcs_type"]
    ).unwrap();

    /// VCS branch cache misses
    pub static ref VCS_BRANCH_CACHE_MISSES: IntCounterVec = register_int_counter_vec!(
        "janitor_runner_vcs_branch_cache_misses_total",
        "Total number of VCS branch cache misses",
        &["vcs_type"]
    ).unwrap();

    /// Error tracking metrics
    pub static ref ERROR_OCCURRENCES_TOTAL: IntCounterVec = register_int_counter_vec!(
        "janitor_runner_error_occurrences_total",
        "Total number of tracked errors by category, component, and severity",
        &["category", "component", "severity"]
    ).unwrap();

    /// Error patterns detected
    pub static ref ERROR_PATTERNS_DETECTED: IntCounterVec = register_int_counter_vec!(
        "janitor_runner_error_patterns_detected_total",
        "Total number of error patterns detected",
        &["category", "component", "operation"]
    ).unwrap();

    /// Memory usage gauge
    pub static ref MEMORY_USAGE_BYTES: IntGaugeVec = register_int_gauge_vec!(
        "janitor_runner_memory_usage_bytes",
        "Memory usage in bytes",
        &["type"]
    ).unwrap();

    /// System metrics
    pub static ref SYSTEM_INFO: GaugeVec = register_gauge_vec!(
        "janitor_runner_system_info",
        "System information",
        &["version", "build_time", "rust_version"]
    ).unwrap();

    /// Resume-branch lookups that failed because the forge needed credentials.
    pub static ref FORGE_LOGIN_REQUIRED_COUNT: IntCounter = register_int_counter!(
        "forge_login_required",
        "Number of times a resume-branch lookup failed for lack of forge credentials"
    ).unwrap();
}

/// Metrics collection helper functions
pub struct MetricsCollector;

impl MetricsCollector {
    /// Record HTTP request
    pub fn record_http_request(method: &str, endpoint: &str, status: u16, duration: f64) {
        HTTP_REQUESTS_TOTAL
            .with_label_values(&[method, endpoint, &status.to_string()])
            .inc();
        HTTP_REQUEST_DURATION
            .with_label_values(&[method, endpoint])
            .observe(duration);
    }

    /// Record run completion
    pub fn record_run_completion(campaign: &str, result_code: &str, worker: &str, duration: f64) {
        RUNS_COMPLETED_TOTAL
            .with_label_values(&[campaign, result_code, worker])
            .inc();
        RUN_DURATION
            .with_label_values(&[campaign, result_code])
            .observe(duration);
    }

    /// Record database operation
    pub fn record_database_operation(operation: &str, success: bool, duration: f64) {
        let status = if success { "success" } else { "error" };
        DATABASE_OPERATIONS_TOTAL
            .with_label_values(&[operation, status])
            .inc();
        DATABASE_OPERATION_DURATION
            .with_label_values(&[operation])
            .observe(duration);
    }

    /// Update active runs count
    pub fn set_active_runs(worker: &str, count: i64) {
        ACTIVE_RUNS.with_label_values(&[worker]).set(count);
    }

    /// Update queue size
    pub fn set_queue_size(campaign: &str, bucket: &str, size: i64) {
        QUEUE_SIZE.with_label_values(&[campaign, bucket]).set(size);
    }

    /// Record worker health check
    pub fn record_worker_health_check(worker: &str, healthy: bool) {
        let status = if healthy { "healthy" } else { "unhealthy" };
        WORKER_HEALTH_CHECKS_TOTAL
            .with_label_values(&[worker, status])
            .inc();
    }

    /// Record watchdog termination
    pub fn record_watchdog_termination(reason: &str) {
        WATCHDOG_TERMINATED_RUNS_TOTAL
            .with_label_values(&[reason])
            .inc();
    }

    /// Update rate limited hosts
    pub fn set_rate_limited_hosts(host: &str, active: bool) {
        RATE_LIMITED_HOSTS
            .with_label_values(&[host])
            .set(if active { 1 } else { 0 });
    }

    /// Record artifact upload
    pub fn record_artifact_upload(storage_type: &str, success: bool, size_bytes: f64) {
        let status = if success { "success" } else { "error" };
        ARTIFACT_UPLOADS_TOTAL
            .with_label_values(&[storage_type, status])
            .inc();
        if success {
            ARTIFACT_UPLOAD_SIZE_BYTES
                .with_label_values(&[storage_type])
                .inc_by(size_bytes);
        }
    }

    /// Record VCS operation
    pub fn record_vcs_operation(operation: &str, vcs_type: &str, success: bool, duration: f64) {
        let status = if success { "success" } else { "error" };
        VCS_OPERATIONS_TOTAL
            .with_label_values(&[operation, vcs_type, status])
            .inc();
        VCS_OPERATION_DURATION
            .with_label_values(&[operation, vcs_type])
            .observe(duration);
    }

    /// Update memory usage
    pub fn set_memory_usage(memory_type: &str, bytes: i64) {
        MEMORY_USAGE_BYTES
            .with_label_values(&[memory_type])
            .set(bytes);
    }

    /// Collect and return all metrics in Prometheus format
    pub fn collect_metrics() -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let encoder = TextEncoder::new();
        let metric_families = prometheus::gather();
        Ok(encoder.encode_to_string(&metric_families)?)
    }

    /// Initialize system info metrics
    pub fn init_system_info() {
        let version = env!("CARGO_PKG_VERSION");
        let build_time = std::env::var("BUILD_TIME").unwrap_or_else(|_| "unknown".to_string());
        let rust_version = std::env::var("RUST_VERSION").unwrap_or_else(|_| "unknown".to_string());

        SYSTEM_INFO
            .with_label_values(&[version, &build_time, &rust_version])
            .set(1.0);
    }
}

/// Initialize metrics system.
pub fn init_metrics() {
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
    use super::FORGE_LOGIN_REQUIRED_COUNT;

    #[test]
    fn forge_login_required_counter_increments() {
        let before = FORGE_LOGIN_REQUIRED_COUNT.get();
        FORGE_LOGIN_REQUIRED_COUNT.inc();
        assert_eq!(FORGE_LOGIN_REQUIRED_COUNT.get(), before + 1);
    }
}
