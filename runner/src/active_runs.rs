//! Redis-backed active-run tracking.
//!
//! Active runs are persisted to a Redis hash so they survive runner
//! restarts (so a worker that uploaded mid-run can still hit `/finish`
//! after the runner pod is recreated) and so multiple runner replicas
//! can share the same view of in-flight work. There is intentionally no
//! local in-memory cache -- every read goes to Redis. The active set is
//! small (tens of entries at most), so the round-trip cost is
//! negligible compared to the consistency guarantees.
//!
//! See salsa.debian.org/janitor-team/janitor.debian.net#117 for the
//! original failure mode (`/finish` 404'd after a runner restart and the
//! worker's run was lost).
//!
//! Concurrent terminations from multiple replicas are safe: the run row
//! is gated by a `UNIQUE` constraint on `run.id` and `HDEL` is
//! idempotent, so two pods racing to write a "worker-timeout" both
//! converge to the same end state.

use crate::ActiveRun;
use redis::AsyncCommands;
use std::collections::HashMap;

/// The default Redis hash key under which active runs are stored.
const DEFAULT_KEY: &str = "runner:active-runs";

/// Shared Redis-backed store of currently-active runs, keyed by `log_id`.
#[derive(Clone)]
pub struct ActiveRunStore {
    redis: redis::Client,
    key: String,
}

impl ActiveRunStore {
    /// Construct a store backed by the given Redis client, using the
    /// default `runner:active-runs` hash key.
    pub fn new(redis: redis::Client) -> Self {
        Self::with_key(redis, DEFAULT_KEY.to_string())
    }

    /// Construct a store with a custom hash key -- useful for tests so
    /// parallel test cases don't collide on a shared key.
    pub fn with_key(redis: redis::Client, key: String) -> Self {
        Self { redis, key }
    }

    async fn conn(&self) -> Option<redis::aio::MultiplexedConnection> {
        match self.redis.get_multiplexed_async_connection().await {
            Ok(c) => Some(c),
            Err(e) => {
                log::warn!("active_runs: redis connect failed: {}", e);
                None
            }
        }
    }

    /// Record a newly-assigned active run. Overwrites any existing
    /// entry with the same `log_id`. Errors are logged; callers don't
    /// get a Result because no caller has anything useful to do on
    /// Redis failure (the queue-item assignment in `assign_queue_item`
    /// follows the same log-and-continue pattern).
    pub async fn store(&self, active_run: ActiveRun) {
        let json = match serde_json::to_string(&active_run) {
            Ok(s) => s,
            Err(e) => {
                log::error!(
                    "active_runs.store: serialize failed for {}: {}",
                    active_run.log_id,
                    e
                );
                return;
            }
        };
        let Some(mut conn) = self.conn().await else {
            return;
        };
        let res: redis::RedisResult<()> = conn.hset(&self.key, &active_run.log_id, json).await;
        if let Err(e) = res {
            log::error!(
                "active_runs.store: HSET failed for {}: {}",
                active_run.log_id,
                e
            );
        }
    }

    /// Fetch a single active run by id.
    pub async fn get(&self, run_id: &str) -> Option<ActiveRun> {
        let mut conn = self.conn().await?;
        let json: Option<String> = match conn.hget(&self.key, run_id).await {
            Ok(v) => v,
            Err(e) => {
                log::warn!("active_runs.get HGET {}: {}", run_id, e);
                return None;
            }
        };
        let json = json?;
        match serde_json::from_str(&json) {
            Ok(r) => Some(r),
            Err(e) => {
                log::warn!("active_runs.get: bad row for {}: {}", run_id, e);
                None
            }
        }
    }

    /// List all currently-active runs. Sorted by `start_time` ascending
    /// so the ordering matches the Python `status_json` output.
    pub async fn list(&self) -> Vec<ActiveRun> {
        let Some(mut conn) = self.conn().await else {
            return Vec::new();
        };
        let map: HashMap<String, String> = match conn.hgetall(&self.key).await {
            Ok(m) => m,
            Err(e) => {
                log::warn!("active_runs.list HGETALL: {}", e);
                return Vec::new();
            }
        };
        let mut runs: Vec<ActiveRun> = map
            .into_iter()
            .filter_map(
                |(log_id, json)| match serde_json::from_str::<ActiveRun>(&json) {
                    Ok(r) => Some(r),
                    Err(e) => {
                        log::warn!("active_runs.list: bad row for {}: {}", log_id, e);
                        None
                    }
                },
            )
            .collect();
        runs.sort_by_key(|r| r.start_time);
        runs
    }

