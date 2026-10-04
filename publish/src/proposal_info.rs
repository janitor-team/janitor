use breezyshim::forge::MergeProposal;
use breezyshim::RevisionId;
use chrono::{DateTime, Utc};
use janitor::publish::MergeProposalStatus;
use redis::AsyncCommands;
use sqlx::{PgPool, Row};

/// Information about a merge proposal stored in the database.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ProposalInfo {
    /// Whether the proposal can be merged.
    pub can_be_merged: Option<bool>,
    /// Current status of the proposal.
    pub status: String,
    /// Source revision ID.
    pub revision: Option<String>,
    /// Target branch URL.
    pub target_branch_url: Option<String>,
    /// Rate limit bucket for this proposal.
    pub rate_limit_bucket: Option<String>,
    /// Codebase this proposal belongs to. Nullable - the
    /// `merge_proposal.codebase` column is `text references
    /// codebase(name) on delete set null`, so a row's codebase can
    /// disappear when the underlying codebase is dropped, and rows
    /// discovered by forge enumeration before a candidate ever ran
    /// also start out with NULL until the publisher's
    /// guess-codebase pass populates them. Decoding as plain
    /// `String` made every `check_existing_mp` call on such rows
    /// 500 with `decoding column "codebase": unexpected null`,
    /// preventing that proposal from ever getting refreshed.
    pub codebase: Option<String>,
    /// When the publisher last persisted forge state for this MP.
    /// Used by `check_existing_mp` to skip the expensive per-MP
    /// forge fetches when nothing has changed since the last scan.
    pub last_scanned: Option<DateTime<Utc>>,
}

/// Manager for handling merge proposal information.
pub struct ProposalInfoManager {
    conn: PgPool,
    redis: Option<redis::aio::ConnectionManager>,
}

impl ProposalInfoManager {
    /// Create a new proposal info manager.
    ///
    /// # Arguments
    /// * `conn` - Database connection pool
    /// * `redis` - Optional Redis connection manager
    ///
    /// # Returns
    /// A new ProposalInfoManager instance
    pub async fn new(conn: PgPool, redis: Option<redis::aio::ConnectionManager>) -> Self {
        Self { conn, redis }
    }

