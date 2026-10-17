//! `PublishWorker::publish_one` must return when the `janitor-publish-one`
//! child writes more than a pipe buffer to stdout or stderr.

use janitor::publish::Mode;
use janitor::vcs::RemoteBzrVcsManager;
use janitor_publish::{PublishError, PublishOneResult, PublishWorker};
use serial_test::serial;
use std::path::PathBuf;
use std::time::Duration;

/// Bytes the fake child writes: far more than a 64 KiB pipe buffer.
const SIZE: usize = 1024 * 1024;

/// A `janitor-publish-one` that acts on the name of its template path.
const FAKE_CHILD: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin
tail=', "target_branch_url": "https://example.com/", "mode": "propose"}'
case "$1" in
*/stderr) head -c 1048576 /dev/zero >&2; echo '{"branch_name": "b"'"$tail" ;;
*/stdout) printf '{"branch_name": "'; head -c 1048576 /dev/zero | tr '\0' b; echo '"'"$tail" ;;
*) exit 2 ;;
esac
"#;

/// Put the fake child on `PATH`.
fn setup() -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin_dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("publish-pipe-bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let fake = bin_dir.join("janitor-publish-one");
    let staged = bin_dir.join("janitor-publish-one.new");
    std::fs::write(&staged, FAKE_CHILD).unwrap();
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(&staged, &fake).unwrap();
    // The tests are serial, so nothing else reads PATH meanwhile.
    std::env::set_var("PATH", &bin_dir);
    bin_dir
}

/// Run `publish_one` against the fake child; panic if it does not return.
async fn publish(kind: &str) -> Result<PublishOneResult, PublishError> {
    let bin_dir = setup();
    let worker = PublishWorker::new(
        Some(bin_dir.join(kind)),
        None,
        "http://localhost:1/".parse().unwrap(),
        None,
        None,
        None,
    )
    .await;
    let target: url::Url = "https://example.com/pipe-test".parse().unwrap();
    let revision = breezyshim::RevisionId::from(b"rev".to_vec());
    let vcs_manager = RemoteBzrVcsManager::new("https://example.com/bzr/".parse().unwrap());
    let publish = worker.publish_one(
        "campaign",
        "codebase",
        "true",
        &target,
        Mode::Propose,
        "main",
        &revision,
        "log-id",
        "unchanged-id",
        "campaign",
        None,
        &vcs_manager,
        None,
        false,
        true,
        None,
        None,
        None,
        None,
        &serde_json::Value::Null,
        None,
        None,
        None,
        None,
    );
    tokio::time::timeout(Duration::from_secs(5), publish)
        .await
        .unwrap_or_else(|_| panic!("publish_one still waits for the child after 5 s"))
}

/// 1 MiB on stderr, then the response on stdout.
#[tokio::test]
#[serial]
async fn large_stderr_does_not_block_publish() {
    let result = publish("stderr").await.unwrap();
    assert_eq!(result.branch_name(), "b");
}

/// A response of more than 1 MiB on stdout is read whole.
#[tokio::test]
#[serial]
async fn large_stdout_does_not_block_publish() {
    let result = publish("stdout").await.unwrap();
    assert_eq!(result.branch_name().len(), SIZE);
}
