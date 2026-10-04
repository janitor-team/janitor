//! Redis integration for publish service.
//!
//! This module provides Redis pub/sub functionality for communicating with
//! other services, particularly the runner service.

use std::sync::Arc;

use janitor::redis::{PubSubMessage, RedisManager};
use log::{debug, error, info};
use redis::aio::ConnectionManager;
use sqlx::Row;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

// Type alias for backward compatibility
/// Type alias for Redis connection manager used throughout the publish service.
pub type RedisConnectionManager = ConnectionManager;

/// Redis publisher for sending messages to other services.
pub struct RedisPublisher {
    /// Redis manager.
    redis_manager: Arc<RedisManager>,
}

impl RedisPublisher {
    /// Create a new Redis publisher.
    ///
    /// # Arguments
    /// * `redis_manager` - Redis manager instance
    ///
    /// # Returns
    /// A new RedisPublisher instance
    pub fn new(redis_manager: Arc<RedisManager>) -> Self {
        Self { redis_manager }
    }

    /// Publish a publish event.
    ///
    /// # Arguments
    /// * `event` - The publish event data
    ///
    /// # Returns
    /// Ok(()) if successful, or a redis::RedisError
    pub async fn publish_event(&self, event: &PublishEvent) -> Result<(), redis::RedisError> {
        let publisher = self.redis_manager.publisher();
        publisher.publish(event).await.map_err(|e| {
            redis::RedisError::from((
                redis::ErrorKind::Io,
                "Failed to publish event",
                e.to_string(),
            ))
        })?;
        debug!(
            "Published publish event for codebase '{}' campaign '{}'",
            event.codebase, event.campaign
        );
        Ok(())
    }

    /// Publish a merge proposal event.
    ///
    /// # Arguments
    /// * `event` - The merge proposal event data
    ///
    /// # Returns
    /// Ok(()) if successful, or a redis::RedisError
    pub async fn publish_merge_proposal(
        &self,
        event: &MergeProposalEvent,
    ) -> Result<(), redis::RedisError> {
        let publisher = self.redis_manager.publisher();
        publisher.publish(event).await.map_err(|e| {
            redis::RedisError::from((
                redis::ErrorKind::Io,
                "Failed to publish merge proposal event",
                e.to_string(),
            ))
        })?;
        debug!("Published merge proposal event for URL '{}'", event.url);
        Ok(())
    }
}

/// Event data for publish notifications.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PublishEvent {
    /// The codebase that was published.
    pub codebase: String,
    /// The campaign/suite that was published.
    pub campaign: String,
    /// The publish mode used.
    pub mode: String,
    /// The result code of the publish operation.
    pub result_code: String,
    /// Optional description of the result.
    pub description: Option<String>,
    /// Optional URL of the merge proposal created.
    pub proposal_url: Option<String>,
    /// Optional web URL of the merge proposal.
    pub proposal_web_url: Option<String>,
    /// The target branch URL.
    pub target_branch_url: Option<String>,
    /// The branch name that was published.
    pub branch_name: String,
    /// The revision that was published.
    pub revision: Option<String>,
    /// Timestamp of the event.
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl PubSubMessage for PublishEvent {
    fn channel() -> &'static str {
        "publish"
    }
}

/// Event data for merge proposal notifications.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MergeProposalEvent {
    /// URL of the merge proposal.
    pub url: String,
    /// Web URL of the merge proposal.
    pub web_url: Option<String>,
    /// Status of the merge proposal.
    pub status: String,
    /// The codebase this proposal belongs to.
    pub codebase: String,
    /// The campaign/suite this proposal belongs to.
    pub campaign: String,
    /// The target branch URL.
    pub target_branch_url: String,
    /// Optional target branch web URL.
    pub target_branch_web_url: Option<String>,
    /// Timestamp of the event.
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl PubSubMessage for MergeProposalEvent {
    fn channel() -> &'static str {
        "merge-proposal"
    }
}

/// Message broadcast by the runner on the `publish-status` channel
/// when a run's `publish_status` column is updated via the runner's
/// `POST /runs/{run_id}` endpoint. Shape matches what
/// `py/janitor/runner.py::handle_update_run` emits, and is what
/// `py/janitor/publish.py::listen_to_runner` consumes.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PublishStatusMessage {
    /// Run id the status applies to.
    pub run_id: String,
    /// New publish_status value. The publisher acts only on `"approved"`.
    pub publish_status: String,
    /// Codebase the run belongs to.
    pub codebase: String,
    /// Campaign (run.suite) the run belongs to.
    pub campaign: String,
}

