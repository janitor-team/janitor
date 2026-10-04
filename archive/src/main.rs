//! CLI entry point for the archive service.

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info, warn};

use janitor::redis::RedisConfig;
use janitor_archive::{
    config::ArchiveConfig,
    database::ArchiveDatabase,
    error::ArchiveResult,
    manager::GeneratorManager,
    periodic::{PeriodicConfig, PeriodicServices},
    redis::RedisSubscriber,
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

    /// Dists directory (required for generation and serving).
    #[arg(long)]
    dists_directory: Option<PathBuf>,

    /// Use Google Cloud logging.
    #[arg(long)]
    gcp_logging: bool,

    /// Don't sign with GPG.
    #[arg(long)]
    no_gpg: bool,

    /// Show more detailed output.
    #[arg(long)]
    verbose: bool,

    /// Database connection URL (overrides config file).
    #[arg(long, env = "DATABASE_URL")]
    database_url: Option<String>,

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

    // Load configuration
    use janitor::shared_config::{ConfigLoader, ConfigSource};
    let config = if cli.config.exists() {
        info!("Loading configuration from: {:?}", cli.config);
        ArchiveConfig::from_sources(&[
            ConfigSource::File(cli.config.clone()),
            ConfigSource::Environment,
        ])?
    } else {
        warn!("Configuration file not found, using environment");
        ArchiveConfig::from_sources(&[ConfigSource::Defaults, ConfigSource::Environment])?
    };

    // Ensure dists directory exists if provided.
    if let Some(ref dists_dir) = cli.dists_directory {
        if let Err(e) = std::fs::create_dir_all(dists_dir) {
            error!("Failed to create dists directory {:?}: {}", dists_dir, e);
            return Err(janitor_archive::error::ArchiveError::Io(e));
        }
    }

    // The web server requires --dists-directory; standalone subcommands
    // that don't need it are allowed to omit it.
    match cli.command {
        Some(Cmd::Generate { ref suite }) => {
            generate_repositories(&config, suite.as_deref()).await?;
        }
        Some(Cmd::Serve) | None => {
            if cli.dists_directory.is_none() {
                error!(
                    "--dists-directory is required when running the web server. \
                     Pass --dists-directory=<path> or run the 'generate' subcommand."
                );
                return Err(janitor_archive::error::ArchiveError::InvalidConfiguration(
                    "--dists-directory is required".to_string(),
                ));
            }
            // With `--no-gpg`, strip the GPG config so every downstream
            // sign_release() call turns into a no-op.
            //
            // Rebase the per-repository `base_path` onto
            // `--dists-directory` so all writers/readers agree. Each
            // apt_repository lives under `<dists_directory>/<name>`.
            // Without this rebase the config's compiled-in
            // `archive_path/name` locations win silently and
            // `--dists-directory` becomes documentation.
            let mut config = config;
            if cli.no_gpg {
                config.gpg = None;
            }
            if let Some(ref dists_dir) = cli.dists_directory {
                config.archive_path = dists_dir.clone();
                for (name, repo) in config.repositories.iter_mut() {
                    repo.base_path = dists_dir.join(name);
                }
            }
            start_web_server(&cli, &config).await?;
        }
        Some(Cmd::Cleanup) => {
            cleanup_repositories(&config).await?;
        }
    }

    Ok(())
}

/// Resolve the artifact-manager URL for the loaded config.
///
/// The URL is split across `artifact_manager_url` (archive-specific)
/// and `external_services.artifact_service_url` (shared);
/// `artifact_manager_url()` resolves that precedence. Falls back to
/// "local://" for deployments that never set either, treating the
/// current directory as the artifact store.
fn artifact_location(config: &ArchiveConfig) -> String {
    config
        .artifact_manager_url()
        .map(str::to_string)
        .unwrap_or_else(|| "local://".to_string())
}

