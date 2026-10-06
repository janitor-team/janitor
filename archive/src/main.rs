//! CLI entry point for the archive service.

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info};

use janitor_archive::{
    config::{ArchiveConfig, GpgConfig},
    database::ArchiveDatabase,
    error::ArchiveResult,
    manager::GeneratorManager,
    periodic::{PeriodicConfig, PeriodicServices},
    repository::{RepositoryGenerationConfig, RepositoryGenerator},
    scanner::PackageScanner,
    web::ArchiveWebService,
};

/// Janitor Archive Service -- APT repository generation and serving.
#[derive(Parser, Debug)]
#[command(name = "janitor-archive", version, about)]
struct Cli {
    /// Listen port.
    #[arg(long, default_value_t = 9914)]
    port: u16,

    /// Listen address.
    #[arg(long, default_value = "localhost")]
    listen_address: String,

    /// Path to configuration file.
    #[arg(short, long, default_value = "janitor.conf")]
    config: PathBuf,

    /// Cache directory.
    #[arg(long)]
    cache_directory: Option<PathBuf>,

    /// Dists directory.
    #[arg(long)]
    dists_directory: PathBuf,

    /// Use Google Cloud logging.
    #[arg(long)]
    gcp_logging: bool,

    /// Don't sign with GPG.
    #[arg(long)]
    no_gpg: bool,

    /// GPG key to sign with (defaults to gpg's default key).
    #[arg(long, conflicts_with = "no_gpg")]
    gpg_key_id: Option<String>,

    /// Show more detailed output.
    #[arg(long)]
    verbose: bool,

    /// Bind address (legacy alias for --listen-address:--port).
    #[arg(short, long)]
    bind: Option<String>,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Generate repositories once and exit.
    Generate {
        /// Suite to generate (optional, generates all if not specified).
        #[arg(short, long)]
        suite: Option<String>,
    },
    /// Start the web server.
    Serve,
    /// Clean up old repository files.
    Cleanup,
}

#[tokio::main]
async fn main() -> ArchiveResult<()> {
    let cli = Cli::parse();

    // Initialize logging via the janitor crate's shared helper.
    // `--verbose` -> DEBUG level; `--gcp-logging` -> JSON layer for
    // GCP log ingestion.
    let debug = cli.verbose
        || std::env::var("DEBUG")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
    let gcp = cli.gcp_logging
        || std::env::var("LOG_JSON")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
    janitor::logging::init_logging(gcp, debug);

    let janitor_config = janitor::config::read_file(&cli.config).map_err(|e| {
        janitor_archive::error::ArchiveError::InvalidConfiguration(format!(
            "Failed to load config from {}: {}",
            cli.config.display(),
            e
        ))
    })?;
    let locations = Locations::from_janitor_config(&janitor_config)?;

    if let Err(e) = std::fs::create_dir_all(&cli.dists_directory) {
        error!(
            "Failed to create dists directory {:?}: {}",
            cli.dists_directory, e
        );
        return Err(janitor_archive::error::ArchiveError::Io(e));
    }

    let gpg = (!cli.no_gpg).then(|| GpgConfig::new(cli.gpg_key_id.clone()));
    let config = ArchiveConfig::from_janitor_config(janitor_config, &cli.dists_directory, gpg)?;

    match cli.command {
        Some(Cmd::Generate { ref suite }) => {
            generate_repositories(&config, &locations, suite.as_deref()).await?;
        }
        Some(Cmd::Serve) | None => {
            start_web_server(&cli, &config, &locations).await?;
        }
        Some(Cmd::Cleanup) => {
            cleanup_repositories(&config, &locations).await?;
        }
    }

    Ok(())
}

/// Service locations taken from `janitor.conf`.
struct Locations {
    database: String,
    redis: Option<String>,
    artifacts: String,
}

impl Locations {
    fn from_janitor_config(config: &janitor::config::Config) -> ArchiveResult<Self> {
        let database = config.database_location.clone().ok_or_else(|| {
            janitor_archive::error::ArchiveError::InvalidConfiguration(
                "database_location must be set".to_string(),
            )
        })?;
        Ok(Self {
            database,
            redis: config.redis_location.clone(),
            // Without an artifact_location, treat the current
            // directory as the artifact store.
            artifacts: config
                .artifact_location
                .clone()
                .unwrap_or_else(|| "local://".to_string()),
        })
    }

    async fn connect_database(&self) -> ArchiveResult<sqlx::PgPool> {
        sqlx::PgPool::connect(&self.database)
            .await
            .map_err(janitor_archive::error::ArchiveError::Database)
    }
}

/// Build a RepositoryGenerator wired for signing when GPG is
/// configured and, when available, the loaded protobuf config so
/// the generator can walk `apt_repository.select` to resolve the
/// right `debian_build.distribution` per suite. Kept here because
/// both `start_web_server` and `generate_repositories` want the
/// same wiring.
async fn build_generator(
    config: &ArchiveConfig,
    locations: &Locations,
    db_pool: sqlx::PgPool,
    cache_directory: Option<PathBuf>,
) -> ArchiveResult<RepositoryGenerator> {
    let scanner =
        Arc::new(PackageScanner::with_cache(&locations.artifacts, cache_directory).await?);
    let database = Arc::new(ArchiveDatabase::new(db_pool));
    let repo_config = RepositoryGenerationConfig::default();
    let mut generator = match config.gpg.clone() {
        Some(gpg) => RepositoryGenerator::with_gpg(scanner, database, repo_config, gpg),
        None => RepositoryGenerator::new(scanner, database, repo_config),
    };
    if let Some(runtime) = config.runtime_config.as_ref() {
        generator = generator.with_runtime_config(runtime.clone());
    }
    Ok(generator)
}

