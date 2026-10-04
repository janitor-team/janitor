//! Watchdog system for monitoring active runs.

use crate::database::RunnerDatabase;
use crate::ActiveRun;
use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::time::{interval, sleep};

/// Reasons why a run might be terminated.
#[derive(Debug, Clone)]
pub enum TerminationReason {
    /// Run exceeded its maximum allowed duration.
    Timeout,
    /// Worker health check failed.
    HealthCheckFailed,
    /// Run was manually killed.
    ManualKill,
    /// Worker disappeared or stopped responding.
    WorkerDisappeared,
    /// Resource constraints or system issues.
    SystemFailure(String),
}

impl TerminationReason {
    /// Get the result code for this termination reason.
    pub fn result_code(&self) -> &'static str {
        match self {
            TerminationReason::Timeout => "worker-timeout",
            TerminationReason::HealthCheckFailed => "worker-failure",
            TerminationReason::ManualKill => "killed",
            TerminationReason::WorkerDisappeared => "worker-disappeared",
            TerminationReason::SystemFailure(_) => "system-failure",
        }
    }

    /// Get a human-readable description.
    pub fn description(&self) -> String {
        match self {
            TerminationReason::Timeout => "Run exceeded maximum allowed duration".to_string(),
            TerminationReason::HealthCheckFailed => "Worker health check failed".to_string(),
            TerminationReason::ManualKill => "Run was manually terminated".to_string(),
            TerminationReason::WorkerDisappeared => "Worker stopped responding".to_string(),
            TerminationReason::SystemFailure(msg) => format!("System failure: {}", msg),
        }
    }

    /// Check if this failure is transient (retriable).
    pub fn is_transient(&self) -> bool {
        match self {
            TerminationReason::Timeout => true,
            TerminationReason::HealthCheckFailed => true,
            TerminationReason::ManualKill => false,
            TerminationReason::WorkerDisappeared => true,
            TerminationReason::SystemFailure(_) => true,
        }
    }

    /// Create structured failure details for database storage.
    pub fn create_failure_details(&self, run: &crate::ActiveRun) -> serde_json::Value {
        use chrono::Utc;
        use serde_json::json;

        let mut details = json!({
            "termination_reason": self.result_code(),
            "description": self.description(),
            "is_transient": self.is_transient(),
            "worker_name": run.worker_name,
            "codebase": run.codebase,
            "campaign": run.campaign,
            "log_id": run.log_id,
            "terminated_at": Utc::now().to_rfc3339(),
        });

        // Add run duration
        let duration = Utc::now().signed_duration_since(run.start_time);
        details["run_duration_seconds"] = json!(duration.num_seconds());

        // Add estimated vs actual duration comparison if available
        if let Some(estimated) = run.estimated_duration {
            details["estimated_duration_seconds"] = json!(estimated.as_secs());
        }

        // Add backchannel information
        details["backchannel"] = run.backchannel.to_json();

        // Add specific details based on termination reason
        match self {
            TerminationReason::Timeout => {
                details["timeout_type"] = json!("watchdog_timeout");
            }
            TerminationReason::HealthCheckFailed => {
                details["health_check_failed"] = json!(true);
            }
            TerminationReason::ManualKill => {
                details["manual_termination"] = json!(true);
            }
            TerminationReason::WorkerDisappeared => {
                details["worker_unreachable"] = json!(true);
            }
            TerminationReason::SystemFailure(msg) => {
                details["system_failure_message"] = json!(msg);
            }
        }

        details
    }
}