/// Build a RepositoryGenerator wired for signing when GPG is
/// configured and, when available, the loaded protobuf config so
/// the generator can walk `apt_repository.select` to resolve the
/// right `debian_build.distribution` per suite. Kept here because
/// both `start_web_server` and `generate_repositories` want the
/// same wiring.
async fn build_generator(
    config: &ArchiveConfig,
    db_pool: sqlx::PgPool,
    cache_directory: Option<PathBuf>,
) -> ArchiveResult<RepositoryGenerator> {
    let scanner =
        Arc::new(PackageScanner::with_cache(&artifact_location(config), cache_directory).await?);
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
async fn generate_repositories(config: &ArchiveConfig, suite: Option<&str>) -> ArchiveResult<()> {
    let database_url = config.base.database.as_ref().ok_or_else(|| {
        janitor_archive::error::ArchiveError::InvalidConfiguration(
            "No database URL configured".to_string(),
        )
    })?;
    let db_pool = sqlx::PgPool::connect(&database_url.url)
        .await
        .map_err(janitor_archive::error::ArchiveError::Database)?;
    let generator = build_generator(config, db_pool, None).await?;

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
async fn start_web_server(cli: &Cli, config: &ArchiveConfig) -> ArchiveResult<()> {
    // Compose the bind address from --listen-address and --port;
    // `--bind` is accepted as an override for deployments that pass
    // a single socket string.
    let bind_address = cli
        .bind
        .clone()
        .unwrap_or_else(|| format!("{}:{}", cli.listen_address, cli.port));
    info!("Starting web server on: {}", bind_address);

    let database_url = cli
        .database_url
        .clone()
        .or_else(|| config.base.database.as_ref().map(|d| d.url.clone()))
        .ok_or_else(|| {
            janitor_archive::error::ArchiveError::InvalidConfiguration(
                "No database URL configured".to_string(),
            )
        })?;

    let db_pool = sqlx::PgPool::connect(&database_url)
        .await
        .map_err(janitor_archive::error::ArchiveError::Database)?;

    // Build shared components for the GeneratorManager and the web service.
    // All scanners share the same cache directory so a Packages/
    // Sources scan performed by one code path is reusable by the
    // others.
    let scanner_for_manager =
        PackageScanner::with_cache(&artifact_location(config), cli.cache_directory.clone()).await?;
    let database_for_manager = ArchiveDatabase::new(db_pool.clone());
    // Build a RepositoryGenerator that signs Release when GPG is
    // configured. `--no-gpg` has already stripped `config.gpg` at
    // this point, so a value of None means the operator explicitly
    // opted out.
    let generator_for_manager =
        build_generator(config, db_pool.clone(), cli.cache_directory.clone()).await?;

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
    // manager.
    if let Some(redis_cfg) = config.base.redis.as_ref() {
        let redis_config = RedisConfig::new(redis_cfg.url.clone());
        match RedisSubscriber::new(redis_config, generator_manager.clone()).await {
            Ok(mut subscriber) => match subscriber.listen_to_runner().await {
                Ok(_handle) => info!("Runner pub/sub listener started"),
                Err(e) => error!("Failed to start runner pub/sub listener: {}", e),
            },
            Err(e) => error!("Failed to connect to Redis for runner listener: {}", e),
        }
    } else {
        warn!(
            "No Redis URL in config; automatic archive regeneration on build completion is disabled"
        );
    }

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
    let generator = build_generator(config, db_pool.clone(), cli.cache_directory.clone()).await?;

    // Initialize web service
    let web_service = ArchiveWebService::with_publish_observer(
        config.clone(),
        generator,
        PackageScanner::with_cache(&artifact_location(config), cli.cache_directory.clone()).await?,
        ArchiveDatabase::new(db_pool),
        generator_manager,
        last_publish_times,
    )
    .await?;

    web_service.serve(&bind_address).await
}

/// Clean up old repository files.
async fn cleanup_repositories(config: &ArchiveConfig) -> ArchiveResult<()> {
    let database_url = config.base.database.as_ref().ok_or_else(|| {
        janitor_archive::error::ArchiveError::InvalidConfiguration(
            "No database URL configured".to_string(),
        )
    })?;
    let db_pool = sqlx::PgPool::connect(&database_url.url)
        .await
        .map_err(janitor_archive::error::ArchiveError::Database)?;
    let database = Arc::new(ArchiveDatabase::new(db_pool));
    let scanner = Arc::new(PackageScanner::new(&artifact_location(config)).await?);
    let repo_config = RepositoryGenerationConfig::default();
    let generator = RepositoryGenerator::new(scanner, database, repo_config);

    for (name, repo_config) in &config.repositories {
        info!("Cleaning up repository: {}", name);
        generator.cleanup_repository(repo_config).await?;
    }

    Ok(())
}
