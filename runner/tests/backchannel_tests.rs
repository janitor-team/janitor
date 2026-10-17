//! Tests for the backchannel implementations that verify actual
//! behavior (not just "unreachable server returns Err").

use axum::http::StatusCode;
use axum::routing::{get, post};
use chrono::Utc;
use janitor_runner::backchannel::{
    Backchannel, Error, HealthStatus, JenkinsBackchannel, PollingBackchannel,
};
use url::Url;

/// Serve `app` on a free local port and return a polling backchannel
/// pointed at it. Routes the app lacks answer 404, like a real worker.
async fn polling_backchannel_for(app: axum::Router) -> PollingBackchannel {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    PollingBackchannel::new(Url::parse(&format!("http://{addr}/")).unwrap())
}

/// Jenkins only exposes a single log file (`worker.log`) via the
/// progressiveText endpoint. `list_log_files` should reflect that
/// without hitting the network.
#[tokio::test]
async fn jenkins_list_log_files_returns_only_worker_log() {
    let base_url = Url::parse("http://jenkins.invalid").unwrap();
    let backchannel = JenkinsBackchannel::new(base_url);

    let files = backchannel.list_log_files().await.unwrap();
    assert_eq!(files, vec!["worker.log".to_string()]);
}

/// `HealthStatus` is exchanged with the watchdog through Redis; the
/// serde round-trip must be stable for all fields.
#[test]
fn health_status_round_trip() {
    let original = HealthStatus {
        alive: true,
        current_run_id: Some("run-1".to_string()),
        status: "healthy".to_string(),
        last_ping: Some(Utc::now()),
        uptime: Some(std::time::Duration::from_secs(3600)),
    };

    let json = serde_json::to_string(&original).unwrap();
    let parsed: HealthStatus = serde_json::from_str(&json).unwrap();

    assert_eq!(parsed.alive, original.alive);
    assert_eq!(parsed.current_run_id, original.current_run_id);
    assert_eq!(parsed.status, original.status);
    assert_eq!(parsed.last_ping, original.last_ping);
    assert_eq!(parsed.uptime, original.uptime);
}

/// A polling backchannel pointed at a non-routable IP should time out
/// well within a minute rather than hanging. Uses the reserved TEST-NET
/// address 192.0.2.1; the ping call bounds work with an internal timeout.
#[tokio::test]
async fn polling_backchannel_ping_times_out_promptly() {
    let base_url = Url::parse("http://192.0.2.1:8080").unwrap();
    let backchannel = PollingBackchannel::new(base_url);

    let start = std::time::Instant::now();
    let result = backchannel.ping("run-1").await;
    let elapsed = start.elapsed();

    assert!(result.is_err(), "ping to non-routable host should error");
    assert!(
        elapsed.as_secs() < 90,
        "ping took {}s, expected timeout under 90s",
        elapsed.as_secs()
    );
}

/// An idle worker answers `/log-id` with 404 and a text body. That
/// body is not a run id: the health status must say the run is gone.
#[tokio::test]
async fn polling_health_status_idle_worker_is_not_found() {
    let app = axum::Router::new().route(
        "/log-id",
        get(|| async { (StatusCode::NOT_FOUND, "no run in progress") }),
    );
    let backchannel = polling_backchannel_for(app).await;

    let health = backchannel.get_health_status("run-1").await.unwrap();
    assert_eq!(health.status, "not-found");
    assert_eq!(health.current_run_id, None);
    assert!(!health.alive);

    let result = backchannel.ping("run-1").await;
    assert!(matches!(result, Err(Error::NotFound)), "got {result:?}");
}

/// Any other error status from `/log-id` is not a run id either.
#[tokio::test]
async fn polling_health_status_server_error_is_unreachable() {
    let app = axum::Router::new().route(
        "/log-id",
        get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "oops") }),
    );
    let backchannel = polling_backchannel_for(app).await;

    let health = backchannel.get_health_status("run-1").await.unwrap();
    assert_eq!(health.status, "unreachable");
    assert_eq!(health.current_run_id, None);

    let result = backchannel.ping("run-1").await;
    assert!(
        matches!(result, Err(Error::WorkerUnreachable(_))),
        "got {result:?}"
    );
}

/// A 200 from `/log-id` is still read as the worker's current run id.
#[tokio::test]
async fn polling_health_status_reads_log_id_on_success() {
    let app = axum::Router::new().route("/log-id", get(|| async { "run-1" }));
    let backchannel = polling_backchannel_for(app).await;

    let health = backchannel.get_health_status("run-1").await.unwrap();
    assert_eq!(health.status, "running");
    assert_eq!(health.current_run_id.as_deref(), Some("run-1"));
    assert!(health.alive);
    backchannel.ping("run-1").await.unwrap();

    let health = backchannel.get_health_status("run-2").await.unwrap();
    assert_eq!(health.status, "different-run");
    assert_eq!(health.current_run_id.as_deref(), Some("run-1"));
    let result = backchannel.ping("run-2").await;
    assert!(
        matches!(result, Err(Error::FatalFailure(_))),
        "got {result:?}"
    );
}

/// A busy worker answers `/kill` with 501 and keeps building, so
/// `kill` and the `terminate` fallback onto it must report an error.
#[tokio::test]
async fn polling_kill_not_supported_is_error() {
    let app = axum::Router::new().route(
        "/kill",
        post(|| async { (StatusCode::NOT_IMPLEMENTED, "kill is not yet supported") }),
    );
    let backchannel = polling_backchannel_for(app).await;

    let result = backchannel.kill().await;
    assert!(
        matches!(result, Err(Error::FatalFailure(_))),
        "got {result:?}"
    );

    let result = backchannel.terminate("run-1").await;
    assert!(
        matches!(result, Err(Error::FatalFailure(_))),
        "got {result:?}"
    );
}

/// A worker without a run answers `/kill` with 410.
#[tokio::test]
async fn polling_kill_without_run_is_not_found() {
    let app = axum::Router::new().route(
        "/kill",
        post(|| async { (StatusCode::GONE, "no run in progress") }),
    );
    let backchannel = polling_backchannel_for(app).await;

    let result = backchannel.kill().await;
    assert!(matches!(result, Err(Error::NotFound)), "got {result:?}");
}

/// Any other error status from `/kill` is an error too.
#[tokio::test]
async fn polling_kill_server_error_is_unreachable() {
    let app = axum::Router::new().route(
        "/kill",
        post(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "oops") }),
    );
    let backchannel = polling_backchannel_for(app).await;

    let result = backchannel.kill().await;
    assert!(
        matches!(result, Err(Error::WorkerUnreachable(_))),
        "got {result:?}"
    );
}

/// A 2xx answer from `/kill` is still a success.
#[tokio::test]
async fn polling_kill_success_is_ok() {
    let app = axum::Router::new().route("/kill", post(|| async { "killed" }));
    let backchannel = polling_backchannel_for(app).await;

    backchannel.kill().await.unwrap();
    backchannel.terminate("run-1").await.unwrap();
}
