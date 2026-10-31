use crate::{Campaign, ChangeSetId, CodebaseName};
use chrono::TimeDelta;
use sqlx::postgres::types::PgInterval;
use sqlx::{Error, FromRow, PgPool};
use std::collections::HashSet;
use std::hash::{Hash, Hasher};

#[derive(Debug, FromRow)]
pub struct QueueItem {
    pub id: i32,
    pub context: Option<String>,
    pub command: Option<String>,
    // queue.estimated_duration is nullable (schema/state.sql).
    pub estimated_duration: Option<PgInterval>,
    pub campaign: Campaign,
    pub refresh: Option<bool>,
    pub requester: Option<String>,
    pub change_set: Option<ChangeSetId>,
    pub codebase: CodebaseName,
}

impl PartialEq for QueueItem {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for QueueItem {}

impl PartialOrd for QueueItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.id.cmp(&other.id))
    }
}

impl Ord for QueueItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.id.cmp(&other.id)
    }
}

impl Hash for QueueItem {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

pub struct Queue<'a> {
    pool: &'a PgPool,
}

#[derive(FromRow)]
pub struct ETA {
    pub position: i64,
    pub wait_time: PgInterval,
}

#[derive(Debug, Default, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct VcsInfo {
    pub branch_url: Option<String>,
    pub subpath: Option<String>,
    pub vcs_type: Option<String>,
}

