//! `PublishWorker::publish_one` must hold the branch lock for as long as the
//! `janitor-publish-one` child runs, and no longer.

use janitor::publish::Mode;
use janitor::vcs::RemoteBzrVcsManager;
use janitor_publish::{PublishError, PublishOneResult, PublishWorker};
use serial_test::serial;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// A `janitor-publish-one` that acts on the name of its template path.
const FAKE_CHILD: &str = r#"#!/bin/sh
ok='{"branch_name": "b", "target_branch_url": "https://example.com/", "mode": "propose"}'
case "$1" in
*/slow) /bin/sleep 5; echo "$ok" ;;
*/quick) /bin/sleep 1; echo "$ok" ;;
*/hang) echo $$ > "${1#--template-env-path=}.pid"; exec /bin/sleep 30 ;;
*) exit 1 ;;
esac
"#;

/// Put the fake child on `PATH`; `None` when there is no Redis to test with.
async fn setup() -> Option<(String, PathBuf)> {
    janitor_runner::test_utils::ensure_redis().await;
    let Ok(redis_url) = std::env::var("TEST_REDIS_URL") else {
        eprintln!("skipping: TEST_REDIS_URL unset and no container");
        return None;
    };
    use std::os::unix::fs::PermissionsExt;
    let bin_dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("publish-lock-renew-bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let fake = bin_dir.join("janitor-publish-one");
    let staged = bin_dir.join("janitor-publish-one.new");
    std::fs::write(&staged, FAKE_CHILD).unwrap();
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(&staged, &fake).unwrap();
    // The tests are serial, so nothing else reads PATH meanwhile.
    std::env::set_var("PATH", &bin_dir);
    Some((redis_url, bin_dir))
}

async fn worker(redis_url: &str, child: PathBuf, lifetime: Duration) -> PublishWorker {
    let mut worker = PublishWorker::new(
        Some(child),
        None,
        "http://localhost:1/".parse().unwrap(),
        None,
        None,
        Some(rslock::LockManager::new(vec![redis_url.to_string()])),
    )
    .await;
    worker.lock_lifetime = lifetime;
    worker
}

fn target() -> url::Url {
    format!("https://example.com/lock-test-{}", uuid::Uuid::new_v4())
        .parse()
        .unwrap()
}

async fn publish(
    worker: &PublishWorker,
    target: &url::Url,
) -> Result<PublishOneResult, PublishError> {
    worker
        .publish_one(
            "campaign",
            "codebase",
            "true",
            target,
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
        .await
}

/// Whether a second publisher could take the branch lock within `wait`.
async fn lock_is_free(redis_url: &str, target: &url::Url, wait: Duration) -> bool {
    let lock_manager = rslock::LockManager::new(vec![redis_url.to_string()]);
    let deadline = Instant::now() + wait;
    loop {
        if let Ok(lock) = lock_manager
            .lock(
                format!("publish:{target}").as_bytes(),
                Duration::from_secs(1),
            )
            .await
        {
            lock_manager.unlock(&lock).await;
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
#[serial]
async fn long_child_keeps_branch_lock() {
    let Some((redis_url, bin_dir)) = setup().await else {
        return;
    };
    let target = target();
    // The child runs for 5 s, more than three lock lifetimes.
    let worker = worker(
        &redis_url,
        bin_dir.join("slow"),
        Duration::from_millis(1500),
    )
    .await;

    let publish = publish(&worker, &target);
    tokio::pin!(publish);
    let started = Instant::now();
    let mut refused = 0;
    let result = loop {
        tokio::select! {
            result = &mut publish => break result,
            _ = tokio::time::sleep(Duration::from_millis(250)) => {
                assert!(
                    !lock_is_free(&redis_url, &target, Duration::ZERO).await,
                    "a second publish got the branch lock after {:?}, with the child still running",
                    started.elapsed()
                );
                refused += 1;
            }
        }
    };
    assert!(result.is_ok(), "unexpected result: {result:?}");
    assert!(started.elapsed() >= Duration::from_secs(5));
    assert!(refused >= 5, "only {refused} lock attempts were made");
    assert!(
        lock_is_free(&redis_url, &target, Duration::ZERO).await,
        "branch lock still held after the child ended"
    );
}

#[tokio::test]
#[serial]
async fn cancelled_publish_kills_child_and_releases_branch_lock() {
    let Some((redis_url, bin_dir)) = setup().await else {
        return;
    };
    let target = target();
    let pid_file = bin_dir.join("hang.pid");
    let _ = std::fs::remove_file(&pid_file);
    let worker = worker(&redis_url, bin_dir.join("hang"), Duration::from_secs(60)).await;

    let cancelled = tokio::time::timeout(Duration::from_secs(1), publish(&worker, &target)).await;
    assert!(cancelled.is_err(), "the child did not hang: {cancelled:?}");

    let pid = std::fs::read_to_string(&pid_file).unwrap();
    let stat = format!("/proc/{}/stat", pid.trim());
    let deadline = Instant::now() + Duration::from_secs(2);
    // A killed child is gone, or a zombie until it is reaped.
    while std::fs::read_to_string(&stat).is_ok_and(|s| !s.contains(") Z ")) {
        assert!(Instant::now() < deadline, "the child is still running");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        lock_is_free(&redis_url, &target, Duration::from_secs(2)).await,
        "branch lock still held after the publish was cancelled"
    );
}

#[tokio::test]
#[serial]
async fn renewal_stops_after_release() {
    let Some((redis_url, bin_dir)) = setup().await else {
        return;
    };
    let target = target();
    let lifetime = Duration::from_millis(300);
    let worker = worker(&redis_url, bin_dir.join("quick"), lifetime).await;

    // Open both Redis connections first: each one keeps a task of its own.
    let lock_manager = worker.lock_manager.as_ref().unwrap();
    let warm_up = lock_manager
        .lock(format!("publish:{}", self::target()).as_bytes(), lifetime)
        .await
        .unwrap();
    lock_manager.unlock(&warm_up).await;
    let mut redis = redis::Client::open(redis_url.as_str())
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    let metrics = tokio::runtime::Handle::current().metrics();
    let tasks_before = metrics.num_alive_tasks();

    let result = publish(&worker, &target).await;
    assert!(result.is_ok(), "unexpected result: {result:?}");

    // Leave time for more renewals, had the task been left running.
    tokio::time::sleep(lifetime * 2).await;
    assert_eq!(
        metrics.num_alive_tasks(),
        tasks_before,
        "the renewal task is still alive"
    );
    let left: bool = redis::cmd("EXISTS")
        .arg(format!("publish:{target}"))
        .query_async(&mut redis)
        .await
        .unwrap();
    assert!(!left, "the lock key is still there");
}

#[tokio::test]
#[serial]
async fn lost_branch_lock_is_left_alone() {
    let Some((redis_url, bin_dir)) = setup().await else {
        return;
    };
    let target = target();
    let key = format!("publish:{target}");
    let worker = worker(
        &redis_url,
        bin_dir.join("quick"),
        Duration::from_millis(600),
    )
    .await;
    let mut redis = redis::Client::open(redis_url.as_str())
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();

    // Another owner takes the key over while the child runs.
    let mut thief = redis.clone();
    let steal = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _: () = redis::cmd("SET")
            .arg(&key)
            .arg("someone-else")
            .arg("PX")
            .arg(30_000)
            .query_async(&mut thief)
            .await
            .unwrap();
    };
    let (result, ()) = tokio::join!(publish(&worker, &target), steal);
    assert!(result.is_ok(), "unexpected result: {result:?}");

    // Leave time for the release.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let owner: Option<String> = redis::cmd("GET")
        .arg(&key)
        .query_async(&mut redis)
        .await
        .unwrap();
    assert_eq!(owner.as_deref(), Some("someone-else"));
    let _: () = redis::cmd("DEL")
        .arg(&key)
        .query_async(&mut redis)
        .await
        .unwrap();
}
