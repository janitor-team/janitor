//! Subscribes to the runner's `result` pub/sub channel and triggers a
//! per-campaign republish when a debian build succeeds.

use std::sync::Arc;

use janitor::redis::{PubSubMessage, RedisConfig, RedisManager};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, error, info};

use crate::error::{ArchiveError, ArchiveResult};
use crate::manager::GeneratorManager;

/// Payloads published on the `"archive:events"` channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[allow(missing_docs)]
pub enum ArchiveEvent {
    BuildCompleted {
        run_id: String,
        codebase: String,
        campaign: String,
        artifacts: Vec<String>,
    },
    CampaignFinished {
        campaign: String,
        successful_runs: u32,
        total_runs: u32,
    },
    ManualRegeneration {
        repository: String,
        campaign: Option<String>,
        requested_by: Option<String>,
    },
    PeriodicRepublish {
        campaign: String,
        interval_type: String,
    },
}

impl PubSubMessage for ArchiveEvent {
    fn channel() -> &'static str {
        "archive:events"
    }
}

/// Parse a runner `result` channel payload and return the campaign
/// name if the message represents a successful Debian build.
///
/// Returns `None` for malformed JSON, non-success codes, non-debian
/// targets, or messages missing the campaign field.
pub fn extract_debian_success_campaign(payload: &str) -> Option<String> {
    let parsed: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(e) => {
            debug!("Ignoring malformed runner result JSON: {}", e);
            return None;
        }
    };

    if parsed.get("code").and_then(|v| v.as_str()) != Some("success") {
        return None;
    }

    let target_name = parsed
        .get("target")
        .and_then(|t| t.get("name"))
        .and_then(|n| n.as_str());
    if target_name != Some("debian") {
        debug!(
            "Ignoring non-debian build result (target: {:?})",
            target_name
        );
        return None;
    }

    parsed
        .get("campaign")
        .and_then(|c| c.as_str())
        .map(|s| s.to_string())
}

impl ArchiveEvent {
    /// Get the campaign name associated with this event.
    pub fn campaign(&self) -> Option<&str> {
        match self {
            ArchiveEvent::BuildCompleted { campaign, .. } => Some(campaign),
            ArchiveEvent::CampaignFinished { campaign, .. } => Some(campaign),
            ArchiveEvent::ManualRegeneration { campaign, .. } => campaign.as_deref(),
            ArchiveEvent::PeriodicRepublish { campaign, .. } => Some(campaign),
        }
    }

    /// Get the repository name if specified.
    pub fn repository(&self) -> Option<&str> {
        match self {
            ArchiveEvent::BuildCompleted { codebase, .. } => Some(codebase),
            ArchiveEvent::ManualRegeneration { repository, .. } => Some(repository),
            _ => None,
        }
    }
}

/// Subscribes to the runner's `result` channel and dispatches
/// successful debian builds to a [`GeneratorManager`].
pub struct RedisSubscriber {
    redis_manager: Arc<RedisManager>,
    generator_manager: Arc<GeneratorManager>,
    shutdown_rx: Option<mpsc::Receiver<()>>,
}

impl RedisSubscriber {
    /// Connect to Redis and perform a health-check ping. Returns
    /// once the connection is verified.
    pub async fn new(
        config: RedisConfig,
        generator_manager: Arc<GeneratorManager>,
    ) -> ArchiveResult<Self> {
        info!("Creating Redis subscriber with URL: {}", config.url);

        let redis_manager =
            Arc::new(RedisManager::new(config).map_err(|e| {
                ArchiveError::Redis(format!("Failed to create Redis manager: {}", e))
            })?);

        // Test connection with health check
        redis_manager
            .health_check()
            .await
            .map_err(|e| ArchiveError::Redis(format!("Redis health check failed: {}", e)))?;

        info!("Successfully connected to Redis");

        Ok(Self {
            redis_manager,
            generator_manager,
            shutdown_rx: None,
        })
    }

    /// Set shutdown channel for graceful shutdown.
    pub fn with_shutdown(mut self, shutdown_rx: mpsc::Receiver<()>) -> Self {
        self.shutdown_rx = Some(shutdown_rx);
        self
    }

