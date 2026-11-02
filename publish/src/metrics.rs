//! Prometheus metrics for the publisher.
//!
//! Metric names are the contract with the existing Grafana dashboards
//! and alerting rules, so they are unprefixed (no `janitor_publish_`).
//!
//! Not every counter has a call site wired up yet. [`install`] registers
//! the unlabelled ones at zero so dashboards see the metric rather than
//! "metric not found"; the call-site wiring is a follow-up task.

use ::metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge};

/// Delay between a build finishing and its result being published.
pub const PUBLISH_LATENCY_SECONDS: &str = "publish_latency_seconds";

/// Number of open proposals.
pub const OPEN_PROPOSALS: &str = "open_proposals";

/// Number of proposals per bucket (labelled by `bucket`).
pub const BUCKET_PROPOSALS: &str = "bucket_proposals";

/// Number of merge proposals by status (labelled by `status`).
pub const MERGE_PROPOSALS: &str = "merge_proposals";

/// Number of new merge proposals opened.
pub const NEW_MERGE_PROPOSALS_TOTAL: &str = "new_merge_proposals_total";

/// Last time pending changes were successfully published (unix ts).
pub const LAST_PUBLISH_PENDING_SUCCESS_TIMESTAMP_SECONDS: &str =
    "last_publish_pending_success_timestamp_seconds";

/// Last time existing merge proposals were successfully scanned (unix ts).
pub const LAST_SCAN_EXISTING_SUCCESS_TIMESTAMP_SECONDS: &str =
    "last_scan_existing_success_timestamp_seconds";

/// Publishing skipped due to exponential backoff.
pub const EXPONENTIAL_BACKOFF_TOTAL: &str = "exponential_backoff_total";

/// Pushes blocked because the per-cycle push limit was hit.
pub const PUSH_LIMIT_TOTAL: &str = "push_limit_total";

/// Runs not published due to a missing branch URL.
pub const MISSING_BRANCH_URL_TOTAL: &str = "missing_branch_url_total";

/// Last merge proposal was rejected.
pub const REJECTED_LAST_MP_TOTAL: &str = "rejected_last_mp_total";

/// Runs not published due to missing publish mode (labelled by `role`).
pub const MISSING_PUBLISH_MODE_TOTAL: &str = "missing_publish_mode_total";

/// Branches not published because auxiliary branches aren't yet
/// published (labelled by `role`).
pub const UNPUBLISHED_AUX_BRANCHES_TOTAL: &str = "unpublished_aux_branches_total";

/// Runs not published because the codemod command changed.
pub const COMMAND_CHANGED_TOTAL: &str = "command_changed_total";

/// Runs not published because there were no result branches.
pub const NO_RESULT_BRANCHES_TOTAL: &str = "no_result_branches_total";

/// Runs not published due to missing main result branch.
pub const MISSING_MAIN_RESULT_BRANCH_TOTAL: &str = "missing_main_result_branch_total";

/// Runs not published because the forge is rate-limiting us
/// (labelled by `forge`).
pub const FORGE_RATE_LIMITED_TOTAL: &str = "forge_rate_limited_total";

/// Unexpected HTTP responses during checks of existing proposals.
pub const UNEXPECTED_HTTP_RESPONSES_TOTAL: &str = "unexpected_http_responses_total";

