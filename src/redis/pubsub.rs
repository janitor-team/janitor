//! Redis pub/sub functionality

use futures::StreamExt;
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
    pub async fn publish<T: PubSubMessage>(&self, message: &T) -> Result<(), redis::RedisError> {
        let channel = T::channel();
        let payload = serde_json::to_string(message).map_err(|e| {
            redis::RedisError::from((
                redis::ErrorKind::Io,
                "Failed to serialize message",
                e.to_string(),
            ))
        })?;

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
    ) -> Result<i32, redis::RedisError> {
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

    /// Subscribe to a message type and process messages. Handler errors
    /// are logged and the subscription continues.
    pub async fn subscribe<T, F, Fut, E>(&self, handler: F) -> Result<(), redis::RedisError>
    where
        T: PubSubMessage,
        F: Fn(T) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<(), E>> + Send,
        E: std::fmt::Display,
    {
        let channel = T::channel();
        let handler = std::sync::Arc::new(handler);

        self.subscribe_to_channel(channel, move |payload| {
            let handler = handler.clone();
            async move {
                match serde_json::from_str::<T>(&payload) {
                    Ok(message) => handler(message).await.map_err(|e| e.to_string()),
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

    /// Subscribe to a specific channel and process raw messages. Handler
    /// errors are logged and the subscription continues.
    pub async fn subscribe_to_channel<F, Fut, E>(
        &self,
        channel: &str,
        mut handler: F,
    ) -> Result<(), redis::RedisError>
    where
        F: FnMut(String) -> Fut + Send,
        Fut: std::future::Future<Output = Result<(), E>> + Send,
        E: std::fmt::Display,
    {
        info!("Subscribing to Redis channel: {}", channel);

        let mut pubsub = self.manager.client().get_async_pubsub().await?;

        pubsub.subscribe(channel).await?;

        info!("Successfully subscribed to channel: {}", channel);

        let mut stream = pubsub.on_message();

        while let Some(msg) = stream.next().await {
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

        warn!("Subscription to channel '{}' ended", channel);
        Ok(())
    }
}