impl PubSubMessage for PublishStatusMessage {
    fn channel() -> &'static str {
        "publish-status"
    }
}

/// Redis subscriber for receiving messages from other services.
pub struct RedisSubscriber {
    /// Redis manager.
    redis_manager: Arc<RedisManager>,
    /// Channel for receiving shutdown signals.
    shutdown_rx: Option<mpsc::Receiver<()>>,
}

impl RedisSubscriber {
    /// Create a new Redis subscriber.
    ///
    /// # Arguments
    /// * `redis_manager` - Redis manager instance
    /// * `shutdown_rx` - Channel for receiving shutdown signals
    ///
    /// # Returns
    /// A new RedisSubscriber instance
    pub fn new(redis_manager: Arc<RedisManager>, shutdown_rx: mpsc::Receiver<()>) -> Self {
        Self {
            redis_manager,
            shutdown_rx: Some(shutdown_rx),
        }
    }

    /// Subscribe to the `publish-status` channel and publish a run
    /// whenever the runner marks it as `approved`. Port of
    /// `py/janitor/publish.py::listen_to_runner`.
    pub async fn listen_to_runner(
        mut self,
        state: Arc<crate::AppState>,
    ) -> Result<JoinHandle<()>, Box<dyn std::error::Error + Send + Sync>> {
        info!("Starting Redis listener on publish-status channel");

        let subscriber = self.redis_manager.subscriber();
        let _shutdown_rx = self.shutdown_rx.take();

        let handle = tokio::spawn(async move {
            match subscriber
                .subscribe::<PublishStatusMessage, _, _>(move |message| {
                    let state = state.clone();
                    async move { Self::handle_publish_status_message(&state, message).await }
                })
                .await
            {
                Ok(_) => info!("publish-status subscription ended normally"),
                Err(e) => error!("publish-status subscription error: {}", e),
            }
        });

        Ok(handle)
    }

    /// Dispatch a single `publish-status` message. Only
    /// `publish_status == "approved"` triggers work; anything else
    /// is dropped silently, matching the Python early return.
    async fn handle_publish_status_message(
        state: &Arc<crate::AppState>,
        message: PublishStatusMessage,
    ) -> Result<(), janitor::error::JanitorError> {
        debug!("Received publish-status message: {:?}", message);

        if message.publish_status != "approved" {
            return Ok(());
        }

        if let Err(e) = Self::process_approved_run(state, &message).await {
            error!("Error processing approved run {}: {}", message.run_id, e);
            return Err(janitor::error::JanitorError::external_service(
                "publish",
                format!("Failed to process approved run {}: {}", message.run_id, e),
            ));
        }

        Ok(())
    }