fn describe() {
    describe_histogram!(
        PUBLISH_LATENCY_SECONDS,
        "Delay between build finish and publish."
    );
    describe_gauge!(OPEN_PROPOSALS, "Number of open proposals.");
    describe_gauge!(BUCKET_PROPOSALS, "Number of proposals per bucket.");
    describe_gauge!(MERGE_PROPOSALS, "Number of merge proposals by status.");
    describe_counter!(
        NEW_MERGE_PROPOSALS_TOTAL,
        "Number of new merge proposals opened."
    );
    describe_gauge!(
        LAST_PUBLISH_PENDING_SUCCESS_TIMESTAMP_SECONDS,
        "Last time pending changes were successfully published"
    );
    describe_gauge!(
        LAST_SCAN_EXISTING_SUCCESS_TIMESTAMP_SECONDS,
        "Last time existing merge proposals were successfully scanned"
    );
    describe_counter!(
        EXPONENTIAL_BACKOFF_TOTAL,
        "Number of times publishing has been skipped due to exponential backoff"
    );
    describe_counter!(
        PUSH_LIMIT_TOTAL,
        "Number of times pushes haven't happened due to the limit"
    );
    describe_counter!(
        MISSING_BRANCH_URL_TOTAL,
        "Number of runs that weren't published because they had a missing branch URL"
    );
    describe_counter!(REJECTED_LAST_MP_TOTAL, "Last merge proposal was rejected");
    describe_counter!(
        MISSING_PUBLISH_MODE_TOTAL,
        "Number of runs not published due to missing publish mode"
    );
    describe_counter!(
        UNPUBLISHED_AUX_BRANCHES_TOTAL,
        "Number of branches not published because auxiliary branches were not yet published"
    );
    describe_counter!(
        COMMAND_CHANGED_TOTAL,
        "Number of runs not published because the codemod command changed"
    );
    describe_counter!(
        NO_RESULT_BRANCHES_TOTAL,
        "Runs not published since there were no result branches"
    );
    describe_counter!(
        MISSING_MAIN_RESULT_BRANCH_TOTAL,
        "Runs not published because of missing main result branch"
    );
    describe_counter!(
        FORGE_RATE_LIMITED_TOTAL,
        "Runs were not published because the relevant forge was rate-limiting"
    );
    describe_counter!(
        UNEXPECTED_HTTP_RESPONSES_TOTAL,
        "Number of unexpected HTTP responses during checks of existing proposals"
    );
}

/// Install the metrics recorder and register the publisher's metrics.
pub fn install() {
    janitor_service::metrics::install(&[]);
    register();
}

fn register() {
    describe();
    crate::middleware::describe_metrics();
    for name in [
        NEW_MERGE_PROPOSALS_TOTAL,
        EXPONENTIAL_BACKOFF_TOTAL,
        PUSH_LIMIT_TOTAL,
        MISSING_BRANCH_URL_TOTAL,
        REJECTED_LAST_MP_TOTAL,
        COMMAND_CHANGED_TOTAL,
        NO_RESULT_BRANCHES_TOTAL,
        MISSING_MAIN_RESULT_BRANCH_TOTAL,
        UNEXPECTED_HTTP_RESPONSES_TOTAL,
    ] {
        counter!(name).increment(0);
    }
    for name in [
        OPEN_PROPOSALS,
        LAST_PUBLISH_PENDING_SUCCESS_TIMESTAMP_SECONDS,
        LAST_SCAN_EXISTING_SUCCESS_TIMESTAMP_SECONDS,
    ] {
        gauge!(name).increment(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use janitor_service::testing::{capture, Snapshot, Value};

    #[test]
    fn unlabelled_metrics_registered_at_zero() {
        let counter = |name: &str| (name.to_string(), Value::Counter(0));
        let gauge = |name: &str| (name.to_string(), Value::Gauge(0.0));
        assert_eq!(
            capture(register),
            Snapshot::from([
                counter(COMMAND_CHANGED_TOTAL),
                counter(EXPONENTIAL_BACKOFF_TOTAL),
                gauge(LAST_PUBLISH_PENDING_SUCCESS_TIMESTAMP_SECONDS),
                gauge(LAST_SCAN_EXISTING_SUCCESS_TIMESTAMP_SECONDS),
                counter(MISSING_BRANCH_URL_TOTAL),
                counter(MISSING_MAIN_RESULT_BRANCH_TOTAL),
                counter(NEW_MERGE_PROPOSALS_TOTAL),
                counter(NO_RESULT_BRANCHES_TOTAL),
                gauge(OPEN_PROPOSALS),
                counter(PUSH_LIMIT_TOTAL),
                counter(REJECTED_LAST_MP_TOTAL),
                counter(UNEXPECTED_HTTP_RESPONSES_TOTAL),
            ])
        );
    }
}
