//! Watchdog that aborts runs whose workers have stopped sending keepalives.
//!
//! This mirrors the Python runner's `QueueProcessor._watchdog`: a run is
//! only pinged once its last keepalive is a third of `run_timeout` old, a
//! successful ping counts as a keepalive, and a run is aborted (without
//! killing the worker) once no keepalive has been received for
//! `run_timeout` minutes.

use crate::database::FinishOutcome;
use crate::{ActiveRun, AppState, PingError};
use chrono::{Duration, Utc};
use std::sync::Arc;

/// How often the watchdog checks the active runs.
pub const KEEPALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

type Error = Box<dyn std::error::Error + Send + Sync>;

/// Background watchdog task for monitoring active runs.
pub struct Watchdog {
    state: Arc<AppState>,
}

impl Watchdog {
    /// Create a new watchdog that aborts runs that haven't sent a
    /// keepalive in `state.run_timeout_minutes`.
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    /// Run the watchdog loop forever.
    pub async fn start(&self) {
        log::info!(
            "Starting watchdog with run timeout {} minutes",
            self.state.run_timeout_minutes
        );
        loop {
            if let Err(e) = self.check_active_runs().await {
                log::error!("Watchdog check failed: {}", e);
            }
            tokio::time::sleep(KEEPALIVE_INTERVAL).await;
        }
    }

    /// Health check every active run whose last keepalive is at least a
    /// third of the run timeout old.
    pub async fn check_active_runs(&self) -> Result<(), Error> {
        let ping_threshold = Duration::minutes((self.state.run_timeout_minutes / 3) as i64);
        let mut checks = Vec::new();
        for run in self.state.active_runs.list().await {
            let last_keepalive = self
                .state
                .active_runs
                .last_keepalive(&run.log_id)
                .await?
                .unwrap_or(run.start_time);
            let keepalive_age = Utc::now() - last_keepalive;
            if keepalive_age < ping_threshold {
                continue;
            }
            checks.push(async move {
                let result = self.healthcheck_active_run(&run, keepalive_age).await;
                (run, result)
            });
        }
        for (run, result) in futures::future::join_all(checks).await {
            if let Err(e) = result {
                log::error!("Failed to healthcheck {}: {}", run.log_id, e);
            }
        }
        Ok(())
    }

    async fn healthcheck_active_run(
        &self,
        run: &ActiveRun,
        mut keepalive_age: Duration,
    ) -> Result<(), Error> {
        match run.ping().await {
            Ok(()) => {
                self.state
                    .active_runs
                    .record_keepalive(&run.log_id, Utc::now())
                    .await?;
                keepalive_age = Duration::zero();
            }
            // Runs without a usable backchannel fall through to the
            // regular keepalive timeout below unless they're over a day old.
            Err(PingError::NotSupported(_)) => {
                if keepalive_age > Duration::days(1) {
                    return self
                        .abort_run(
                            run,
                            "run-disappeared",
                            "no support for ping, and haven't heard back in > 1 day",
                        )
                        .await;
                }
            }
            Err(PingError::Fatal(reason)) => {
                return self.abort_run(run, "run-disappeared", &reason).await;
            }
            Err(e) => {
                log::warn!("Failed to ping {}: {}", run.log_id, e);
            }
        }

        if keepalive_age > Duration::minutes(self.state.run_timeout_minutes as i64) {
            let age = format_timedelta(keepalive_age);
            log::warn!(
                "No keepalives received from {} for {} in {}, aborting.",
                run.worker_name,
                run.log_id,
                age
            );
            self.abort_run(
                run,
                "worker-timeout",
                &format!("No keepalives received in {}.", age),
            )
            .await?;
        }
        Ok(())
    }

    async fn abort_run(&self, run: &ActiveRun, code: &str, description: &str) -> Result<(), Error> {
        let outcome =
            crate::web::abort_run(&self.state, run, code, description, Some(true)).await?;
        if outcome == FinishOutcome::AlreadyStored {
            log::warn!("Run {} exists. Not properly cleaned up?", run.log_id);
        }
        Ok(())
    }
}

/// Format a duration like Python's `str(timedelta)`, e.g.
/// `1 day, 2:03:04.000005`.
pub fn format_timedelta(d: Duration) -> String {
    let total_us = d.num_seconds() as i128 * 1_000_000 + (d.subsec_nanos() / 1000) as i128;
    let days = total_us.div_euclid(86_400_000_000);
    let rest = total_us.rem_euclid(86_400_000_000);
    let micros = rest % 1_000_000;
    let secs = rest / 1_000_000;
    let mut s = format!("{}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60);
    if days != 0 {
        let plural = if days.abs() != 1 { "s" } else { "" };
        s = format!("{} day{}, {}", days, plural, s);
    }
    if micros != 0 {
        s.push_str(&format!(".{:06}", micros));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_timedelta_matches_python() {
        assert_eq!(format_timedelta(Duration::zero()), "0:00:00");
        assert_eq!(format_timedelta(Duration::minutes(61)), "1:01:00");
        assert_eq!(
            format_timedelta(Duration::seconds(86400 + 3723) + Duration::microseconds(5)),
            "1 day, 1:02:03.000005"
        );
        assert_eq!(format_timedelta(Duration::days(3)), "3 days, 0:00:00");
        assert_eq!(format_timedelta(Duration::seconds(-1)), "-1 day, 23:59:59");
    }
}
