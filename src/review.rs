//! Review management functionality for the Janitor platform

use crate::api::runner::UpdateRunRequest;
use crate::api::RunPublishStatus;
use crate::error::JanitorError;
use crate::schedule::do_schedule;
use crate::{Campaign, CodebaseName, RunId};
use chrono::{DateTime, Utc};
use log::{error, info};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use url::Url;

/// Represents a review verdict
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReviewVerdict {
    /// The change is approved
    Approved,
    /// The change is rejected
    Rejected,
    /// The reviewer abstained from making a decision
    Abstained,
    /// The change should be rescheduled (treated as rejected but triggers reschedule)
    Reschedule,
}

impl ReviewVerdict {
    /// Convert to string representation for database storage
    pub fn as_str(&self) -> &'static str {
        match self {
            ReviewVerdict::Approved => "approved",
            ReviewVerdict::Rejected => "rejected",
            ReviewVerdict::Abstained => "abstained",
            ReviewVerdict::Reschedule => "rejected", // Stored as rejected in DB
        }
    }
}

/// Represents a review in the database
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Review {
    /// The run ID this review is for
    pub run_id: RunId,
    /// Optional comment from the reviewer
    pub comment: Option<String>,
    /// The reviewer's identifier (username or email)
    pub reviewer: String,
    /// The verdict of the review
    pub verdict: String,
    /// When the review was submitted
    pub reviewed_at: DateTime<Utc>,
}

/// Store a review for a run
///
/// # Arguments
/// * `conn` - Database connection pool
/// * `http_client` - HTTP client for API calls
/// * `runner_url` - Base URL for the runner service
/// * `run_id` - The ID of the run being reviewed
/// * `verdict` - The review verdict
/// * `comment` - Optional comment from the reviewer
/// * `reviewer` - The reviewer's identifier
/// * `is_qa_reviewer` - Whether the reviewer has QA privileges
pub async fn store_review(
    conn: &PgPool,
    http_client: &Client,
    runner_url: &Url,
    run_id: &RunId,
    verdict: ReviewVerdict,
    comment: Option<&str>,
    reviewer: &str,
    is_qa_reviewer: bool,
) -> Result<(), JanitorError> {
    // Start a transaction
    let mut tx = conn.begin().await?;

    // Handle reschedule verdict
    let actual_verdict = if verdict == ReviewVerdict::Reschedule {
        // Get run information for rescheduling
        #[derive(sqlx::FromRow)]
        struct RunInfo {
            suite: Campaign,
            codebase: CodebaseName,
        }

        let run = sqlx::query_as::<_, RunInfo>("SELECT suite, codebase FROM run WHERE id = $1")
            .bind(run_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| JanitorError::Database(e))?;

        // Reschedule the run
        do_schedule(
            conn,
            &run.suite,                                // campaign
            &run.codebase,                             // codebase
            "default",                                 // bucket
            None,                                      // change_set
            None,                                      // offset
            true,                                      // refresh
            Some(&format!("reviewer ({})", reviewer)), // requester
            None,                                      // estimated_duration
            None,                                      // command
        )
        .await
        .map_err(|e| JanitorError::Internal(format!("Failed to reschedule run: {}", e)))?;

        info!("Rescheduled run {} for codebase {}", run_id, run.codebase);

        ReviewVerdict::Rejected
    } else {
        verdict.clone()
    };

    // Update publish status if reviewer has QA privileges and didn't abstain
    if actual_verdict != ReviewVerdict::Abstained && is_qa_reviewer {
        let publish_status = match &actual_verdict {
            ReviewVerdict::Approved => RunPublishStatus::Approved,
            ReviewVerdict::Rejected => RunPublishStatus::Rejected,
            _ => unreachable!(),
        };

        // Call runner API to update publish status
        let response = http_client
            .post(runner_url.join(&format!("runs/{}", run_id))?)
            .json(&UpdateRunRequest {
                publish_status: publish_status.clone(),
            })
            .send()
            .await
            .map_err(|e| JanitorError::Http(e))?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            error!(
                "Failed to update publish status for run {}: {} - {}",
                run_id, status, error_text
            );
            return Err(JanitorError::Internal(format!(
                "Runner API returned error: {} - {}",
                status, error_text
            )));
        }

        info!(
            "Updated publish status for run {} to {:?}",
            run_id, publish_status
        );
    }

    // Insert or update the review
    sqlx::query(
        r#"
        INSERT INTO review (run_id, comment, reviewer, verdict)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (run_id, reviewer)
        DO UPDATE SET
            verdict = EXCLUDED.verdict,
            comment = EXCLUDED.comment,
            reviewed_at = NOW()
        "#,
    )
    .bind(run_id)
    .bind(comment)
    .bind(reviewer)
    .bind(actual_verdict.as_str())
    .execute(&mut *tx)
    .await
    .map_err(|e| JanitorError::Database(e))?;

    // Commit the transaction
    tx.commit().await?;

    info!(
        "Stored review for run {} by {} with verdict {:?}",
        run_id, reviewer, actual_verdict
    );

    Ok(())
}

