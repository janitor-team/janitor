//! Tests for the watchdog that aborts runs without keepalives.

use chrono::{Duration, Utc};
use janitor_runner::watchdog::Watchdog;
use janitor_runner::{ActiveRun, AppState, Backchannel, VcsInfo};
use janitor_types::{Campaign, CodebaseName, RunId};
use std::sync::Arc;

const RUN_TIMEOUT_MINUTES: u64 = 60;

fn rid(s: &str) -> RunId {
    RunId::new(s).unwrap()
}

/// Register `run` as active with its last keepalive `keepalive_age`
/// ago. Returns `None` if no database/redis is available.
async fn setup(run: &ActiveRun, keepalive_age: Duration) -> Option<Arc<AppState>> {
    janitor_runner::test_utils::ensure_redis().await;
    let state = janitor_runner::test_utils::create_test_app_state_if_available()
        .await
        .unwrap()?;
    let pool = state.database.pool();
    sqlx::query(
        "INSERT INTO codebase (name, branch_url, url, vcs_type)
         VALUES ($1, 'https://example.invalid/x', 'https://example.invalid/x', 'git')",
    )
    .bind(&run.codebase)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO worker (name, password) VALUES ($1, 'pw')")
        .bind(&run.worker_name)
        .execute(pool)
        .await
        .unwrap();
    state.active_runs.store(run.clone()).await;
    state
        .active_runs
        .record_keepalive(&run.log_id, Utc::now() - keepalive_age)
        .await
        .unwrap();
    Some(state)
}

fn make_run(log_id: &str, start_age: Duration, backchannel: Backchannel) -> ActiveRun {
    ActiveRun {
        worker_name: "worker-1".to_string(),
        worker_link: None,
        queue_id: 1,
        log_id: rid(log_id),
        start_time: Utc::now() - start_age,
        finish_time: None,
        estimated_duration: Some(std::time::Duration::from_secs(30)),
        campaign: Campaign::new("lintian-fixes").unwrap(),
        change_set: None,
        command: "lintian-brush".to_string(),
        backchannel,
        vcs_info: VcsInfo::default(),
        codebase: CodebaseName::new("watchdog-codebase").unwrap(),
        instigated_context: None,
        resume_from: None,
    }
}

