//! Service entry point: spawn the web server and the Redis listener, and
//! wait for either to finish.
//!
//! Neither task is expected to return under normal operation. The Redis
//! listener auto-reconnects on error; task failures are propagated to the
//! caller. A one-shot backfill can run alongside without terminating the
//! service when it completes.

use std::sync::Arc;
use std::time::Duration;

use tracing::{error, info, warn};

use crate::backfill::BackfillProcessor;
use crate::config::Config;
use crate::error::{Result, UploadError};
use crate::message_handler::MessageHandler;
use crate::redis_client::RedisClient;
use crate::upload::UploadConfig;
use crate::web;

/// Run the web server and the Redis listener until one of them exits.
///
/// If `run_backfill` is `true`, a one-shot backfill is spawned alongside; its
/// completion is logged but does not stop the service.
pub async fn run(
    config: Config,
    listen_addr: &str,
    port: u16,
    upload_config: UploadConfig,
    run_backfill: bool,
) -> Result<()> {
    let redis_url = config
        .redis_location()
        .ok_or(UploadError::MissingConfig("redis_location"))?
        .to_string();
    let artifact_location = config
        .artifact_location()
        .ok_or(UploadError::MissingConfig("artifact_location"))?
        .to_string();
    let database_url = if run_backfill {
        Some(
            config
                .database_location()
                .ok_or(UploadError::MissingConfig("database_location"))?
                .to_string(),
        )
    } else {
        None
    };

    let web_task = {
        let listen_addr = listen_addr.to_string();
        tokio::spawn(
            async move { web::run_web_server(web::create_app(), &listen_addr, port).await },
        )
    };

    let redis_task = tokio::spawn({
        let artifact_location = artifact_location.clone();
        let upload_config = upload_config.clone();
        async move { listen_to_runner(&redis_url, &artifact_location, upload_config).await }
    });

    if let Some(database_url) = database_url {
        tokio::spawn(async move {
            match run_backfill_task(&database_url, &artifact_location, upload_config).await {
                Ok(summary) => info!("backfill finished: {}", summary),
                Err(e) => error!("backfill failed: {}", e),
            }
        });
    }

    tokio::select! {
        res = web_task => join_result("web", res),
        res = redis_task => join_result("redis", res),
    }
}

async fn run_backfill_task(
    database_url: &str,
    artifact_location: &str,
    upload_config: UploadConfig,
) -> Result<crate::backfill::BackfillSummary> {
    let processor = BackfillProcessor::new(database_url, artifact_location, upload_config).await?;
    processor.run_backfill().await
}

fn join_result(
    name: &'static str,
    res: std::result::Result<Result<()>, tokio::task::JoinError>,
) -> Result<()> {
    match res {
        Ok(Ok(())) => {
            warn!("{name} task exited");
            Ok(())
        }
        Ok(Err(e)) => {
            error!("{name} task failed: {}", e);
            Err(e)
        }
        Err(source) => {
            error!("{name} task panicked: {}", source);
            Err(UploadError::TaskPanic { name, source })
        }
    }
}

/// Subscribe to the runner's `result` channel and hand each message to the
/// [`MessageHandler`]. On transient errors, wait five seconds and retry.
async fn listen_to_runner(
    redis_url: &str,
    artifact_location: &str,
    upload_config: UploadConfig,
) -> Result<()> {
    let redis_client = RedisClient::new(redis_url).await?;
    let message_handler = Arc::new(MessageHandler::new(artifact_location, upload_config).await?);

    loop {
        info!("Starting Redis subscription");
        let handle = redis_client
            .subscribe_to_results({
                let handler = message_handler.clone();
                move |message| {
                    let handler = handler.clone();
                    async move { handler.handle_message(message).await }
                }
            })
            .await?;

        match handle.await {
            Ok(_) => warn!("Redis subscription ended normally, reconnecting in 5s"),
            Err(e) => error!(
                "Redis subscription join failed: {:?}; reconnecting in 5s",
                e
            ),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