/// Configuration for the watchdog system.
#[derive(Debug, Clone)]
pub struct WatchdogConfig {
    /// How often to check active runs (in seconds).
    pub check_interval: u64,
    /// Default timeout for runs without explicit timeout (in seconds).
    pub default_timeout: u64,
    /// How long to wait before considering a worker disappeared (in seconds).
    pub worker_heartbeat_timeout: u64,
    /// Maximum number of health check failures before terminating.
    pub max_health_failures: u32,
    /// How long a run is allowed to keep reporting `idle` before the
    /// watchdog gives up on it (in seconds). `idle` is a transient,
    /// healthy state -- it is what the worker `/status` endpoint
    /// returns when `state.assignment.is_none()` (just-started or
    /// just-finished a run, hasn't pulled the next one) and what the
    /// Jenkins backchannel returns when the job is queued but no
    /// build has started yet. Treating it as failure-on-third-poll
    /// killed real runs while their builds were sitting in the
    /// Jenkins queue. Bound it instead, so a worker that's lost its
    /// assignment forever still gets cleaned up eventually.
    pub worker_idle_timeout: u64,
    /// How often to run maintenance tasks (in seconds).
    pub maintenance_interval: u64,
    /// Maximum age for stale runs before cleanup (in hours).
    pub max_run_age_hours: i64,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            check_interval: 30,            // 30 seconds
            default_timeout: 3600,         // 1 hour
            worker_heartbeat_timeout: 300, // 5 minutes
            max_health_failures: 3,
            worker_idle_timeout: 900,  // 15 minutes
            maintenance_interval: 300, // 5 minutes
            max_run_age_hours: 6,      // 6 hours
        }
    }
}

impl WatchdogConfig {
    /// Build a watchdog config from the runner's [`WorkerConfig`],
    /// inheriting the rest from [`Default`]. Pulls the
    /// `default_timeout` (in seconds) from the runner-level
    /// `run_timeout_minutes` so deployments can bump it via the
    /// `WORKER_RUN_TIMEOUT_MINUTES` env var without recompiling.
    pub fn from_worker_config(worker: &crate::config::WorkerConfig) -> Self {
        Self {
            default_timeout: worker.run_timeout_minutes * 60,
            ..Self::default()
        }
    }
}

/// Background watchdog task for monitoring active runs.
pub struct Watchdog {
    database: Arc<RunnerDatabase>,
    active_runs: crate::active_runs::ActiveRunStore,
    config: WatchdogConfig,
    health_failures: HashMap<String, u32>,
    /// First time each run was seen reporting `idle` since its last
    /// non-idle poll. Used to bound how long we'll wait for a queued
    /// Jenkins build / freshly-assigned worker to actually start.
    idle_since: HashMap<String, DateTime<Utc>>,
}

impl Watchdog {
    /// Create a new watchdog instance.
    pub fn new(
        database: Arc<RunnerDatabase>,
        active_runs: crate::active_runs::ActiveRunStore,
        config: WatchdogConfig,
    ) -> Self {
        Self {
            database,
            active_runs,
            config,
            health_failures: HashMap::new(),
            idle_since: HashMap::new(),
        }
    }

    /// Start the watchdog monitoring loop.
    pub async fn start(&mut self) {
        log::info!(
            "Starting watchdog with check interval {} seconds, maintenance interval {} seconds",
            self.config.check_interval,
            self.config.maintenance_interval
        );

        let mut check_timer = interval(std::time::Duration::from_secs(self.config.check_interval));
        let mut maintenance_timer = interval(std::time::Duration::from_secs(
            self.config.maintenance_interval,
        ));

        loop {
            tokio::select! {
                _ = check_timer.tick() => {
                    if let Err(e) = self.check_active_runs().await {
                        log::error!("Watchdog check failed: {}", e);
                    }
                }
                _ = maintenance_timer.tick() => {
                    if let Err(e) = self.run_maintenance().await {
                        log::error!("Watchdog maintenance failed: {}", e);
                    }
                }
            }
        }
    }

    /// Check all active runs for timeouts and health issues.
    async fn check_active_runs(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let active_runs = self.active_runs.list().await;
        let now = Utc::now();

        log::debug!("Checking {} active runs", active_runs.len());

        for run in active_runs {
            if let Err(e) = self.check_single_run(&run, now).await {
                log::error!("Failed to check run {}: {}", run.log_id, e);
            }
        }

        Ok(())
    }

