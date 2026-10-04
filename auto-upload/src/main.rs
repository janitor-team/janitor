use clap::Parser;
use janitor_auto_upload::{run_service, Config, UploadError};
use tracing::info;

#[derive(Debug, Parser)]
#[command(
    name = "janitor-auto-upload",
    about = "Automatically upload Debian packages to repositories"
)]
struct Args {
    /// Port to listen on for HTTP server.
    #[arg(long, default_value = "9933", env = "AUTO_UPLOAD_PORT")]
    port: u16,

    /// Address to listen on for HTTP server.
    #[arg(long, default_value = "localhost", env = "AUTO_UPLOAD_LISTEN_ADDRESS")]
    listen_address: String,

    /// Path to configuration file.
    #[arg(long, default_value = "janitor.conf", env = "JANITOR_CONFIG")]
    config: String,

    /// dput host to upload packages to. If unset, dput picks the default host.
    #[arg(long, env = "DPUT_HOST")]
    dput_host: Option<String>,

    /// GPG key ID to use for signing packages.
    #[arg(long, env = "DEBSIGN_KEYID")]
    debsign_keyid: Option<String>,

    /// Also re-upload previously built packages once at startup.
    #[arg(long)]
    backfill: bool,

    /// Only upload `_source.changes` files.
    #[arg(long)]
    source_only: bool,

    /// Build distributions to upload (repeatable).
    #[arg(long = "distribution", env = "AUTO_UPLOAD_DISTRIBUTIONS")]
    distributions: Vec<String>,

    /// Enable verbose logging.
    #[arg(short, long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<(), UploadError> {
    let args = Args::parse();

    let level = if args.verbose {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
    };
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .init();

    let config = Config::from_file(&args.config)?;

    info!("Starting Janitor auto-upload service");
    info!("Listen address: {}:{}", args.listen_address, args.port);
    if let Some(host) = &args.dput_host {
        info!("dput host: {}", host);
    }
    if let Some(keyid) = &args.debsign_keyid {
        info!("GPG key ID: {}", keyid);
    }
    if !args.distributions.is_empty() {
        info!("Distributions: {:?}", args.distributions);
    }
    if args.backfill {
        info!("Backfill: enabled");
    }

    run_service(
        config,
        &args.listen_address,
        args.port,
        args.dput_host.as_deref(),
        args.debsign_keyid.as_deref(),
        args.source_only,
        args.distributions,
        args.backfill,
    )
    .await
}
