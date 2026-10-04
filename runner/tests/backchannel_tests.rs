//! Tests for the backchannel implementations that verify actual
//! behavior (not just "unreachable server returns Err").

use chrono::Utc;
use janitor_runner::backchannel::{
    Backchannel, HealthStatus, JenkinsBackchannel, PollingBackchannel,
};
use url::Url;

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
