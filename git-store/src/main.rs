//! janitor-git-store CLI entry point.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use clap::Parser;
use janitor_git_store::{
    config::Config, database::DatabaseManager, repository::RepositoryManager, tracing_setup, web,
    web_utils::HealthChecker,
};
use tokio::net::TcpListener;
use tracing::info;

/// Serve admin and public HTTP interfaces to a directory of
/// per-codebase bare git repositories.
#[derive(Parser, Debug)]
#[command(name = "janitor-git-store", version, about)]
struct Cli {
    /// Admin listen port.
    #[arg(long, default_value_t = 9923)]
    port: u16,

    /// Public listen port.
    #[arg(long, default_value_t = 9924)]
    public_port: u16,

    #[arg(long, default_value = "localhost")]
    listen_address: String,

    #[arg(long, default_value = "janitor.conf")]
    config: PathBuf,

    /// Directory of per-codebase bare git repositories.
    #[arg(long)]
    vcs_path: Option<PathBuf>,

    /// Max HTTP request body in bytes (0 = unlimited).
    #[arg(long, default_value_t = 1024u64 * 1024 * 1024)]
    client_max_size: u64,

    /// Accepted for CLI compatibility; no-op (Rust always uses `git
    /// http-backend`).
    #[arg(long)]
    dulwich_server: bool,

    /// Emit logs as JSON for Google Cloud Logging.
    #[arg(long)]
    gcp_logging: bool,

    #[arg(long)]
    debug: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let vcs_path = cli
        .vcs_path
        .ok_or_else(|| anyhow!("--vcs-path is required"))?;
    if !vcs_path.exists() {
        return Err(anyhow!("vcs path {:?} does not exist", vcs_path));
    }

    let janitor_config = janitor::config::read_file(&cli.config)
        .map_err(|e| anyhow!("Failed to read janitor config {:?}: {}", cli.config, e))?;

    // Hold the guard until the end of main so queued spans flush on
    // shutdown.
    let _tracing_guard = tracing_setup::init(
        "git-store",
        cli.debug,
        cli.gcp_logging,
        janitor_config.zipkin_address.as_deref(),
    )?;

    // CLI flags override env/TOML; the janitor.conf database_location
    // is the last-resort fallback.
    let mut config = Config::from_env().unwrap_or_else(|_| Config::default());
    config.git.local_path = vcs_path;
    config.git.host = cli.listen_address.clone();
    config.git.admin_port = cli.port;
    config.git.public_port = cli.public_port;

    if config.base.database.is_none() {
        if let Some(db_url) = janitor_config.database_location.as_ref() {
            config.base.database = Some(janitor::shared_config::DatabaseConfig {
                url: db_url.clone(),
                ..janitor::shared_config::DatabaseConfig::default()
            });
        }
    }

    let config = Arc::new(config);
    info!("Starting Git Store service");
    info!(
        "Admin interface:  {}:{}",
        config.git.host, config.git.admin_port
    );
    info!(
        "Public interface: {}:{}",
        config.git.host, config.git.public_port
    );
    info!("Repository path:  {:?}", config.git.local_path);
    info!(
        "Client max size:  {} bytes",
        if cli.client_max_size == 0 {
            u64::MAX
        } else {
            cli.client_max_size
        }
    );
    if cli.dulwich_server {
        info!("--dulwich-server is a no-op; Rust git-store always uses `git http-backend`");
    }

    let repo_manager = Arc::new(RepositoryManager::new(config.git.local_path.clone()));

    let database_url = config
        .base
        .database
        .as_ref()
        .ok_or_else(|| anyhow!("No database configuration found"))?
        .url
        .clone();
    let db_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await?;
    let db_manager = Arc::new(DatabaseManager::new(db_pool.clone()));

    let tera = Arc::new(web::init_templates(config.git.templates_path.as_deref())?);

    let health_checker = Arc::new(HealthChecker::new().with_db(db_pool));

    let admin_state = web::AppState {
        repo_manager: repo_manager.clone(),
        config: config.clone(),
        tera: tera.clone(),
        db_manager: db_manager.clone(),
        health_checker: health_checker.clone(),
        role: web::AppRole::Admin,
    };
    let public_state = web::AppState {
        repo_manager,
        config: config.clone(),
        tera,
        db_manager,
        health_checker,
        role: web::AppRole::Public,
    };

    let client_max_size = usize::try_from(cli.client_max_size).unwrap_or(usize::MAX);
    let admin_app = web::create_admin_app(admin_state, client_max_size);
    let public_app = web::create_public_app(public_state, client_max_size);

    let admin_addr = format!("{}:{}", config.git.host, config.git.admin_port);
    let public_addr = format!("{}:{}", config.git.host, config.git.public_port);

    let admin_listener = TcpListener::bind(&admin_addr).await?;
    info!("Admin server listening on {}", admin_addr);
    let admin_server = axum::serve(admin_listener, admin_app);

    let public_listener = TcpListener::bind(&public_addr).await?;
    info!("Public server listening on {}", public_addr);
    let public_server = axum::serve(public_listener, public_app);

    tokio::try_join!(admin_server, public_server)?;
    Ok(())
}
