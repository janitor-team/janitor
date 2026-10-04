//! Queue processing functionality for the publish service.
//!
//! This module handles the main queue processing loop and related functionality,
//! ported from the Python implementation.

use crate::{consider_publish_run, AppState, PublishError};
use chrono::Utc;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;

/// Represents a publish-ready run with its associated metadata.
///
/// NOTE: retained for external API shape (`crate::state::iter_publish_ready`
/// now returns a tuple). Prefer that function directly in new code -
/// see commit history for why the previous in-module
/// `PublishReadyIterator` was removed.
#[derive(Debug, Clone)]
pub struct PublishReadyRun {
    /// The run information.
    pub run: janitor::state::Run,
    /// The rate limit bucket for this run.
    pub rate_limit_bucket: String,
    /// The command that was executed.
    pub command: String,
    /// List of unpublished branches for this run.
    pub unpublished_branches: Vec<crate::state::UnpublishedBranch>,
}

// `PublishReadyIterator` was removed: its hand-rolled join bypassed
// the `publish_ready` view and the associated gating (publish_status,
// change_set_state, mode, max_frequency_days, ordering), hardcoded
// `publish_mode = "propose"`, and returned a single run per call.
// `publish_pending_ready` and the single-run path in
// `handle_publish_run` now both go through
// `crate::state::iter_publish_ready`.

/// Periodically check existing merge proposals and publish pending
/// ready changes.
pub async fn process_queue_loop(
    state: Arc<AppState>,
    interval: chrono::Duration,
    auto_publish: bool,
    push_limit: Option<usize>,
    modify_mp_limit: Option<i32>,
    require_binary_diff: bool,
) {
    log::info!(
        "Starting publish queue processing (auto_publish: {}, interval: {:?})",
        auto_publish,
        interval
    );

    // Run the merge-proposal scan and the publish phase as two
    // INDEPENDENT loops rather than one serial cycle.
    //
    // `check_existing` walks every merge proposal we own on each forge -
    // tens of thousands of them on a production instance - to refresh the
    // per-bucket open-proposal counts the rate limiter needs. The
    // original port ran it inline *before* `publish_pending_ready` in the
    // same cycle, so a slow scan starved publishing: the publish phase
    // was never reached and approved runs never went out.
    //
    // Decoupling them is only safe because the publish loop gates itself
    // on a *recent completed* scan (see `run_publish_loop`): it never
    // creates proposals against a stale or incomplete view of what we
    // already own, which is exactly what the rate limiter depends on.
    tokio::spawn(run_scan_loop(state.clone(), interval, modify_mp_limit));

    if auto_publish {
        run_publish_loop(state, interval, push_limit, require_binary_diff).await;
    } else {
        log::info!("Auto-publish disabled; running merge-proposal scan loop only");
        // Nothing to publish, but keep the process on the scan loop so a
        // no-auto-publish deployment still maintains merge proposals -
        // matching the previous single-loop behaviour.
        std::future::pending::<()>().await;
    }
}

/// Publishing is gated on a full merge-proposal scan having completed
/// within this window. Every regular scan refreshes the timestamp, so in
/// steady state the gate is always open; it only bites on cold start
/// (before the first scan finishes) or if scans stop completing for this
/// long, in which case we pause publishing rather than act on a stale
/// view of the proposals we own.
const MAX_FULL_SCAN_AGE_DAYS: i64 = 7;

/// While gated, re-log the reason at most this often so a stuck gate is
/// visible without flooding the log.
const GATE_WAIT_RELOG_SECS: u64 = 60;

/// Upper bound on how long a single run's publish attempt may take before
/// we give up on it and move to the next. A publish attempt that blocks on
/// a network or lock operation without its own timeout would otherwise
/// wedge the whole publish loop - and the entire backlog - indefinitely.
/// A timed-out run stays publish-ready and is retried next cycle.
const PER_RUN_PUBLISH_TIMEOUT_SECS: u64 = 300;

/// Whether `last` (the completion time of the most recent full
/// merge-proposal scan, `None` if none has finished) is recent enough to
/// publish against. Both publish paths - the periodic
/// [`run_publish_loop`] and the Redis-triggered `process_approved_run` -
/// consult this so neither creates proposals against a stale or
/// incomplete view of the proposals we already own.
pub(crate) fn scan_ts_is_recent(last: Option<chrono::DateTime<Utc>>) -> bool {
    last.map(|t| {
        Utc::now().signed_duration_since(t) <= chrono::Duration::days(MAX_FULL_SCAN_AGE_DAYS)
    })
    .unwrap_or(false)
}

