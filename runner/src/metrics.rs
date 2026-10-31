//! Prometheus metrics for the runner.

use lazy_static::lazy_static;
use prometheus::{
    register_gauge, register_gauge_vec, register_histogram_vec, register_int_counter,
    register_int_counter_vec, register_int_gauge_vec, Gauge, GaugeVec, HistogramVec, IntCounter,
    IntCounterVec, IntGaugeVec, TextEncoder,
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
        "active_runs",
        "Number of active runs",
        &["worker"]
    ).unwrap();

    /// Number of runs handed out to workers.
    pub static ref RUN_COUNT: IntCounter = register_int_counter!(
        "run_count_total",
        "Number of runs executed."
    ).unwrap();

    /// Assignment requests, by worker.
    pub static ref ASSIGNMENT_COUNT: IntCounterVec = register_int_counter_vec!(
        "assignments_total",
        "Number of assignments handed out",
        &["worker"]
    ).unwrap();

    /// Assignment requests that found the queue empty.
    pub static ref QUEUE_EMPTY_COUNT: IntCounter = register_int_counter!(
        "queue_empty_total",
        "Number of times the queue was empty when an assignment was requested"
    ).unwrap();

    /// Hosts that were found to be rate limiting us.
    pub static ref RATE_LIMITED_COUNT: IntCounterVec = register_int_counter_vec!(
        "rate_limited_host_total",
        "Rate limiting per host",
        &["host"]
    ).unwrap();

    /// Unix timestamp at which the runner last successfully finished a
    /// run. Matches the Python `last_success_gauge` /
    /// `job_last_success_unixtime` exported via aiohttp_openmetrics so
    /// existing dashboards keep working.
    pub static ref LAST_SUCCESS_GAUGE: Gauge = register_gauge!(
        "job_last_success_unixtime",
        "Last time a batch job successfully finished"
    ).unwrap();

    /// Finished runs, by campaign and result code.
    pub static ref RUN_RESULT_COUNT: IntCounterVec = register_int_counter_vec!(
        "result_total",
        "Result counts",
        &["campaign", "result_code"]
    ).unwrap();

    /// Duration of finished runs, in seconds.
    pub static ref BUILD_DURATION: HistogramVec = register_histogram_vec!(
        "build_duration",
        "Build duration",
        &["campaign"]
    ).unwrap();

    /// Redis operation metrics
    pub static ref REDIS_OPERATIONS_TOTAL: IntCounterVec = register_int_counter_vec!(
        "janitor_runner_redis_operations_total",
        "Total number of Redis operations",
        &["operation", "status"]
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

    /// Error tracking metrics
    pub static ref ERROR_OCCURRENCES_TOTAL: IntCounterVec = register_int_counter_vec!(
        "janitor_runner_error_occurrences_total",
        "Total number of tracked errors by category, component, and severity",
        &["category", "component", "severity"]
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

    /// Runs whose artifacts could not be stored.
    pub static ref ARTIFACT_UPLOAD_FAILED_COUNT: IntCounter = register_int_counter!(
        "artifact_upload_failed_total",
        "Number of failed artifact uploads"
    ).unwrap();
}

/// Metrics collection helper functions
pub struct MetricsCollector;

impl MetricsCollector {
    /// Record run completion
    pub fn record_run_completion(campaign: &janitor::Campaign, result_code: &str, duration: f64) {
        RUN_RESULT_COUNT
            .with_label_values(&[campaign.as_str(), result_code])
            .inc();
        BUILD_DURATION
            .with_label_values(&[campaign.as_str()])
            .observe(duration);
    }

    /// Update active runs count
    pub fn set_active_runs(worker: &str, count: i64) {
        ACTIVE_RUNS.with_label_values(&[worker]).set(count);
    }

    /// Collect and return all metrics in Prometheus format
    pub fn collect_metrics() -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        register_unlabelled_metrics();
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

/// Register the metrics without labels, so that they are exported
/// (as zero) before their first use, like the Python runner did.
fn register_unlabelled_metrics() {
    lazy_static::initialize(&RUN_COUNT);
    lazy_static::initialize(&QUEUE_EMPTY_COUNT);
    lazy_static::initialize(&LAST_SUCCESS_GAUGE);
}

/// Initialize metrics system.
pub fn init_metrics() {
    MetricsCollector::init_system_info();
    log::info!("Metrics system initialized");
}
