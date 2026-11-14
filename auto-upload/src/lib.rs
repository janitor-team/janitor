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

use lazy_static::lazy_static;
use prometheus::{register_counter, Counter};

pub use config::Config;
pub use error::UploadError;

lazy_static! {
    /// Counter for failed package signings.
    pub static ref DEBSIGN_FAILED_COUNT: Counter = register_counter!(
        "debsign_failed_total",
        "Number of packages for which signing failed."
    )
    .unwrap();

    /// Counter for failed package uploads.
    pub static ref UPLOAD_FAILED_COUNT: Counter = register_counter!(
        "upload_failed_total",
        "Number of packages for which uploading failed."
    )
    .unwrap();

    /// Counter for result messages that could not be handled.
    pub static ref RESULT_HANDLING_FAILED_COUNT: Counter = register_counter!(
        "result_handling_failed_total",
        "Number of result messages that could not be handled."
    )
    .unwrap();
}

/// Run the auto-upload service: start the metrics web server and the Redis
/// listener and wait until one of them exits.
///
/// When `dput_host` is `None`, dput picks the default host from `dput.cf`.
/// When `run_backfill` is `true`, a one-shot backfill runs concurrently with
/// the listener.
#[allow(clippy::too_many_arguments)]
pub async fn run_service(
    config: Config,
    listen_addr: std::net::IpAddr,
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