/// Sleep for whatever remains of `interval` after a cycle that started at
/// `cycle_start`, logging a warning when the cycle overran.
async fn sleep_remaining(
    label: &str,
    cycle_start: chrono::DateTime<Utc>,
    interval: chrono::Duration,
) {
    let cycle_duration = Utc::now() - cycle_start;
    let sleep_duration = interval - cycle_duration;
    if sleep_duration > chrono::Duration::zero() {
        log::debug!(
            "{} cycle completed in {:?}, sleeping for {:?}",
            label,
            cycle_duration,
            sleep_duration
        );
        tokio::time::sleep(std::time::Duration::from_millis(
            sleep_duration.num_milliseconds().max(0) as u64,
        ))
        .await;
    } else {
        log::warn!(
            "{} cycle took {:?}, longer than interval {:?}",
            label,
            cycle_duration,
            interval
        );
    }
}

/// Loop that keeps the local view of the merge proposals we own current
/// (`check_existing`) and rescans stragglers. Runs independently of the
/// publish loop so its long, network-bound passes cannot starve
/// publishing. On every *completed* pass it stamps `last_full_scan_at`,
/// which is what re-opens the publish gate.
async fn run_scan_loop(
    state: Arc<AppState>,
    interval: chrono::Duration,
    modify_mp_limit: Option<i32>,
) {
    loop {
        let cycle_start = Utc::now();
        log::debug!("Checking existing merge proposals");
        let completed = crate::check_existing(
            state.conn.clone(),
            state.redis.clone(),
            state.config,
            &state.publish_worker,
            &state.bucket_rate_limiter,
            state.forge_rate_limiter.clone(),
            &state.vcs_managers,
            modify_mp_limit,
            state.unexpected_mp_limit,
        )
        .await;

        log::debug!("Checking straggler merge proposals");
        if let Err(e) = check_stragglers(&state.conn, state.redis.clone()).await {
            log::warn!("Error checking stragglers: {}", e);
        }

        if completed {
            state.last_full_scan_at.send_replace(Some(Utc::now()));
            log::info!("Full merge-proposal scan completed; publish gate refreshed");
        } else {
            log::warn!(
                "Merge-proposal scan did not complete a full pass; publish gate not refreshed"
            );
        }

        sleep_remaining("merge-proposal scan", cycle_start, interval).await;
    }
}

/// Block until a full merge-proposal scan has completed within
/// `MAX_FULL_SCAN_AGE_DAYS`, so we never publish against a stale or
/// incomplete view of the proposals we already own (which would let the
/// rate limiter under-count and over-create). Wakes as soon as the scan
/// loop records a fresh pass.
async fn await_recent_scan(
    scan_rx: &mut tokio::sync::watch::Receiver<Option<chrono::DateTime<Utc>>>,
) {
    loop {
        let last = *scan_rx.borrow();
        if scan_ts_is_recent(last) {
            return;
        }
        log::info!(
            "Publishing gated: waiting for a merge-proposal scan to complete \
             within {} days (last full scan: {:?})",
            MAX_FULL_SCAN_AGE_DAYS,
            last,
        );
        // Wake on the next recorded scan; the timeout just bounds how
        // often we re-log while waiting.
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(GATE_WAIT_RELOG_SECS),
            scan_rx.changed(),
        )
        .await;
    }
}

/// Publish loop: repeatedly publishes approved, publish-ready runs on a
/// fixed cadence, but only once a recent full scan has refreshed the
/// bucket counts (see [`await_recent_scan`]).
async fn run_publish_loop(
    state: Arc<AppState>,
    interval: chrono::Duration,
    push_limit: Option<usize>,
    require_binary_diff: bool,
) {
    let mut scan_rx = state.last_full_scan_at.subscribe();
    loop {
        let cycle_start = Utc::now();

        await_recent_scan(&mut scan_rx).await;

        log::debug!("Publishing pending ready changes");
        if let Err(e) = publish_pending_ready(state.clone(), push_limit, require_binary_diff).await
        {
            log::error!("Error publishing pending ready changes: {}", e);
        }

        sleep_remaining("publish", cycle_start, interval).await;
    }
}

