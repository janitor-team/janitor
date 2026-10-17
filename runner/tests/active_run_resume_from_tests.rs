//! Test that an active run keeps `resume_from` in the active run
//! store. Skipped cleanly when there's no test database or Redis
//! available.

use chrono::Utc;
use janitor::queue::VcsInfo;
use janitor_runner::{test_utils, ActiveRun, AppState, Backchannel};
use std::sync::Arc;

/// Set up the test router + AppState, or skip if no DB/Redis.
async fn setup() -> Option<(axum::Router, Arc<AppState>)> {
    test_utils::create_test_app_with_state_if_available()
        .await
        .expect("test app setup should either succeed or return None cleanly")
}

/// An active run keeps its `resume_from` when it is stored and read
/// back.
#[tokio::test]
async fn active_run_store_keeps_resume_from() {
    let Some((_app, state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let active_run = ActiveRun {
        worker_name: "resume-worker".to_string(),
        worker_link: None,
        queue_id: 1,
        log_id: "run-with-resume".to_string(),
        start_time: Utc::now(),
        finish_time: None,
        estimated_duration: None,
        campaign: "test-campaign".to_string(),
        change_set: None,
        command: "true".to_string(),
        codebase: "resume-cb".to_string(),
        backchannel: Backchannel::None {},
        vcs_info: VcsInfo::default(),
        instigated_context: None,
        resume_from: Some("earlier-run".to_string()),
    };
    state.active_runs.store(active_run).await;

    let reloaded = state
        .active_runs
        .get("run-with-resume")
        .await
        .expect("stored run must be readable");
    assert_eq!(reloaded.resume_from.as_deref(), Some("earlier-run"));
}
