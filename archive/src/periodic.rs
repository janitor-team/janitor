//! The periodic republish loop.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::info;

use crate::manager::GeneratorManager;

/// Interval between sweeps that republish every apt_repository.
pub const REPUBLISH_INTERVAL: Duration = Duration::from_secs(60 * 60 * 12);

/// Spawn a loop that republishes every configured apt_repository
/// every [`REPUBLISH_INTERVAL`], starting immediately.
///
/// Iterates the apt_repositories rather than the campaign mapping, so
/// a suite still republishes even when no campaign has produced new
/// builds.
pub fn spawn_republish_loop(generator_manager: Arc<GeneratorManager>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(REPUBLISH_INTERVAL);
        loop {
            interval.tick().await;
            info!("Starting periodic republish");
            generator_manager.trigger_all_repositories().await;
        }
    })
}
