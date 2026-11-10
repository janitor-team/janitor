use clap::Parser;
use janitor_runner::application::Application;
use std::path::PathBuf;

#[derive(Parser)]
struct Args {
    /// Address to listen on; repeat for more. 0.0.0.0 is IPv4 only, :: is IPv6 only.
    #[clap(long, default_value = "localhost")]
    listen_address: Vec<String>,

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

    #[clap(long, default_value_t = true, action = clap::ArgAction::Set)]
    /// Redirect artifact downloads to the public URL of the artifact
    /// store, when it has an http(s) one (such as for GCS). Set to false
    /// if the store is not publicly readable.
    redirect_artifact_downloads: bool,

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
        .with_avoid_hosts(args.avoid_host)
        .with_redirect_artifact_downloads(args.redirect_artifact_downloads);

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

        let mut servers = tokio::task::JoinSet::new();
        for (name, port, router) in [
            ("Private (admin)", port, private_router),
            ("Public (worker)", public_port, public_router),
        ] {
            for listener in janitor_service::listen::bind_hosts(&listen_address, port).await? {
                log::info!("{} listener on {}", name, listener.local_addr()?);
                let router = router.clone();
                servers
                    .spawn(async move { axum::serve(listener, router.into_make_service()).await });
            }
        }

        // Run them all. If one errors we surface the first error and
        // the listeners shut down.
        while let Some(res) = servers.join_next().await {
            res??;
        }

        Ok(())
    })
    .await
    .map_err(|e| {
        eprintln!("Application error: {}", e);
        1
    })?;

    Ok(())
}
