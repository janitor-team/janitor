//! Prometheus metrics for the publisher.
//!
//! Metric names are the contract with the existing Grafana dashboards
//! and alerting rules, so the existing ones are unprefixed (no
//! `janitor_publish_`).
//!
//! Not every counter has a call site wired up yet. [`install`] registers
//! the unlabelled ones at zero so dashboards see the metric rather than
//! "metric not found"; the call-site wiring is a follow-up task.

use ::metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge};

/// Delay between a build finishing and its result being published.
pub const PUBLISH_LATENCY: &str = "publish_latency";

/// Number of open proposals.
pub const OPEN_PROPOSAL_COUNT: &str = "open_proposal_count";

/// Number of proposals per bucket (labelled by `bucket`).
pub const BUCKET_PROPOSAL_COUNT: &str = "bucket_proposal_count";

/// Number of merge proposals by status (labelled by `status`).
pub const MERGE_PROPOSAL_COUNT: &str = "merge_proposal_count";

/// Number of new merge proposals opened.
pub const NEW_MERGE_PROPOSAL_COUNT: &str = "new_merge_proposal_count";

/// Last time pending changes were successfully published (unix ts).
pub const LAST_PUBLISH_PENDING_SUCCESS: &str = "last_publish_pending_success";

/// Last time existing merge proposals were successfully scanned (unix ts).
pub const LAST_SCAN_EXISTING_SUCCESS: &str = "last_scan_existing_success";

/// Publishing skipped due to exponential backoff.
pub const EXPONENTIAL_BACKOFF_COUNT: &str = "exponential_backoff_count";

/// Pushes blocked because the per-cycle push limit was hit.
pub const PUSH_LIMIT_COUNT: &str = "push_limit_count";

/// Runs not published due to a missing branch URL.
pub const MISSING_BRANCH_URL_COUNT: &str = "missing_branch_url_count";

/// Last merge proposal was rejected. Unsuffixed name for dashboard
/// parity.
pub const REJECTED_LAST_MP_COUNT: &str = "rejected_last_mp";

/// Runs not published due to missing publish mode (labelled by `role`).
pub const MISSING_PUBLISH_MODE_COUNT: &str = "missing_publish_mode_count";

/// Branches not published because auxiliary branches aren't yet
/// published (labelled by `role`).
pub const UNPUBLISHED_AUX_BRANCHES_COUNT: &str = "unpublished_aux_branches_count";

/// Runs not published because the codemod command changed.
pub const COMMAND_CHANGED_COUNT: &str = "command_changed_count";

/// Runs not published because there were no result branches.
pub const NO_RESULT_BRANCHES_COUNT: &str = "no_result_branches_count";

/// Runs not published due to missing main result branch.
pub const MISSING_MAIN_RESULT_BRANCH_COUNT: &str = "missing_main_result_branch_count";

/// Runs not published because the forge is rate-limiting us
/// (labelled by `forge`).
pub const FORGE_RATE_LIMITED_COUNT: &str = "forge_rate_limited_count";

/// Unexpected HTTP responses during checks of existing proposals.
pub const UNEXPECTED_HTTP_RESPONSE_COUNT: &str = "unexpected_http_response_count";

/// Reviews in the review table (labelled by `reviewer` and `verdict`).
pub const REVIEWS: &str = "janitor_publish_reviews";

fn describe() {
    describe_histogram!(PUBLISH_LATENCY, "Delay between build finish and publish.");
    describe_gauge!(OPEN_PROPOSAL_COUNT, "Number of open proposals.");
    describe_gauge!(BUCKET_PROPOSAL_COUNT, "Number of proposals per bucket.");
    describe_gauge!(MERGE_PROPOSAL_COUNT, "Number of merge proposals by status.");
    describe_counter!(
        NEW_MERGE_PROPOSAL_COUNT,
        "Number of new merge proposals opened."
    );
    describe_gauge!(
        LAST_PUBLISH_PENDING_SUCCESS,
        "Last time pending changes were successfully published"
    );
    describe_gauge!(
        LAST_SCAN_EXISTING_SUCCESS,
        "Last time existing merge proposals were successfully scanned"
    );
    describe_counter!(
        EXPONENTIAL_BACKOFF_COUNT,
        "Number of times publishing has been skipped due to exponential backoff"
    );
    describe_counter!(
        PUSH_LIMIT_COUNT,
        "Number of times pushes haven't happened due to the limit"
    );
    describe_counter!(
        MISSING_BRANCH_URL_COUNT,
        "Number of runs that weren't published because they had a missing branch URL"
    );
    describe_counter!(REJECTED_LAST_MP_COUNT, "Last merge proposal was rejected");
    describe_counter!(
        MISSING_PUBLISH_MODE_COUNT,
        "Number of runs not published due to missing publish mode"
    );
    describe_counter!(
        UNPUBLISHED_AUX_BRANCHES_COUNT,
        "Number of branches not published because auxiliary branches were not yet published"
    );
    describe_counter!(
        COMMAND_CHANGED_COUNT,
        "Number of runs not published because the codemod command changed"
    );
    describe_counter!(
        NO_RESULT_BRANCHES_COUNT,
        "Runs not published since there were no result branches"
    );
    describe_counter!(
        MISSING_MAIN_RESULT_BRANCH_COUNT,
        "Runs not published because of missing main result branch"
    );
    describe_counter!(
        FORGE_RATE_LIMITED_COUNT,
        "Runs were not published because the relevant forge was rate-limiting"
    );
    describe_counter!(
        UNEXPECTED_HTTP_RESPONSE_COUNT,
        "Number of unexpected HTTP responses during checks of existing proposals"
    );
    describe_gauge!(
        REVIEWS,
        "Reviews in the review table, by verdict and reviewer."
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
        NEW_MERGE_PROPOSAL_COUNT,
        EXPONENTIAL_BACKOFF_COUNT,
        PUSH_LIMIT_COUNT,
        MISSING_BRANCH_URL_COUNT,
        REJECTED_LAST_MP_COUNT,
        COMMAND_CHANGED_COUNT,
        NO_RESULT_BRANCHES_COUNT,
        MISSING_MAIN_RESULT_BRANCH_COUNT,
        UNEXPECTED_HTTP_RESPONSE_COUNT,
    ] {
        counter!(name).increment(0);
    }
    for name in [
        OPEN_PROPOSAL_COUNT,
        LAST_PUBLISH_PENDING_SUCCESS,
        LAST_SCAN_EXISTING_SUCCESS,
    ] {
        gauge!(name).increment(0.0);
    }
}