    /// Check a single active run for issues.
    async fn check_single_run(
        &mut self,
        run: &ActiveRun,
        now: DateTime<Utc>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(reason) = self.check_timeout(run, now) {
            log::warn!("Terminating run {} due to: {:?}", run.log_id, reason);
            self.terminate_run(run, reason).await?;
            return Ok(());
        }

        if let Some(reason) = self.check_worker_health(run, now).await? {
            log::warn!(
                "Terminating run {} due to health check: {:?}",
                run.log_id,
                reason
            );
            self.terminate_run(run, reason).await?;
            return Ok(());
        }

        Ok(())
    }

    /// Check if a run has exceeded its timeout.
    ///
    /// estimated_duration comes from the scheduler's per-codebase/campaign
    /// average and is typically tens of seconds -- much shorter than the
    /// actual run time on cold caches. Using it as an *upper* bound for
    /// the watchdog deadline killed real runs within 30 s of starting
    /// and orphaned their active_run entries before the worker could
    /// post results.
    ///
    /// Use it as a *floor* instead: allow at least the scheduler's
    /// estimate, but always at least `default_timeout` seconds.
    fn check_timeout(&self, run: &ActiveRun, now: DateTime<Utc>) -> Option<TerminationReason> {
        let estimate_secs = run.estimated_duration.map(|d| d.as_secs()).unwrap_or(0);
        let timeout_duration = compute_run_deadline_secs(&self.config, estimate_secs);

        let timeout_time = run.start_time + Duration::seconds(timeout_duration as i64);

        if now > timeout_time {
            Some(TerminationReason::Timeout)
        } else {
            None
        }
    }

