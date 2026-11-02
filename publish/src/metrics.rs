//! Prometheus metrics for the publisher.
//!
//! Metric names are the contract with the existing Grafana dashboards
//! and alerting rules, so they are unprefixed (no `janitor_publish_`).
//!
//! Not every counter has a call site wired up yet. Registering them
//! anyway means dashboards see the metric at zero rather than
//! "metric not found"; the call-site wiring is a follow-up task.

use lazy_static::lazy_static;
use prometheus::{
    register_counter, register_counter_vec, register_gauge, register_gauge_vec, register_histogram,
    Counter, CounterVec, Gauge, GaugeVec, Histogram,
};

lazy_static! {
    /// Delay between a build finishing and its result being published.
    pub static ref PUBLISH_LATENCY: Histogram = register_histogram!(
        "publish_latency",
        "Delay between build finish and publish."
    )
    .unwrap();

    /// Number of open proposals.
    pub static ref OPEN_PROPOSAL_COUNT: Gauge = register_gauge!(
        "open_proposal_count",
        "Number of open proposals."
    )
    .unwrap();

    /// Number of proposals per bucket (labelled by `bucket`).
    pub static ref BUCKET_PROPOSAL_COUNT: GaugeVec = register_gauge_vec!(
        "bucket_proposal_count",
        "Number of proposals per bucket.",
        &["bucket"]
    )
    .unwrap();

    /// Number of merge proposals by status (labelled by `status`).
    pub static ref MERGE_PROPOSAL_COUNT: GaugeVec = register_gauge_vec!(
        "merge_proposal_count",
        "Number of merge proposals by status.",
        &["status"]
    )
    .unwrap();

    /// Number of new merge proposals opened.
    pub static ref NEW_MERGE_PROPOSAL_COUNT: Counter = register_counter!(
        "new_merge_proposal_count_total",
        "Number of new merge proposals opened."
    )
    .unwrap();

    /// Last time pending changes were successfully published (unix ts).
    pub static ref LAST_PUBLISH_PENDING_SUCCESS: Gauge = register_gauge!(
        "last_publish_pending_success",
        "Last time pending changes were successfully published"
    )
    .unwrap();

    /// Last time existing merge proposals were successfully scanned (unix ts).
    pub static ref LAST_SCAN_EXISTING_SUCCESS: Gauge = register_gauge!(
        "last_scan_existing_success",
        "Last time existing merge proposals were successfully scanned"
    )
    .unwrap();

    /// Publishing skipped due to exponential backoff.
    pub static ref EXPONENTIAL_BACKOFF_COUNT: Counter = register_counter!(
        "exponential_backoff_count_total",
        "Number of times publishing has been skipped due to exponential backoff"
    )
    .unwrap();

    /// Pushes blocked because the per-cycle push limit was hit.
    pub static ref PUSH_LIMIT_COUNT: Counter = register_counter!(
        "push_limit_count_total",
        "Number of times pushes haven't happened due to the limit"
    )
    .unwrap();

    /// Runs not published due to a missing branch URL.
    pub static ref MISSING_BRANCH_URL_COUNT: Counter = register_counter!(
        "missing_branch_url_count_total",
        "Number of runs that weren't published because they had a missing branch URL"
    )
    .unwrap();

    /// Last merge proposal was rejected.
    pub static ref REJECTED_LAST_MP_COUNT: Counter = register_counter!(
        "rejected_last_mp_total",
        "Last merge proposal was rejected"
    )
    .unwrap();

    /// Runs not published due to missing publish mode (labelled by `role`).
    pub static ref MISSING_PUBLISH_MODE_COUNT: CounterVec = register_counter_vec!(
        "missing_publish_mode_count_total",
        "Number of runs not published due to missing publish mode",
        &["role"]
    )
    .unwrap();

    /// Branches not published because auxiliary branches aren't yet
    /// published (labelled by `role`).
    pub static ref UNPUBLISHED_AUX_BRANCHES_COUNT: CounterVec = register_counter_vec!(
        "unpublished_aux_branches_count_total",
        "Number of branches not published because auxiliary branches were not yet published",
        &["role"]
    )
    .unwrap();

    /// Runs not published because the codemod command changed.
    pub static ref COMMAND_CHANGED_COUNT: Counter = register_counter!(
        "command_changed_count_total",
        "Number of runs not published because the codemod command changed"
    )
    .unwrap();

    /// Runs not published because there were no result branches.
    pub static ref NO_RESULT_BRANCHES_COUNT: Counter = register_counter!(
        "no_result_branches_count_total",
        "Runs not published since there were no result branches"
    )
    .unwrap();

    /// Runs not published due to missing main result branch.
    pub static ref MISSING_MAIN_RESULT_BRANCH_COUNT: Counter = register_counter!(
        "missing_main_result_branch_count_total",
        "Runs not published because of missing main result branch"
    )
    .unwrap();

    /// Runs not published because the forge is rate-limiting us
    /// (labelled by `forge`).
    pub static ref FORGE_RATE_LIMITED_COUNT: CounterVec = register_counter_vec!(
        "forge_rate_limited_count",
        "Runs were not published because the relevant forge was rate-limiting",
        &["forge"]
    )
    .unwrap();

    /// Unexpected HTTP responses during checks of existing proposals.
    pub static ref UNEXPECTED_HTTP_RESPONSE_COUNT: Counter = register_counter!(
        "unexpected_http_response_count_total",
        "Number of unexpected HTTP responses during checks of existing proposals"
    )
    .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_latency_observe_increments_sample_count() {
        let before = PUBLISH_LATENCY.get_sample_count();
        PUBLISH_LATENCY.observe(1.5);
        let after = PUBLISH_LATENCY.get_sample_count();
        assert_eq!(after, before + 1);
    }
}
