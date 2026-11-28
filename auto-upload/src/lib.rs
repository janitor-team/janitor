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

pub use config::Config;
pub use error::UploadError;

/// Counter for failed package signings.
pub const DEBSIGN_FAILURES_TOTAL: &str = "debsign_failures_total";

/// Counter for failed package uploads.
pub const UPLOAD_FAILURES_TOTAL: &str = "upload_failures_total";

/// Counter for result messages that could not be handled.
pub const RESULT_HANDLING_FAILURES_TOTAL: &str = "result_handling_failures_total";

/// Install the metrics recorder and register the auto-upload metrics.
pub fn install_metrics() {
    janitor_service::metrics::install(&[]);
    register_metrics();
}

fn register_metrics() {
    metrics::describe_counter!(
        DEBSIGN_FAILURES_TOTAL,
        "Number of packages for which signing failed."
    );
    metrics::describe_counter!(
        UPLOAD_FAILURES_TOTAL,
        "Number of packages for which uploading failed."
    );
    metrics::describe_counter!(
        RESULT_HANDLING_FAILURES_TOTAL,
        "Number of result messages that could not be handled."
    );
    for name in [
        DEBSIGN_FAILURES_TOTAL,
        UPLOAD_FAILURES_TOTAL,
        RESULT_HANDLING_FAILURES_TOTAL,
    ] {
        metrics::counter!(name).increment(0);
    }
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
    install_metrics();
    let upload_config = upload::UploadConfig {
        dput_host: dput_host.map(str::to_string),
        debsign_keyid: debsign_keyid.map(str::to_string),
        source_only,
        distributions,
    };
    service::run(config, listen_addr, port, upload_config, run_backfill).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use janitor_service::testing::{capture, Snapshot, Value};

    #[test]
    fn counters_registered_at_zero() {
        assert_eq!(
            capture(register_metrics),
            Snapshot::from([
                (DEBSIGN_FAILURES_TOTAL.to_string(), Value::Counter(0)),
                (
                    RESULT_HANDLING_FAILURES_TOTAL.to_string(),
                    Value::Counter(0)
                ),
                (UPLOAD_FAILURES_TOTAL.to_string(), Value::Counter(0)),
            ])
        );
    }
}
