//! Subscribes to the runner's `result` pub/sub channel and triggers a
//! per-campaign republish when a debian build succeeds.

use std::sync::Arc;

use janitor::redis::{RedisConfig, RedisManager};
use janitor::Campaign;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::error::{ArchiveError, ArchiveResult};
use crate::manager::GeneratorManager;

/// Parse a runner `result` channel payload and return the campaign
/// name if the message represents a successful Debian build.
///
/// Returns `None` for malformed JSON, non-success codes, non-debian
/// targets, or messages missing the campaign field or carrying an
/// invalid campaign name.
pub fn extract_debian_success_campaign(payload: &str) -> Option<Campaign> {
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

    let name = parsed.get("campaign").and_then(|c| c.as_str())?;
    match Campaign::new(name) {
        Ok(campaign) => Some(campaign),
        Err(e) => {
            warn!("Ignoring runner result: {}", e);
            None
        }
    }
}

/// Subscribes to the runner's `result` channel and dispatches
/// successful debian builds to a [`GeneratorManager`].
pub struct RedisSubscriber {
    redis_manager: Arc<RedisManager>,
    generator_manager: Arc<GeneratorManager>,
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
        })
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
        generator_manager.trigger_campaign(&campaign).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_campaign_success_debian() {
        let payload = r#"{
            "code": "success",
            "campaign": "lintian-fixes",
            "target": {"name": "debian", "details": {}}
        }"#;
        assert_eq!(
            extract_debian_success_campaign(payload),
            Some(Campaign::new("lintian-fixes").unwrap())
        );
    }

    #[test]
    fn test_extract_campaign_invalid_name() {
        let payload = r#"{
            "code": "success",
            "campaign": "../etc",
            "target": {"name": "debian", "details": {}}
        }"#;
        assert_eq!(extract_debian_success_campaign(payload), None);
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