/// Generate repositories.
async fn generate_repositories(
    config: &ArchiveConfig,
    locations: &Locations,
    suite: Option<&str>,
) -> ArchiveResult<()> {
    let db_pool = locations.connect_database().await?;
    let generator = build_generator(config, locations, db_pool, None).await?;

    if let Some(suite_name) = suite {
        if let Some(repo_config) = config.repositories.get(suite_name) {
            info!("Generating repository for suite: {}", suite_name);
            generator.generate_repository(repo_config).await?;
        } else {
            error!("Suite not found in configuration: {}", suite_name);
            return Err(janitor_archive::error::ArchiveError::InvalidConfiguration(
                format!("Unknown suite: {}", suite_name),
            ));
        }
    } else {
        generator
            .generate_repositories(&config.repositories)
            .await?;
    }

    Ok(())
}

/// Start the web server and spawn the runner pub/sub listener.
async fn start_web_server(
    cli: &Cli,
    config: &ArchiveConfig,
    locations: &Locations,
) -> ArchiveResult<()> {
    // Compose the bind address from --listen-address and --port;
    // `--bind` is accepted as an override for deployments that pass
    // a single socket string.
    let bind_address = cli
        .bind
        .clone()
        .unwrap_or_else(|| format!("{}:{}", cli.listen_address, cli.port));
    info!("Starting web server on: {}", bind_address);

    let db_pool = locations.connect_database().await?;

    // Build shared components for the GeneratorManager and the web service.
    // All scanners share the same cache directory so a Packages/
    // Sources scan performed by one code path is reusable by the
    // others.
    let scanner_for_manager =
        PackageScanner::with_cache(&locations.artifacts, cli.cache_directory.clone()).await?;
    let database_for_manager = ArchiveDatabase::new(db_pool.clone());
    // Build a RepositoryGenerator that signs Release when GPG is
    // configured. `--no-gpg` has already stripped `config.gpg` at
    // this point, so a value of None means the operator explicitly
    // opted out.
    let generator_for_manager = build_generator(
        config,
        locations,
        db_pool.clone(),
        cli.cache_directory.clone(),
    )
    .await?;

    let generator_manager = Arc::new(
        GeneratorManager::new(
            config.clone(),
            generator_for_manager,
            scanner_for_manager,
            database_for_manager,
            janitor_archive::manager::GeneratorManagerConfig::default(),
        )
        .await?,
    );

    // Track last-publish times in a shared map so the web /ready and
    // /last-publish handlers can report accurately.
    let last_publish_times = janitor_archive::web::new_last_publish_times();
    generator_manager
        .set_publish_observer(last_publish_times.clone())
        .await;

    // Wire the runner 'result' pub/sub listener to the generator
    // manager. Failing to set it up is fatal.
    let _runner_listener = janitor_archive::redis::start_runner_listener(
        locations.redis.as_deref(),
        generator_manager.clone(),
    )
    .await?;
    info!("Runner pub/sub listener started");

    // Kick off the 12-hour periodic republish loop, alongside the web
    // server and the runner listener.
    let mut periodic =
        PeriodicServices::new(PeriodicConfig::default(), generator_manager.clone(), None);
    if let Err(e) = periodic.start().await {
        error!("Failed to start periodic services: {}", e);
    }
    // `PeriodicServices` owns its JoinHandles internally; keep the value
    // alive for the process lifetime by handing it to a detached task
    // that idles on the first shutdown signal. Dropping `periodic` here
    // would abort the loops via its Drop impl.
    tokio::spawn(async move {
        let _keep_alive = periodic;
        // Park forever; when the process exits, the tokio runtime tears
        // this task down along with everything else.
        std::future::pending::<()>().await;
    });

    // Build a separate RepositoryGenerator for the web service (the
    // manager already owns the other instances). Must be GPG-aware
    // for the same reason as the manager's generator: on-demand and
    // /publish paths call through it too.
    let generator = build_generator(
        config,
        locations,
        db_pool.clone(),
        cli.cache_directory.clone(),
    )
    .await?;

    // Initialize web service
    let web_service = ArchiveWebService::with_publish_observer(
        config.clone(),
        generator,
        PackageScanner::with_cache(&locations.artifacts, cli.cache_directory.clone()).await?,
        ArchiveDatabase::new(db_pool),
        generator_manager,
        last_publish_times,
    )
    .await?;

    web_service.serve(&bind_address).await
}

/// Clean up old repository files.
async fn cleanup_repositories(config: &ArchiveConfig, locations: &Locations) -> ArchiveResult<()> {
    let db_pool = locations.connect_database().await?;
    let database = Arc::new(ArchiveDatabase::new(db_pool));
    let scanner = Arc::new(PackageScanner::new(&locations.artifacts).await?);
    let repo_config = RepositoryGenerationConfig::default();
    let generator = RepositoryGenerator::new(scanner, database, repo_config);

    for (name, repo_config) in &config.repositories {
        info!("Cleaning up repository: {}", name);
        generator.cleanup_repository(repo_config).await?;
    }

    Ok(())
}
