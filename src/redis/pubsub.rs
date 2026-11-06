//! Redis pub/sub functionality

use crate::error::JanitorError;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

/// Trait for messages that can be sent via Redis pub/sub
pub trait PubSubMessage: Serialize + for<'de> Deserialize<'de> + Send + Sync {
    /// Get the channel name for this message type
    fn channel() -> &'static str;
}

/// Publisher for sending messages via Redis pub/sub
#[derive(Clone)]
pub struct Publisher {
    manager: super::RedisManager,
}

impl Publisher {
    /// Create a new publisher
    pub(super) fn new(manager: super::RedisManager) -> Self {
        Self { manager }
    }

    /// Publish a message to its channel
    pub async fn publish<T: PubSubMessage>(&self, message: &T) -> Result<(), JanitorError> {
        let channel = T::channel();
        let payload = serde_json::to_string(message)
            .map_err(|e| JanitorError::redis_msg(format!("Failed to serialize message: {}", e)))?;

        let mut cmd = redis::cmd("PUBLISH");
        cmd.arg(channel).arg(&payload);

        let subscribers: i32 = self.manager.execute(&mut cmd).await?;

        debug!(
            "Published message to channel '{}', {} subscribers received it",
            channel, subscribers
        );

        Ok(())
    }

    /// Publish a message to a specific channel
    pub async fn publish_to_channel(
        &self,
        channel: &str,
        message: &str,
    ) -> Result<i32, JanitorError> {
        let mut cmd = redis::cmd("PUBLISH");
        cmd.arg(channel).arg(message);

        let subscribers: i32 = self.manager.execute(&mut cmd).await?;

        debug!(
            "Published raw message to channel '{}', {} subscribers",
            channel, subscribers
        );

        Ok(subscribers)
    }
}

/// Subscriber for receiving messages via Redis pub/sub
pub struct Subscriber {
    manager: super::RedisManager,
}

impl Subscriber {
    /// Create a new subscriber
    pub(super) fn new(manager: super::RedisManager) -> Self {
        Self { manager }
    }

    /// Subscribe to a message type and process messages
    pub async fn subscribe<T, F, Fut>(&self, handler: F) -> Result<(), JanitorError>
    where
        T: PubSubMessage,
        F: Fn(T) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<(), JanitorError>> + Send,
    {
        let channel = T::channel();
        let handler = std::sync::Arc::new(handler);

        self.subscribe_to_channel(channel, move |payload| {
            let handler = handler.clone();
            async move {
                match serde_json::from_str::<T>(&payload) {
                    Ok(message) => handler(message).await,
                    Err(e) => {
                        error!(
                            "Failed to deserialize message from channel '{}': {}",
                            channel, e
                        );
                        Ok(())
                    }
                }
            }
        })
        .await
    }

    /// Subscribe to a specific channel and process raw messages
    pub async fn subscribe_to_channel<F, Fut>(
        &self,
        channel: &str,
        mut handler: F,
    ) -> Result<(), JanitorError>
    where
        F: FnMut(String) -> Fut + Send,
        Fut: std::future::Future<Output = Result<(), JanitorError>> + Send,
    {
        info!("Subscribing to Redis channel: {}", channel);

        // Over RESP3 the connection manager reconnects and subscribes again after a restart.
        let info = self.manager.client().get_connection_info().clone();
        let settings = info.redis_settings().clone();
        let client = redis::Client::open(
            info.set_redis_settings(settings.set_protocol(redis::ProtocolVersion::RESP3)),
        )?;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let config = redis::aio::ConnectionManagerConfig::new()
            .set_push_sender(tx)
            .set_automatic_resubscription()
            .set_number_of_retries(usize::MAX)
            .set_max_delay(std::time::Duration::from_secs(5));
        let mut conn = client.get_connection_manager_with_config(config).await?;
        conn.subscribe(channel).await?;

        info!("Successfully subscribed to channel: {}", channel);

        while let Some(push) = rx.recv().await {
            if push.kind == redis::PushKind::Disconnection {
                warn!("Lost Redis connection; subscribing to '{}' again", channel);
            }
            let Some(msg) = redis::Msg::from_push_info(push) else {
                continue;
            };
            match msg.get_payload::<String>() {
                Ok(payload) => {
                    debug!(
                        "Received message on channel '{}': {} bytes",
                        channel,
                        payload.len()
                    );
                    if let Err(e) = handler(payload).await {
                        error!("Error handling message: {}", e);
                    }
                }
                Err(e) => {
                    warn!("Failed to get message payload: {}", e);
                }
            }
        }

        Ok(())
    }
}
