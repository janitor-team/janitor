//! Tests for the backchannel implementations that verify actual
//! behavior (not just "unreachable server returns Err").

use axum::http::StatusCode;
use axum::routing::{get, post};
use chrono::Utc;
use janitor_runner::backchannel::{
    Backchannel, HealthStatus, JenkinsBackchannel, PollingBackchannel,
};
use url::Url;

/// Serve `app` on a free local port; return a polling backchannel to it and its base URL.
async fn polling_backchannel_for(app: axum::Router) -> (PollingBackchannel, String) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (PollingBackchannel::new(Url::parse(&base).unwrap()), base)
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
    let result = backchannel
        .ping(&janitor::RunId::new("run-1").unwrap())
        .await;
    let elapsed = start.elapsed();

    assert!(result.is_err(), "ping to non-routable host should error");
    assert!(
        elapsed.as_secs() < 90,
        "ping took {}s, expected timeout under 90s",
        elapsed.as_secs()
    );
}

/// An idle worker's 404 body from `/log-id` is not a run id.
#[tokio::test]
async fn polling_health_status_idle_worker_is_not_found() {
    let app = axum::Router::new().route(
        "/log-id",
        get(|| async { (StatusCode::NOT_FOUND, "no run in progress") }),
    );
    let (backchannel, _) = polling_backchannel_for(app).await;

    let health = backchannel.get_health_status("run-1").await.unwrap();
    assert_eq!(
        (health.alive, health.current_run_id, health.status),
        (false, None, "not-found".to_string())
    );

    let result = backchannel.ping("run-1").await.map_err(|e| e.to_string());
    assert_eq!(result, Err("Job not found".to_string()));
}

/// Any other error status from `/log-id` is not a run id either.
#[tokio::test]
async fn polling_health_status_server_error_is_unreachable() {
    let app = axum::Router::new().route(
        "/log-id",
        get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "oops") }),
    );
    let (backchannel, base) = polling_backchannel_for(app).await;

    let health = backchannel.get_health_status("run-1").await.unwrap();
    assert_eq!(
        (health.alive, health.current_run_id, health.status),
        (false, None, "unreachable".to_string())
    );

    let result = backchannel.ping("run-1").await.map_err(|e| e.to_string());
    assert_eq!(
        result,
        Err(format!(
            "Worker unreachable: HTTP status server error (500 Internal Server Error) for url ({base}log-id)"
        ))
    );
}

/// A 200 from `/log-id` is still read as the worker's current run id.
#[tokio::test]
async fn polling_health_status_reads_log_id_on_success() {
    let app = axum::Router::new().route("/log-id", get(|| async { "run-1" }));
    let (backchannel, _) = polling_backchannel_for(app).await;

    let health = backchannel.get_health_status("run-1").await.unwrap();
    assert_eq!(
        (health.alive, health.current_run_id, health.status),
        (true, Some("run-1".to_string()), "running".to_string())
    );
    backchannel.ping("run-1").await.unwrap();

    let health = backchannel.get_health_status("run-2").await.unwrap();
    assert_eq!(
        (health.alive, health.current_run_id, health.status),
        (
            false,
            Some("run-1".to_string()),
            "different-run".to_string()
        )
    );
    let result = backchannel.ping("run-2").await.map_err(|e| e.to_string());
    assert_eq!(
        result,
        Err("Fatal failure: Worker started processing new run run-1 rather than run-2".to_string())
    );
}

/// A 501 answer to `/kill` makes `kill` and `terminate` fail.
#[tokio::test]
async fn polling_kill_not_supported_is_error() {
    let app = axum::Router::new().route(
        "/kill",
        post(|| async { (StatusCode::NOT_IMPLEMENTED, "kill is not yet supported") }),
    );
    let (backchannel, base) = polling_backchannel_for(app).await;
    let expected = Err(format!(
        "Fatal failure: kill not supported by worker at {base}"
    ));

    assert_eq!(
        backchannel.kill().await.map_err(|e| e.to_string()),
        expected
    );
    assert_eq!(
        backchannel
            .terminate("run-1")
            .await
            .map_err(|e| e.to_string()),
        expected
    );
}

/// A worker without a run answers `/kill` with 410.
#[tokio::test]
async fn polling_kill_without_run_is_not_found() {
    let app = axum::Router::new().route(
        "/kill",
        post(|| async { (StatusCode::GONE, "no run in progress") }),
    );
    let (backchannel, _) = polling_backchannel_for(app).await;

    let result = backchannel.kill().await.map_err(|e| e.to_string());
    assert_eq!(result, Err("Job not found".to_string()));
}

/// Any other error status from `/kill` is an error too.
#[tokio::test]
async fn polling_kill_server_error_is_unreachable() {
    let app = axum::Router::new().route(
        "/kill",
        post(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "oops") }),
    );
    let (backchannel, _) = polling_backchannel_for(app).await;

    let result = backchannel.kill().await.map_err(|e| e.to_string());
    assert_eq!(
        result,
        Err(
            "Worker unreachable: kill request failed with status: 500 Internal Server Error"
                .to_string()
        )
    );
}

/// A 2xx answer from `/kill` is still a success.
#[tokio::test]
async fn polling_kill_success_is_ok() {
    let app = axum::Router::new().route("/kill", post(|| async { "killed" }));
    let (backchannel, _) = polling_backchannel_for(app).await;

    backchannel.kill().await.unwrap();
    backchannel.terminate("run-1").await.unwrap();
}