    /// Check worker health via backchannel.
    async fn check_worker_health(
        &mut self,
        run: &ActiveRun,
        now: DateTime<Utc>,
    ) -> Result<Option<TerminationReason>, Box<dyn std::error::Error + Send + Sync>> {
        match run.backchannel.get_health_status(&run.log_id).await {
            Ok(health) => {
                if let Some(last_ping) = health.last_ping {
                    let heartbeat_timeout =
                        Duration::seconds(self.config.worker_heartbeat_timeout as i64);
                    let last_heartbeat_cutoff = now - heartbeat_timeout;

                    if last_ping < last_heartbeat_cutoff {
                        log::warn!(
                            "Worker heartbeat timeout for run {}: last ping was {} seconds ago",
                            run.log_id,
                            (now - last_ping).num_seconds()
                        );

                        let failures = self.health_failures.entry(run.log_id.clone()).or_insert(0);
                        *failures += 1;

                        if *failures >= self.config.max_health_failures {
                            return Ok(Some(TerminationReason::WorkerDisappeared));
                        } else {
                            log::warn!(
                                "Heartbeat timeout {}/{} for run {}",
                                failures,
                                self.config.max_health_failures,
                                run.log_id
                            );
                            return Ok(None);
                        }
                    }
                }

                // Any non-idle reading clears the idle timer; idle
                // is the only status whose duration we track here.
                if health.status != "idle" {
                    self.idle_since.remove(&run.log_id);
                }

                match health.status.as_str() {
                    "idle" => Ok(check_idle_timeout(
                        &mut self.idle_since,
                        &run.log_id,
                        now,
                        self.config.worker_idle_timeout,
                    )),
                    "healthy" | "running" | "building" | "completed" => {
                        // Worker is alive and responding properly
                        self.health_failures.remove(&run.log_id);

                        // Additional check: if run is completed but still in active list,
                        // this might indicate a cleanup issue
                        if health.status == "completed"
                            && health.current_run_id.as_ref() != Some(&run.log_id)
                        {
                            log::warn!("Worker reports completion of different run ({:?}) than expected ({})", 
                                     health.current_run_id, run.log_id);
                            return Ok(Some(TerminationReason::WorkerDisappeared));
                        }

                        Ok(None)
                    }
                    "unhealthy" | "failed" | "aborted" => {
                        let failures = self.health_failures.entry(run.log_id.clone()).or_insert(0);
                        *failures += 1;

                        if *failures >= self.config.max_health_failures {
                            Ok(Some(TerminationReason::HealthCheckFailed))
                        } else {
                            log::warn!(
                                "Health check failure {}/{} for run {} (status: {})",
                                failures,
                                self.config.max_health_failures,
                                run.log_id,
                                health.status
                            );
                            Ok(None)
                        }
                    }
                    "not-found" | "unreachable" => {
                        // Worker or job no longer exists
                        Ok(Some(TerminationReason::WorkerDisappeared))
                    }
                    "different-run" => {
                        // Worker is on a different run than what we
                        // have in active-runs for this assignment.
                        //
                        // Three cases, distinguished by what we know
                        // about the worker's reported run:
                        //
                        // 1) The reported run is ALSO in active-runs
                        //    AND it started AFTER the run we're
                        //    checking -- the worker has advanced
                        //    forward (older run finished, /finish
                        //    cleanup hasn't completed, worker
                        //    already accepted the next one). Drop
                        //    the stale older entry; the worker is
                        //    fine.
                        //
                        // 2) The reported run is in active-runs but
                        //    started BEFORE the run we're checking
                        //    -- the worker hasn't yet picked up the
                        //    new assignment we just gave it (its
                        //    `state.assignment` is still the
                        //    previous one). This is a transient
                        //    state; defer judgement and let the
                        //    soft-failure counter accumulate so we
                        //    don't kill a freshly-assigned run.
                        //
                        // 3) The reported run is NOT in active-runs
                        //    -- genuine drift / lost assignment. Kill
                        //    the stale side.
                        let other_run = match health.current_run_id.as_ref() {
                            Some(other) => self.active_runs.get(other).await,
                            None => None,
                        };
                        match other_run {
                            Some(other) if other.start_time > run.start_time => {
                                log::info!(
                                    "Worker {} (link={:?}) advanced from run {} (started {}) to {} \
                                     (started {}) -- dropping stale older entry without killing",
                                    run.worker_name,
                                    run.worker_link,
                                    run.log_id,
                                    run.start_time,
                                    other.log_id,
                                    other.start_time,
                                );
                                self.active_runs.remove(&run.log_id).await;
                                Ok(None)
                            }
                            Some(other) => {
                                // Worker still on the older run; the
                                // newer one we just assigned hasn't
                                // landed in `state.assignment` yet.
                                // Soft-fail-then-give-up so a
                                // genuinely stuck worker still gets
                                // killed eventually.
                                let failures =
                                    self.health_failures.entry(run.log_id.clone()).or_insert(0);
                                *failures += 1;
                                log::debug!(
                                    "Worker {} hasn't yet picked up run {} (still on {}, started {}); \
                                     soft-failure {}/{}",
                                    run.worker_name,
                                    run.log_id,
                                    other.log_id,
                                    other.start_time,
                                    failures,
                                    self.config.max_health_failures,
                                );
                                if *failures >= self.config.max_health_failures {
                                    Ok(Some(TerminationReason::WorkerDisappeared))
                                } else {
                                    Ok(None)
                                }
                            }
                            None => {
                                log::warn!(
                                    "Worker {} (link={:?}) drifted off run {}: reports current_run={:?}, \
                                     no matching active run -- terminating stale side",
                                    run.worker_name,
                                    run.worker_link,
                                    run.log_id,
                                    health.current_run_id,
                                );
                                Ok(Some(TerminationReason::WorkerDisappeared))
                            }
                        }
                    }
                    _ => {
                        log::warn!(
                            "Unknown health status '{}' for run {}",
                            health.status,
                            run.log_id
                        );

                        // Treat unknown status as potential issue but not immediate failure
                        let failures = self.health_failures.entry(run.log_id.clone()).or_insert(0);
                        *failures += 1;

                        if *failures >= self.config.max_health_failures {
                            Ok(Some(TerminationReason::HealthCheckFailed))
                        } else {
                            Ok(None)
                        }
                    }
                }
            }
            Err(e) => {
                log::debug!("Failed to get health status for run {}: {}", run.log_id, e);

                let error_string = e.to_string();
                let is_fatal_error = error_string.contains("Job not found")
                    || error_string.contains("Fatal failure")
                    || error_string.contains("not-found");
                let is_unreachable = error_string.contains("Worker unreachable")
                    || error_string.contains("timeout")
                    || error_string.contains("connection");

                if is_fatal_error {
                    log::info!("Fatal error for run {}: {}", run.log_id, e);
                    return Ok(Some(TerminationReason::WorkerDisappeared));
                }

                let failure_reason = if is_unreachable {
                    TerminationReason::WorkerDisappeared
                } else {
                    TerminationReason::HealthCheckFailed
                };

                // Increment failure count for non-fatal errors
                let failures = self.health_failures.entry(run.log_id.clone()).or_insert(0);
                *failures += 1;

                if *failures >= self.config.max_health_failures {
                    Ok(Some(failure_reason))
                } else {
                    log::warn!(
                        "Health check error {}/{} for run {}: {}",
                        failures,
                        self.config.max_health_failures,
                        run.log_id,
                        e
                    );
                    Ok(None)
                }
            }
        }
    }