    /// Fetch the run, campaign config, and publish policy, then call
    /// `publish_from_policy` for each role in the policy.
    async fn process_approved_run(
        state: &Arc<crate::AppState>,
        message: &PublishStatusMessage,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Same gate as the periodic publish loop: don't create proposals
        // until a full merge-proposal scan has refreshed the bucket
        // counts, or the rate limiter would act on an incomplete view of
        // what we already own and could over-create. Skipping here is
        // safe - the run stays approved and publish-ready, so the
        // periodic `publish_pending_ready` loop publishes it once the
        // scan gate opens.
        if !crate::queue::scan_ts_is_recent(*state.last_full_scan_at.borrow()) {
            log::info!(
                "Deferring publish of approved run {} until a merge-proposal scan completes; \
                 the periodic publish loop will pick it up",
                message.run_id
            );
            return Ok(());
        }

        // Look up the codebase's branch_url - used as the
        // target_branch_url that publish_from_policy feeds into
        // role_branch_url. If the codebase row is gone, log and bail
        // instead of crashing.
        let branch_url: Option<String> =
            sqlx::query_scalar("SELECT branch_url FROM codebase WHERE name = $1")
                .bind(&message.codebase)
                .fetch_optional(&state.conn)
                .await?;
        let branch_url = match branch_url {
            Some(u) => u,
            None => {
                log::warn!(
                    "Codebase {} not in database when handling publish-status for run {}",
                    message.codebase,
                    message.run_id
                );
                return Ok(());
            }
        };
        let target_branch_url: url::Url = match branch_url.parse() {
            Ok(u) => u,
            Err(e) => {
                log::warn!(
                    "Codebase {}: invalid branch_url {:?}: {}",
                    message.codebase,
                    branch_url,
                    e
                );
                return Ok(());
            }
        };

        // Fetch the full run + result branches so publish_from_policy
        // can resolve role -> (remote_name, base_revision, revision).
        let run = match crate::state::get_run(&state.conn, &message.run_id).await? {
            Some(r) => r,
            None => {
                log::warn!("Run {} not found for publish-status", message.run_id);
                return Ok(());
            }
        };

        // Campaign config from the static config loaded at startup.
        let campaign_config = match state.config.campaign.iter().find(|c| c.name() == run.suite) {
            Some(c) => c,
            None => {
                log::warn!(
                    "No campaign config for suite {} when handling publish-status",
                    run.suite
                );
                return Ok(());
            }
        };

        // Publish policy = (role -> (mode, max_frequency_days), command, rate_limit_bucket).
        let (policy_map, command, rate_limit_bucket) =
            match crate::get_publish_policy(&state.conn, &run.codebase, &run.suite).await? {
                Some(p) => p,
                None => {
                    log::warn!(
                        "No publish policy for {}/{}, skipping",
                        run.codebase,
                        run.suite
                    );
                    return Ok(());
                }
            };
        let command = command.unwrap_or_default();

        for (role, (mode_str, max_frequency_days)) in policy_map.iter() {
            let mode = match <crate::Mode as std::str::FromStr>::from_str(mode_str) {
                Ok(m) => m,
                Err(e) => {
                    log::warn!(
                        "Unknown mode {:?} in publish policy for {}/{}/{}: {}",
                        mode_str,
                        run.codebase,
                        run.suite,
                        role,
                        e
                    );
                    continue;
                }
            };
            if let Err(e) = crate::publish_from_policy(
                &state.conn,
                state.redis_manager.as_ref(),
                campaign_config,
                &state.publish_worker,
                &state.bucket_rate_limiter,
                &state.vcs_managers,
                &run,
                role,
                rate_limit_bucket.as_deref(),
                &target_branch_url,
                mode,
                *max_frequency_days,
                &command,
                state.require_binary_diff,
                true, // force: runner-approved path bypasses already_published
                Some("runner"),
            )
            .await
            {
                log::warn!(
                    "publish_from_policy failed for {}/{}/{}: {}",
                    run.codebase,
                    run.suite,
                    role,
                    e
                );
            }
        }

        Ok(())
    }
}

/// Health check functionality for Redis connections.
pub struct RedisHealthChecker {
    /// Redis manager.
    redis_manager: Arc<RedisManager>,
}

impl RedisHealthChecker {
    /// Create a new Redis health checker.
    pub fn new(redis_manager: Arc<RedisManager>) -> Self {
        Self { redis_manager }
    }

    /// Perform a health check on the Redis connection.
    ///
    /// # Returns
    /// Ok(()) if healthy, or an error
    pub async fn health_check(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.redis_manager.health_check().await.map_err(|e| {
            Box::new(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                format!("Redis health check failed: {}", e),
            )) as Box<dyn std::error::Error + Send + Sync>
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_publish_event_channel() {
        assert_eq!(PublishEvent::channel(), "publish");
    }

    #[test]
    fn test_merge_proposal_event_channel() {
        assert_eq!(MergeProposalEvent::channel(), "merge-proposal");
    }

    #[test]
    fn test_publish_status_message_channel() {
        assert_eq!(PublishStatusMessage::channel(), "publish-status");
    }

    #[test]
    fn test_publish_status_message_serialization() {
        // Exact shape emitted by py/janitor/runner.py::handle_update_run.
        let json = r#"{
            "run_id": "run-1",
            "publish_status": "approved",
            "codebase": "cb-1",
            "campaign": "lintian-fixes"
        }"#;
        let message: PublishStatusMessage = serde_json::from_str(json).unwrap();
        assert_eq!(message.run_id, "run-1");
        assert_eq!(message.publish_status, "approved");
        assert_eq!(message.codebase, "cb-1");
        assert_eq!(message.campaign, "lintian-fixes");
    }
}
