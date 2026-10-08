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
    config: PathBuf,

    #[clap(long)]
    /// Backup directory to write files to if artifact or log manager is unreachable.
    backup_directory: Option<PathBuf>,

    #[clap(long)]
    /// Public vcs location (used for URLs handed to worker).
    public_vcs_location: String,

    #[clap(long)]
    /// Base location for our own APT archive
    public_apt_archive_location: Option<String>,

    #[clap(long)]
    /// URL of the dependency server handed to workers.
    public_dep_server_url: Option<String>,

    #[clap(flatten)]
    logging: janitor::logging::LoggingArgs,

    #[clap(long, default_value = "60")]
    /// Time before marking a run as having timed out (minutes)
    run_timeout: u64,

    #[clap(long)]
    /// Avoid processing runs on a host (e.g. 'salsa.debian.org')
    avoid_host: Vec<String>,

    // The Python runner accepted these but never used them; they're
    // kept so existing deployments don't fail to start.
    #[clap(long, hide = true)]
    pre_check: Option<String>,

    #[clap(long, hide = true)]
    post_check: Option<String>,

    #[clap(long, hide = true)]
    use_cached_only: bool,
}

#[tokio::main]
async fn main() -> Result<(), i32> {
    let args = Args::parse();

    // Tracing is initialized inside Application::build() via
    // crate::tracing::init_tracing; avoid installing a second log
    // subscriber here (LoggingArgs::init installs env_logger, which
    // conflicts with the tracing-subscriber registry later).

    let unused_flags = [
        ("--pre-check", args.pre_check.is_some()),
        ("--post-check", args.post_check.is_some()),
        ("--use-cached-only", args.use_cached_only),
    ];
    for (flag, _) in unused_flags.iter().filter(|(_, set)| *set) {
        eprintln!("warning: {} has no effect and will be removed", flag);
    }

    #[cfg(feature = "gcp")]
    let gcp_logging = args.logging.gcp_logging;
    #[cfg(not(feature = "gcp"))]
    let gcp_logging = false;

    let app_builder = Application::builder_from_file(&args.config)
        .map_err(|e| {
            eprintln!("{}", e);
            1
        })?
        .with_debug(args.logging.debug)
        .with_gcp_logging(gcp_logging)
        .with_backup_directory(args.backup_directory)
        .with_public_apt_archive_location(args.public_apt_archive_location)
        .with_public_vcs_location(args.public_vcs_location)
        .with_public_dep_server_url(args.public_dep_server_url)
        .with_run_timeout_minutes(args.run_timeout)
        .with_avoid_hosts(args.avoid_host);

    let listen_address = args.listen_address;
    let port = args.port;
    let public_port = args.public_port;

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
    //   (`web::public_app`), which like the Python runner mounts the
    //   worker routes under `/runner/` behind `authenticate_worker`.
    //   A reverse proxy should forward `/runner/` to this port
    //   without stripping the prefix.
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