    /// Terminate a run and clean up its state.
    async fn terminate_run(
        &mut self,
        run: &ActiveRun,
        reason: TerminationReason,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        log::info!(
            "Terminating run {} (worker: {}): {}",
            run.log_id,
            run.worker_name,
            reason.description()
        );

        // Try to signal the worker to stop via backchannel
        if let Err(e) = run.backchannel.terminate(&run.log_id).await {
            log::warn!(
                "Failed to signal worker termination for run {}: {}",
                run.log_id,
                e
            );
        }

        // Wait a bit for graceful shutdown
        sleep(std::time::Duration::from_secs(5)).await;

        // Build a synthetic JanitorResult for the failed run and run
        // the same finish_run transaction the worker would have run
        // had it reported success.
        let result_code = reason.result_code();
        let description = reason.description();
        let mut janitor_result =
            run.create_result(result_code.to_string(), Some(description.to_string()));
        janitor_result.transient = Some(reason.is_transient());
        janitor_result.failure_details = Some(reason.create_failure_details(run));

        if let Err(e) = self
            .database
            .finish_run(
                &mut janitor_result,
                &run.command,
                run.instigated_context.as_ref(),
                run.queue_id,
            )
            .await
        {
            // Persisting a worker-timeout row failed (likely because
            // the original `run` row was already inserted by another
            // path, e.g. a concurrent finish_run call from the
            // worker). Log and continue: dropping the active-run
            // entry below is more important than recording the
            // failure twice.
            log::warn!(
                "Failed to record terminated run {} in database: {}",
                run.log_id,
                e
            );
        } else {
            log::info!("Recorded terminated run {} as {}", run.log_id, result_code);
        }

        self.active_runs.remove(&run.log_id).await;

        // Clean up health failure tracking
        self.health_failures.remove(&run.log_id);
        self.idle_since.remove(&run.log_id);

        log::info!("Successfully terminated and cleaned up run {}", run.log_id);
        Ok(())
    }

    /// Run periodic maintenance tasks.
    async fn run_maintenance(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        log::debug!("Running watchdog maintenance tasks");

        // Drop in-memory active runs that have been running longer than
        // the configured max age. Matches the Python watchdog's periodic
        // timeout sweep. The queue row is left in place so another
        // worker can pick it up.
        let stale = self
            .active_runs
            .drain_older_than(chrono::Duration::hours(self.config.max_run_age_hours))
            .await;
        if !stale.is_empty() {
            log::info!("Cleaned up {} stale active runs", stale.len());
            for run in stale {
                log::warn!(
                    "Dropped stale active run {} (campaign={}, worker={}, started {})",
                    run.log_id,
                    run.campaign,
                    run.worker_name,
                    run.start_time
                );
            }
        }

        // General database maintenance. The old `mark_runs_for_retry`
        // step was removed because it wrote to `queue.schedule_time`
        // and `queue.retry_count`, columns that don't exist in the
        // schema. Retry, if reintroduced, needs a schema design first.
        self.database.maintenance_cleanup().await?;

        Ok(())
    }