/// Serve a worker backchannel whose `/log-id` reports `log_id`.
async fn serve_worker(log_id: &'static str) -> Backchannel {
    let app =
        axum::Router::new().route("/log-id", axum::routing::get(move || async move { log_id }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Backchannel::Polling {
        my_url: format!("http://{}", addr).parse().unwrap(),
    }
}

/// An unreachable worker: pings fail, but not fatally.
fn unreachable_worker() -> Backchannel {
    Backchannel::Polling {
        my_url: "http://127.0.0.1:1".parse().unwrap(),
    }
}

async fn stored_result(state: &AppState, log_id: &str) -> Option<(String, String, Option<bool>)> {
    sqlx::query_as("SELECT result_code, description, failure_transient FROM run WHERE id = $1")
        .bind(log_id)
        .fetch_optional(state.database.pool())
        .await
        .unwrap()
}

fn watchdog(state: &AppState) -> Watchdog {
    Watchdog::new(
        state.database.clone(),
        state.active_runs.clone(),
        RUN_TIMEOUT_MINUTES,
    )
}

/// A run that has been going for much longer than the run timeout
/// (and its estimate) is left alone as long as keepalives arrive.
#[tokio::test]
async fn test_watchdog_ignores_long_runs_with_recent_keepalive() {
    let run = make_run("long-run", Duration::days(2), unreachable_worker());
    let Some(state) = setup(&run, Duration::minutes(5)).await else {
        return;
    };
    watchdog(&state).check_active_runs().await.unwrap();
    assert!(state.active_runs.get(&rid("long-run")).await.is_some());
    assert_eq!(stored_result(&state, "long-run").await, None);
}

/// Without keepalives for longer than the run timeout, the run is
/// aborted as a transient worker-timeout.
#[tokio::test]
async fn test_watchdog_aborts_run_without_keepalives() {
    let run = make_run("stale-run", Duration::minutes(90), unreachable_worker());
    let Some(state) = setup(&run, Duration::minutes(61)).await else {
        return;
    };
    watchdog(&state).check_active_runs().await.unwrap();
    assert!(state.active_runs.get(&rid("stale-run")).await.is_none());
    let (code, description, transient) = stored_result(&state, "stale-run").await.unwrap();
    assert_eq!(code, "worker-timeout");
    assert!(
        description.starts_with("No keepalives received in 1:01:0"),
        "{}",
        description
    );
    assert_eq!(transient, Some(true));
}

/// A successful ping counts as a keepalive.
#[tokio::test]
async fn test_watchdog_ping_refreshes_keepalive() {
    let backchannel = serve_worker("alive-run").await;
    let run = make_run("alive-run", Duration::minutes(90), backchannel);
    let Some(state) = setup(&run, Duration::minutes(61)).await else {
        return;
    };
    let before = Utc::now();
    watchdog(&state).check_active_runs().await.unwrap();
    assert!(state.active_runs.get(&rid("alive-run")).await.is_some());
    let keepalive = state
        .active_runs
        .last_keepalive(&rid("alive-run"))
        .await
        .unwrap()
        .unwrap();
    assert!(keepalive >= before - Duration::seconds(1));
    assert_eq!(stored_result(&state, "alive-run").await, None);
}

/// A worker that has moved on to another run is reported as
/// run-disappeared, regardless of keepalive age.
#[tokio::test]
async fn test_watchdog_aborts_run_when_worker_moved_on() {
    let backchannel = serve_worker("other-run").await;
    let run = make_run("lost-run", Duration::minutes(30), backchannel);
    let Some(state) = setup(&run, Duration::minutes(25)).await else {
        return;
    };
    watchdog(&state).check_active_runs().await.unwrap();
    assert!(state.active_runs.get(&rid("lost-run")).await.is_none());
    assert_eq!(
        stored_result(&state, "lost-run").await,
        Some((
            "run-disappeared".to_string(),
            "Worker started processing new run other-run rather than lost-run".to_string(),
            Some(true)
        ))
    );
}

/// Runs whose keepalive is younger than a third of the timeout are
/// not pinged at all.
#[tokio::test]
async fn test_watchdog_does_not_ping_fresh_runs() {
    let backchannel = serve_worker("other-run").await;
    let run = make_run("fresh-run", Duration::minutes(30), backchannel);
    let Some(state) = setup(&run, Duration::minutes(19)).await else {
        return;
    };
    watchdog(&state).check_active_runs().await.unwrap();
    assert!(state.active_runs.get(&rid("fresh-run")).await.is_some());
}

/// Runs without a backchannel time out like any other run.
#[tokio::test]
async fn test_watchdog_aborts_run_without_backchannel_on_timeout() {
    let run = make_run("no-bc-run", Duration::minutes(90), Backchannel::None {});
    let Some(state) = setup(&run, Duration::minutes(61)).await else {
        return;
    };
    watchdog(&state).check_active_runs().await.unwrap();
    let (code, _, _) = stored_result(&state, "no-bc-run").await.unwrap();
    assert_eq!(code, "worker-timeout");
}

/// Runs without a backchannel that haven't been heard from in over
/// a day are reported as run-disappeared.
#[tokio::test]
async fn test_watchdog_aborts_run_without_backchannel_after_a_day() {
    let run = make_run("old-no-bc-run", Duration::days(3), Backchannel::None {});
    let Some(state) = setup(&run, Duration::days(2)).await else {
        return;
    };
    watchdog(&state).check_active_runs().await.unwrap();
    assert_eq!(
        stored_result(&state, "old-no-bc-run").await,
        Some((
            "run-disappeared".to_string(),
            "no support for ping, and haven't heard back in > 1 day".to_string(),
            Some(true)
        ))
    );
}
