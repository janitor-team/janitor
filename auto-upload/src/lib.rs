//! Auto-upload service for the Janitor project.
//!
//! Listens on the janitor runner's `result` Redis channel and, for each
//! successful debian build, signs the resulting `.changes` file with
//! `debsign` and uploads it via `dput`.

#![deny(missing_docs)]

pub mod artifacts;
pub mod backfill;
pub mod config;
pub mod database;
pub mod error;
pub mod message_handler;
pub mod process;
pub mod redis_client;
pub mod service;
pub mod upload;
pub mod utils;
pub mod web;

use std::sync::LazyLock;

use prometheus::{register_counter, Counter};

pub use config::Config;
pub use error::UploadError;

/// Counter for failed package signings.
pub static DEBSIGN_FAILED_COUNT: LazyLock<Counter> = LazyLock::new(|| {
    register_counter!(
        "debsign_failed",
        "Number of packages for which signing failed."
    )
    .expect("register debsign_failed counter")
});

/// Counter for failed package uploads.
pub static UPLOAD_FAILED_COUNT: LazyLock<Counter> = LazyLock::new(|| {
    register_counter!(
        "upload_failed",
        "Number of packages for which uploading failed."
    )
    .expect("register upload_failed counter")
});

/// Run the auto-upload service: start the metrics web server and the Redis
/// listener and wait until one of them exits.
///
/// When `dput_host` is `None`, dput picks the default host from `dput.cf`.
/// When `run_backfill` is `true`, a one-shot backfill runs concurrently with
/// the listener.
#[allow(clippy::too_many_arguments)]
pub async fn run_service(
    config: Config,
    listen_addr: &str,
    port: u16,
    dput_host: Option<&str>,
    debsign_keyid: Option<&str>,
    source_only: bool,
    distributions: Vec<String>,
    run_backfill: bool,
) -> Result<(), UploadError> {
    let upload_config = upload::UploadConfig {
        dput_host: dput_host.map(str::to_string),
        debsign_keyid: debsign_keyid.map(str::to_string),
        source_only,
        distributions,
    };
    service::run(config, listen_addr, port, upload_config, run_backfill).await
}