    /// Start listening for archive events.
    pub async fn start_listening(&mut self) -> ArchiveResult<JoinHandle<()>> {
        info!("Starting Redis subscriber for archive events");

        let redis_manager = self.redis_manager.clone();
        let generator_manager = Arc::clone(&self.generator_manager);
        let _shutdown_rx = self.shutdown_rx.take();

        let handle = tokio::spawn(async move {
            // Use the RedisManager's built-in subscription with automatic retry
            let subscriber = redis_manager.subscriber();

            match subscriber
                .subscribe::<ArchiveEvent, _, _>(move |event| {
                    let generator_manager = generator_manager.clone();
                    async move { Self::handle_archive_event(&generator_manager, event).await }
                })
                .await
            {
                Ok(_) => info!("Redis subscriber exited normally"),
                Err(e) => error!("Redis subscriber error: {}", e),
            }
        });

        Ok(handle)
    }

    /// Listen to the runner's `result` pub/sub channel and trigger
    /// archive regeneration whenever a successful Debian build
    /// completes.
    ///
    /// The runner publishes `JanitorResult.json()` on the bare `result`
    /// channel. The JSON has at least these fields:
    /// ```text
    /// {
    ///   "code": "success" | "...",
    ///   "campaign": "...",
    ///   "target": { "name": "debian" | "generic", "details": {...} }
    /// }
    /// ```
    /// For every `code == "success"` debian build, we trigger
    /// `generator_manager.trigger_campaign(campaign)`.
    pub async fn listen_to_runner(&mut self) -> ArchiveResult<JoinHandle<()>> {
        info!("Starting runner 'result' pub/sub listener");

        let redis_manager = self.redis_manager.clone();
        let generator_manager = self.generator_manager.clone();

        let handle = tokio::spawn(async move {
            loop {
                let subscriber = redis_manager.subscriber();
                let gm = generator_manager.clone();
                let result = subscriber
                    .subscribe_to_channel("result", move |payload| {
                        let gm = gm.clone();
                        async move {
                            Self::handle_runner_result(&gm, &payload).await;
                            Ok(())
                        }
                    })
                    .await;
                match result {
                    Ok(()) => {
                        info!("Runner 'result' subscriber exited cleanly; restarting");
                    }
                    Err(e) => {
                        error!("Runner 'result' subscriber error: {}; retrying in 5s", e);
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                }
            }
        });

        Ok(handle)
    }

    /// Parse a raw `result` channel payload and trigger the archive
    /// regenerator if it's a successful Debian build.
    async fn handle_runner_result(generator_manager: &Arc<GeneratorManager>, payload: &str) {
        let Some(campaign) = extract_debian_success_campaign(payload) else {
            return;
        };

        info!(
            "Runner reported successful debian build for campaign '{}'; triggering regeneration",
            campaign
        );
        match generator_manager.trigger_campaign(&campaign).await {
            Ok(job_ids) => info!(
                "Triggered {} regeneration job(s) for campaign '{}'",
                job_ids.len(),
                campaign
            ),
            Err(e) => error!(
                "Failed to trigger regeneration for campaign '{}': {}",
                campaign, e
            ),
        }
    }

    /// Handle an archive event by triggering appropriate repository generation.
    async fn handle_archive_event(
        generator_manager: &Arc<GeneratorManager>,
        event: ArchiveEvent,
    ) -> Result<(), janitor::error::JanitorError> {
        match event {
            ArchiveEvent::BuildCompleted { campaign, .. } => {
                info!(
                    "Triggering repository generation for build completion in campaign: {}",
                    campaign
                );

                match generator_manager.trigger_campaign(&campaign).await {
                    Ok(job_ids) => {
                        info!("Triggered {} jobs for campaign {}", job_ids.len(), campaign);
                    }
                    Err(e) => {
                        error!("Failed to trigger campaign {}: {}", campaign, e);
                    }
                }
            }
            ArchiveEvent::CampaignFinished {
                campaign,
                successful_runs,
                total_runs,
            } => {
                info!(
                    "Campaign {} finished: {}/{} successful runs",
                    campaign, successful_runs, total_runs
                );

                if successful_runs > 0 {
                    match generator_manager.trigger_campaign(&campaign).await {
                        Ok(job_ids) => {
                            info!(
                                "Triggered {} jobs for finished campaign {}",
                                job_ids.len(),
                                campaign
                            );
                        }
                        Err(e) => {
                            error!("Failed to trigger finished campaign {}: {}", campaign, e);
                        }
                    }
                }
            }
            ArchiveEvent::ManualRegeneration {
                repository,
                campaign,
                requested_by,
            } => {
                info!(
                    "Manual regeneration requested for repository {} (campaign: {:?}, by: {:?})",
                    repository, campaign, requested_by
                );

                match campaign {
                    Some(ref campaign_name) => {
                        match generator_manager.trigger_campaign(campaign_name).await {
                            Ok(job_ids) => {
                                info!("Triggered {} jobs for manual request", job_ids.len());
                            }
                            Err(e) => {
                                error!(
                                    "Failed to trigger manual campaign {}: {}",
                                    campaign_name, e
                                );
                            }
                        }
                    }
                    None => {
                        // Trigger all repositories when no specific campaign is requested
                        match generator_manager.trigger_all_repositories().await {
                            Ok(job_ids) => {
                                info!(
                                    "Triggered {} total jobs for manual regeneration request",
                                    job_ids.len()
                                );
                            }
                            Err(e) => {
                                error!("Failed to trigger all repositories: {}", e);
                            }
                        }
                    }
                }
            }
            ArchiveEvent::PeriodicRepublish {
                campaign,
                interval_type,
            } => {
                info!(
                    "Periodic republish triggered for campaign {} (interval: {})",
                    campaign, interval_type
                );

                match generator_manager.trigger_campaign(&campaign).await {
                    Ok(job_ids) => {
                        info!("Triggered {} jobs for periodic republish", job_ids.len());
                    }
                    Err(e) => {
                        error!(
                            "Failed to trigger periodic republish for {}: {}",
                            campaign, e
                        );
                    }
                }
            }
        }

        Ok(())
    }
}

/// Publishes [`ArchiveEvent`]s on the `"archive:events"` channel.
pub struct RedisPublisher {
    redis_manager: Arc<RedisManager>,
}

impl RedisPublisher {
    /// Connect to Redis and health-check the connection.
    pub async fn new(config: RedisConfig) -> ArchiveResult<Self> {
        info!("Creating Redis publisher with URL: {}", config.url);

        let redis_manager =
            Arc::new(RedisManager::new(config).map_err(|e| {
                ArchiveError::Redis(format!("Failed to create Redis manager: {}", e))
            })?);

        // Test connection with health check
        redis_manager
            .health_check()
            .await
            .map_err(|e| ArchiveError::Redis(format!("Redis health check failed: {}", e)))?;

        info!("Successfully connected to Redis");

        Ok(Self { redis_manager })
    }

