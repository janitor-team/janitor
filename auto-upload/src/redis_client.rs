//! Redis pub/sub subscription for the runner's `result` channel.

use std::sync::Arc;

use janitor::redis::{RedisConfig, RedisManager};
pub use janitor_types::api::auto_upload::{
    BuildResultMessage, BuildTarget, DebianTargetDetails, GenericTargetDetails,
};
use tokio::task::JoinHandle;
use tracing::{error, info};

use crate::error::{Result, UploadError};

/// Wraps the shared [`RedisManager`] and dispatches messages to a handler.
pub struct RedisClient {
    redis_manager: Arc<RedisManager>,
}

impl RedisClient {
    /// Set up the Redis client. The connection is made when subscribing.
    pub fn new(redis_url: &str) -> Result<Self> {
        info!("using Redis at {}", redis_url);
        let config = RedisConfig {
            url: redis_url.to_string(),
            ..Default::default()
        };
        let redis_manager = Arc::new(RedisManager::new(config)?);
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
                .subscribe::<BuildResultMessage, _, _, UploadError>(move |message| {
                    let handler = handler.clone();
                    async move {
                        if let Err(e) = handler(message).await {
                            metrics::counter!(crate::RESULT_HANDLING_FAILURES_TOTAL).increment(1);
                            error!("error handling result: {}", e);
                            return Err(e);
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
    use janitor_types::api::PubSubMessage;

    #[test]
    fn subscribes_to_result_channel() {
        assert_eq!(BuildResultMessage::channel(), "result");
    }

    #[test]
    fn tolerates_null_target() {
        let payload = r#"{"log_id": "abc", "target": null}"#;
        let msg: BuildResultMessage = serde_json::from_str(payload).unwrap();
        assert_eq!(msg.log_id, "abc");
        assert!(msg.target.is_none());
    }

    #[test]
    fn rejects_empty_target_object() {
        let payload = r#"{"log_id": "abc", "target": {}}"#;
        assert!(serde_json::from_str::<BuildResultMessage>(payload).is_err());
    }

    #[test]
    fn rejects_unknown_target_name() {
        let payload = r#"{"log_id": "abc", "target": {"name": "ocaml", "details": {}}}"#;
        assert!(serde_json::from_str::<BuildResultMessage>(payload).is_err());
    }

    #[test]
    fn rejects_invalid_log_id() {
        let payload = r#"{"log_id": "../etc", "target": null}"#;
        assert!(serde_json::from_str::<BuildResultMessage>(payload).is_err());
    }

    #[test]
    fn parses_non_success_run_shape() {
        // Non-success runs publish `target: null`; auto-upload filters
        // them out on `code` before dispatching on `target`.
        let payload =
            r#"{"log_id": "abc", "code": "codemod-error", "description": "boom", "target": null}"#;
        let msg: BuildResultMessage = serde_json::from_str(payload).unwrap();
        assert_eq!(msg.code.as_deref(), Some("codemod-error"));
        assert!(msg.target.is_none());
    }

    #[test]
    fn parses_generic_target() {
        let payload = r#"{"log_id": "abc", "target": {"name": "generic", "details": {}}}"#;
        let msg: BuildResultMessage = serde_json::from_str(payload).unwrap();
        assert!(matches!(msg.target, Some(BuildTarget::Generic(_))));
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
        match msg.target.unwrap() {
            BuildTarget::Debian(details) => {
                assert_eq!(details.build_distribution, "unstable");
            }
            BuildTarget::Generic(_) => panic!("expected Debian target"),
        }
    }
}
