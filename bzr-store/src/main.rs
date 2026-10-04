//! BZR Store service main entry point.
//!
//! CLI flags mirror `py/janitor/bzr_store.py` so existing deployment
//! scripts and systemd units keep working: `--port`, `--public-port`,
//! `--listen-address`, `--config`, `--vcs-path`, `--client-max-size`,
//! `--gcp-logging`, `--debug`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use clap::Parser;
use janitor_bzr_store::{
    config::{BzrConfig, BzrStoreConfig, Config},
    web::create_applications,
};
use tracing::info;
use tracing_subscriber::{EnvFilter, FmtSubscriber};

/// BZR Store service. Serves an admin and a public HTTP interface to a
/// directory of per-codebase shared bzr repositories, including the
/// Bazaar smart protocol for remote client access.
#[derive(Parser, Debug)]
#[command(name = "janitor-bzr-store", version, about)]
struct Cli {
    /// Admin listen port.
    #[arg(long, default_value_t = 9929)]
    port: u16,

    /// Public listen port (for reverse-proxied worker-authenticated
    /// writes and anonymous reads).
    #[arg(long, default_value_t = 9930)]
    public_port: u16,

    /// Listen address bound by both admin and public servers.
    #[arg(long, default_value = "localhost")]
    listen_address: String,

    /// Path to the janitor configuration file. Must parse with the
    /// same protobuf-text format `janitor::config::read_file` expects;
    /// provides the campaign list used to validate smart protocol
    /// campaign names.
    #[arg(long, default_value = "janitor.conf")]
    config: PathBuf,

    /// Path to the on-disk bzr repository directory. Each first-level
    /// entry is treated as a shared repo for one codebase.
    #[arg(long)]
    vcs_path: Option<PathBuf>,

    /// Maximum HTTP request body size in bytes (0 = no limit).
    #[arg(long, default_value_t = 1024u64 * 1024 * 1024)]
    client_max_size: u64,

    /// Emit logs in Google Cloud Logging's structured JSON format.
    #[arg(long)]
    gcp_logging: bool,

    /// Enable debug-level logging.
    #[arg(long)]
    debug: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    init_tracing(cli.debug, cli.gcp_logging);

    let vcs_path = cli
        .vcs_path
        .ok_or_else(|| anyhow!("--vcs-path is required"))?;
    if !vcs_path.exists() {
        return Err(anyhow!("vcs path {:?} does not exist", vcs_path));
    }

    let janitor_config = Arc::new(
        janitor::config::read_file(&cli.config)
            .map_err(|e| anyhow!("Failed to read janitor config {:?}: {}", cli.config, e))?,
    );

    // Build the BzrStoreConfig. Environment variables and any config
    // file at $BZR_CONFIG_PATH still work; the CLI flags supersede the
    // admin/public bind addresses and the on-disk repository path so
    // the Python deployment contract is preserved.
    let mut config: Config = BzrStoreConfig::load()
        .await
        .unwrap_or_else(|_| BzrStoreConfig::default());

    let admin_bind = format!("{}:{}", cli.listen_address, cli.port)
        .parse()
        .map_err(|e| anyhow!("invalid admin bind: {}", e))?;
    let public_bind = format!("{}:{}", cli.listen_address, cli.public_port)
        .parse()
        .map_err(|e| anyhow!("invalid public bind: {}", e))?;

    config.bzr = BzrConfig {
        repository_path: vcs_path,
        admin_bind,
        public_bind,
        python_path: config.bzr.python_path,
        request_timeout: config.bzr.request_timeout,
    };

    // Use janitor.conf's database_location if the BzrStoreConfig
    // didn't get one from env / toml. This matches Python, where the
    // single janitor.conf carries the DB URL.
    if config.base.database.is_none() {
        if let Some(db_url) = janitor_config.database_location.as_ref() {
            config.base.database = Some(janitor::shared_config::DatabaseConfig {
                url: db_url.clone(),
                ..janitor::shared_config::DatabaseConfig::default()
            });
        }
    }

    info!("Starting BZR Store service");
    info!("Admin interface:  {}", config.admin_bind);
    info!("Public interface: {}", config.public_bind);
    info!("Repository path:  {}", config.repository_path.display());
    info!(
        "Client max size:  {} bytes",
        if cli.client_max_size == 0 {
            u64::MAX
        } else {
            cli.client_max_size
        }
    );

    // Initialize breezyshim once on the main thread. Loggerhead and
    // our own smart-protocol / repository handlers all go through
    // breezyshim; a single init() is enough.
    breezyshim::init();

    let (admin_app, public_app) = create_applications(config.clone(), janitor_config).await?;

    let admin_server = {
        let listener = tokio::net::TcpListener::bind(config.admin_bind).await?;
        info!("Admin server listening on {}", config.admin_bind);
        axum::serve(listener, admin_app)
    };
    let public_server = {
        let listener = tokio::net::TcpListener::bind(config.public_bind).await?;
        info!("Public server listening on {}", config.public_bind);
        axum::serve(listener, public_app)
    };

    tokio::try_join!(admin_server, public_server)?;
    Ok(())
}

/// Set up `tracing` with either plain or GCP-structured JSON output.
///
/// `RUST_LOG` wins when set; otherwise `--debug` lifts the default
/// level to `debug`.
fn init_tracing(debug: bool, gcp_logging: bool) {
    let default_filter = if debug { "debug" } else { "info" };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    let builder = FmtSubscriber::builder().with_env_filter(filter);
    if gcp_logging {
        builder.json().init();
    } else {
        builder.init();
    }
}