    /// Retrieve proposal info URLs that haven't been scanned in the
    /// given duration.
    pub async fn iter_outdated_proposal_info_urls(
        &self,
        duration: chrono::Duration,
    ) -> Result<Vec<url::Url>, sqlx::Error> {
        // Bind the interval as a sqlx::postgres::types::PgInterval
        // rather than format!()-ing it into the SQL string. Even
        // though duration.num_days() is an i64 (so injection isn't
        // strictly possible) the format!() pattern is the wrong
        // shape and trips lints / future careless edits.
        let interval = sqlx::postgres::types::PgInterval {
            months: 0,
            days: duration.num_days() as i32,
            microseconds: 0,
        };
        let urls: Vec<String> = sqlx::query_scalar(
            "SELECT url FROM merge_proposal \
             WHERE last_scanned IS NULL OR now() - last_scanned > $1",
        )
        .bind(interval)
        .fetch_all(&self.conn)
        .await?;

        urls.iter()
            .map(|url| url.parse::<url::Url>())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| sqlx::Error::Protocol(format!("Invalid URL in database: {}", e)))
    }

    /// Retrieve proposal information for a given URL.
    ///
    /// # Arguments
    /// * `url` - The URL of the merge proposal
    ///
    /// # Returns
    /// Proposal info if found, or None
    pub async fn get_proposal_info(
        &self,
        url: &url::Url,
    ) -> Result<Option<ProposalInfo>, sqlx::Error> {
        // `merge_proposal.status` is a Postgres ENUM
        // (`merge_proposal_status`). sqlx won't decode that into
        // `String` without either a typed Rust enum wrapper or an
        // explicit `::text` cast in the SELECT - and `row.get::<String,
        // _>("status")` will otherwise panic with a ColumnDecode
        // mismatched-types error at runtime. Cast here; the callers
        // just string-compare the status anyway.
        let row = sqlx::query(
            r#"SELECT
                merge_proposal.rate_limit_bucket,
                merge_proposal.revision,
                merge_proposal.status::text AS status,
                merge_proposal.target_branch_url,
                merge_proposal.codebase,
                merge_proposal.can_be_merged,
                merge_proposal.last_scanned
            FROM merge_proposal
            WHERE merge_proposal.url = $1"#,
        )
        .bind(url.to_string())
        .fetch_optional(&self.conn)
        .await?;

        if let Some(row) = row {
            Ok(Some(ProposalInfo {
                rate_limit_bucket: row.try_get("rate_limit_bucket").ok(),
                revision: row.try_get("revision").ok(),
                status: row.try_get("status")?,
                target_branch_url: row.try_get("target_branch_url").ok(),
                can_be_merged: row.try_get("can_be_merged").ok(),
                codebase: row.try_get("codebase")?,
                last_scanned: row.try_get("last_scanned").ok(),
            }))
        } else {
            Ok(None)
        }
    }

    /// Delete a merge-proposal row by URL:
    /// `DELETE FROM merge_proposal WHERE url = $1`.
    pub async fn delete_proposal_info(&self, url: &url::Url) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM merge_proposal WHERE url = $1")
            .bind(url.to_string())
            .execute(&self.conn)
            .await?;
        Ok(())
    }

    /// Update the canonical URL for a proposal.
    pub async fn update_canonical_url(
        &self,
        old_url: &url::Url,
        canonical_url: &url::Url,
    ) -> Result<(), sqlx::Error> {
        let old_url_from_db: Option<String> = sqlx::query_scalar(
            "UPDATE merge_proposal canonical SET codebase = COALESCE(canonical.codebase, old.codebase), rate_limit_bucket = COALESCE(canonical.rate_limit_bucket, old.rate_limit_bucket) FROM merge_proposal old WHERE old.url = $1 AND canonical.url = $2 RETURNING old.url").bind(old_url.to_string()).bind(canonical_url.to_string()).fetch_optional(&self.conn).await?;
        sqlx::query("UPDATE publish SET merge_proposal_url = $1 WHERE merge_proposal_url = $2")
            .bind(canonical_url.to_string())
            .bind(old_url.to_string())
            .execute(&self.conn)
            .await?;

        if let Some(old_url_str) = old_url_from_db.as_ref() {
            // The canonical URL row already existed; drop the stale
            // alias row. Python publish.py:2385 does the same DELETE.
            // (The previous implementation tried to tombstone with
            // status='redirected', which isn't in the
            // merge_proposal_status enum - 'open','closed','merged',
            // 'applied','abandoned','rejected' - and would have raised
            // at runtime.)
            sqlx::query("DELETE FROM merge_proposal WHERE url = $1")
                .bind(old_url_str)
                .execute(&self.conn)
                .await?;
        } else {
            sqlx::query("UPDATE merge_proposal SET url = $1 WHERE url = $2")
                .bind(canonical_url.to_string())
                .bind(old_url.to_string())
                .execute(&self.conn)
                .await?;
        }
        Ok(())
    }

    /// Update proposal information in the database.
    ///
    /// # Arguments
    /// * `mp` - The merge proposal
    /// * `status` - Current status of the proposal
    /// * `revision` - Source revision ID
    /// * `codebase` - Codebase name
    /// * `target_branch_url` - Target branch URL
    /// * `campaign` - Campaign name
    /// * `can_be_merged` - Whether the proposal can be merged
    /// * `rate_limit_bucket` - Rate limit bucket
    ///
    /// # Returns
    /// Ok(()) if successful, or a sqlx::Error
    pub async fn update_proposal_info(
        &mut self,
        mp: &MergeProposal,
        status: MergeProposalStatus,
        revision: Option<&RevisionId>,
        codebase: Option<&str>,
        target_branch_url: &url::Url,
        campaign: &str,
        can_be_merged: Option<bool>,
        rate_limit_bucket: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        // Write the forge-supplied status verbatim. Don't try to
        // reclassify Closed to Merged based on other MPs with the same
        // revision: the extra SELECTs leak invented rows into the DB
        // and break rate-limit bucket counts and change_set
        // done-detection.
        //
        // TODO(jelmer): check if changes were applied manually and
        // mark as applied rather than closed.
        let effective_status = status;
        let url = match mp.url() {
            Ok(url) => url,
            Err(e) => {
                log::error!("Failed to get merge proposal URL: {}", e);
                return Err(sqlx::Error::RowNotFound);
            }
        };
        let (merged_by, merged_by_url, merged_at) = if effective_status
            == MergeProposalStatus::Merged
        {
            let mp = mp.clone();
            tokio::task::spawn_blocking(move || {
                let merged_by = match mp.get_merged_by() {
                    Ok(merged_by) => merged_by,
                    Err(e) => {
                        log::error!("Failed to get merged_by from merge proposal: {}", e);
                        None
                    }
                };
                let merged_by_url = if let Some(mb) = merged_by.clone().as_ref() {
                    match mp.url() {
                        Ok(mp_url) => match crate::get_merged_by_user_url(&mp_url, mb) {
                            Ok(url) => url,
                            Err(e) => {
                                log::error!("Failed to get merged_by user URL: {}", e);
                                None
                            }
                        },
                        Err(e) => {
                            log::error!("Failed to get merge proposal URL for merged_by: {}", e);
                            None
                        }
                    }
                } else {
                    None
                };
                let merged_at = match mp.get_merged_at() {
                    Ok(merged_at) => merged_at,
                    Err(e) => {
                        log::error!("Failed to get merged_at from merge proposal: {}", e);
                        None
                    }
                };
                (merged_by, merged_by_url, merged_at)
            })
            .await
            .unwrap()
        } else {
            (None, None, None)
        };
        let mut tx = self.conn.begin().await?;

        // `merge_proposal.status` is the `merge_proposal_status`
        // enum (schema/state.sql). sqlx binds Rust `String` as TEXT
        // and Postgres won't auto-cast TEXT -> enum - without an
        // explicit `::merge_proposal_status` cast every
        // check_existing_mp ingest fails with "column "status" is of
        // type merge_proposal_status but expression is of type text".
        sqlx::query(sqlx::AssertSqlSafe(
            &*r###"INSERT INTO merge_proposal (
                    url, status, revision, merged_by, merged_at,
                    target_branch_url, last_scanned, can_be_merged, rate_limit_bucket,
                    codebase)
                VALUES ($1, $2::merge_proposal_status, $3, $4, $5, $6, NOW(), $7, $8, $9)
                ON CONFLICT (url)
                DO UPDATE SET
                  status = EXCLUDED.status,
                  revision = EXCLUDED.revision,
                  merged_by = EXCLUDED.merged_by,
                  merged_at = EXCLUDED.merged_at,
                  target_branch_url = EXCLUDED.target_branch_url,
                  last_scanned = EXCLUDED.last_scanned,
                  can_be_merged = EXCLUDED.can_be_merged,
                  rate_limit_bucket = EXCLUDED.rate_limit_bucket,
                  codebase = EXCLUDED.codebase
                "###,
        ))
        .bind(url.to_string())
        .bind(effective_status.to_string())
        .bind(revision)
        .bind(merged_by.clone())
        .bind(merged_at)
        .bind(target_branch_url.to_string())
        .bind(can_be_merged)
        .bind(rate_limit_bucket)
        .bind(codebase)
        .execute(&mut *tx)
        .await?;
        if let Some(revision) = revision.as_ref() {
            sqlx::query(r#"UPDATE new_result_branch SET absorbed = $1 WHERE revision = $2"#)
                .bind(effective_status == MergeProposalStatus::Merged)
                .bind(revision)
                .execute(&mut *tx)
                .await?;
        }

        tx.commit().await?;

        // The change_set `state` column is driven by the
        // `new_result_branch_trigger_refresh_change_set_state` trigger
        // (schema/state.sql:306-335), which calls
        // `refresh_change_set_state` and transitions to 'done' once all
        // branches are absorbed. The UPDATE above on `new_result_branch`
        // fires that trigger, so there is nothing to do manually here -
        // Python publish.py does not touch change_set state either.
        // (A previous version of this block wrote
        // `change_set.state = 'published'`, which isn't in the
        // change_set_state enum ('created','working','ready',
        // 'publishing','done') and would have raised at runtime.)

        if let Some(redis) = self.redis.as_mut() {
            redis
                .publish::<_, _, i32>(
                    "merge-proposal",
                    serde_json::to_string(&serde_json::json!({
                        "url": url,
                        "target_branch_url": target_branch_url,
                        "rate_limit_bucket": rate_limit_bucket,
                        "status": effective_status,
                        "codebase": codebase,
                        "merged_by": merged_by,
                        "merged_by_url": merged_by_url,
                        "merged_at": merged_at,
                        "campaign": campaign,
                    }))
                    .unwrap(),
                )
                .await
                .unwrap();
        }
        Ok(())
    }

    /// Check if a proposal exists in the database.
    ///
    /// # Arguments
    /// * `url` - The URL of the merge proposal
    ///
    /// # Returns
    /// True if the proposal exists, false otherwise
    pub async fn proposal_exists(&self, url: &url::Url) -> Result<bool, sqlx::Error> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM merge_proposal WHERE url = $1")
            .bind(url.to_string())
            .fetch_one(&self.conn)
            .await?;

        Ok(count > 0)
    }

    /// Get all proposals for a specific codebase.
    ///
    /// # Arguments
    /// * `codebase` - The codebase name
    /// * `status_filter` - Optional status filter
    ///
    /// # Returns
    /// List of proposal info for the codebase
    pub async fn get_proposals_for_codebase(
        &self,
        codebase: &str,
        status_filter: Option<&str>,
    ) -> Result<Vec<ProposalInfo>, sqlx::Error> {
        const BASE: &str = "SELECT rate_limit_bucket, revision, status, target_branch_url, \
                            codebase, can_be_merged, last_scanned \
                            FROM merge_proposal WHERE codebase = $1";

        if let Some(status) = status_filter {
            let sql = format!("{} AND status = $2", BASE);
            sqlx::query_as::<_, ProposalInfo>(sqlx::AssertSqlSafe(&*sql))
                .bind(codebase)
                .bind(status)
                .fetch_all(&self.conn)
                .await
        } else {
            sqlx::query_as::<_, ProposalInfo>(sqlx::AssertSqlSafe(BASE))
                .bind(codebase)
                .fetch_all(&self.conn)
                .await
        }
    }

    /// Update the last scanned timestamp for a proposal.
    ///
    /// # Arguments
    /// * `url` - The URL of the merge proposal
    ///
    /// # Returns
    /// Ok(()) if successful, or a sqlx::Error
    pub async fn touch_proposal(&self, url: &url::Url) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE merge_proposal SET last_scanned = NOW() WHERE url = $1")
            .bind(url.to_string())
            .execute(&self.conn)
            .await?;
        Ok(())
    }

    /// Get statistics about proposals in the database.
    ///
    /// # Returns
    /// A map of status -> count
    pub async fn get_proposal_statistics(
        &self,
    ) -> Result<std::collections::HashMap<String, i64>, sqlx::Error> {
        let rows =
            sqlx::query("SELECT status, COUNT(*) as count FROM merge_proposal GROUP BY status")
                .fetch_all(&self.conn)
                .await?;

        let mut stats = std::collections::HashMap::new();
        for row in rows {
            let status: String = row.get("status");
            let count: i64 = row.get("count");
            stats.insert(status, count);
        }

        Ok(stats)
    }

    /// Clean up old closed proposals.
    /// Now uses tombstone approach - marks very old proposals as archived instead of deleting.
    ///
    /// # Arguments
    /// * `days_old` - Archive proposals closed more than this many days ago
    ///
    /// # Returns
    /// Number of proposals archived
    pub async fn cleanup_old_proposals(&self, days_old: i32) -> Result<u64, sqlx::Error> {
        // Use tombstone approach - mark as archived instead of deleting
        // Only archive proposals that are already closed/merged and very old
        let result = sqlx::query(
            "UPDATE merge_proposal SET status = 'archived' WHERE status IN ('closed', 'merged') AND last_scanned < NOW() - INTERVAL '$1 days' AND status != 'archived'"
        )
        .bind(days_old)
        .execute(&self.conn)
        .await?;

        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_proposal_info_manager_creation() {
        // This would need a test database connection
        // let manager = ProposalInfoManager::new(pool, None).await;
        // assert!(manager.redis.is_none());
    }

    #[test]
    fn test_proposal_info_serialization() {
        let info = ProposalInfo {
            can_be_merged: Some(true),
            status: "open".to_string(),
            revision: Some("abc123".to_string()),
            target_branch_url: Some("https://github.com/test/repo".to_string()),
            rate_limit_bucket: Some("default".to_string()),
            codebase: Some("test-codebase".to_string()),
            last_scanned: None,
        };

        assert_eq!(info.status, "open");
        assert_eq!(info.codebase.as_deref(), Some("test-codebase"));
        assert!(info.can_be_merged.unwrap());
    }

    /// Regression: `merge_proposal.codebase` is `text references
    /// codebase(name) on delete set null`, so rows discovered by
    /// forge enumeration before they're matched to a candidate (and
    /// rows whose codebase later gets dropped) carry NULL there.
    /// Decoding it as `String` panicked every `check_existing_mp`
    /// call on such rows, which is what stalled the dashboard at
    /// the same handful of proposals while the publisher quietly
    /// failed to refresh thousands more. Confirm the SELECT now
    /// produces a `ProposalInfo` whose `codebase` is `None` instead
    /// of erroring.
    #[tokio::test]
    async fn test_get_proposal_info_handles_null_codebase() {
        use janitor::schema::setup_test_database;
        use janitor::test_utils::TestDatabase;

        let Ok(Some(db)) = TestDatabase::new_optional().await else {
            eprintln!("Skipping: no Postgres available");
            return;
        };
        if let Err(e) = setup_test_database(&db.pool).await {
            eprintln!("Skipping: schema setup failed: {}", e);
            return;
        }

        let url = "https://github.com/example/repo/pull/1";
        sqlx::query(
            "INSERT INTO merge_proposal (url, status, codebase, last_scanned) \
             VALUES ($1, 'open', NULL, NOW())",
        )
        .bind(url)
        .execute(&db.pool)
        .await
        .unwrap();

        let mgr = ProposalInfoManager::new(db.pool.clone(), None).await;
        let parsed = url::Url::parse(url).unwrap();
        let info = mgr
            .get_proposal_info(&parsed)
            .await
            .expect("NULL codebase must decode cleanly")
            .expect("row was just inserted");

        assert_eq!(info.status, "open");
        assert!(
            info.codebase.is_none(),
            "expected codebase=None on a NULL row, got {:?}",
            info.codebase
        );
    }
}