    /// Drop an active run from the store. Returns `true` if a row was
    /// actually removed.
    pub async fn remove(&self, run_id: &str) -> bool {
        let Some(mut conn) = self.conn().await else {
            return false;
        };
        match conn.hdel::<_, _, u32>(&self.key, run_id).await {
            Ok(n) => n > 0,
            Err(e) => {
                log::warn!("active_runs.remove HDEL {}: {}", run_id, e);
                false
            }
        }
    }

    /// Count the number of active runs for a given worker. Used by the
    /// concurrency-limit check in `auth::SecurityService::can_start_run`.
    pub async fn count_for_worker(&self, worker_name: &str) -> usize {
        self.list()
            .await
            .into_iter()
            .filter(|r| r.worker_name == worker_name)
            .count()
    }

    /// Count the total number of active runs.
    pub async fn len(&self) -> usize {
        let Some(mut conn) = self.conn().await else {
            return 0;
        };
        match conn.hlen::<_, usize>(&self.key).await {
            Ok(n) => n,
            Err(e) => {
                log::warn!("active_runs.len HLEN: {}", e);
                0
            }
        }
    }

    /// Whether the active-run store has zero entries.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    /// Count the number of distinct workers with at least one active
    /// run. Used by `SecurityService::get_security_stats`.
    pub async fn distinct_worker_count(&self) -> usize {
        let runs = self.list().await;
        let mut workers: std::collections::HashSet<String> = std::collections::HashSet::new();
        for run in runs {
            workers.insert(run.worker_name);
        }
        workers.len()
    }

    /// Remove runs that have been active for longer than `max_age`.
    /// Returns the removed entries so the caller can run any cleanup
    /// (logging, recording a worker-timeout result, etc.).
    pub async fn drain_older_than(&self, max_age: chrono::Duration) -> Vec<ActiveRun> {
        let cutoff = chrono::Utc::now() - max_age;
        let runs = self.list().await;
        let stale: Vec<ActiveRun> = runs.into_iter().filter(|r| r.start_time < cutoff).collect();
        if stale.is_empty() {
            return stale;
        }
        let Some(mut conn) = self.conn().await else {
            return Vec::new();
        };
        for run in &stale {
            let res: redis::RedisResult<u32> = conn.hdel(&self.key, &run.log_id).await;
            if let Err(e) = res {
                log::warn!("active_runs.drain HDEL {}: {}", run.log_id, e);
            }
        }
        stale
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ActiveRun, Backchannel, VcsInfo};
    use chrono::Utc;

    fn make_run(log_id: &str, worker: &str) -> ActiveRun {
        ActiveRun {
            worker_name: worker.to_string(),
            worker_link: None,
            queue_id: 0,
            log_id: log_id.to_string(),
            start_time: Utc::now(),
            finish_time: None,
            estimated_duration: None,
            campaign: "test".to_string(),
            change_set: None,
            command: "echo test".to_string(),
            backchannel: Backchannel::default(),
            vcs_info: VcsInfo::default(),
            codebase: "test-codebase".to_string(),
            instigated_context: None,
            resume_from: None,
        }
    }

    /// Construct a per-test ActiveRunStore against a real Redis. Returns
    /// `None` if Redis isn't reachable so the test no-ops cleanly in
    /// environments without a Redis (CI without the service, fresh dev
    /// machines). `TEST_REDIS_URL` overrides the default
    /// `redis://127.0.0.1:6379`. Each call uses a unique hash key so
    /// parallel tests never collide.
    async fn try_test_store() -> Option<ActiveRunStore> {
        // Spin up an ephemeral Redis via testcontainers if the caller
        // hasn't pointed us at one; without this the test silently
        // skips on any environment that doesn't have a preinstalled
        // Redis (including fresh dev machines and any CI without the
        // service).
        crate::test_utils::ensure_redis().await;
        let url = std::env::var("TEST_REDIS_URL")
            .unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
        let client = redis::Client::open(url).ok()?;
        let mut conn = client.get_multiplexed_async_connection().await.ok()?;
        let _: String = redis::cmd("PING").query_async(&mut conn).await.ok()?;
        let key = format!("runner:active-runs:test:{}", uuid::Uuid::new_v4().simple());
        Some(ActiveRunStore::with_key(client, key))
    }

    /// Drop the test hash key so we don't leak state across runs.
    async fn cleanup(store: &ActiveRunStore) {
        if let Some(mut conn) = store.conn().await {
            let _: redis::RedisResult<u32> = conn.del(&store.key).await;
        }
    }

