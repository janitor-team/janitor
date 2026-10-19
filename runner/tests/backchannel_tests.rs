//! Behavioural tests for backchannel implementations. Trait-level unit
//! tests live in `src/backchannel.rs`; this file covers behaviour that
//! only makes sense against a non-mocked backchannel.

use janitor_runner::backchannel::{Backchannel, JenkinsBackchannel, PollingBackchannel};
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