    /// Publish an archive event.
    pub async fn publish(&self, event: &ArchiveEvent) -> ArchiveResult<()> {
        info!("Publishing archive event: {:?}", event);

        let publisher = self.redis_manager.publisher();
        publisher
            .publish(event)
            .await
            .map_err(|e| ArchiveError::Redis(format!("Failed to publish event: {}", e)))?;

        debug!("Successfully published archive event");
        Ok(())
    }

    /// Perform a health check.
    pub async fn health_check(&self) -> ArchiveResult<()> {
        self.redis_manager
            .health_check()
            .await
            .map_err(|e| ArchiveError::Redis(format!("Redis health check failed: {}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_archive_event_channel() {
        assert_eq!(ArchiveEvent::channel(), "archive:events");
    }

    #[test]
    fn test_redis_config_default() {
        let config = RedisConfig::default();
        assert!(!config.url.is_empty());
        assert!(config.retry_attempts > 0);
    }

    #[test]
    fn test_extract_campaign_success_debian() {
        let payload = r#"{
            "code": "success",
            "campaign": "lintian-fixes",
            "target": {"name": "debian", "details": {}}
        }"#;
        assert_eq!(
            extract_debian_success_campaign(payload),
            Some("lintian-fixes".to_string())
        );
    }

    #[test]
    fn test_extract_campaign_non_success() {
        let payload = r#"{
            "code": "build-failed",
            "campaign": "lintian-fixes",
            "target": {"name": "debian", "details": {}}
        }"#;
        assert_eq!(extract_debian_success_campaign(payload), None);
    }

    #[test]
    fn test_extract_campaign_non_debian() {
        let payload = r#"{
            "code": "success",
            "campaign": "lintian-fixes",
            "target": {"name": "generic", "details": {}}
        }"#;
        assert_eq!(extract_debian_success_campaign(payload), None);
    }

    #[test]
    fn test_extract_campaign_missing_target() {
        let payload = r#"{"code": "success", "campaign": "x"}"#;
        assert_eq!(extract_debian_success_campaign(payload), None);
    }

    #[test]
    fn test_extract_campaign_missing_campaign() {
        let payload = r#"{
            "code": "success",
            "target": {"name": "debian", "details": {}}
        }"#;
        assert_eq!(extract_debian_success_campaign(payload), None);
    }

    #[test]
    fn test_extract_campaign_malformed() {
        assert_eq!(extract_debian_success_campaign("not json"), None);
        assert_eq!(extract_debian_success_campaign(""), None);
    }
}
