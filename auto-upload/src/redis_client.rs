//! Redis pub/sub subscription for the runner's `result` channel.

use std::sync::Arc;

use janitor::redis::{PubSubMessage, RedisConfig, RedisManager};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tracing::{error, info};

use crate::error::Result;

/// Wraps the shared [`RedisManager`] and dispatches messages to a handler.
pub struct RedisClient {
    redis_manager: Arc<RedisManager>,
}

/// Build result message from the runner service.
///
/// Matches the shape of `ActiveRun.result.json()` on the runner side. Only the
/// fields consulted by auto-upload are modelled; the rest of the payload is
/// ignored. `target` may be an empty object when the run has no builder
/// result, so it is deserialized permissively.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BuildResultMessage {
    /// Unique identifier for the build run.
    pub log_id: String,
    /// Runner result code (e.g. `"success"`, `"nothing-to-do"`,
    /// `"codemod-error"`). Non-success runs also publish to this channel
    /// but emit `target: {}`, so we filter them out before touching
    /// `target.name`.
    #[serde(default)]
    pub code: Option<String>,
    /// Empty object when the run had no builder result.
    #[serde(default)]
    pub target: BuildTarget,
}

/// Target field of a build result.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct BuildTarget {
    /// e.g. `"debian"`; absent when the target is `{}`.
    #[serde(default)]
    pub name: Option<String>,
    /// Absent when the target is `{}`.
    #[serde(default)]
    pub details: Option<BuildTargetDetails>,
}

/// Target-specific details; auto-upload only reads `build_distribution`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BuildTargetDetails {
    /// Debian suite/distribution the build targets.
    pub build_distribution: String,
    /// Everything else the runner sends; kept for round-trip fidelity.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl PubSubMessage for BuildResultMessage {
    fn channel() -> &'static str {
        "result"
    }
}

impl RedisClient {
    /// Connect to Redis and verify the connection with a health check.
    pub async fn new(redis_url: &str) -> Result<Self> {
        info!("connecting to Redis: {}", redis_url);
        let config = RedisConfig {
            url: redis_url.to_string(),
            ..Default::default()
        };
        let redis_manager = Arc::new(RedisManager::new(config)?);
        redis_manager.health_check().await?;
        info!("connected to Redis");
        Ok(Self { redis_manager })
    }

    /// Subscribe to build result messages and process them asynchronously.
    ///
    /// The handler is awaited before the next message is dispatched. If it
    /// returns an error the message is logged and the subscription continues.
    pub async fn subscribe_to_results<F, Fut>(&self, handler: F) -> Result<JoinHandle<()>>
    where
        F: Fn(BuildResultMessage) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<()>> + Send + 'static,
    {
        info!("subscribing to build results");

        let subscriber = self.redis_manager.subscriber();
        let handler = Arc::new(handler);

        let handle = tokio::spawn(async move {
            let result = subscriber
                .subscribe::<BuildResultMessage, _, _>(move |message| {
                    let handler = handler.clone();
                    async move {
                        if let Err(e) = handler(message).await {
                            error!("error handling result: {}", e);
                            return Err(janitor::error::JanitorError::external_service(
                                "auto-upload",
                                format!("Handler error: {}", e),
                            ));
                        }
                        Ok(())
                    }
                })
                .await;
            match result {
                Ok(_) => info!("subscription ended"),
                Err(e) => error!("subscription error: {}", e),
            }
        });

        Ok(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscribes_to_result_channel() {
        assert_eq!(BuildResultMessage::channel(), "result");
    }

    #[test]
    fn roundtrips_through_serde() {
        let message = BuildResultMessage {
            log_id: "test-123".to_string(),
            code: Some("success".to_string()),
            target: BuildTarget {
                name: Some("debian".to_string()),
                details: Some(BuildTargetDetails {
                    build_distribution: "unstable".to_string(),
                    extra: serde_json::Map::new(),
                }),
            },
        };

        let serialized = serde_json::to_string(&message).unwrap();
        let deserialized: BuildResultMessage = serde_json::from_str(&serialized).unwrap();

        assert_eq!(message.log_id, deserialized.log_id);
        assert_eq!(message.code, deserialized.code);
        assert_eq!(message.target.name, deserialized.target.name);
    }

    #[test]
    fn tolerates_empty_target_object() {
        let payload = r#"{"log_id": "abc", "target": {}}"#;
        let msg: BuildResultMessage = serde_json::from_str(payload).unwrap();
        assert_eq!(msg.log_id, "abc");
        assert!(msg.target.name.is_none());
        assert!(msg.target.details.is_none());
    }

    #[test]
    fn parses_non_success_run_shape() {
        // Regression: this is what the runner publishes for a non-success
        // run. The Python service crashed on `result["target"]["name"]`
        // when target was `{}` (see py commit 38fbc2025). We must at least
        // be able to deserialise it.
        let payload =
            r#"{"log_id": "abc", "code": "codemod-error", "description": "boom", "target": {}}"#;
        let msg: BuildResultMessage = serde_json::from_str(payload).unwrap();
        assert_eq!(msg.code.as_deref(), Some("codemod-error"));
        assert!(msg.target.name.is_none());
    }

    #[test]
    fn ignores_extra_payload_fields() {
        let payload = r#"{
            "log_id": "abc",
            "code": "success",
            "description": "ok",
            "target": {"name": "debian", "details": {"build_distribution": "unstable"}}
        }"#;
        let msg: BuildResultMessage = serde_json::from_str(payload).unwrap();
        assert_eq!(msg.target.name.as_deref(), Some("debian"));
        assert_eq!(msg.target.details.unwrap().build_distribution, "unstable");
    }
}
