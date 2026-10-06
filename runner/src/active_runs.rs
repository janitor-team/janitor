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
use chrono::{DateTime, NaiveDateTime, Utc};
use redis::AsyncCommands;
use std::collections::HashMap;

/// The default Redis hash key under which active runs are stored. This
/// is the key the Python runner uses, so the two can take over from each
/// other without losing in-flight runs.
const DEFAULT_KEY: &str = "active-runs";

/// Errors from talking to Redis or decoding the stored runs.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Redis command or connection failure.
    #[error("Redis error: {0}")]
    Redis(#[from] redis::RedisError),

    /// A stored run is not valid JSON, or a run could not be encoded.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// A stored run is missing fields or has fields of the wrong type.
    #[error("Invalid active run: {0}")]
    InvalidRun(Box<dyn std::error::Error + Send + Sync>),
}

/// Shared Redis-backed store of currently-active runs, keyed by `log_id`.
#[derive(Clone)]
pub struct ActiveRunStore {
    redis: redis::Client,
    key: String,
}

impl ActiveRunStore {
    /// Construct a store backed by the given Redis client, using the
    /// default `active-runs` hash key.
    pub fn new(redis: redis::Client) -> Self {
        Self::with_key(redis, DEFAULT_KEY.to_string())
    }

    /// Construct a store with a custom hash key -- useful for tests so
    /// parallel test cases don't collide on a shared key.
    pub fn with_key(redis: redis::Client, key: String) -> Self {
        Self { redis, key }
    }

    async fn conn(&self) -> Result<redis::aio::MultiplexedConnection, Error> {
        Ok(self.redis.get_multiplexed_async_connection().await?)
    }

    /// Record a newly-assigned active run. Overwrites any existing
    /// entry with the same `log_id`.
    pub async fn store(&self, active_run: ActiveRun) -> Result<(), Error> {
        let json = serde_json::to_string(&active_run.to_json())?;
        let mut conn = self.conn().await?;
        let _: () = conn.hset(&self.key, &active_run.log_id, json).await?;
        Ok(())
    }

    /// Fetch a single active run by id.
    pub async fn get(&self, run_id: &str) -> Result<Option<ActiveRun>, Error> {
        let mut conn = self.conn().await?;
        let json: Option<String> = conn.hget(&self.key, run_id).await?;
        json.map(|json| parse_active_run(&json)).transpose()
    }

    /// List all currently-active runs. Sorted by `start_time` ascending
    /// so the ordering matches the Python `status_json` output.
    pub async fn list(&self) -> Result<Vec<ActiveRun>, Error> {
        let mut conn = self.conn().await?;
        let map: HashMap<String, String> = conn.hgetall(&self.key).await?;
        let mut runs = map
            .values()
            .map(|json| parse_active_run(json))
            .collect::<Result<Vec<_>, _>>()?;
        runs.sort_by_key(|r| r.start_time);
        Ok(runs)
    }

    /// Drop an active run from the store. Returns `true` if a row was
    /// actually removed.
    pub async fn remove(&self, run_id: &str) -> Result<bool, Error> {
        let mut conn = self.conn().await?;
        let removed: u32 = conn.hdel(&self.key, run_id).await?;
        Ok(removed > 0)
    }

    /// Count the number of active runs for a given worker. Used by the
    /// concurrency-limit check in `auth::SecurityService::can_start_run`.
    pub async fn count_for_worker(&self, worker_name: &str) -> Result<usize, Error> {
        Ok(self
            .list()
            .await?
            .into_iter()
            .filter(|r| r.worker_name == worker_name)
            .count())
    }

    /// Count the total number of active runs.
    pub async fn len(&self) -> Result<usize, Error> {
        let mut conn = self.conn().await?;
        Ok(conn.hlen(&self.key).await?)
    }

    /// Whether the active-run store has zero entries.
    pub async fn is_empty(&self) -> Result<bool, Error> {
        Ok(self.len().await? == 0)
    }

    /// Count the number of distinct workers with at least one active
    /// run. Used by `SecurityService::get_security_stats`.
    pub async fn distinct_worker_count(&self) -> Result<usize, Error> {
        let runs = self.list().await?;
        let workers: std::collections::HashSet<String> =
            runs.into_iter().map(|run| run.worker_name).collect();
        Ok(workers.len())
    }

