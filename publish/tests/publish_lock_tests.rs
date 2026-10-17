//! `PublishWorker::publish_one` must release the branch lock when the
//! `janitor-publish-one` child fails.

use janitor::publish::Mode;
use janitor::vcs::RemoteBzrVcsManager;
use janitor_publish::{PublishError, PublishWorker};
use std::time::Duration;

#[tokio::test]
async fn failed_child_releases_branch_lock() {
    janitor_runner::test_utils::ensure_redis().await;
    let Ok(redis_url) = std::env::var("TEST_REDIS_URL") else {
        eprintln!("skipping: TEST_REDIS_URL unset and no container");
        return;
    };

    // A `janitor-publish-one` that exits 1 without writing a response.
    let bin_dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("publish-lock-bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let fake = bin_dir.join("janitor-publish-one");
    let _ = std::fs::remove_file(&fake);
    std::os::unix::fs::symlink("/bin/false", &fake).unwrap();
    // This is the only test in this binary, so nothing else reads PATH.
    std::env::set_var("PATH", &bin_dir);

    let target: url::Url = format!("https://example.com/lock-test-{}", uuid::Uuid::new_v4())
        .parse()
        .unwrap();
    let worker = PublishWorker::new(
        None,
        None,
        "http://localhost:1/".parse().unwrap(),
        None,
        None,
        Some(rslock::LockManager::new(vec![redis_url.clone()])),
    )
    .await;

    let result = worker
        .publish_one(
            "campaign",
            "codebase",
            "true",
            &target,
            Mode::Propose,
            "main",
            &breezyshim::RevisionId::from(b"rev".to_vec()),
            "log-id",
            "unchanged-id",
            "campaign",
            None,
            &RemoteBzrVcsManager::new("https://example.com/bzr/".parse().unwrap()),
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
        )
        .await;
    assert!(
        matches!(&result, Err(PublishError::Failure { code, .. }) if code == "publisher-invalid-response"),
        "unexpected result: {result:?}"
    );

    let lock_manager = rslock::LockManager::new(vec![redis_url]);
    let lock = lock_manager
        .lock(
            format!("publish:{target}").as_bytes(),
            Duration::from_secs(1),
        )
        .await
        .expect("branch lock still held after the child failed");
    lock_manager.unlock(&lock).await;
}