/// Get all reviews for a run
pub async fn get_reviews_for_run(
    conn: &PgPool,
    run_id: &RunId,
) -> Result<Vec<Review>, JanitorError> {
    let reviews = sqlx::query_as::<_, Review>(
        r#"
        SELECT
            run_id,
            comment,
            reviewer,
            verdict,
            reviewed_at
        FROM review
        WHERE run_id = $1
        ORDER BY reviewed_at DESC
        "#,
    )
    .bind(run_id)
    .fetch_all(conn)
    .await
    .map_err(|e| JanitorError::Database(e))?;

    Ok(reviews)
}

/// Get review statistics for a campaign
#[derive(Debug, Serialize)]
pub struct ReviewStats {
    pub total_reviews: i64,
    pub approved: i64,
    pub rejected: i64,
    pub abstained: i64,
    pub unique_reviewers: i64,
}

pub async fn get_review_stats_for_campaign(
    conn: &PgPool,
    campaign: &Campaign,
) -> Result<ReviewStats, JanitorError> {
    #[derive(sqlx::FromRow)]
    struct StatsRow {
        total_reviews: Option<i64>,
        unique_reviewers: Option<i64>,
        approved: Option<i64>,
        rejected: Option<i64>,
        abstained: Option<i64>,
    }

    let stats = sqlx::query_as::<_, StatsRow>(
        r#"
        SELECT
            COUNT(*) as total_reviews,
            COUNT(DISTINCT reviewer) as unique_reviewers,
            COUNT(*) FILTER (WHERE verdict = 'approved') as approved,
            COUNT(*) FILTER (WHERE verdict = 'rejected') as rejected,
            COUNT(*) FILTER (WHERE verdict = 'abstained') as abstained
        FROM review r
        INNER JOIN run ON run.id = r.run_id
        WHERE run.suite = $1
        "#,
    )
    .bind(campaign)
    .fetch_one(conn)
    .await
    .map_err(|e| JanitorError::Database(e))?;

    Ok(ReviewStats {
        total_reviews: stats.total_reviews.unwrap_or(0),
        approved: stats.approved.unwrap_or(0),
        rejected: stats.rejected.unwrap_or(0),
        abstained: stats.abstained.unwrap_or(0),
        unique_reviewers: stats.unique_reviewers.unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_review_verdict_conversion() {
        assert_eq!(ReviewVerdict::Approved.as_str(), "approved");
        assert_eq!(ReviewVerdict::Rejected.as_str(), "rejected");
        assert_eq!(ReviewVerdict::Abstained.as_str(), "abstained");
        assert_eq!(ReviewVerdict::Reschedule.as_str(), "rejected");
    }

    #[test]
    fn test_review_verdict_serialization() {
        let verdict = ReviewVerdict::Approved;
        let json = serde_json::to_string(&verdict).unwrap();
        assert_eq!(json, "\"approved\"");

        let deserialized: ReviewVerdict = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, verdict);
    }
}