    /// Remove runs that have been active for longer than `max_age`.
    /// Returns the removed entries so the caller can run any cleanup
    /// (logging, recording a worker-timeout result, etc.).
    pub async fn drain_older_than(
        &self,
        max_age: chrono::Duration,
    ) -> Result<Vec<ActiveRun>, Error> {
        let cutoff = chrono::Utc::now() - max_age;
        let runs = self.list().await?;
        let stale: Vec<ActiveRun> = runs.into_iter().filter(|r| r.start_time < cutoff).collect();
        if stale.is_empty() {
            return Ok(stale);
        }
        let mut conn = self.conn().await?;
        for run in &stale {
            let _: u32 = conn.hdel(&self.key, &run.log_id).await?;
        }
        Ok(stale)
    }
}

fn parse_active_run(json: &str) -> Result<ActiveRun, Error> {
    ActiveRun::from_json(&serde_json::from_str(json)?).map_err(Error::InvalidRun)
}

/// Format a timestamp the way the Python runner's naive
/// `utcnow().isoformat()` does, so both runners can read each other's
/// state.
pub fn format_python_datetime(when: DateTime<Utc>) -> String {
    let naive = when.naive_utc();
    if naive.and_utc().timestamp_subsec_micros() == 0 {
        naive.format("%Y-%m-%dT%H:%M:%S").to_string()
    } else {
        naive.format("%Y-%m-%dT%H:%M:%S%.6f").to_string()
    }
}

