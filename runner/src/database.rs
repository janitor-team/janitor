//! Database operations for the runner.

use crate::{BuilderResult, JanitorResult};
use breezyshim::RevisionId;
use chrono::{DateTime, Duration, Utc};
use redis::AsyncCommands;
use sqlx::postgres::types::PgInterval;
use sqlx::{PgPool, Row};
use std::collections::HashMap;

/// Wire-shape of a queue row joined against `codebase`, used by both
/// scoring lookups and single-item fetches. Kept next to those
/// callers so column drift is a single compile-time fix.
///
/// `command`, `refresh`, `estimated_duration`, `success_chance` and
/// `computed_score` are decoded through Option because the queue and
/// candidate tables leave them nullable (see schema/state.sql).
#[derive(sqlx::FromRow)]
struct QueueRowJoined {
    id: i64,
    command: Option<String>,
    context: Option<String>,
    estimated_duration: Option<PgInterval>,
    campaign: String,
    refresh: Option<bool>,
    requester: Option<String>,
    change_set: Option<String>,
    codebase: String,
    vcs_type: Option<String>,
    branch_url: Option<String>,
    subpath: Option<String>,
}

impl QueueRowJoined {
    fn into_queue_item(self) -> QueueItem {
        let estimated_duration = self.estimated_duration.map(|iv| {
            let secs = iv.microseconds.max(0) as u64 / 1_000_000;
            std::time::Duration::from_secs(secs)
        });
        // `queue.context` is TEXT (Python stored plain strings there);
        // QueueItem.context is Option<Value>. Parse as JSON when it
        // looks like it, otherwise wrap so Value is always valid.
        let context = self
            .context
            .map(|s| serde_json::from_str(&s).unwrap_or_else(|_| serde_json::Value::String(s)));
        QueueItem {
            id: self.id,
            context,
            command: self.command.unwrap_or_default(),
            estimated_duration,
            campaign: self.campaign,
            refresh: self.refresh.unwrap_or(false),
            requester: self.requester,
            change_set: self.change_set,
            codebase: self.codebase,
        }
    }

    /// Consume the row into the two halves of a `QueueAssignment`
    /// without cloning the VCS strings.
    fn into_assignment(self) -> QueueAssignment {
        let estimated_duration = self.estimated_duration.map(|iv| {
            let secs = iv.microseconds.max(0) as u64 / 1_000_000;
            std::time::Duration::from_secs(secs)
        });
        // `queue.context` is TEXT (Python stored plain strings there);
        // QueueItem.context is Option<Value>. Parse as JSON when it
        // looks like it, otherwise wrap so Value is always valid.
        let context = self
            .context
            .map(|s| serde_json::from_str(&s).unwrap_or_else(|_| serde_json::Value::String(s)));
        QueueAssignment {
            queue_item: QueueItem {
                id: self.id,
                context,
                command: self.command.unwrap_or_default(),
                estimated_duration,
                campaign: self.campaign,
                refresh: self.refresh.unwrap_or(false),
                requester: self.requester,
                change_set: self.change_set,
                codebase: self.codebase,
            },
            vcs_info: janitor::queue::VcsInfo {
                vcs_type: self.vcs_type,
                branch_url: self.branch_url,
                subpath: self.subpath,
            },
        }
    }
}

// Re-export from main crate
use crate::{QueueAssignment, QueueItem};
pub use janitor::state::{create_pool, Run};

/// Outcome of a `/finish` request from a worker.
///
/// Distinguishes a fresh store from a duplicate retry so the web
/// handler can return 409 Conflict instead of 500 (or a misleading
/// 201). See `RunnerDatabase::finish_run` for the rationale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishOutcome {
    /// First time we've seen this `run_id`; row was inserted.
    Stored,
    /// A row with this `run_id` already exists. The worker is retrying
    /// a finish whose previous response was dropped; it should stop.
    AlreadyStored,
}