/// Verdict and reviewer pairs set by the last [`refresh_review_metrics`].
static EXPORTED_REVIEW_LABELS: std::sync::Mutex<Vec<(String, String)>> =
    std::sync::Mutex::new(Vec::new());

/// Export the review counts from the review table to the metrics.
pub async fn refresh_review_metrics(conn: &sqlx::PgPool) -> Result<(), sqlx::Error> {
    export_review_counts_to_metrics(count_reviews_per_reviewer(conn).await?);
    Ok(())
}

async fn count_reviews_per_reviewer(
    conn: &sqlx::PgPool,
) -> Result<Vec<(String, String, i64)>, sqlx::Error> {
    sqlx::query_as::<_, (String, String, i64)>(
        "SELECT verdict::text, reviewer, count(*) FROM review GROUP BY verdict, reviewer",
    )
    .fetch_all(conn)
    .await
}

fn export_review_counts_to_metrics(rows: Vec<(String, String, i64)>) {
    let mut labels = EXPORTED_REVIEW_LABELS.lock().unwrap();
    for (verdict, reviewer) in labels.drain(..) {
        gauge!(REVIEWS, "reviewer" => reviewer, "verdict" => verdict).set(0.0);
    }
    for (verdict, reviewer, count) in rows {
        gauge!(REVIEWS, "reviewer" => reviewer.clone(), "verdict" => verdict.clone())
            .set(count as f64);
        labels.push((verdict, reviewer));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use janitor::test_with_database;
    use janitor_service::testing::{capture, Snapshot, Value};

    #[test]
    fn unlabelled_metrics_registered_under_python_names() {
        let counter = |name: &str| (name.to_string(), Value::Counter(0));
        let gauge = |name: &str| (name.to_string(), Value::Gauge(0.0));
        assert_eq!(
            capture(register),
            Snapshot::from([
                counter("command_changed_count"),
                counter("exponential_backoff_count"),
                gauge("last_publish_pending_success"),
                gauge("last_scan_existing_success"),
                counter("missing_branch_url_count"),
                counter("missing_main_result_branch_count"),
                counter("new_merge_proposal_count"),
                counter("no_result_branches_count"),
                gauge("open_proposal_count"),
                counter("push_limit_count"),
                counter("rejected_last_mp"),
                counter("unexpected_http_response_count"),
            ])
        );
    }

    test_with_database! {
        schema = Core;
        async fn test_review_metrics(test_db: TestDatabase) {
            let pool = test_db.pool();
            for query in [
                "INSERT INTO codebase (name) VALUES ('cb')",
                "INSERT INTO change_set (id, campaign) VALUES ('cs-1', 'lintian-fixes')",
                "INSERT INTO run (
                    id, codebase, campaign, command, result_code, change_set, logfilenames
                 ) VALUES
                    ('run-1', 'cb', 'lintian-fixes', 'noop', 'success', 'cs-1', '{}'),
                    ('run-2', 'cb', 'lintian-fixes', 'noop', 'success', 'cs-1', '{}')",
                "INSERT INTO review (run_id, reviewer, verdict) VALUES
                    ('run-1', 'a@example.com', 'approved'),
                    ('run-2', 'a@example.com', 'approved'),
                    ('run-1', 'b@example.com', 'rejected')",
            ] {
                sqlx::query(query).execute(pool).await.unwrap();
            }
            let reviews = |name: &str, n: f64| {
                (format!("{}{{{}}}", REVIEWS, name), Value::Gauge(n))
            };
            let rows = count_reviews_per_reviewer(pool).await.unwrap();
            capture(|| export_review_counts_to_metrics(rows));
            sqlx::query("UPDATE review SET verdict = 'rejected' WHERE run_id = 'run-2'")
                .execute(pool)
                .await
                .unwrap();
            let rows = count_reviews_per_reviewer(pool).await.unwrap();
            assert_eq!(
                capture(|| export_review_counts_to_metrics(rows)),
                Snapshot::from([
                    reviews(r#"reviewer="a@example.com",verdict="approved""#, 1.0),
                    reviews(r#"reviewer="a@example.com",verdict="rejected""#, 1.0),
                    reviews(r#"reviewer="b@example.com",verdict="rejected""#, 1.0),
                ])
            );
            sqlx::query("UPDATE review SET verdict = 'rejected' WHERE run_id = 'run-1'")
                .execute(pool)
                .await
                .unwrap();
            let rows = count_reviews_per_reviewer(pool).await.unwrap();
            assert_eq!(
                capture(|| export_review_counts_to_metrics(rows)),
                Snapshot::from([
                    reviews(r#"reviewer="a@example.com",verdict="approved""#, 0.0),
                    reviews(r#"reviewer="a@example.com",verdict="rejected""#, 2.0),
                    reviews(r#"reviewer="b@example.com",verdict="rejected""#, 1.0),
                ])
            );
        }
    }
}