    #[tokio::test]
    async fn store_and_get_roundtrip() {
        let Some(store) = try_test_store().await else {
            eprintln!("skipping: Redis unavailable");
            return;
        };
        store.store(make_run("run-1", "worker-a")).await;
        let got = store.get("run-1").await.unwrap();
        assert_eq!(got.log_id, "run-1");
        assert_eq!(got.worker_name, "worker-a");
        cleanup(&store).await;
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let Some(store) = try_test_store().await else {
            eprintln!("skipping: Redis unavailable");
            return;
        };
        assert!(store.get("absent").await.is_none());
        cleanup(&store).await;
    }

    #[tokio::test]
    async fn list_sorted_by_start_time() {
        let Some(store) = try_test_store().await else {
            eprintln!("skipping: Redis unavailable");
            return;
        };
        let mut r1 = make_run("a", "w");
        let mut r2 = make_run("b", "w");
        let mut r3 = make_run("c", "w");
        r1.start_time = Utc::now() - chrono::Duration::seconds(30);
        r2.start_time = Utc::now() - chrono::Duration::seconds(10);
        r3.start_time = Utc::now() - chrono::Duration::seconds(20);
        store.store(r2).await;
        store.store(r3).await;
        store.store(r1).await;
        let ids: Vec<String> = store.list().await.into_iter().map(|r| r.log_id).collect();
        assert_eq!(ids, vec!["a".to_string(), "c".to_string(), "b".to_string()]);
        cleanup(&store).await;
    }

    #[tokio::test]
    async fn remove_returns_whether_present() {
        let Some(store) = try_test_store().await else {
            eprintln!("skipping: Redis unavailable");
            return;
        };
        store.store(make_run("run-1", "w")).await;
        assert!(store.remove("run-1").await);
        assert!(!store.remove("run-1").await);
        cleanup(&store).await;
    }

    #[tokio::test]
    async fn count_for_worker_counts_matching_runs() {
        let Some(store) = try_test_store().await else {
            eprintln!("skipping: Redis unavailable");
            return;
        };
        store.store(make_run("run-1", "worker-a")).await;
        store.store(make_run("run-2", "worker-a")).await;
        store.store(make_run("run-3", "worker-b")).await;
        assert_eq!(store.count_for_worker("worker-a").await, 2);
        assert_eq!(store.count_for_worker("worker-b").await, 1);
        assert_eq!(store.count_for_worker("worker-c").await, 0);
        cleanup(&store).await;
    }

    #[tokio::test]
    async fn distinct_worker_count_ignores_duplicates() {
        let Some(store) = try_test_store().await else {
            eprintln!("skipping: Redis unavailable");
            return;
        };
        store.store(make_run("run-1", "worker-a")).await;
        store.store(make_run("run-2", "worker-a")).await;
        store.store(make_run("run-3", "worker-b")).await;
        assert_eq!(store.distinct_worker_count().await, 2);
        cleanup(&store).await;
    }

    #[tokio::test]
    async fn drain_older_than_returns_stale_runs() {
        let Some(store) = try_test_store().await else {
            eprintln!("skipping: Redis unavailable");
            return;
        };
        let mut old = make_run("old", "w");
        old.start_time = Utc::now() - chrono::Duration::hours(10);
        let fresh = make_run("fresh", "w");
        store.store(old).await;
        store.store(fresh).await;
        let drained = store.drain_older_than(chrono::Duration::hours(1)).await;
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].log_id, "old");
        assert!(store.get("fresh").await.is_some());
        assert!(store.get("old").await.is_none());
        cleanup(&store).await;
    }

    /// Exercises the cross-restart story end-to-end: a `store` followed
    /// by *fresh* `ActiveRunStore` instances pointing at the same key
    /// should see the data -- there is no local cache the second store
    /// could be relying on. This is the regression test for salsa #117.
    #[tokio::test]
    async fn survives_dropped_in_memory_state() {
        let Some(store) = try_test_store().await else {
            eprintln!("skipping: Redis unavailable");
            return;
        };
        store.store(make_run("persisted", "worker-a")).await;
        // Build a second store that shares only the redis client + key
        // -- equivalent to what happens when the runner restarts and
        // reconstructs `ActiveRunStore::new(redis)` from scratch.
        let reconstructed = ActiveRunStore::with_key(store.redis.clone(), store.key.clone());
        let got = reconstructed.get("persisted").await.unwrap();
        assert_eq!(got.worker_name, "worker-a");
        cleanup(&store).await;
    }
}