/// A codebase entry's VCS URL and the key it came from. Absent, null
/// and empty all mean "no VCS URL". `Err` carries the bad key.
pub(crate) fn effective_branch_url(
    entry: &serde_json::Value,
) -> Result<Option<(&'static str, &str)>, &'static str> {
    for key in ["branch_url", "url"] {
        match entry.get(key) {
            None | Some(serde_json::Value::Null) => continue,
            Some(serde_json::Value::String(s)) if s.is_empty() => continue,
            Some(serde_json::Value::String(s)) => return Ok(Some((key, s))),
            Some(_) => return Err(key),
        }
    }
    Ok(None)
}

/// Database manager for runner operations using shared infrastructure.
#[derive(Clone)]
pub struct RunnerDatabase {
    shared_db: janitor::database::Database,
}

impl RunnerDatabase {
    /// Create a new database manager from shared database.
    pub fn new(pool: PgPool) -> Self {
        Self {
            shared_db: janitor::database::Database::from_pool(pool),
        }
    }

    /// Create from a janitor Database instance.
    pub fn from_database(database: janitor::database::Database) -> Self {
        Self {
            shared_db: database,
        }
    }

    /// Create a new database manager with Redis support.
    pub fn new_with_redis(pool: PgPool, redis: redis::Client) -> Self {
        Self {
            shared_db: janitor::database::Database::from_pool_with_redis(pool, redis),
        }
    }

    /// Create a new database instance with optional Redis connection from URL.
    pub async fn new_with_redis_url(
        pool: PgPool,
        redis_url: Option<String>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let shared_db = if let Some(url) = redis_url {
            // Use shared RedisManager instead of direct Redis client
            let redis_config = janitor::redis::RedisConfig::new(url);
            let redis_manager = janitor::redis::RedisManager::new(redis_config)?;
            let redis_client = redis_manager.client().as_ref().clone();
            janitor::database::Database::from_pool_with_redis(pool, redis_client)
        } else {
            janitor::database::Database::from_pool(pool)
        };

        Ok(Self { shared_db })
    }

    /// Perform a health check on the database connection.
    pub async fn health_check(&self) -> Result<(), sqlx::Error> {
        self.shared_db.test_connection().await.map_err(|e| match e {
            janitor::database::DatabaseError::Connection(e) => e,
            _ => sqlx::Error::Configuration("Database configuration error".into()),
        })
    }

    /// Get a reference to the database pool.
    pub fn pool(&self) -> &PgPool {
        self.shared_db.pool()
    }

    /// Get a reference to the Redis client if available.
    pub fn redis(&self) -> Option<&redis::Client> {
        self.shared_db.redis()
    }

    /// Convert a database Run to JanitorResult.
    pub fn run_to_janitor_result(&self, run: Run) -> JanitorResult {
        JanitorResult {
            log_id: run.id,
            branch_url: run.branch_url,
            subpath: None, // Not stored in run table currently
            code: run.result_code,
            transient: run.failure_transient,
            codebase: run.codebase,
            campaign: run.suite,
            description: run.description,
            codemod: run.result,
            value: run.value.map(|v| v as u64),
            logfilenames: run.logfilenames.unwrap_or_default(),
            start_time: run.start_time,
            finish_time: run.finish_time,
            revision: run.revision,
            main_branch_revision: run.main_branch_revision,
            change_set: Some(run.change_set),
            tags: run.result_tags.map(|tags| {
                tags.into_iter()
                    .map(|(name, rev)| (name, Some(RevisionId::from(rev.as_bytes()))))
                    .collect()
            }),
            remotes: None, // Use run_to_janitor_result_async() to fetch remotes
            branches: run.result_branches.map(|branches| {
                branches
                    .into_iter()
                    .map(|(fn_name, name, br, r)| (Some(fn_name), Some(name), br, r))
                    .collect()
            }),
            failure_details: run.failure_details,
            failure_stage: run.failure_stage.map(|s| vec![s]),
            resume: None, // Use run_to_janitor_result_async() to fetch resume info
            target: None, // Use run_to_janitor_result_async() to fetch target
            worker_name: run.worker_name,
            vcs_type: Some(run.vcs_type),
            target_branch_url: run.target_branch_url,
            context: run.context.and_then(|s| serde_json::from_str(&s).ok()),
            builder_result: None, // Use run_to_janitor_result_async() to fetch builder result
        }
    }

    /// Convert a Run to JanitorResult with async database calls for related data.
    pub async fn run_to_janitor_result_async(
        &self,
        run: Run,
    ) -> Result<JanitorResult, sqlx::Error> {
        let remotes_data = self.get_run_remotes(&run.id).await?;
        let resume = self
            .get_resume_info(&run.id, &run.codebase, &run.suite)
            .await?;
        let target_data = self.get_run_target(&run.id).await?;
        let builder_result_data = self.get_run_builder_result(&run.id).await?;

        // Convert JSON data to proper structs
        let remotes = remotes_data.map(|data| {
            data.into_iter()
                .map(|(name, remote_info)| {
                    let url = remote_info
                        .get("url")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    (name, crate::ResultRemote { url })
                })
                .collect()
        });

        let target = target_data.and_then(|data| {
            let name = data.get("name")?.as_str()?.to_string();
            let details = data.get("details")?.clone();
            Some(crate::ResultTarget { name, details })
        });

        let builder_result = builder_result_data.and_then(|data| serde_json::from_value(data).ok());

        Ok(JanitorResult {
            log_id: run.id,
            branch_url: run.branch_url,
            subpath: None, // Not stored in run table currently
            code: run.result_code,
            transient: run.failure_transient,
            codebase: run.codebase,
            campaign: run.suite,
            description: run.description,
            codemod: run.result,
            value: run.value.map(|v| v as u64),
            logfilenames: run.logfilenames.unwrap_or_default(),
            start_time: run.start_time,
            finish_time: run.finish_time,
            revision: run.revision,
            main_branch_revision: run.main_branch_revision,
            change_set: Some(run.change_set),
            tags: run.result_tags.map(|tags| {
                tags.into_iter()
                    .map(|(name, rev)| (name, Some(RevisionId::from(rev.as_bytes()))))
                    .collect()
            }),
            remotes,
            branches: run.result_branches.map(|branches| {
                branches
                    .into_iter()
                    .map(|(fn_name, name, br, r)| (Some(fn_name), Some(name), br, r))
                    .collect()
            }),
            failure_details: run.failure_details,
            failure_stage: run.failure_stage.map(|s| vec![s]),
            resume,
            target,
            worker_name: run.worker_name,
            vcs_type: Some(run.vcs_type),
            target_branch_url: run.target_branch_url,
            context: run.context.and_then(|s| serde_json::from_str(&s).ok()),
            builder_result,
        })
    }

    /// Get a run by ID.
    pub async fn get_run(&self, run_id: &str) -> Result<Option<JanitorResult>, sqlx::Error> {
        // run.vcs_type and run.suite are postgres enums; cast both to
        // text so sqlx decodes them as Strings for the Run struct.
        let run: Option<Run> = sqlx::query_as(
            r#"
            SELECT id, command, description, result_code, main_branch_revision, revision,
                   context, result, suite::text AS suite, instigated_context, vcs_type::text AS vcs_type, branch_url,
                   logfilenames, worker_name, result_branches, result_tags, target_branch_url,
                   change_set, failure_details, failure_transient, failure_stage, codebase,
                   start_time, finish_time, value
            FROM run WHERE id = $1
            "#,
        )
        .bind(run_id)
        .fetch_optional(self.pool())
        .await?;

        match run {
            Some(r) => {
                let result = self.run_to_janitor_result_async(r).await?;
                Ok(Some(result))
            }
            None => Ok(None),
        }
    }

    // NOTE: store_active_run / get_active_run / get_active_runs /
    // remove_active_run / cleanup_stale_runs used to live here and
    // queried a Postgres `active_runs` table that is never created in
    // any schema, any migration, or any DDL in the repo. They have
    // been replaced by the in-memory [`crate::active_runs::ActiveRunStore`]
    // on `AppState`, matching Python's
    // `QueueProcessor.active_runs` dict.

    /// Get basic queue statistics.
    pub async fn get_queue_stats(&self) -> Result<HashMap<String, i64>, sqlx::Error> {
        // "active" counts live in Redis via `ActiveRunStore`, not
        // Postgres -- so the SQL side only reports queue depth and
        // historical run outcomes. Callers that need active-run
        // counts should read them from `AppState.active_runs`.
        #[derive(sqlx::FromRow)]
        struct StatRow {
            key: String,
            value: i64,
        }
        let rows: Vec<StatRow> = sqlx::query_as(
            r#"
            SELECT 'total' as key, COUNT(*)::bigint as value FROM queue
            UNION ALL
            SELECT 'succeeded' as key, COUNT(*)::bigint as value FROM run WHERE result_code = 'success'
            UNION ALL
            SELECT 'failed' as key, COUNT(*)::bigint as value FROM run WHERE result_code != 'success'
            "#,
        )
        .fetch_all(self.pool())
        .await?;

        Ok(rows.into_iter().map(|r| (r.key, r.value)).collect())
    }

    /// Check if a run exists.
    pub async fn run_exists(&self, run_id: &str) -> Result<bool, sqlx::Error> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM run WHERE id = $1")
            .bind(run_id)
            .fetch_one(self.pool())
            .await?;

        Ok(count > 0)
    }

    /// Update a run result.
    pub async fn update_run_result(
        &self,
        run_id: &str,
        result_code: &str,
        description: Option<&str>,
        failure_details: Option<&serde_json::Value>,
        failure_transient: Option<bool>,
        finish_time: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE run SET
                result_code = $2,
                description = $3,
                failure_details = $4,
                failure_transient = $5,
                finish_time = $6
            WHERE id = $1
            "#,
        )
        .bind(run_id)
        .bind(result_code)
        .bind(description)
        .bind(failure_details)
        .bind(failure_transient)
        .bind(finish_time)
        .execute(self.pool())
        .await?;

        Ok(())
    }

    /// Insert a change_set row, ignoring conflicts.
    pub async fn store_change_set(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        name: &str,
        campaign: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO change_set (id, campaign) VALUES ($1, $2) ON CONFLICT DO NOTHING")
            .bind(name)
            .bind(campaign)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    /// Insert a completed run into the `run` table along with its
    /// per-role `new_result_branch` rows. The caller is responsible for
    /// providing a transaction; `finish_run` wraps this,
    /// `store_change_set`, and the `queue` deletion in a single
    /// transaction so that either all of them happen or none do.
    pub async fn store_run(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        result: &JanitorResult,
        command: &str,
        instigated_context: Option<&serde_json::Value>,
    ) -> Result<(), sqlx::Error> {
        // result_tags is a result_tag[] (composite of (actual_name, revision)).
        // Reconstruct it server-side from two parallel text[] binds via
        // unnest, which avoids having to encode composite arrays from Rust.
        let (tag_names, tag_revisions): (Vec<String>, Vec<String>) =
            if let Some(tags) = &result.tags {
                tags.iter()
                    .map(|(n, r)| {
                        (
                            n.clone(),
                            r.as_ref().map(|r| r.to_string()).unwrap_or_default(),
                        )
                    })
                    .unzip()
            } else {
                (Vec::new(), Vec::new())
            };

        let logfilenames: Vec<String> = result.logfilenames.clone();
        let result_tags_present = !tag_names.is_empty();

        sqlx::query(
            r#"
-- run.vcs_type is a postgres enum; cast from bound text so postgres
-- doesn't refuse with "column vcs_type is of type vcs_type but
-- expression is of type text".
INSERT INTO run (
    id, command, description, result_code,
    start_time, finish_time, instigated_context, context,
    main_branch_revision, revision, result, suite,
    vcs_type, branch_url, subpath, logfilenames,
    value, worker,
    result_tags,
    resume_from, failure_details, failure_stage,
    target_branch_url, change_set, failure_transient, codebase
) VALUES (
    $1, $2, $3, $4,
    $5, $6, $7, $8,
    $9, $10, $11, $12,
    $13::text::vcs_type, $14, $15, $16,
    $17, $18,
    CASE WHEN $21::boolean THEN (
        SELECT array_agg(ROW(n, r)::result_tag)
        FROM unnest($19::text[], $20::text[]) AS t(n, r)
    ) ELSE NULL END,
    $22, $23, $24,
    $25, $26, $27, $28
)
"#,
        )
        .bind(&result.log_id)
        .bind(command)
        .bind(result.description.as_deref())
        .bind(&result.code)
        .bind(result.start_time)
        .bind(result.finish_time)
        .bind(instigated_context)
        .bind(result.context.as_ref())
        .bind(result.main_branch_revision.as_ref().map(|r| r.to_string()))
        .bind(result.revision.as_ref().map(|r| r.to_string()))
        .bind(result.codemod.as_ref())
        .bind(&result.campaign)
        .bind(result.vcs_type.as_deref())
        .bind(result.branch_url.as_str())
        .bind(result.subpath.as_deref())
        .bind(&logfilenames)
        .bind(result.value.map(|v| v as i32))
        .bind(result.worker_name.as_deref())
        .bind(&tag_names)
        .bind(&tag_revisions)
        .bind(result_tags_present)
        .bind(result.resume.as_ref().map(|r| r.run_id.as_str()))
        .bind(result.failure_details.as_ref())
        .bind(result.failure_stage.as_ref())
        .bind(result.target_branch_url.as_deref())
        .bind(result.change_set.as_deref())
        .bind(result.transient)
        .bind(&result.codebase)
        .execute(&mut **tx)
        .await?;

        if let Some(branches) = &result.branches {
            // Sanity check: roles must be unique within a single run.
            let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
            for (role, _, _, _) in branches {
                if let Some(r) = role.as_deref() {
                    if !seen.insert(r) {
                        log::error!(
                            "Duplicate result branch role {} for run {}",
                            r,
                            result.log_id
                        );
                    }
                }
            }
            for (role, remote_name, base_revision, revision) in branches {
                sqlx::query(
                    r#"INSERT INTO new_result_branch
                        (run_id, role, remote_name, base_revision, revision)
                       VALUES ($1, $2, $3, $4, $5)"#,
                )
                .bind(&result.log_id)
                .bind(role.as_deref())
                .bind(remote_name.as_deref())
                .bind(base_revision.as_ref().map(|r| r.to_string()))
                .bind(revision.as_ref().map(|r| r.to_string()))
                .execute(&mut **tx)
                .await?;
            }
        }

        Ok(())
    }

    /// Persist a completed run atomically: create a change_set row if
    /// the run doesn't already belong to one, insert the `run` row and
    /// any per-role `new_result_branch` rows, insert the `debian_build`
    /// row when present, and delete the corresponding queue entry --
    /// all in a single transaction.
    ///
    /// Returns `FinishOutcome::AlreadyStored` if the run row already
    /// exists; the web handler turns that into a 409 Conflict so the
    /// worker can abort its retry loop. Without this guard the second
    /// `INSERT INTO run` would hit the primary-key constraint and the
    /// handler would surface 500, which a worker can't distinguish
    /// from a transient server error -> infinite retry.
    pub async fn finish_run(
        &self,
        result: &mut JanitorResult,
        command: &str,
        instigated_context: Option<&serde_json::Value>,
        queue_id: i64,
    ) -> Result<FinishOutcome, sqlx::Error> {
        // Idempotency guard. Cheap (PK lookup); we surface
        // AlreadyStored to the caller rather than silently no-op'ing
        // so the client sees the truth (409, not 201) and the
        // operator can spot retry storms in metrics.
        let already: Option<String> = sqlx::query_scalar("SELECT id FROM run WHERE id = $1")
            .bind(&result.log_id)
            .fetch_optional(self.pool())
            .await?;
        if already.is_some() {
            // Best-effort queue cleanup so the queued item doesn't sit
            // around when the worker re-finishes after a transient
            // network hiccup. Harmless if the previous attempt already
            // deleted it.
            let _ = sqlx::query("DELETE FROM queue WHERE id = $1")
                .bind(queue_id)
                .execute(self.pool())
                .await;
            log::info!(
                "finish_run: run {} already stored; signalling AlreadyStored",
                result.log_id
            );
            return Ok(FinishOutcome::AlreadyStored);
        }

        let mut tx = self.pool().begin().await?;

        if result.change_set.is_none() || result.change_set.as_deref() == Some("") {
            result.change_set = Some(result.log_id.clone());
            self.store_change_set(&mut tx, &result.log_id, &result.campaign)
                .await?;
        }

        self.store_run(&mut tx, result, command, instigated_context)
            .await?;

        if let Some(builder_result) = &result.builder_result {
            self.store_builder_result_tx(&mut tx, &result.log_id, builder_result)
                .await?;
        }

        sqlx::query("DELETE FROM queue WHERE id = $1")
            .bind(queue_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(FinishOutcome::Stored)
    }

    /// Store builder result data.
    pub async fn store_builder_result(
        &self,
        run_id: &str,
        builder_result: &BuilderResult,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool().begin().await?;
        self.store_builder_result_tx(&mut tx, run_id, builder_result)
            .await?;
        tx.commit().await
    }

    /// Transaction-aware counterpart of [`store_builder_result`], so
    /// callers that already hold a transaction (e.g. the finish-run
    /// path) can include the debian_build insert in the same atomic
    /// unit as the `run` insert.
    pub async fn store_builder_result_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        run_id: &str,
        builder_result: &BuilderResult,
    ) -> Result<(), sqlx::Error> {
        match builder_result {
            BuilderResult::Generic => {
                // No additional data to store for generic builds
            }
            BuilderResult::Debian {
                source,
                build_version,
                build_distribution,
                changes_filenames: _,
                lintian_result,
                binary_packages,
            } => {
                // Skip the insert when no .changes was produced. The
                // schema marks version/source/distribution NOT NULL,
                // so a partial result (e.g. codemod-only run with just
                // a lintian blob) would otherwise trip the constraint
                // and roll the entire /finish transaction back.
                let (Some(source), Some(build_version), Some(build_distribution)) =
                    (source, build_version, build_distribution)
                else {
                    return Ok(());
                };
                // No ON CONFLICT: schema/debian/debian.sql has only a
                // (non-unique) index on run_id, so ON CONFLICT (run_id)
                // raises "no unique or exclusion constraint matching".
                // The /finish handler runs this in the same transaction
                // as the run insert, so a failed retry rolls everything
                // back and a fresh attempt does a clean insert.
                sqlx::query(
                    r#"
                    INSERT INTO debian_build (
                        run_id, source, version, distribution, lintian_result, binary_packages
                    ) VALUES ($1, $2, $3, $4, $5, $6)
                    "#,
                )
                .bind(run_id)
                .bind(source)
                .bind(build_version)
                .bind(build_distribution)
                .bind(lintian_result)
                .bind(binary_packages)
                .execute(&mut **tx)
                .await?;
            }
        }
        Ok(())
    }

    /// Get the next available queue item for assignment.
    pub async fn next_queue_item(
        &self,
        codebase: Option<&str>,
        campaign: Option<&str>,
        exclude_hosts: &[String],
        assigned_queue_items: &[i64],
    ) -> Result<Option<QueueAssignment>, sqlx::Error> {
        let mut query = r#"
            SELECT
                queue.command,
                queue.context,
                queue.id::bigint AS id,
                queue.estimated_duration,
                queue.suite::text AS campaign,
                queue.refresh,
                queue.requester,
                queue.change_set,
                codebase.vcs_type::text AS vcs_type,
                codebase.branch_url,
                codebase.subpath,
                queue.codebase
            FROM
                queue
            LEFT JOIN codebase ON codebase.name = queue.codebase
        "#
        .to_string();

        let mut conditions = Vec::new();
        let mut bind_count = 0;

        // Exclude already assigned queue items
        if !assigned_queue_items.is_empty() {
            bind_count += 1;
            conditions.push(format!("NOT (queue.id = ANY(${}::int[]))", bind_count));
        }

        // Filter by codebase if specified
        if codebase.is_some() {
            bind_count += 1;
            conditions.push(format!("queue.codebase = ${}", bind_count));
        }

        // Filter by campaign if specified
        if campaign.is_some() {
            bind_count += 1;
            conditions.push(format!("queue.suite = ${}", bind_count));
        }

        // Exclude hosts
        if !exclude_hosts.is_empty() {
            bind_count += 1;
            conditions.push(format!(
                "NOT (codebase.branch_url IS NOT NULL AND SUBSTRING(codebase.branch_url from '.*://(?:[^/@]*@)?([^/]*)') = ANY(${}::text[]))",
                bind_count
            ));
        }

        if !conditions.is_empty() {
            query.push_str(" WHERE ");
            query.push_str(&conditions.join(" AND "));
        }

        query.push_str(
            r#"
            ORDER BY
            queue.bucket ASC,
            queue.priority ASC,
            queue.id ASC
            LIMIT 1
        "#,
        );

        let mut sqlx_query = sqlx::query_as::<_, QueueRowJoined>(sqlx::AssertSqlSafe(&*query));

        if !assigned_queue_items.is_empty() {
            sqlx_query = sqlx_query.bind(assigned_queue_items);
        }
        if let Some(cb) = codebase {
            sqlx_query = sqlx_query.bind(cb);
        }
        if let Some(camp) = campaign {
            sqlx_query = sqlx_query.bind(camp);
        }
        if !exclude_hosts.is_empty() {
            sqlx_query = sqlx_query.bind(exclude_hosts);
        }

        Ok(sqlx_query
            .fetch_optional(self.pool())
            .await?
            .map(QueueRowJoined::into_assignment))
    }

    /// Get a queue item by ID.
    pub async fn get_queue_item(&self, queue_id: i64) -> Result<Option<QueueItem>, sqlx::Error> {
        // Join through codebase to satisfy `QueueRowJoined`'s
        // `vcs_type` / `branch_url` / `subpath` fields; the caller
        // discards them via `into_queue_item()`.
        let row: Option<QueueRowJoined> = sqlx::query_as(
            r#"
            SELECT
                queue.id::bigint AS id,
                queue.command,
                queue.context,
                queue.estimated_duration,
                queue.suite::text AS campaign,
                queue.refresh,
                queue.requester,
                queue.change_set,
                queue.codebase,
                codebase.vcs_type::text AS vcs_type,
                codebase.branch_url,
                codebase.subpath
            FROM queue
            LEFT JOIN codebase ON codebase.name = queue.codebase
            WHERE queue.id = $1
            "#,
        )
        .bind(queue_id)
        .fetch_optional(self.pool())
        .await?;

        Ok(row.map(|r| r.into_queue_item()))
    }

    /// Get queue position for a specific codebase and campaign.
    pub async fn get_queue_position(
        &self,
        codebase: &str,
        campaign: &str,
    ) -> Result<Option<(i32, std::time::Duration)>, sqlx::Error> {
        // `queue_positions.wait_time` is a Postgres INTERVAL (not
        // INT8) and `position` is BIGINT from `row_number()`. Decode
        // both via a typed row.
        #[derive(sqlx::FromRow)]
        struct PositionRow {
            position: i64,
            wait_time: sqlx::postgres::types::PgInterval,
        }
        let row: Option<PositionRow> = sqlx::query_as(
            "SELECT position, wait_time FROM queue_positions WHERE codebase = $1 AND suite = $2",
        )
        .bind(codebase)
        .bind(campaign)
        .fetch_optional(self.pool())
        .await?;

        Ok(row.map(|r| {
            let secs = r.wait_time.microseconds.max(0) as u64 / 1_000_000;
            (r.position as i32, std::time::Duration::from_secs(secs))
        }))
    }

    /// Update run publish status.
    pub async fn update_run_publish_status(
        &self,
        run_id: &str,
        publish_status: &str,
    ) -> Result<Option<(String, String, String)>, sqlx::Error> {
        // `publish_status` is a Postgres ENUM and `suite` is a custom
        // domain -- cast both explicitly so sqlx's text-based encode
        // and decode paths succeed.
        #[derive(sqlx::FromRow)]
        struct UpdatedRow {
            id: String,
            codebase: String,
            suite: String,
        }
        let row: Option<UpdatedRow> = sqlx::query_as(
            "UPDATE run SET publish_status = $2::publish_status \
             WHERE id = $1 \
             RETURNING id, codebase, suite::text AS suite",
        )
        .bind(run_id)
        .bind(publish_status)
        .fetch_optional(self.pool())
        .await?;

        Ok(row.map(|r| (r.id, r.codebase, r.suite)))
    }

    /// Add a host to the rate limit list.
    pub async fn rate_limit_host(
        &self,
        host: &str,
        retry_after: DateTime<Utc>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(redis_client) = self.redis() {
            let mut conn = redis_client.get_multiplexed_async_connection().await?;
            let _: () = conn
                .hset("rate-limit-hosts", host, retry_after.to_rfc3339())
                .await?;
        }
        Ok(())
    }

    /// Get all rate limited hosts that are still active.
    pub async fn get_rate_limited_hosts(
        &self,
    ) -> Result<HashMap<String, DateTime<Utc>>, Box<dyn std::error::Error + Send + Sync>> {
        let mut result = HashMap::new();

        if let Some(redis_client) = self.redis() {
            let mut conn = redis_client.get_multiplexed_async_connection().await?;
            let hosts: HashMap<String, String> = conn.hgetall("rate-limit-hosts").await?;

            let now = Utc::now();
            for (host, time_str) in hosts {
                if let Ok(retry_time) = DateTime::parse_from_rfc3339(&time_str) {
                    let retry_time = retry_time.with_timezone(&Utc);
                    if retry_time > now {
                        result.insert(host, retry_time);
                    }
                }
            }
        }

        Ok(result)
    }

    /// Get assigned queue items from Redis.
    pub async fn get_assigned_queue_items(
        &self,
    ) -> Result<Vec<i64>, Box<dyn std::error::Error + Send + Sync>> {
        let mut result = Vec::new();

        if let Some(redis_client) = self.redis() {
            let mut conn = redis_client.get_multiplexed_async_connection().await?;
            let items: Vec<String> = conn.hkeys("assigned-queue-items").await?;

            for item in items {
                if let Ok(id) = item.parse::<i64>() {
                    result.push(id);
                }
            }
        }

        Ok(result)
    }

    /// Assign a queue item to a worker in Redis.
    pub async fn assign_queue_item(
        &self,
        queue_id: i64,
        worker_name: &str,
        log_id: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(redis_client) = self.redis() {
            let mut conn = redis_client.get_multiplexed_async_connection().await?;

            // Store assignment with worker info and timestamp
            let assignment_info = serde_json::json!({
                "worker_name": worker_name,
                "log_id": log_id,
                "assigned_at": Utc::now().to_rfc3339(),
            });

            // Atomically claim the queue item. HSETNX only writes the field
            // when it does not already exist, so two concurrent assigns for
            // the same queue_id can never both win. The previous
            // hget-then-hset check was racy: both callers could observe the
            // slot unclaimed and both proceed to hset.
            let claimed: bool = conn
                .hset_nx(
                    "assigned-queue-items",
                    queue_id.to_string(),
                    assignment_info.to_string(),
                )
                .await?;
            if !claimed {
                return Err(format!("Queue item {} already assigned", queue_id).into());
            }
            let _: () = conn
                .sadd(format!("worker-queue-items:{}", worker_name), queue_id)
                .await?;

            // Do not EXPIRE the `assigned-queue-items` hash. The
            // previous 3600s TTL nuked the whole hash every hour --
            // which silently freed long-running runs' claims and
            // undermined the double-assignment guard around them.
            // Python never sets a TTL; stale entries are removed
            // explicitly via `finish_run` / `unassign_queue_item`.
        }

        Ok(())
    }

    /// Get detailed assigned queue items from Redis with worker info.
    pub async fn get_assigned_queue_items_detailed(
        &self,
    ) -> Result<Vec<(i64, String, String, String)>, Box<dyn std::error::Error + Send + Sync>> {
        let mut result = Vec::new();

        if let Some(redis_client) = self.redis() {
            let mut conn = redis_client.get_multiplexed_async_connection().await?;
            let assignments: HashMap<String, String> = conn.hgetall("assigned-queue-items").await?;

            for (queue_id_str, assignment_info_str) in assignments {
                if let (Ok(queue_id), Ok(assignment_info)) = (
                    queue_id_str.parse::<i64>(),
                    serde_json::from_str::<serde_json::Value>(&assignment_info_str),
                ) {
                    if let (Some(worker_name), Some(log_id), Some(assigned_at)) = (
                        assignment_info.get("worker_name").and_then(|v| v.as_str()),
                        assignment_info.get("log_id").and_then(|v| v.as_str()),
                        assignment_info.get("assigned_at").and_then(|v| v.as_str()),
                    ) {
                        result.push((
                            queue_id,
                            worker_name.to_string(),
                            log_id.to_string(),
                            assigned_at.to_string(),
                        ));
                    }
                }
            }
        }

        Ok(result)
    }

    /// Get queue items assigned to a specific worker.
    pub async fn get_worker_queue_items(
        &self,
        worker_name: &str,
    ) -> Result<Vec<i64>, Box<dyn std::error::Error + Send + Sync>> {
        let mut result = Vec::new();

        if let Some(redis_client) = self.redis() {
            let mut conn = redis_client.get_multiplexed_async_connection().await?;
            let queue_ids: Vec<String> = conn
                .smembers(format!("worker-queue-items:{}", worker_name))
                .await?;

            for queue_id_str in queue_ids {
                if let Ok(queue_id) = queue_id_str.parse::<i64>() {
                    result.push(queue_id);
                }
            }
        }

        Ok(result)
    }

    /// Check if a queue item is currently assigned.
    pub async fn is_queue_item_assigned(
        &self,
        queue_id: i64,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(redis_client) = self.redis() {
            let mut conn = redis_client.get_multiplexed_async_connection().await?;
            let exists: bool = conn
                .hexists("assigned-queue-items", queue_id.to_string())
                .await?;
            Ok(exists)
        } else {
            Ok(false)
        }
    }

    /// Get the worker assigned to a queue item.
    pub async fn get_queue_item_assignment(
        &self,
        queue_id: i64,
    ) -> Result<Option<(String, String)>, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(redis_client) = self.redis() {
            let mut conn = redis_client.get_multiplexed_async_connection().await?;

            if let Ok(assignment_info_str) = conn
                .hget::<&str, String, String>("assigned-queue-items", queue_id.to_string())
                .await
            {
                if let Ok(assignment_info) =
                    serde_json::from_str::<serde_json::Value>(&assignment_info_str)
                {
                    if let (Some(worker_name), Some(log_id)) = (
                        assignment_info.get("worker_name").and_then(|v| v.as_str()),
                        assignment_info.get("log_id").and_then(|v| v.as_str()),
                    ) {
                        return Ok(Some((worker_name.to_string(), log_id.to_string())));
                    }
                }
            }
        }

        Ok(None)
    }

    /// Publish a JSON payload to a Redis pub/sub channel. A runner
    /// without Redis configured is a no-op rather than an error,
    /// matching the existing pattern in [`Self::unassign_queue_item`].
    pub async fn publish(
        &self,
        channel: &str,
        payload: &serde_json::Value,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(redis_client) = self.redis() {
            let mut conn = redis_client.get_multiplexed_async_connection().await?;
            let body = serde_json::to_string(payload)?;
            let _: () = conn.publish(channel, body).await?;
        }
        Ok(())
    }

    /// Remove queue item assignment from Redis.
    pub async fn unassign_queue_item(
        &self,
        queue_id: i64,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(redis_client) = self.redis() {
            let mut conn = redis_client.get_multiplexed_async_connection().await?;

            if let Ok(assignment_info_str) = conn
                .hget::<&str, String, String>("assigned-queue-items", queue_id.to_string())
                .await
            {
                if let Ok(assignment_info) =
                    serde_json::from_str::<serde_json::Value>(&assignment_info_str)
                {
                    if let Some(worker_name) =
                        assignment_info.get("worker_name").and_then(|v| v.as_str())
                    {
                        // Remove from worker's set
                        let _: () = conn
                            .srem(format!("worker-queue-items:{}", worker_name), queue_id)
                            .await?;
                    }
                }
            }

            // Remove from assignments hash
            let _: () = conn
                .hdel("assigned-queue-items", queue_id.to_string())
                .await?;
        }

        Ok(())
    }

    /// Coordinate worker health status via Redis.
    pub async fn update_worker_health(
        &self,
        worker_name: &str,
        status: &str,
        current_run: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(redis_client) = self.redis() {
            let mut conn = redis_client.get_multiplexed_async_connection().await?;

            let health_info = serde_json::json!({
                "status": status,
                "current_run": current_run,
                "last_heartbeat": Utc::now().to_rfc3339(),
            });

            let _: () = conn
                .hset("worker-health", worker_name, health_info.to_string())
                .await?;

            // Set expiration for worker health (cleanup stale workers)
            let _: () = conn.expire("worker-health", 1800).await?; // 30 minutes
        }

        Ok(())
    }

    /// Get worker health status from Redis.
    pub async fn get_worker_health(
        &self,
        worker_name: &str,
    ) -> Result<Option<serde_json::Value>, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(redis_client) = self.redis() {
            let mut conn = redis_client.get_multiplexed_async_connection().await?;

            match conn
                .hget::<&str, &str, String>("worker-health", worker_name)
                .await
            {
                Ok(health_str) => {
                    let health_info: serde_json::Value = serde_json::from_str(&health_str)?;
                    return Ok(Some(health_info));
                }
                Err(_) => {
                    // Worker health not found
                }
            }
        }

        Ok(None)
    }

    /// Get all worker health statuses.
    pub async fn get_all_worker_health(
        &self,
    ) -> Result<HashMap<String, serde_json::Value>, Box<dyn std::error::Error + Send + Sync>> {
        let mut result = HashMap::new();

        if let Some(redis_client) = self.redis() {
            let mut conn = redis_client.get_multiplexed_async_connection().await?;
            let workers: HashMap<String, String> = conn.hgetall("worker-health").await?;

            for (worker_name, health_str) in workers {
                if let Ok(health_info) = serde_json::from_str::<serde_json::Value>(&health_str) {
                    result.insert(worker_name, health_info);
                }
            }
        }

        Ok(result)
    }

    /// Store coordination lock in Redis.
    pub async fn acquire_lock(
        &self,
        lock_name: &str,
        holder: &str,
        ttl_seconds: u64,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(redis_client) = self.redis() {
            let mut conn = redis_client.get_multiplexed_async_connection().await?;

            // Use SET with NX (only if not exists) and EX (expiration)
            let result: Option<String> = conn
                .set_options(
                    format!("lock:{}", lock_name),
                    holder,
                    redis::SetOptions::default()
                        .conditional_set(redis::ExistenceCheck::NX)
                        .get(true)
                        .with_expiration(redis::SetExpiry::EX(ttl_seconds)),
                )
                .await?;

            Ok(result.is_some())
        } else {
            // No Redis, assume lock acquired
            Ok(true)
        }
    }

    /// Release coordination lock in Redis.
    pub async fn release_lock(
        &self,
        lock_name: &str,
        holder: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(redis_client) = self.redis() {
            let mut conn = redis_client.get_multiplexed_async_connection().await?;

            // Lua script to atomically check holder and delete
            let script = r#"
                if redis.call("GET", KEYS[1]) == ARGV[1] then
                    return redis.call("DEL", KEYS[1])
                else
                    return 0
                end
            "#;

            let result: i32 = redis::Script::new(script)
                .key(format!("lock:{}", lock_name))
                .arg(holder)
                .invoke_async(&mut conn)
                .await?;

            Ok(result == 1)
        } else {
            // No Redis, assume lock released
            Ok(true)
        }
    }

    /// Assign the next queue item, filtering out rate-limited hosts
    /// and queue items already claimed by another worker.
    pub async fn next_queue_item_with_rate_limiting(
        &self,
        codebase: Option<&str>,
        campaign: Option<&str>,
        avoid_hosts: &[String],
    ) -> Result<Option<QueueAssignment>, sqlx::Error> {
        let rate_limited_hosts = self.get_rate_limited_hosts().await.unwrap_or_default();
        let mut exclude_hosts = avoid_hosts.to_vec();
        exclude_hosts.extend(rate_limited_hosts.keys().cloned());

        let assigned_items = self.get_assigned_queue_items().await.unwrap_or_default();

        self.next_queue_item_with_scoring(codebase, campaign, &exclude_hosts, &assigned_items)
            .await
    }

    /// Assign the next queue item using a computed score (candidate
    /// value + historical success rate) as the primary sort key.
    pub async fn next_queue_item_with_scoring(
        &self,
        codebase: Option<&str>,
        campaign: Option<&str>,
        exclude_hosts: &[String],
        assigned_queue_items: &[i64],
    ) -> Result<Option<QueueAssignment>, sqlx::Error> {
        // The `queue` table has no `schedule_time` or `success_chance`
        // columns (see schema/state.sql). The score is derived from
        // the candidate's `success_chance` via a join.
        let mut query = r#"
            SELECT
                queue.command,
                queue.context,
                queue.id::bigint AS id,
                queue.estimated_duration,
                queue.suite::text AS campaign,
                queue.refresh,
                queue.requester,
                queue.change_set,
                codebase.vcs_type::text AS vcs_type,
                codebase.branch_url,
                codebase.subpath,
                queue.codebase,
                candidate.success_chance,
                queue.priority,
                queue.bucket::text AS bucket,
                CASE
                    WHEN candidate.success_chance IS NOT NULL THEN
                        (candidate.success_chance * 100) + (100 - queue.priority)
                    ELSE
                        (100 - queue.priority)
                END as computed_score
            FROM
                queue
            LEFT JOIN codebase ON codebase.name = queue.codebase
            LEFT JOIN candidate
                   ON candidate.codebase = queue.codebase
                  AND candidate.suite = queue.suite
                  AND coalesce(candidate.change_set, '') = coalesce(queue.change_set, '')
        "#
        .to_string();

        let mut conditions = Vec::new();
        let mut bind_count = 0;

        // Exclude already assigned queue items
        if !assigned_queue_items.is_empty() {
            bind_count += 1;
            conditions.push(format!("NOT (queue.id = ANY(${}::int[]))", bind_count));
        }

        // Filter by codebase if specified
        if codebase.is_some() {
            bind_count += 1;
            conditions.push(format!("queue.codebase = ${}", bind_count));
        }

        // Filter by campaign if specified
        if campaign.is_some() {
            bind_count += 1;
            conditions.push(format!("queue.suite = ${}", bind_count));
        }

        // Exclude hosts
        if !exclude_hosts.is_empty() {
            bind_count += 1;
            conditions.push(format!(
                "NOT (codebase.branch_url IS NOT NULL AND SUBSTRING(codebase.branch_url from '.*://(?:[^/@]*@)?([^/]*)') = ANY(${}::text[]))",
                bind_count
            ));
        }

        if !conditions.is_empty() {
            query.push_str(" WHERE ");
            query.push_str(&conditions.join(" AND "));
        }

        // Order by computed score (higher is better), then bucket, then priority.
        query.push_str(
            r#"
            ORDER BY
                queue.bucket ASC,
                computed_score DESC,
                queue.priority ASC,
                queue.id ASC
            LIMIT 1
        "#,
        );

        let mut sqlx_query = sqlx::query_as::<_, QueueRowJoined>(sqlx::AssertSqlSafe(&*query));

        if !assigned_queue_items.is_empty() {
            sqlx_query = sqlx_query.bind(assigned_queue_items);
        }
        if let Some(cb) = codebase {
            sqlx_query = sqlx_query.bind(cb);
        }
        if let Some(camp) = campaign {
            sqlx_query = sqlx_query.bind(camp);
        }
        if !exclude_hosts.is_empty() {
            sqlx_query = sqlx_query.bind(exclude_hosts);
        }

        Ok(sqlx_query
            .fetch_optional(self.pool())
            .await?
            .map(QueueRowJoined::into_assignment))
    }

    /// Calculate queue position for a specific queue item or codebase/campaign combination.
    pub async fn calculate_queue_position(
        &self,
        codebase: Option<&str>,
        campaign: Option<&str>,
        queue_id: Option<i64>,
    ) -> Result<Option<i64>, sqlx::Error> {
        // success_chance lives on `candidate`, joined via
        // (codebase, suite, change_set); the queue table itself has
        // no schedule_time/success_chance columns.
        if let Some(id) = queue_id {
            let position: Option<i64> = sqlx::query_scalar(
                r#"
                WITH ranked_queue AS (
                    SELECT
                        queue.id::bigint AS id,
                        ROW_NUMBER() OVER (
                            ORDER BY
                                queue.bucket ASC,
                                CASE
                                    WHEN candidate.success_chance IS NOT NULL THEN
                                        (candidate.success_chance * 100) + (100 - queue.priority)
                                    ELSE
                                        (100 - queue.priority)
                                END DESC,
                                queue.priority ASC,
                                queue.id ASC
                        ) as position
                    FROM queue
                    LEFT JOIN candidate
                           ON candidate.codebase = queue.codebase
                          AND candidate.suite = queue.suite
                          AND coalesce(candidate.change_set, '') = coalesce(queue.change_set, '')
                )
                SELECT position FROM ranked_queue WHERE id = $1
                "#,
            )
            .bind(id)
            .fetch_optional(self.pool())
            .await?;

            Ok(position)
        } else if let (Some(cb), Some(camp)) = (codebase, campaign) {
            let position: Option<i64> = sqlx::query_scalar(
                r#"
                WITH ranked_queue AS (
                    SELECT
                        queue.id::bigint AS id,
                        queue.codebase,
                        queue.suite,
                        ROW_NUMBER() OVER (
                            ORDER BY
                                queue.bucket ASC,
                                CASE
                                    WHEN candidate.success_chance IS NOT NULL THEN
                                        (candidate.success_chance * 100) + (100 - queue.priority)
                                    ELSE
                                        (100 - queue.priority)
                                END DESC,
                                queue.priority ASC,
                                queue.id ASC
                        ) as position
                    FROM queue
                    LEFT JOIN candidate
                           ON candidate.codebase = queue.codebase
                          AND candidate.suite = queue.suite
                          AND coalesce(candidate.change_set, '') = coalesce(queue.change_set, '')
                )
                SELECT MIN(position) FROM ranked_queue
                WHERE codebase = $1 AND suite = $2
                "#,
            )
            .bind(cb)
            .bind(camp)
            .fetch_optional(self.pool())
            .await?;

            Ok(position)
        } else {
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM queue")
                .fetch_one(self.pool())
                .await?;

            Ok(Some(count))
        }
    }

    /// Clean up orphaned data and maintain database consistency.
    pub async fn maintenance_cleanup(&self) -> Result<(), sqlx::Error> {
        // The old "DELETE FROM active_run" query lived here but that
        // table never existed in the real schema -- active-run state is
        // in-process in `crate::active_runs::ActiveRunStore` and the
        // watchdog drives eviction via `drain_older_than`. The DELETE
        // made this function fail every maintenance tick with
        // "relation 'active_run' does not exist"; removed.

        // Clean up old rate limit entries from Redis
        if let Some(redis_client) = self.redis() {
            if let Ok(mut conn) = redis_client.get_multiplexed_async_connection().await {
                let hosts: HashMap<String, String> =
                    conn.hgetall("rate-limit-hosts").await.unwrap_or_default();
                let now = Utc::now();

                for (host, time_str) in hosts.into_iter() {
                    if let Ok(retry_time) = DateTime::parse_from_rfc3339(&time_str) {
                        let retry_time = retry_time.with_timezone(&Utc);
                        if retry_time <= now {
                            let _: () = conn.hdel("rate-limit-hosts", &host).await.unwrap_or(());
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Get statistics about failed runs and retries.
    pub async fn get_failure_stats(&self) -> Result<HashMap<String, i64>, sqlx::Error> {
        let rows = sqlx::query(
            r#"
            SELECT 
                'total_failed' as stat_name, 
                COUNT(*) as count
            FROM run 
            WHERE result_code != 'success' AND finish_time > NOW() - INTERVAL '24 hours'
            UNION ALL
            SELECT 
                'transient_failures' as stat_name, 
                COUNT(*) as count
            FROM run 
            WHERE failure_transient = true AND finish_time > NOW() - INTERVAL '24 hours'
            UNION ALL
            SELECT 
                'retry_eligible' as stat_name, 
                COUNT(*) as count
            FROM queue 
            WHERE COALESCE(retry_count, 0) > 0
            "#,
        )
        .fetch_all(self.pool())
        .await?;

        let mut stats = HashMap::new();
        for row in rows {
            let stat_name: String = row.get("stat_name");
            let count: i64 = row.get("count");
            stats.insert(stat_name, count);
        }

        Ok(stats)
    }

    /// Get all codebases from the database.
    pub async fn get_codebases(&self) -> Result<Vec<serde_json::Value>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT name, branch_url, url, branch, subpath, vcs_type::text AS vcs_type, web_url, vcs_last_revision, value FROM codebase"
        )
        .fetch_all(self.pool())
        .await?;

        let mut codebases = Vec::new();
        for row in rows {
            let codebase = serde_json::json!({
                "name": row.get::<Option<String>, _>("name"),
                "branch_url": row.get::<Option<String>, _>("branch_url"),
                "url": row.get::<Option<String>, _>("url"),
                "branch": row.get::<Option<String>, _>("branch"),
                "subpath": row.get::<Option<String>, _>("subpath"),
                "vcs_type": row.get::<Option<String>, _>("vcs_type"),
                "web_url": row.get::<Option<String>, _>("web_url"),
                "vcs_last_revision": row.get::<Option<String>, _>("vcs_last_revision"),
                "value": row.get::<Option<i64>, _>("value")
            });
            codebases.push(codebase);
        }

        Ok(codebases)
    }

    /// Upload/update codebases in the database.
    pub async fn upload_codebases(
        &self,
        codebases: &[serde_json::Value],
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool().begin().await?;

        for codebase in codebases {
            let (url, branch_url, branch) = match effective_branch_url(codebase).ok().flatten() {
                Some((_, value)) => (
                    Some(value.to_string()),
                    Some(value.to_string()),
                    codebase
                        .get("branch")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                ),
                None => (None, None, None),
            };

            sqlx::query(
                r#"
                INSERT INTO codebase
                (name, branch_url, url, branch, subpath, vcs_type, vcs_last_revision, value, web_url)
                VALUES ($1, $2, $3, $4, $5, $6::text::vcs_type, $7, $8, $9)
                ON CONFLICT (name) DO UPDATE SET
                    branch_url = EXCLUDED.branch_url,
                    subpath = EXCLUDED.subpath,
                    vcs_type = EXCLUDED.vcs_type,
                    vcs_last_revision = EXCLUDED.vcs_last_revision,
                    value = EXCLUDED.value,
                    url = EXCLUDED.url,
                    branch = EXCLUDED.branch,
                    web_url = EXCLUDED.web_url
                "#
            )
            .bind(codebase.get("name").and_then(|v| v.as_str()))
            .bind(branch_url)
            .bind(url)
            .bind(branch)
            // Store an empty subpath as "" rather than NULL when unset, so
            // downstream consumers get a string.
            .bind(codebase.get("subpath").and_then(|v| v.as_str()).unwrap_or(""))
            .bind(codebase.get("vcs_type").and_then(|v| v.as_str()))
            .bind(codebase.get("vcs_last_revision").and_then(|v| v.as_str()))
            .bind(codebase.get("value").and_then(|v| v.as_i64()))
            .bind(codebase.get("web_url").and_then(|v| v.as_str()))
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    /// Get all candidates from the database.
    pub async fn get_candidates(&self) -> Result<Vec<serde_json::Value>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, codebase, suite, command, publish_policy, change_set, context, value, success_chance FROM candidate"
        )
        .fetch_all(self.pool())
        .await?;

        let mut candidates = Vec::new();
        for row in rows {
            let candidate = serde_json::json!({
                "id": row.get::<i64, _>("id"),
                "codebase": row.get::<String, _>("codebase"),
                "campaign": row.get::<String, _>("suite"), // Note: "suite" maps to "campaign" in API
                "command": row.get::<Option<String>, _>("command"),
                "publish-policy": row.get::<Option<String>, _>("publish_policy"),
                "change_set": row.get::<Option<String>, _>("change_set"),
                "context": row.get::<Option<serde_json::Value>, _>("context"),
                "value": row.get::<Option<i64>, _>("value"),
                "success_chance": row.get::<Option<f64>, _>("success_chance")
            });
            candidates.push(candidate);
        }

        Ok(candidates)
    }

    /// Upload/update candidates in the database.
    pub async fn upload_candidates(
        &self,
        candidates: &[serde_json::Value],
    ) -> Result<Vec<String>, sqlx::Error> {
        let mut tx = self.pool().begin().await?;
        let mut errors = Vec::new();

        for candidate in candidates {
            let codebase = match candidate.get("codebase").and_then(|v| v.as_str()) {
                Some(cb) => cb,
                None => {
                    errors.push("Missing or invalid codebase field".to_string());
                    continue;
                }
            };

            let campaign = match candidate.get("campaign").and_then(|v| v.as_str()) {
                Some(c) => c,
                None => {
                    errors.push("Missing or invalid campaign field".to_string());
                    continue;
                }
            };

            let command = candidate.get("command").and_then(|v| v.as_str());
            let publish_policy = candidate.get("publish-policy").and_then(|v| v.as_str());
            let change_set = candidate.get("change_set").and_then(|v| v.as_str());
            let context = candidate.get("context");
            let value = candidate.get("value").and_then(|v| v.as_i64());
            let success_chance = candidate.get("success_chance").and_then(|v| v.as_f64());

            let result = sqlx::query(
                r#"
                INSERT INTO candidate 
                (suite, command, change_set, context, value, success_chance, publish_policy, codebase) 
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8) 
                ON CONFLICT (codebase, suite, coalesce(change_set, ''::text)) 
                DO UPDATE SET
                    context = EXCLUDED.context,
                    value = EXCLUDED.value,
                    success_chance = EXCLUDED.success_chance,
                    command = EXCLUDED.command,
                    publish_policy = COALESCE(EXCLUDED.publish_policy, candidate.publish_policy),
                    codebase = EXCLUDED.codebase
                RETURNING id
                "#
            )
            .bind(campaign)
            .bind(command)
            .bind(change_set)
            .bind(context)
            .bind(value)
            .bind(success_chance)
            .bind(publish_policy)
            .bind(codebase)
            .fetch_one(&mut *tx)
            .await;

            if let Err(e) = result {
                errors.push(format!(
                    "Failed to insert candidate for {}/{}: {}",
                    codebase, campaign, e
                ));
            }
        }

        if errors.is_empty() {
            tx.commit().await?;
        } else {
            tx.rollback().await?;
        }

        Ok(errors)
    }

    /// Delete a candidate by ID.
    pub async fn delete_candidate(&self, candidate_id: i64) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool().begin().await?;

        sqlx::query("DELETE FROM followup WHERE candidate = $1")
            .bind(candidate_id)
            .execute(&mut *tx)
            .await?;

        let candidate_info =
            sqlx::query("DELETE FROM candidate WHERE id = $1 RETURNING suite, codebase")
                .bind(candidate_id)
                .fetch_optional(&mut *tx)
                .await?;

        if let Some(row) = candidate_info {
            let suite: String = row.get("suite");
            let codebase: String = row.get("codebase");

            sqlx::query("DELETE FROM queue WHERE suite = $1 AND codebase = $2")
                .bind(&suite)
                .bind(&codebase)
                .execute(&mut *tx)
                .await?;

            tx.commit().await?;
            Ok(true)
        } else {
            tx.rollback().await?;
            Ok(false)
        }
    }

    /// Get resume information for a run, looking for related runs that can be resumed.
    pub async fn get_resume_info(
        &self,
        run_id: &str,
        codebase: &str,
        campaign: &str,
    ) -> Result<Option<crate::ResultResume>, sqlx::Error> {
        // Look for a previous run on the same codebase and campaign that might be resumable
        let resume_run = sqlx::query(
            r#"
            SELECT id FROM run 
            WHERE codebase = $1 AND suite = $2 AND id != $3
            AND result_code IN ('interrupted', 'worker-failure', 'timeout')
            AND finish_time > NOW() - INTERVAL '7 days'
            ORDER BY finish_time DESC 
            LIMIT 1
            "#,
        )
        .bind(codebase)
        .bind(campaign)
        .bind(run_id)
        .fetch_optional(self.pool())
        .await?;

        if let Some(row) = resume_run {
            let resume_run_id: String = row.get("id");
            Ok(Some(crate::ResultResume {
                run_id: resume_run_id,
            }))
        } else {
            Ok(None)
        }
    }

    /// Get codebase configuration from database.
    pub async fn get_codebase_config(
        &self,
        codebase_name: &str,
    ) -> Result<Option<CodebaseConfig>, sqlx::Error> {
        // codebase.vcs_type is a postgres enum; cast to text so
        // `row.get::<Option<String>, _>("vcs_type")` decodes cleanly.
        let row = sqlx::query(
            r#"
            SELECT name, branch_url, vcs_type::text AS vcs_type, subpath
            FROM codebase
            WHERE name = $1
            "#,
        )
        .bind(codebase_name)
        .fetch_optional(self.pool())
        .await?;

        if let Some(row) = row {
            Ok(Some(CodebaseConfig {
                name: row.get("name"),
                branch_url: row.get("branch_url"),
                vcs_type: row.get("vcs_type"),
                subpath: row.get("subpath"),
            }))
        } else {
            Ok(None)
        }
    }

    /// Get distribution configuration from database.
    // TODO: implement lookup against a distributions table.
    // Python's runner reads distribution config from janitor.conf; when
    // that moves into the database this needs to query and return the
    // matching row. Callers currently fall back to config-file entries.
    pub async fn get_distribution_config(
        &self,
        _distribution_name: &str,
    ) -> Result<Option<DistributionConfig>, sqlx::Error> {
        Ok(None)
    }

    /// Get remotes information for a run from the new_result_branch table.
    pub async fn get_run_remotes(
        &self,
        run_id: &str,
    ) -> Result<
        Option<
            std::collections::HashMap<String, std::collections::HashMap<String, serde_json::Value>>,
        >,
        sqlx::Error,
    > {
        let rows = sqlx::query(
            r#"
            SELECT DISTINCT remote_name, base_revision, revision
            FROM new_result_branch
            WHERE run_id = $1 AND remote_name IS NOT NULL
            "#,
        )
        .bind(run_id)
        .fetch_all(self.pool())
        .await?;

        if rows.is_empty() {
            return Ok(None);
        }

        let mut remotes = std::collections::HashMap::new();
        for row in rows {
            let remote_name: String = row.get("remote_name");
            let base_revision: Option<String> = row.get("base_revision");
            let revision: Option<String> = row.get("revision");

            let mut remote_data = std::collections::HashMap::new();
            if let Some(base_rev) = base_revision {
                remote_data.insert(
                    "base_revision".to_string(),
                    serde_json::Value::String(base_rev),
                );
            }
            if let Some(rev) = revision {
                remote_data.insert("revision".to_string(), serde_json::Value::String(rev));
            }

            remotes.insert(remote_name, remote_data);
        }

        Ok(Some(remotes))
    }

    /// Get target information for a run from the debian_build table.
    pub async fn get_run_target(
        &self,
        run_id: &str,
    ) -> Result<Option<serde_json::Value>, sqlx::Error> {
        let row = sqlx::query(
            r#"
            SELECT source, version, distribution, binary_packages, lintian_result
            FROM debian_build
            WHERE run_id = $1
            "#,
        )
        .bind(run_id)
        .fetch_optional(self.pool())
        .await?;

        if let Some(row) = row {
            let target_details = serde_json::json!({
                "source": row.get::<String, _>("source"),
                "version": row.get::<String, _>("version"),
                "distribution": row.get::<String, _>("distribution"),
                "binary_packages": row.get::<Option<Vec<String>>, _>("binary_packages"),
                "lintian_result": row.get::<Option<serde_json::Value>, _>("lintian_result")
            });

            let target = serde_json::json!({
                "name": "apt",
                "details": target_details
            });

            Ok(Some(target))
        } else {
            Ok(None)
        }
    }

    /// Get builder result information for a run from the debian_build table.
    pub async fn get_run_builder_result(
        &self,
        run_id: &str,
    ) -> Result<Option<serde_json::Value>, sqlx::Error> {
        let row = sqlx::query(
            r#"
            SELECT source, version, distribution, binary_packages, lintian_result
            FROM debian_build
            WHERE run_id = $1
            "#,
        )
        .bind(run_id)
        .fetch_optional(self.pool())
        .await?;

        if let Some(row) = row {
            let builder_result = serde_json::json!({
                "kind": "apt",
                "source": row.get::<String, _>("source"),
                "build_version": row.get::<String, _>("version"),
                "build_distribution": row.get::<String, _>("distribution"),
                "binary_packages": row.get::<Option<Vec<String>>, _>("binary_packages"),
                "lintian": row.get::<Option<serde_json::Value>, _>("lintian_result"),
                "changes_filenames": null // Not stored in current schema
            });

            Ok(Some(builder_result))
        } else {
            Ok(None)
        }
    }
}

/// Configuration for a codebase from database.
#[derive(Debug, Clone)]
pub struct CodebaseConfig {
    /// Name of the codebase.
    pub name: String,
    /// URL of the branch.
    pub branch_url: Option<String>,
    /// Type of version control system.
    pub vcs_type: Option<String>,
    /// Subpath within the repository.
    pub subpath: Option<String>,
}

/// Configuration for a distribution from database.
#[derive(Debug, Clone)]
pub struct DistributionConfig {
    /// Name of the distribution.
    pub name: String,
    /// Archive mirror URI.
    pub archive_mirror_uri: Option<String>,
    /// Chroot environment name.
    pub chroot: Option<String>,
    /// Distribution vendor.
    pub vendor: Option<String>,
}

impl RunnerDatabase {
    /// Track worker activity by updating last seen time in Redis
    pub async fn track_worker_activity(
        &self,
        worker_name: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(redis) = self.redis() {
            let mut conn = redis.get_multiplexed_async_connection().await?;
            let key = format!("worker:last_seen:{}", worker_name);
            let timestamp = chrono::Utc::now().timestamp();
            let _: () = redis::cmd("SET")
                .arg(&key)
                .arg(timestamp)
                .arg("EX")
                .arg(86400) // Expire after 24 hours
                .query_async(&mut conn)
                .await?;
        }
        Ok(())
    }

    /// Get last seen times for all workers
    pub async fn get_workers_last_seen(
        &self,
    ) -> Result<HashMap<String, DateTime<Utc>>, Box<dyn std::error::Error + Send + Sync>> {
        let mut last_seen_map = HashMap::new();

        if let Some(redis) = self.redis() {
            let mut conn = redis.get_multiplexed_async_connection().await?;

            let keys: Vec<String> = redis::cmd("KEYS")
                .arg("worker:last_seen:*")
                .query_async(&mut conn)
                .await?;

            for key in keys {
                if let Some(worker_name) = key.strip_prefix("worker:last_seen:") {
                    let timestamp: Option<i64> =
                        redis::cmd("GET").arg(&key).query_async(&mut conn).await?;

                    if let Some(ts) = timestamp {
                        if let Some(datetime) = DateTime::from_timestamp(ts, 0) {
                            last_seen_map.insert(worker_name.to_string(), datetime);
                        }
                    }
                }
            }
        }

        Ok(last_seen_map)
    }

    /// Check if a worker is considered failed (not seen for too long)
    pub async fn get_failed_workers(
        &self,
        timeout_minutes: i64,
    ) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        let mut failed_workers = Vec::new();
        let last_seen = self.get_workers_last_seen().await?;
        let timeout = Duration::minutes(timeout_minutes);
        let now = Utc::now();

        let all_workers = sqlx::query("SELECT name FROM worker")
            .fetch_all(self.pool())
            .await?;

        for row in all_workers {
            let worker_name: String = row.get("name");

            match last_seen.get(&worker_name) {
                Some(last_seen_time) => {
                    if now.signed_duration_since(*last_seen_time) > timeout {
                        failed_workers.push(worker_name);
                    }
                }
                None => {
                    // Worker has never been seen - consider it failed
                    failed_workers.push(worker_name);
                }
            }
        }

        Ok(failed_workers)
    }
}