/// Parse a timestamp written by [`format_python_datetime`] or by the
/// Python runner. Naive timestamps are taken to be in UTC.
pub fn parse_python_datetime(s: &str) -> Result<DateTime<Utc>, chrono::ParseError> {
    match NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
        Ok(naive) => Ok(naive.and_utc()),
        Err(_) => Ok(DateTime::parse_from_rfc3339(s)?.with_timezone(&Utc)),
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
        let mut conn = store.conn().await.unwrap();
        let _: u32 = conn.del(&store.key).await.unwrap();
    }

    #[tokio::test]
    async fn store_and_get_roundtrip() {
        let Some(store) = try_test_store().await else {
            eprintln!("skipping: Redis unavailable");
            return;
        };
        store.store(make_run("run-1", "worker-a")).await.unwrap();
        let got = store.get("run-1").await.unwrap().unwrap();
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
        assert!(store.get("absent").await.unwrap().is_none());
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
        store.store(r2).await.unwrap();
        store.store(r3).await.unwrap();
        store.store(r1).await.unwrap();
        let ids: Vec<String> = store
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.log_id)
            .collect();
        assert_eq!(ids, vec!["a".to_string(), "c".to_string(), "b".to_string()]);
        cleanup(&store).await;
    }

    #[tokio::test]
    async fn remove_returns_whether_present() {
        let Some(store) = try_test_store().await else {
            eprintln!("skipping: Redis unavailable");
            return;
        };
        store.store(make_run("run-1", "w")).await.unwrap();
        assert!(store.remove("run-1").await.unwrap());
        assert!(!store.remove("run-1").await.unwrap());
        cleanup(&store).await;
    }

    #[tokio::test]
    async fn count_for_worker_counts_matching_runs() {
        let Some(store) = try_test_store().await else {
            eprintln!("skipping: Redis unavailable");
            return;
        };
        store.store(make_run("run-1", "worker-a")).await.unwrap();
        store.store(make_run("run-2", "worker-a")).await.unwrap();
        store.store(make_run("run-3", "worker-b")).await.unwrap();
        assert_eq!(store.count_for_worker("worker-a").await.unwrap(), 2);
        assert_eq!(store.count_for_worker("worker-b").await.unwrap(), 1);
        assert_eq!(store.count_for_worker("worker-c").await.unwrap(), 0);
        cleanup(&store).await;
    }

    #[tokio::test]
    async fn distinct_worker_count_ignores_duplicates() {
        let Some(store) = try_test_store().await else {
            eprintln!("skipping: Redis unavailable");
            return;
        };
        store.store(make_run("run-1", "worker-a")).await.unwrap();
        store.store(make_run("run-2", "worker-a")).await.unwrap();
        store.store(make_run("run-3", "worker-b")).await.unwrap();
        assert_eq!(store.distinct_worker_count().await.unwrap(), 2);
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
        store.store(old).await.unwrap();
        store.store(fresh).await.unwrap();
        let drained = store
            .drain_older_than(chrono::Duration::hours(1))
            .await
            .unwrap();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].log_id, "old");
        assert!(store.get("fresh").await.unwrap().is_some());
        assert!(store.get("old").await.unwrap().is_none());
        cleanup(&store).await;
    }

    #[test]
    fn default_key_matches_python() {
        let store = ActiveRunStore::new(redis::Client::open("redis://127.0.0.1/").unwrap());
        assert_eq!(store.key, "active-runs");
    }

    /// A run registered by the Python runner can be read back, and runs
    /// we store use the same encoding.
    #[tokio::test]
    async fn python_encoding() {
        let Some(store) = try_test_store().await else {
            eprintln!("skipping: Redis unavailable");
            return;
        };
        let python_json = serde_json::json!({
            "queue_id": 42,
            "id": "run-py",
            "codebase": "cb",
            "change_set": null,
            "campaign": "lintian-fixes",
            "command": "lintian-brush",
            "estimated_duration": 12.5,
            "current_duration": 3.2,
            "start_time": "2026-10-06T12:34:56.123456",
            "worker": "w1",
            "worker_link": null,
            "vcs": {"vcs_type": "git", "branch_url": "https://example.invalid/cb"},
            "backchannel": {"my_url": "http://w1:8080/"},
            "instigated_context": null,
            "resume_from": null,
        });
        let mut conn = store.conn().await.unwrap();
        let _: () = conn
            .hset(&store.key, "run-py", python_json.to_string())
            .await
            .unwrap();

        let run = store.get("run-py").await.unwrap().unwrap();
        assert_eq!(run.queue_id, 42);
        assert_eq!(run.worker_name, "w1");
        assert_eq!(
            run.start_time,
            DateTime::parse_from_rfc3339("2026-10-06T12:34:56.123456Z")
                .unwrap()
                .with_timezone(&Utc)
        );
        assert_eq!(
            run.estimated_duration,
            Some(std::time::Duration::from_millis(12500))
        );
        assert_eq!(
            run.vcs_info.branch_url.as_deref(),
            Some("https://example.invalid/cb")
        );
        assert!(matches!(
            run.backchannel,
            crate::Backchannel::Polling { ref my_url } if my_url == "http://w1:8080/"
        ));

        store.store(run).await.unwrap();
        let stored: String = conn.hget(&store.key, "run-py").await.unwrap();
        let mut stored: serde_json::Value = serde_json::from_str(&stored).unwrap();
        let mut expected = python_json.clone();
        stored.as_object_mut().unwrap().remove("current_duration");
        expected.as_object_mut().unwrap().remove("current_duration");
        expected["vcs"]["subpath"] = serde_json::Value::Null;
        assert_eq!(stored, expected);
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
        store
            .store(make_run("persisted", "worker-a"))
            .await
            .unwrap();
        // Build a second store that shares only the redis client + key
        // -- equivalent to what happens when the runner restarts and
        // reconstructs `ActiveRunStore::new(redis)` from scratch.
        let reconstructed = ActiveRunStore::with_key(store.redis.clone(), store.key.clone());
        let got = reconstructed.get("persisted").await.unwrap().unwrap();
        assert_eq!(got.worker_name, "worker-a");
        cleanup(&store).await;
    }

    #[tokio::test]
    async fn redis_errors_are_returned() {
        // Nothing listens on port 1.
        let store = ActiveRunStore::new(redis::Client::open("redis://127.0.0.1:1/").unwrap());
        assert!(store.get("run-1").await.is_err());
        assert!(store.list().await.is_err());
        assert!(store.len().await.is_err());
        assert!(store.remove("run-1").await.is_err());
        assert!(store.store(make_run("run-1", "w")).await.is_err());
    }

    #[tokio::test]
    async fn invalid_rows_are_errors() {
        let Some(store) = try_test_store().await else {
            eprintln!("skipping: Redis unavailable");
            return;
        };
        let mut conn = store.conn().await.unwrap();
        let _: () = conn.hset(&store.key, "bad", "not json").await.unwrap();
        assert!(store.get("bad").await.is_err());
        assert!(store.list().await.is_err());
        cleanup(&store).await;
    }
}