impl<'a> Queue<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Queue { pool }
    }

    pub async fn get_position(
        &self,
        campaign: &Campaign,
        codebase: &CodebaseName,
    ) -> Result<Option<ETA>, Error> {
        let row: Option<ETA> = sqlx::query_as::<_, ETA>(
            "SELECT position, wait_time FROM queue_positions WHERE codebase = $1 AND suite = $2",
        )
        .bind(codebase)
        .bind(campaign)
        .fetch_optional(self.pool)
        .await?;

        Ok(row)
    }

    pub async fn get_item(&self, queue_id: i32) -> Result<Option<QueueItem>, Error> {
        let row = sqlx::query_as::<_, QueueItem>(
            "SELECT id, context, command, estimated_duration, suite AS campaign, refresh, requester, change_set, codebase
             FROM queue
             WHERE id = $1"
        )
        .bind(queue_id)
        .fetch_optional(self.pool)
        .await?;

        Ok(row)
    }

    /// Get the next item in the queue that is not assigned to any worker
    ///
    /// If `codebase` is provided, only items from that codebase will be considered.
    /// If `campaign` is provided, only items from that campaign will be considered.
    ///
    /// # Arguments
    /// * `codebase` - The codebase to filter by
    /// * `campaign` - The campaign to filter by
    /// * `exclude_hosts` - A set of VCS URL hosts to exclude
    /// * `assigned_queue_items` - A set of queue items that are already assigned
    pub async fn next_item(
        &self,
        codebase: Option<&CodebaseName>,
        campaign: Option<&Campaign>,
        exclude_hosts: Option<HashSet<String>>,
        assigned_queue_items: Option<HashSet<i32>>,
    ) -> Result<(Option<QueueItem>, Option<VcsInfo>), Error> {
        let mut query = String::from(
            "SELECT
                queue.id, queue.context, queue.command, queue.estimated_duration, queue.suite AS campaign, 
                queue.refresh, queue.requester, queue.change_set, queue.codebase,
                codebase.vcs_type, codebase.branch_url, codebase.subpath
             FROM queue
             LEFT JOIN codebase ON codebase.name = queue.codebase"
        );

        let mut conditions = Vec::new();

        if assigned_queue_items.is_some() {
            conditions.push("NOT (queue.id = ANY($1::int[]))");
        }

        if codebase.is_some() {
            conditions.push("queue.codebase = $2");
        }

        if campaign.is_some() {
            conditions.push("queue.suite = $3");
        }

        if exclude_hosts.is_some() {
            conditions.push(
                "NOT (codebase.branch_url IS NOT NULL AND SUBSTRING(codebase.branch_url from '.*://(?:[^/@]*@)?([^/]*)') = ANY($4::text[]))"
            );
        }

        if !conditions.is_empty() {
            query += " WHERE ";
            query += &conditions.join(" AND ");
        }

        query += " ORDER BY queue.bucket ASC, queue.priority ASC, queue.id ASC LIMIT 1";

        let mut query_builder = sqlx::query(sqlx::AssertSqlSafe(query));

        if let Some(assigned_queue_items) = assigned_queue_items {
            query_builder =
                query_builder.bind(assigned_queue_items.into_iter().collect::<Vec<_>>());
        }

        if let Some(codebase) = codebase {
            query_builder = query_builder.bind(codebase);
        }

        if let Some(campaign) = campaign {
            query_builder = query_builder.bind(campaign);
        }

        if let Some(exclude_hosts) = exclude_hosts.as_ref() {
            query_builder = query_builder.bind(exclude_hosts.iter().collect::<Vec<_>>());
        }

        let row = query_builder.fetch_optional(self.pool).await?;

        if let Some(row) = row {
            let vcs_info = VcsInfo::from_row(&row)?;

            let queue_item: QueueItem = QueueItem::from_row(&row)?;

            Ok((Some(queue_item), Some(vcs_info)))
        } else {
            Ok((None, None))
        }
    }

    pub async fn add(
        &self,
        codebase: &CodebaseName,
        command: &str,
        campaign: &Campaign,
        change_set: Option<&ChangeSetId>,
        offset: f64,
        bucket: &str,
        context: Option<&str>,
        estimated_duration: Option<TimeDelta>,
        refresh: bool,
        requester: Option<&str>,
    ) -> Result<(i32, String), Error> {
        let row: Option<(i32, String)> = sqlx::query_as(
            "INSERT INTO queue (command, priority, bucket, context, estimated_duration, suite, refresh, requester, change_set, codebase)
             VALUES ($1, (SELECT COALESCE(MIN(priority), 0) FROM queue) + $2::bigint, $3::queue_bucket, $4, $5, $6::suite_name, $7, $8, $9, $10)
             ON CONFLICT (codebase, suite, coalesce(change_set, ''::text))
             DO UPDATE SET context = EXCLUDED.context,
                           priority = EXCLUDED.priority,
                           bucket = EXCLUDED.bucket,
                           estimated_duration = EXCLUDED.estimated_duration,
                           refresh = EXCLUDED.refresh,
                           requester = EXCLUDED.requester,
                           command = EXCLUDED.command,
                           codebase = EXCLUDED.codebase
             WHERE queue.bucket >= EXCLUDED.bucket OR
                   (queue.bucket = EXCLUDED.bucket AND queue.priority >= EXCLUDED.priority)
             RETURNING id, bucket::text"
        )
        .bind(command)
        .bind(offset as i64)
        .bind(bucket)
        .bind(context)
        .bind(estimated_duration)
        .bind(campaign)
        .bind(refresh)
        .bind(requester)
        .bind(change_set)
        .bind(codebase)
        .fetch_optional(self.pool)
        .await?;

        if let Some(row) = row {
            Ok(row)
        } else {
            sqlx::query_as(
                "SELECT id, bucket::text FROM queue WHERE codebase = $1 AND suite = $2 AND coalesce(change_set, ''::text) = $3"
            )
            .bind(codebase)
            .bind(campaign)
            .bind(change_set.map_or("", |c| c.as_str()))
            .fetch_one(self.pool)
            .await
        }
    }

    /// Iterate items currently queued.
    ///
    /// Rows are ordered the same way `next_item` would pick them
    /// (bucket, priority, id ascending); `limit` caps the number of
    /// rows, `campaign` optionally restricts to a single campaign.
    pub async fn iter_queue(
        &self,
        limit: Option<u32>,
        campaign: Option<&Campaign>,
    ) -> Result<Vec<QueueItem>, Error> {
        let query = if campaign.is_some() {
            r#"
            SELECT queue.id, queue.context, queue.command, queue.estimated_duration,
                   queue.suite AS campaign, queue.refresh, queue.requester,
                   queue.change_set, queue.codebase
            FROM queue
            WHERE queue.suite = $1
            ORDER BY bucket ASC, priority ASC, queue.id ASC
            LIMIT $2
            "#
        } else {
            r#"
            SELECT queue.id, queue.context, queue.command, queue.estimated_duration,
                   queue.suite AS campaign, queue.refresh, queue.requester,
                   queue.change_set, queue.codebase
            FROM queue
            ORDER BY bucket ASC, priority ASC, queue.id ASC
            LIMIT $1
            "#
        };

        let sqlx_query = if let Some(campaign) = campaign {
            sqlx::query_as::<_, QueueItem>(sqlx::AssertSqlSafe(query))
                .bind(campaign)
                .bind(limit.map_or(i64::MAX, i64::from))
        } else {
            sqlx::query_as::<_, QueueItem>(sqlx::AssertSqlSafe(query))
                .bind(limit.map_or(i64::MAX, i64::from))
        };

        sqlx_query.fetch_all(self.pool).await
    }

    pub async fn get_buckets(&self) -> Result<Vec<(String, i64)>, Error> {
        sqlx::query_as(
            "SELECT bucket::text, count(*) FROM queue GROUP BY bucket ORDER BY bucket ASC",
        )
        .fetch_all(self.pool)
        .await
    }
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_add_and_get_buckets() {
        let Some(db) = crate::test_utils::TestDatabase::new_optional()
            .await
            .unwrap()
        else {
            return;
        };
        crate::schema::setup_test_database(db.pool()).await.unwrap();
        sqlx::query(
            "INSERT INTO codebase (name, branch_url, url, vcs_type) \
             VALUES ('cb', 'https://example.invalid/cb', 'https://example.invalid/cb', 'git')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        let queue = Queue::new(db.pool());

        let (id, bucket) = queue
            .add(
                &CodebaseName::new("cb").unwrap(),
                "cmd",
                &Campaign::new("lintian-fixes").unwrap(),
                None,
                0.0,
                "missing-deps",
                None,
                None,
                false,
                None,
            )
            .await
            .unwrap();
        assert_eq!(bucket, "missing-deps");

        // Re-adding with a lower-priority bucket leaves the existing
        // entry alone and returns it.
        let (id2, bucket2) = queue
            .add(
                &CodebaseName::new("cb").unwrap(),
                "cmd",
                &Campaign::new("lintian-fixes").unwrap(),
                None,
                0.0,
                "default",
                None,
                None,
                false,
                None,
            )
            .await
            .unwrap();
        assert_eq!((id2, bucket2.as_str()), (id, "missing-deps"));

        assert_eq!(
            queue.get_buckets().await.unwrap(),
            vec![("missing-deps".to_string(), 1)]
        );
    }
}
