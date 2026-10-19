//! Redis connection pooling implementation

use crate::error::JanitorError;
use redis::{aio::ConnectionManager, Client};
use std::sync::Arc;
use tokio::sync::{Semaphore, SemaphorePermit};
use tokio::time::{timeout, Duration};
use tracing::{debug, warn};

/// Connection pool for managing Redis connections
pub struct ConnectionPool {
    /// Redis client
    client: Arc<Client>,
    /// Semaphore to limit concurrent connections
    semaphore: Arc<Semaphore>,
    /// Connection timeout
    timeout_ms: u64,
}

impl ConnectionPool {
    /// Create a new connection pool
    pub fn new(client: Arc<Client>, max_connections: usize, timeout_ms: u64) -> Self {
        Self {
            client,
            semaphore: Arc::new(Semaphore::new(max_connections)),
            timeout_ms,
        }
    }

    /// Get a connection from the pool
    pub async fn get(&self) -> Result<ConnectionManager, JanitorError> {
        // Acquire permit from semaphore
        let _permit = self.acquire_permit().await?;

        // Create connection manager with timeout
        let connection_future = ConnectionManager::new((*self.client).clone());

        match timeout(Duration::from_millis(self.timeout_ms), connection_future).await {
            Ok(Ok(conn)) => {
                debug!("Successfully acquired Redis connection from pool");
                Ok(conn)
            }
            Ok(Err(e)) => {
                warn!("Failed to create Redis connection: {}", e);
                Err(JanitorError::Redis(e))
            }
            Err(_) => {
                warn!("Redis connection timeout after {}ms", self.timeout_ms);
                Err(JanitorError::redis_msg(format!(
                    "Connection timeout after {}ms",
                    self.timeout_ms
                )))
            }
        }
    }

    /// Acquire a permit from the semaphore
    async fn acquire_permit(&self) -> Result<SemaphorePermit<'_>, JanitorError> {
        match self.semaphore.acquire().await {
            Ok(permit) => Ok(permit),
            Err(_) => Err(JanitorError::redis_msg(
                "Failed to acquire connection pool permit",
            )),
        }
    }
}