    /// Manually terminate a specific run.
    pub async fn kill_run(
        &mut self,
        run_id: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(run) = self.active_runs.get(run_id).await {
            self.terminate_run(&run, TerminationReason::ManualKill)
                .await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Get current watchdog statistics.
    pub fn get_stats(&self) -> WatchdogStats {
        WatchdogStats {
            runs_with_health_failures: self.health_failures.len(),
            total_health_failures: self.health_failures.values().sum(),
        }
    }

    /// Get detailed health status for all active runs.
    pub async fn get_detailed_health_status(
        &self,
    ) -> Result<Vec<RunHealthStatus>, Box<dyn std::error::Error + Send + Sync>> {
        let active_runs = self.active_runs.list().await;
        let mut health_statuses = Vec::new();

        for run in active_runs {
            let health_status = match run.backchannel.get_health_status(&run.log_id).await {
                Ok(health) => Some(health),
                Err(e) => {
                    log::debug!("Failed to get health for run {}: {}", run.log_id, e);
                    None
                }
            };

            let failure_count = self.health_failures.get(&run.log_id).copied().unwrap_or(0);

            health_statuses.push(RunHealthStatus {
                log_id: run.log_id,
                worker_name: run.worker_name,
                start_time: run.start_time,
                estimated_duration: run.estimated_duration,
                health: health_status,
                failure_count,
                max_failures: self.config.max_health_failures,
            });
        }

        Ok(health_statuses)
    }

    /// Force a health check on a specific run.
    pub async fn check_run_health(
        &mut self,
        run_id: &str,
    ) -> Result<Option<RunHealthStatus>, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(run) = self.active_runs.get(run_id).await {
            let now = Utc::now();

            let termination_reason = self.check_worker_health(&run, now).await?;

            let health_status = (run.backchannel.get_health_status(&run.log_id).await).ok();

            let failure_count = self.health_failures.get(&run.log_id).copied().unwrap_or(0);

            // If termination was triggered, handle it
            if let Some(reason) = termination_reason {
                log::info!(
                    "Health check triggered termination for run {}: {:?}",
                    run_id,
                    reason
                );
                self.terminate_run(&run, reason).await?;
            }

            Ok(Some(RunHealthStatus {
                log_id: run.log_id,
                worker_name: run.worker_name,
                start_time: run.start_time,
                estimated_duration: run.estimated_duration,
                health: health_status,
                failure_count,
                max_failures: self.config.max_health_failures,
            }))
        } else {
            Ok(None)
        }
    }

    /// Combine per-database failure counters with in-process
    /// health-failure counts into a single stats map.
    pub async fn failure_stats_including_health(
        &self,
    ) -> Result<HashMap<String, i64>, Box<dyn std::error::Error + Send + Sync>> {
        let mut stats = self.database.get_failure_stats().await?;
        stats.insert(
            "runs_with_health_failures".to_string(),
            self.health_failures.len() as i64,
        );
        stats.insert(
            "total_health_failures".to_string(),
            self.health_failures.values().sum::<u32>() as i64,
        );

        Ok(stats)
    }
}

/// Statistics about watchdog operation.
#[derive(Debug, Clone)]
pub struct WatchdogStats {
    /// Number of runs currently experiencing health check failures.
    pub runs_with_health_failures: usize,
    /// Total number of health check failures across all monitored runs.
    pub total_health_failures: u32,
}

/// Detailed health status for a specific run.
#[derive(Debug, Clone)]
pub struct RunHealthStatus {
    /// The log ID of the run.
    pub log_id: String,
    /// Name of the worker processing this run.
    pub worker_name: String,
    /// When the run started.
    pub start_time: DateTime<Utc>,
    /// Estimated duration for the run.
    pub estimated_duration: Option<std::time::Duration>,
    /// Current health status from the backchannel.
    pub health: Option<crate::HealthStatus>,
    /// Number of consecutive health check failures.
    pub failure_count: u32,
    /// Maximum allowed failures before termination.
    pub max_failures: u32,
}

/// Compute how long a run is allowed to run, in seconds, given the
/// scheduler's per-codebase estimate. The scheduler estimate is used
/// as a floor on top of `default_timeout`, never as an upper bound on
/// its own (see `check_timeout` for the rationale). Pulled out as a
/// free function so the wiring of `WORKER_RUN_TIMEOUT_MINUTES` into
/// this calculation is unit-testable without constructing a full
/// `Watchdog`.
fn compute_run_deadline_secs(config: &WatchdogConfig, estimate_secs: u64) -> u64 {
    estimate_secs.max(config.default_timeout)
}

/// Decide whether a run that is reporting `idle` has been idle for so
/// long that we should give up on it. Records the first idle reading
/// in `idle_since` if not already there; on subsequent calls compares
/// `now - first_seen` against `timeout_secs`. Returns
/// `Some(HealthCheckFailed)` when the limit is exceeded (and clears
/// the entry so a re-assignment of the same `log_id` starts fresh),
/// otherwise `None`.
///
/// Pulled out as a free function so the timer logic is unit-testable
/// without spinning up a `RunnerDatabase` / `ActiveRunStore` /
/// backchannel.
fn check_idle_timeout(
    idle_since: &mut HashMap<String, DateTime<Utc>>,
    log_id: &str,
    now: DateTime<Utc>,
    timeout_secs: u64,
) -> Option<TerminationReason> {
    let first_seen = *idle_since.entry(log_id.to_string()).or_insert(now);
    let elapsed = now - first_seen;
    let timeout = Duration::seconds(timeout_secs as i64);
    if elapsed >= timeout {
        log::warn!(
            "Run {} has been idle for {} s (>= worker_idle_timeout {} s); giving up",
            log_id,
            elapsed.num_seconds(),
            timeout_secs,
        );
        idle_since.remove(log_id);
        Some(TerminationReason::HealthCheckFailed)
    } else {
        log::debug!(
            "Run {} idle for {} s (< worker_idle_timeout {} s)",
            log_id,
            elapsed.num_seconds(),
            timeout_secs,
        );
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(secs_from_epoch: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs_from_epoch, 0).unwrap()
    }

    /// First idle reading just records `now` and returns None -- never
    /// fail on the first poll, even if `timeout_secs` is 0 elsewhere.
    #[test]
    fn test_check_idle_timeout_first_reading_returns_none() {
        let mut map = HashMap::new();
        let res = check_idle_timeout(&mut map, "run-a", t(1000), 900);
        assert!(res.is_none());
        assert_eq!(map.get("run-a"), Some(&t(1000)));
    }

    /// Subsequent idle readings inside the window return None and
    /// keep the original first-seen timestamp.
    #[test]
    fn test_check_idle_timeout_within_window_returns_none() {
        let mut map = HashMap::new();
        let _ = check_idle_timeout(&mut map, "run-a", t(1000), 900);
        let res = check_idle_timeout(&mut map, "run-a", t(1500), 900);
        assert!(res.is_none());
        assert_eq!(map.get("run-a"), Some(&t(1000)));
    }

    /// Reading after the timeout returns HealthCheckFailed and
    /// drops the entry so a re-assigned id starts a fresh timer.
    #[test]
    fn test_check_idle_timeout_past_window_returns_failure_and_clears() {
        let mut map = HashMap::new();
        let _ = check_idle_timeout(&mut map, "run-a", t(1000), 900);
        let res = check_idle_timeout(&mut map, "run-a", t(1900), 900);
        match res {
            Some(TerminationReason::HealthCheckFailed) => {}
            other => panic!("expected HealthCheckFailed past window, got {:?}", other),
        }
        assert!(
            !map.contains_key("run-a"),
            "entry must be cleared after firing"
        );
    }

    /// `elapsed == timeout` is the boundary: we fire (>=). This pins
    /// the inequality direction so a future refactor can't flip it
    /// silently and end up never firing.
    #[test]
    fn test_check_idle_timeout_boundary_is_inclusive() {
        let mut map = HashMap::new();
        let _ = check_idle_timeout(&mut map, "run-a", t(1000), 900);
        let res = check_idle_timeout(&mut map, "run-a", t(1900), 900);
        assert!(matches!(res, Some(TerminationReason::HealthCheckFailed)));
    }

    /// After firing once, a fresh idle reading for the same log_id
    /// (e.g. the run was retried/reassigned) restarts the timer
    /// instead of immediately re-firing.
    #[test]
    fn test_check_idle_timeout_resets_after_firing() {
        let mut map = HashMap::new();
        let _ = check_idle_timeout(&mut map, "run-a", t(1000), 900);
        let _ = check_idle_timeout(&mut map, "run-a", t(1900), 900); // fires
                                                                     // Restart timer.
        let res = check_idle_timeout(&mut map, "run-a", t(1901), 900);
        assert!(
            res.is_none(),
            "first idle reading after a fire must restart the timer"
        );
        assert_eq!(map.get("run-a"), Some(&t(1901)));
    }

    /// Per-run independence: clearing one run's idle state must not
    /// affect another run's tracking.
    #[test]
    fn test_check_idle_timeout_runs_are_independent() {
        let mut map = HashMap::new();
        let _ = check_idle_timeout(&mut map, "run-a", t(1000), 900);
        let _ = check_idle_timeout(&mut map, "run-b", t(1500), 900);
        // run-a fires.
        let _ = check_idle_timeout(&mut map, "run-a", t(1900), 900);
        // run-b is still within its own window.
        let res = check_idle_timeout(&mut map, "run-b", t(1900), 900);
        assert!(res.is_none());
        assert_eq!(map.get("run-b"), Some(&t(1500)));
    }

    /// `from_worker_config` converts the runner-level minute field
    /// into the watchdog's second-granularity `default_timeout`. Pin
    /// the conversion so a future refactor can't silently swap
    /// minutes for seconds, and confirm unrelated fields keep their
    /// `Default` values.
    #[test]
    fn test_watchdog_config_from_worker_config_minutes_to_seconds() {
        let worker = crate::config::WorkerConfig {
            run_timeout_minutes: 240,
            ..crate::config::WorkerConfig::default()
        };
        let cfg = WatchdogConfig::from_worker_config(&worker);
        assert_eq!(cfg.default_timeout, 240 * 60);
        let defaults = WatchdogConfig::default();
        assert_eq!(cfg.check_interval, defaults.check_interval);
        assert_eq!(
            cfg.worker_heartbeat_timeout,
            defaults.worker_heartbeat_timeout
        );
        assert_eq!(cfg.worker_idle_timeout, defaults.worker_idle_timeout);
    }

    /// `compute_run_deadline_secs` uses the configured default as a
    /// floor: a missing-or-tiny scheduler estimate must never make
    /// the watchdog kill a run sooner than the deployment configured.
    /// The floor is what the `WORKER_RUN_TIMEOUT_MINUTES` env var
    /// actually controls (issue #130).
    #[test]
    fn test_watchdog_deadline_uses_default_as_floor() {
        let worker = crate::config::WorkerConfig {
            run_timeout_minutes: 240, // 4h
            ..crate::config::WorkerConfig::default()
        };
        let cfg = WatchdogConfig::from_worker_config(&worker);
        // No estimate -> exactly the default floor, in seconds.
        assert_eq!(compute_run_deadline_secs(&cfg, 0), 240 * 60);
        // Estimate below floor -> clamped up to floor.
        assert_eq!(compute_run_deadline_secs(&cfg, 30 * 60), 240 * 60);
        // Estimate above floor -> respected verbatim, no upper cap.
        assert_eq!(compute_run_deadline_secs(&cfg, 5 * 60 * 60), 5 * 60 * 60);
    }
}