/// Publish all pending ready changes.
///
/// This function identifies runs that are ready to be published and initiates
/// the publishing process for them.
///
/// # Arguments
/// * `state` - The application state
/// * `push_limit` - Optional limit on the number of pushes
/// * `require_binary_diff` - Whether to require binary diffs
///
/// # Returns
/// Ok(()) if successful, or a PublishError
pub async fn publish_pending_ready(
    state: Arc<AppState>,
    push_limit: Option<usize>,
    require_binary_diff: bool,
) -> Result<(), PublishError> {
    let start_time = std::time::Instant::now();
    let mut actions: HashMap<Option<String>, usize> = HashMap::new();
    let mut published_count = 0;
    let mut error_count = 0;

    log::info!(
        "Starting publish_pending_ready (push_limit: {:?}, require_binary_diff: {})",
        push_limit,
        require_binary_diff
    );

    // Pull publish-ready runs from the `publish_ready` SQL view via
    // `state::iter_publish_ready`. The view already applies
    // `publish_status = 'approved'`,
    // `change_set_state IN ('ready','publishing')`, and the
    // `mode IN ('propose','attempt-push','push-derived','push')`
    // filter on unpublished_branches, plus the canonical ordering
    // (`publishing DESC, value DESC NULLS LAST, finish_time DESC`).
    let ready_runs = crate::state::iter_publish_ready(&state.conn, None)
        .await
        .map_err(|e| {
            log::error!("Database error iterating publish-ready runs: {}", e);
            PublishError::Failure {
                code: "database-error".to_string(),
                description: format!("Failed to iterate publish-ready runs: {}", e),
            }
        })?;

    for (run, rate_limit_bucket, policy_command, unpublished_branches) in ready_runs {
        log::info!(
            "Processing publish-ready run: {} (campaign: {}, codebase: {})",
            run.id,
            run.suite,
            run.codebase
        );

        // Check push limit
        if let Some(limit) = push_limit {
            if published_count >= limit {
                log::info!("Reached push limit of {}, stopping", limit);
                break;
            }
        }

        // Consider publishing this run. Pass `policy_command` (the
        // candidate's canonical command) rather than `run.command`; the
        // two can drift when a campaign's command changes after a run.
        let considered = tokio::time::timeout(
            std::time::Duration::from_secs(PER_RUN_PUBLISH_TIMEOUT_SECS),
            consider_publish_run(
                &state.conn,
                state.redis.clone(),
                state.config,
                &state.publish_worker,
                &state.vcs_managers,
                &state.bucket_rate_limiter,
                &run,
                &rate_limit_bucket,
                &unpublished_branches,
                &policy_command,
                push_limit.map(|limit| limit - published_count),
                require_binary_diff,
            ),
        )
        .await;

        match considered {
            Err(_elapsed) => {
                error_count += 1;
                log::error!(
                    "Timed out after {}s considering run {} (campaign: {}, codebase: {}) for \
                     publishing; skipping so the queue can make progress. The run stays \
                     publish-ready and will be retried next cycle.",
                    PER_RUN_PUBLISH_TIMEOUT_SECS,
                    run.id,
                    run.suite,
                    run.codebase,
                );
                *actions.entry(Some("timeout".to_string())).or_insert(0) += 1;
            }
            Ok(Ok(results)) => {
                if let Some(pre_flight) = results.get("__status").and_then(|v| v.as_ref()) {
                    *actions.entry(Some(pre_flight.clone())).or_insert(0) += 1;
                } else {
                    for (role, mode) in &results {
                        match mode {
                            Some(m) => {
                                *actions.entry(Some(m.clone())).or_insert(0) += 1;
                                if m != "build-only" && m != "skip" {
                                    published_count += 1;
                                }
                            }
                            None => {
                                *actions.entry(Some(format!("gated:{}", role))).or_insert(0) += 1;
                            }
                        }
                    }
                }

                log::debug!("Successfully considered run {} for publishing", run.id);
            }
            Ok(Err(e)) => {
                error_count += 1;
                log::error!("Error considering run {} for publishing: {}", run.id, e);
                *actions.entry(Some("error".to_string())).or_insert(0) += 1;
            }
        }
    }

    let duration = start_time.elapsed();

    log::info!(
        "Completed publish_pending_ready in {:?}: {} published, {} errors, actions: {:?}",
        duration,
        published_count,
        error_count,
        actions
    );

    // Unix seconds since epoch.
    crate::metrics::LAST_PUBLISH_PENDING_SUCCESS.set(chrono::Utc::now().timestamp() as f64);

    if published_count > 0 {
        log::info!("Published {} changes this cycle", published_count);
    }

    Ok(())
}

/// Check for straggler merge proposals that need attention.
///
/// Pulls proposal URLs not scanned in the last 5 days via
/// `ProposalInfoManager::iter_outdated_proposal_info_urls`, then
/// re-fetches each and updates or deletes the row depending on the
/// forge's HTTP status. The per-URL logic is shared with the
/// `POST /check-stragglers` handler via `crate::web::check_straggler`.
pub async fn check_stragglers(
    conn: &PgPool,
    redis: Option<redis::aio::ConnectionManager>,
) -> Result<(), sqlx::Error> {
    log::debug!("Checking for straggler merge proposals");

    let proposal_info_manager =
        crate::proposal_info::ProposalInfoManager::new(conn.clone(), redis).await;

    // Python hardcodes 5 days (publish.py:2551). Keep that constant
    // here so the background cadence and the endpoint stay aligned;
    // the endpoint still accepts a `ndays` query parameter for manual
    // scans with different windows.
    let urls = proposal_info_manager
        .iter_outdated_proposal_info_urls(chrono::Duration::days(5))
        .await?;

    if urls.is_empty() {
        log::debug!("No straggler merge proposals found");
        return Ok(());
    }

    log::info!("Rescanning {} straggler merge proposals", urls.len());
    for url in urls {
        crate::web::check_straggler(&proposal_info_manager, &url).await;
    }

    Ok(())
}
