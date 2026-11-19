use chrono::TimeDelta;
use janitor_types::{Campaign, ChangeSetId, CodebaseName};
use serde::{Deserialize, Serialize};
use sqlx::postgres::types::PgInterval;
use sqlx::{Error, FromRow, PgPool};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

#[derive(Debug, FromRow)]
pub struct QueueItem {
    pub id: i32,
    pub context: Option<String>,
    pub command: String,
    // `queue.estimated_duration` is an `interval` in the schema and
    // is NULLable. Most rows have it (the scheduler estimates from
    // historical runs), but manual inserts — admin reschedules,
    // cleanup operations — leave it unset. Decoding as the
    // non-nullable `PgInterval` made `iter_queue` 500 the moment a
    // single such row entered the queue.
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
    pub wait_time: Option<PgInterval>,
}

#[derive(FromRow, Debug, Clone, Serialize, Deserialize, Default)]
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
            "SELECT position, wait_time FROM queue_positions WHERE codebase = $1 AND campaign = $2",
        )
        .bind(codebase)
        .bind(campaign)
        .fetch_optional(self.pool)
        .await?;

        Ok(row)
    }

    pub async fn get_item(&self, queue_id: i32) -> Result<Option<QueueItem>, Error> {
        let row = sqlx::query_as::<_, QueueItem>(
            "SELECT id, context, command, estimated_duration, campaign, refresh, requester, change_set, codebase
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
                queue.id, queue.context, queue.command, queue.estimated_duration, queue.campaign, 
                queue.refresh, queue.requester, queue.change_set, queue.codebase,
                codebase.vcs_type, codebase.branch_url, codebase.subpath
             FROM queue
             LEFT JOIN codebase ON codebase.name = queue.codebase",
        );

        let mut conditions: Vec<String> = Vec::new();
        let mut next_param: usize = 1;

        if assigned_queue_items.is_some() {
            conditions.push(format!("NOT (queue.id = ANY(${}::int[]))", next_param));
            next_param += 1;
        }

        if codebase.is_some() {
            conditions.push(format!("queue.codebase = ${}", next_param));
            next_param += 1;
        }

        if campaign.is_some() {
            conditions.push(format!("queue.campaign = ${}", next_param));
            next_param += 1;
        }

        if exclude_hosts.is_some() {
            conditions.push(format!(
                "NOT (codebase.branch_url IS NOT NULL AND SUBSTRING(codebase.branch_url from '.*://(?:[^/@]*@)?([^/]*)') = ANY(${}::text[]))",
                next_param
            ));
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
        // bucket and campaign are a Postgres enum and domain, so binding
        // &str/newtypes needs the explicit casts.
        sqlx::query_as(
            "INSERT INTO queue (command, priority, bucket, context, estimated_duration, campaign, refresh, requester, change_set, codebase)
             VALUES ($1, (SELECT COALESCE(MIN(priority), 0) FROM queue) + $2::bigint, $3::queue_bucket, $4, $5, $6::campaign_name, $7, $8, $9, $10)
             ON CONFLICT (codebase, campaign, coalesce(change_set, ''::text))
             DO UPDATE SET (context, priority, bucket, estimated_duration, refresh, requester, command) = (
                               SELECT n.context, n.priority, n.bucket, n.estimated_duration, n.refresh, n.requester, n.command
                               FROM (SELECT (CASE WHEN queue.bucket >= EXCLUDED.bucket THEN EXCLUDED ELSE queue END).*) AS n),
                           codebase = EXCLUDED.codebase
             RETURNING id, bucket::text"
        )
        .bind(command)
        // queue.priority is bigint; bind as i64 so Postgres doesn't refuse
        // the float8 literal when adding it to MIN(priority).
        .bind(offset as i64)
        .bind(bucket)
        .bind(context)
        .bind(estimated_duration)
        .bind(campaign)
        .bind(refresh)
        .bind(requester)
        .bind(change_set)
        .bind(codebase)
        .fetch_one(self.pool)
        .await
    }

    pub async fn get_buckets(&self) -> Result<Vec<(String, i64)>, Error> {
        // bucket::text so the queue_bucket enum decodes as a String.
        sqlx::query_as(
            "SELECT bucket::text, count(*) FROM queue GROUP BY bucket ORDER BY bucket ASC",
        )
        .fetch_all(self.pool)
        .await
    }

    /// Iterator for queue items with filtering and limiting capabilities
    ///
    /// This matches the Python iter_queue() method functionality
    ///
    /// # Arguments
    /// * `limit` - Optional limit on number of items to return
    /// * `campaign` - Optional campaign filter
    ///
    /// # Returns
    /// Vector of QueueItem objects in priority order (bucket ASC, priority ASC, id ASC)
    pub async fn iter_queue(
        &self,
        limit: Option<u32>,
        campaign: Option<&Campaign>,
    ) -> Result<Vec<QueueItem>, Error> {
        let query = if campaign.is_some() {
            r#"
            SELECT queue.id, queue.context, queue.command, queue.estimated_duration,
                   queue.campaign, queue.refresh, queue.requester,
                   queue.change_set, queue.codebase
            FROM queue
            WHERE queue.campaign = $1
            ORDER BY bucket ASC, priority ASC, queue.id ASC
            LIMIT $2
            "#
        } else {
            r#"
            SELECT queue.id, queue.context, queue.command, queue.estimated_duration,
                   queue.campaign, queue.refresh, queue.requester,
                   queue.change_set, queue.codebase
            FROM queue
            ORDER BY bucket ASC, priority ASC, queue.id ASC
            LIMIT $1
            "#
        };

        let sqlx_query = if let Some(campaign) = campaign {
            sqlx::query_as::<_, QueueItem>(sqlx::AssertSqlSafe(&*query))
                .bind(campaign)
                .bind(limit.map_or(i64::MAX, i64::from))
        } else {
            sqlx::query_as::<_, QueueItem>(sqlx::AssertSqlSafe(query))
                .bind(limit.map_or(i64::MAX, i64::from))
        };

        sqlx_query.fetch_all(self.pool).await
    }

    /// Get next queue item with fixed return type matching Python API
    ///
    /// # Returns
    /// (QueueItem, VCS info dict) where VCS dict is never None, just empty
    pub async fn next_item_tuple(
        &self,
        codebase: Option<&CodebaseName>,
        campaign: Option<&Campaign>,
        exclude_hosts: Option<HashSet<String>>,
        assigned_queue_items: Option<HashSet<i32>>,
    ) -> Result<(Option<QueueItem>, HashMap<String, String>), Error> {
        let (item, vcs_info) = self
            .next_item(codebase, campaign, exclude_hosts, assigned_queue_items)
            .await?;

        // Convert VcsInfo to HashMap, filtering out None values
        let mut vcs_dict = HashMap::new();
        if let Some(vcs) = vcs_info {
            if let Some(branch_url) = vcs.branch_url {
                if !branch_url.is_empty() {
                    vcs_dict.insert("branch_url".to_string(), branch_url);
                }
            }
            if let Some(subpath) = vcs.subpath {
                if !subpath.is_empty() {
                    vcs_dict.insert("subpath".to_string(), subpath);
                }
            }
            if let Some(vcs_type) = vcs.vcs_type {
                if !vcs_type.is_empty() {
                    vcs_dict.insert("vcs_type".to_string(), vcs_type);
                }
            }
        }

        Ok((item, vcs_dict))
    }
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    //! Database-backed integration tests for `Queue`. These mirror the
    //! Python `tests/test_queue.py` cases (`test_get_buckets`,
    //! `test_add`, `test_double_add`, `test_vcs_only`) so that
    //! retiring the Python test file doesn't drop coverage of the
    //! queue's add/next_item/get_buckets path.
    //!
    //! Each test uses its own unique scratch database (the
    //! TestDatabase default creates one per call), so they're
    //! parallel-safe without `#[serial]`. If no Postgres is reachable
    //! the per-test setup errors and the test logs + skips.

    use super::*;
    use crate::schema::setup_test_database;
    use crate::test_utils::TestDatabase;

    /// Set up a fresh TestDatabase + janitor schema. Returns None
    /// (after logging) when no Postgres is available, so these tests
    /// degrade to skipped rather than failing in CI without a DB.
    async fn make_db() -> Option<TestDatabase> {
        match TestDatabase::new_optional().await {
            Ok(Some(db)) => match setup_test_database(&db.pool).await {
                Ok(()) => Some(db),
                Err(e) => {
                    eprintln!("Skipping queue test: schema setup failed: {}", e);
                    None
                }
            },
            _ => {
                eprintln!("Skipping queue test: no Postgres available");
                None
            }
        }
    }

    #[tokio::test]
    async fn test_get_buckets_empty() {
        let Some(db) = make_db().await else { return };
        let queue = Queue::new(&db.pool);
        // Fresh schema → no queue rows → no buckets.
        let buckets = queue.get_buckets().await.unwrap();
        assert_eq!(buckets, Vec::<(String, i64)>::new());
    }

    #[tokio::test]
    async fn test_add_returns_default_bucket() {
        let Some(db) = make_db().await else { return };
        sqlx::query("INSERT INTO codebase (name) VALUES ('foo')")
            .execute(&db.pool)
            .await
            .unwrap();

        let queue = Queue::new(&db.pool);
        // First add: id 1, default bucket. Mirrors Python's
        // assert == (1, "default").
        let (id, bucket) = queue
            .add(
                &CodebaseName::new("foo").unwrap(),
                "true",
                &Campaign::new("bar").unwrap(), None, 0.0, "default", None, None, false, None,
            )
            .await
            .unwrap();
        assert_eq!(id, 1);
        assert_eq!(bucket, "default");

        // next_item should hand back exactly the row we just
        // added, with the codebase/campaign we set.
        let (item, _vcs) = queue.next_item(None, None, None, None).await.unwrap();
        let item = item.expect("queue should have an item");
        assert_eq!(item.codebase, "foo");
        assert_eq!(item.campaign, "bar");
    }

    #[tokio::test]
    async fn test_double_add_is_idempotent() {
        let Some(db) = make_db().await else { return };
        sqlx::query("INSERT INTO codebase (name) VALUES ('foo')")
            .execute(&db.pool)
            .await
            .unwrap();

        let queue = Queue::new(&db.pool);
        let first = queue
            .add(
                &CodebaseName::new("foo").unwrap(),
                "true",
                &Campaign::new("bar").unwrap(), None, 0.0, "default", None, None, false, None,
            )
            .await
            .unwrap();
        let second = queue
            .add(
                &CodebaseName::new("foo").unwrap(),
                "true",
                &Campaign::new("bar").unwrap(), None, 0.0, "default", None, None, false, None,
            )
            .await
            .unwrap();
        // Same (codebase, campaign, change_set) → ON CONFLICT path
        // returns the same id and bucket, not a new row.
        assert_eq!(first, second);
        assert_eq!(first, (1, "default".to_string()));
    }

    #[tokio::test]
    async fn test_vcs_only_codebase_yields_vcs_info() {
        let Some(db) = make_db().await else { return };
        // Codebase with vcs_type but NULL branch_url — mirrors
        // Python's "vcs only" case.
        sqlx::query(
            "INSERT INTO codebase (name, vcs_type, branch_url) VALUES ('foo', 'git', NULL)",
        )
        .execute(&db.pool)
        .await
        .unwrap();

        let queue = Queue::new(&db.pool);
        let (id, bucket) = queue
            .add(
                &CodebaseName::new("foo").unwrap(),
                "true",
                &Campaign::new("bar").unwrap(), None, 0.0, "default", None, None, false, None,
            )
            .await
            .unwrap();
        assert_eq!((id, bucket), (1, "default".to_string()));

        let (item, vcs_dict) = queue.next_item_tuple(None, None, None, None).await.unwrap();
        let item = item.expect("queue should have an item");
        assert_eq!(item.codebase, "foo");
        assert_eq!(item.campaign, "bar");
        // VCS dict reports the vcs_type from the codebase row but
        // omits branch_url because it was NULL — mirrors the
        // Python assertion `vcs_info == {"vcs_type": "git"}`.
        assert_eq!(vcs_dict.get("vcs_type").map(String::as_str), Some("git"));
        assert!(!vcs_dict.contains_key("branch_url"));
    }

    #[tokio::test]
    async fn test_get_buckets_groups_by_bucket() {
        let Some(db) = make_db().await else { return };
        sqlx::query("INSERT INTO codebase (name) VALUES ('a'), ('b'), ('c')")
            .execute(&db.pool)
            .await
            .unwrap();

        let queue = Queue::new(&db.pool);
        // Two items in 'default', one in 'manual'.
        queue
            .add(
                &CodebaseName::new("a").unwrap(),
                "true",
                &Campaign::new("lf").unwrap(), None, 0.0, "default", None, None, false, None,
            )
            .await
            .unwrap();
        queue
            .add(
                &CodebaseName::new("b").unwrap(),
                "true",
                &Campaign::new("lf").unwrap(), None, 0.0, "default", None, None, false, None,
            )
            .await
            .unwrap();
        queue
            .add(
                &CodebaseName::new("c").unwrap(),
                "true",
                &Campaign::new("lf").unwrap(), None, 0.0, "manual", None, None, false, None,
            )
            .await
            .unwrap();

        let mut buckets = queue.get_buckets().await.unwrap();
        buckets.sort();
        assert_eq!(
            buckets,
            vec![
                ("default".to_string(), 2_i64),
                ("manual".to_string(), 1_i64),
            ]
        );
    }

    /// Regression: `iter_queue` 500ed any time a row had a NULL
    /// `estimated_duration` because `QueueItem.estimated_duration`
    /// was decoded as the non-nullable `PgInterval`. Manual queue
    /// inserts (admin reschedule, cleanup-runs) never populate that
    /// column, so /api/queue went down the moment one such row
    /// landed in the table. Insert a row with NULL
    /// `estimated_duration` directly and confirm `iter_queue`
    /// decodes it cleanly into `Option<PgInterval>`.
    #[tokio::test]
    async fn test_iter_queue_handles_null_estimated_duration() {
        let Some(db) = make_db().await else { return };
        sqlx::query("INSERT INTO codebase (name) VALUES ('foo')")
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO queue (codebase, campaign, command, priority, bucket, requester) \
             VALUES ('foo', 'lintian-fixes'::campaign_name, 'true', 0, 'manual', 'test')",
        )
        .execute(&db.pool)
        .await
        .unwrap();

        let queue = Queue::new(&db.pool);
        let items = queue
            .iter_queue(Some(10), None)
            .await
            .expect("iter_queue must accept NULL estimated_duration");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].codebase, "foo");
        assert_eq!(items[0].campaign, "lintian-fixes");
        assert!(items[0].estimated_duration.is_none());
    }

    // Covers the ON CONFLICT bucket-priority path: re-adding under a
    // lower-priority bucket must leave the existing entry alone.

    #[tokio::test]
    async fn test_add_and_get_buckets() {
        let Some(db) = crate::test_utils::TestDatabase::new_optional_with_schema(
            crate::test_utils::TestSchema::Core,
        )
        .await
        .unwrap() else {
            return;
        };
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

        // Re-adding with a stronger bucket replaces the entry.
        let (id3, bucket3) = queue
            .add(
                &CodebaseName::new("cb").unwrap(),
                "cmd",
                &Campaign::new("lintian-fixes").unwrap(),
                None,
                0.0,
                "manual",
                None,
                None,
                false,
                None,
            )
            .await
            .unwrap();
        assert_eq!((id3, bucket3.as_str()), (id, "manual"));

        assert_eq!(
            queue.get_buckets().await.unwrap(),
            vec![("manual".to_string(), 1)]
        );
    }
}
