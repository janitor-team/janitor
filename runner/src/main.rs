use clap::Parser;
use janitor_runner::application::Application;
use std::path::PathBuf;

#[derive(Parser)]
struct Args {
    #[clap(long, default_value = "localhost")]
    listen_address: String,

    #[clap(long, default_value = "9911")]
    port: u16,

    #[clap(long, default_value = "9919")]
    public_port: u16,

    #[clap(long, default_value = "janitor.conf")]
    /// Path to configuration.
    config: Option<PathBuf>,

    #[clap(long)]
    /// Backup directory to write files to if artifact or log manager is unreachable.
    backup_directory: Option<PathBuf>,

    #[clap(long)]
    /// Public vcs location (used for URLs handed to worker).
    /// If omitted, `git_location` from the config file is used.
    public_vcs_location: Option<String>,

    #[clap(long)]
    /// Base location for our own APT archive
    public_apt_archive_location: Option<String>,

    #[clap(flatten)]
    logging: janitor::logging::LoggingArgs,

    #[clap(long, default_value = "60")]
    /// Time before marking a run as having timed out (minutes)
    run_timeout: u64,

    #[clap(long)]
    /// Avoid processing runs on a host (e.g. 'salsa.debian.org')
    avoid_host: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<(), i32> {
    let args = Args::parse();

    // Tracing is initialized inside Application::build() via
    // crate::tracing::init_tracing; avoid installing a second log
    // subscriber here (LoggingArgs::init installs env_logger, which
    // conflicts with the tracing-subscriber registry later).

    // Build application from config file or use defaults
    let config_path = args.config.unwrap_or_else(|| PathBuf::from("janitor.conf"));

    let mut app_builder = if config_path.exists() {
        Application::builder_from_file(&config_path).map_err(|e| {
            eprintln!(
                "Failed to load config from {}: {}",
                config_path.display(),
                e
            );
            1
        })?
    } else {
        log::info!(
            "Config file {} not found, using defaults",
            config_path.display()
        );
        Application::builder()
    };

    // Store values before moving args
    let listen_address = args.listen_address.clone();
    let port = args.port;
    let public_port = args.public_port;

    // Override config with command line arguments
    app_builder = app_builder
        .with_listen_address(args.listen_address)
        .with_port(args.port)
        .with_debug(args.logging.debug)
        .with_backup_directory(args.backup_directory)
        .with_public_apt_archive_location(args.public_apt_archive_location)
        .with_public_vcs_location(args.public_vcs_location)
        .with_run_timeout_minutes(args.run_timeout)
        .with_avoid_hosts(args.avoid_host);

    // Build and initialize the application
    let app = app_builder.build().await.map_err(|e| {
        eprintln!("Failed to initialize application: {}", e);
        1
    })?;

    // Run the application with graceful shutdown.
    //
    // The runner serves two routers on separate ports:
    //
    // - private (--port, default 9911): admin/intra-cluster app
    //   (`web::app`) with no auth. Should never be reachable
    //   from outside the cluster -- endpoints include kill, schedule,
    //   admin/workers, candidates upload, etc.
    // - public (--public-port, default 9919): worker-facing app
    //   (`web::public_app`) with `authenticate_worker` middleware on
    //   the `/runner/...` worker routes. This is what the nginx
    //   ingress should target.
    //
    // The public listener was missing entirely -- only the private
    // app was being bound. The nginx ingress was forwarding
    // `/runner/(.*)` -> port 9911 (rewrite-target /$2) so all worker
    // traffic landed on the no-auth private app, defeating
    // authenticate_worker and exposing every admin endpoint to the
    // internet. Bind both now; ingress should be repointed at the
    // public port (and stop stripping `/runner/` since public_app
    // already mounts its routes under `/runner/`).
    app.run_with_graceful_shutdown(|state| async move {
        let trace = || axum::middleware::from_fn(janitor_runner::tracing::http_tracing_middleware);

        let private_router = janitor_runner::web::app(state.clone()).layer(trace());
        let public_router = janitor_runner::web::public_app(state.clone())
            .with_state(state.clone())
            .layer(trace());

        let private_addr = format!("{}:{}", listen_address, port);
        let public_addr = format!("{}:{}", listen_address, public_port);
        log::info!("Private (admin) listener on {}", private_addr);
        log::info!("Public (worker) listener on {}", public_addr);

        let private_listener = tokio::net::TcpListener::bind(&private_addr).await?;
        let public_listener = tokio::net::TcpListener::bind(&public_addr).await?;

        let private_serve = axum::serve(private_listener, private_router.into_make_service());
        let public_serve = axum::serve(public_listener, public_router.into_make_service());

        // Run both. If either errors we surface the first error and
        // both listeners shut down.
        tokio::try_join!(
            async {
                private_serve
                    .await
                    .map_err(Box::<dyn std::error::Error + Send + Sync>::from)
            },
            async {
                public_serve
                    .await
                    .map_err(Box::<dyn std::error::Error + Send + Sync>::from)
            },
        )?;

        Ok(())
    })
    .await
    .map_err(|e| {
        eprintln!("Application error: {}", e);
        1
    })?;

    Ok(())
}
