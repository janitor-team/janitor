use clap::Parser;
use janitor_publish::rate_limiter::{
    FixedRateLimiter, NonRateLimiter, RateLimiter, SlowStartRateLimiter,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use url::Url;

#[derive(Parser)]
struct Args {
    /// Maximum number of open merge proposals per bucket.
    #[clap(long)]
    max_mps_per_bucket: Option<usize>,

    /// Prometheus push gateway to export to.
    #[clap(long)]
    prometheus: Option<Url>,

    /// Just do one pass over the queue, don't run as a daemon.
    #[clap(long, conflicts_with = "no_auto_publish")]
    once: bool,

    /// Listen address
    #[clap(long, default_value = "0.0.0.0")]
    listen_address: std::net::IpAddr,

    /// Listen port
    #[clap(long, default_value = "9912")]
    port: u16,

    /// Seconds to wait in between publishing pending proposals
    #[clap(long, default_value = "7200")]
    interval: i64,

    /// Do not create merge proposals automatically.
    #[clap(long, conflicts_with = "once")]
    no_auto_publish: bool,

    /// Path to load configuration from.
    #[clap(long, default_value = "janitor.conf")]
    config: std::path::PathBuf,

    /// Use slow start rate limiter.
    #[clap(long)]
    slowstart: bool,

    /// Limit number of pushes per cycle.
    #[clap(long)]
    push_limit: Option<usize>,

    /// Require a binary diff when publishing merge requests.
    #[clap(long)]
    require_binary_diff: bool,

    /// Maximum number of merge proposals to update per cycle.
    #[clap(long)]
    modify_mp_limit: Option<i32>,

    /// Maximum number of unexpected errors to encounter before stopping.
    #[clap(long)]
    unexpected_mp_limit: Option<i32>,

    /// External URL
    #[clap(long)]
    external_url: Option<Url>,

    /// Differ URL.
    #[clap(long, default_value = "http://localhost:9920/")]
    differ_url: Url,

    #[clap(flatten)]
    logging: janitor::logging::LoggingArgs,

    /// Path to merge proposal templates
    #[clap(long)]
    template_env_path: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), i32> {
    let args = Args::parse();

    args.logging.init();

    // breezyshim::init() runs init_git() + init_bzr() which import
    // breezy.git and breezy.bzr — that's what registers
    // RemoteGitProber / LocalGitProber and the Bazaar formats.
    // Without this, Branch.open(<git url>) falls through every
    // registered prober (only the Bazaar ones load eagerly) and
    // raises NotBranchError; silver_platter logs it as "Failed to
    // open VCS branch (treating as unguessable)" and publish_one
    // surfaces it as `result_code=local-branch-missing` for every
    // git-store push attempt. Calling load_plugins() alone isn't
    // enough — that walks BRZ_PLUGIN_PATH (gitlab/github/launchpad
    // forges) but doesn't touch the built-in breezy submodules.
    // spawn_blocking because Python initialization (and plugin loading,
    // which may do network probing for forge connectors) must not run
    // directly on the Tokio executor thread.
    tokio::task::spawn_blocking(|| {
        breezyshim::init();
        let _ = breezyshim::plugin::load_plugins();
    })
    .await
    .map_err(|e| {
        log::error!("breezyshim init failed: {}", e);
        1
    })?;

    log::info!(
        "janitor-publish starting: loading config from {:?}",
        args.config
    );
    let config = Box::new(janitor::config::read_file(&args.config).map_err(|e| {
        log::error!("Failed to read config: {}", e);
        1
    })?);

    let config: &'static _ = Box::leak(config);

    let bucket_rate_limiter: Mutex<Box<dyn RateLimiter>> =
        std::sync::Mutex::new(if args.slowstart {
            Box::new(SlowStartRateLimiter::new(args.max_mps_per_bucket)) as Box<dyn RateLimiter>
        } else if let Some(max_mps_per_bucket) = args.max_mps_per_bucket {
            Box::new(FixedRateLimiter::new(max_mps_per_bucket)) as Box<dyn RateLimiter>
        } else {
            Box::new(NonRateLimiter) as Box<dyn RateLimiter>
        });

    let forge_rate_limiter = std::sync::Arc::new(RwLock::new(HashMap::new()));

    let vcs_managers = janitor::vcs::get_vcs_managers_from_config(config).map_err(|e| {
        log::error!("Failed to get VCS managers from config: {}", e);
        1
    })?;
    log::info!("creating database pool");
    let db = janitor::state::create_pool(config).await.map_err(|e| {
        log::error!("Failed to create database pool: {}", e);
        1
    })?;
    log::info!("database pool created");

    let (redis_async_connection, redis_manager) = if let Some(redis_location) =
        config.redis_location.as_ref()
    {
        log::info!("creating RedisManager for {}", redis_location);
        let redis_config = janitor::redis::RedisConfig::new(redis_location.to_string());
        let manager = janitor::redis::RedisManager::new(redis_config).map_err(|e| {
            log::error!("Failed to create redis manager: {}", e);
            1
        })?;
        log::info!("RedisManager created; fetching connection manager");

        let manager = std::sync::Arc::new(manager);

        // redis-rs ConnectionManager::new has no default timeout and
        // can hang indefinitely when the remote side accepts the TCP
        // connection but never completes the initial handshake.
        // Guard with a 30-second timeout so the pod doesn't sit
        // silently in startup probes if something's off.
        let connection = match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            manager.get_connection_manager(),
        )
        .await
        {
            Ok(Ok(c)) => {
                log::info!("redis connection manager obtained");
                Some(c)
            }
            Ok(Err(e)) => {
                log::error!(
                    "Failed to get redis connection: {}; continuing without async connection",
                    e
                );
                None
            }
            Err(_) => {
                log::warn!(
                        "redis ConnectionManager::new timed out after 30s; continuing without async connection"
                    );
                None
            }
        };

        (connection, Some(manager))
    } else {
        log::info!("no redis_location configured; skipping redis");
        (None, None)
    };

    let lock_manager = config
        .redis_location
        .as_deref()
        .map(|redis_location| rslock::LockManager::new(vec![redis_location]));

    log::info!("building publish worker");
    let publish_worker = janitor_publish::PublishWorker::new(
        args.template_env_path,
        args.external_url,
        args.differ_url,
        redis_async_connection.clone(),
        redis_manager.clone(),
        lock_manager,
    )
    .await;
    log::info!("publish worker built; initializing GPG context");

    // Set up health checker
    let mut health_checker = janitor_publish::health::BasicHealthChecker::with_info(
        "publish".to_string(),
        env!("CARGO_PKG_VERSION").to_string(),
    );

    // Add database health check
    health_checker = health_checker.add_component_check(Arc::new(
        janitor_publish::health::DatabaseHealthChecker::new(db.clone()),
    ));

    // Add Redis health check if Redis is configured
    if let Some(redis_location) = config.redis_location.as_ref() {
        if let Ok(client) = redis::Client::open(redis_location.as_str()) {
            health_checker = health_checker.add_component_check(Arc::new(
                janitor_publish::health::RedisHealthChecker::new(client),
            ));
        }
    }

    let health_checker = Arc::new(health_checker);

    log::info!("constructing GPG context (may take a moment)");
    let gpg = Arc::new(breezyshim::gpg::GPGContext::new());
    log::info!("GPG context constructed; constructing AppState");
    let state = Arc::new(janitor_publish::AppState {
        conn: db.clone(),
        bucket_rate_limiter: Arc::new(bucket_rate_limiter),
        forge_rate_limiter,
        push_limit: args.push_limit,
        config,
        redis: redis_async_connection,
        redis_manager,
        vcs_managers: Arc::new(vcs_managers),
        publish_worker,
        modify_mp_limit: args.modify_mp_limit,
        unexpected_mp_limit: args.unexpected_mp_limit,
        gpg,
        require_binary_diff: args.require_binary_diff,
        health_checker,
        last_full_scan_at: tokio::sync::watch::channel(None).0,
    });

    if args.once {
        janitor_publish::publish_pending_ready(state, None, false)
            .await
            .map_err(|e| {
                log::error!("Failed to publish pending proposals: {}", e);
                1
            })?;

        if let Some(prometheus) = args.prometheus.as_ref() {
            janitor::prometheus::push_to_gateway(
                prometheus,
                "janitor.publish",
                maplit::hashmap! {},
                prometheus::default_registry(),
            )
            .await
            .unwrap();
        }
    } else {
        tokio::spawn(janitor_publish::process_queue_loop(
            state.clone(),
            chrono::Duration::seconds(args.interval),
            !args.no_auto_publish,
            None,  // push_limit
            None,  // modify_mp_limit
            false, // require_binary_diff
        ));

        tokio::spawn(janitor_publish::refresh_bucket_mp_counts(state.clone()));

        let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel(1);
        tokio::spawn(janitor_publish::listen_to_runner(
            state.clone(),
            shutdown_rx,
        ));

        let app = janitor_publish::web::app(state.clone());

        // run it
        let addr = SocketAddr::new(args.listen_address, args.port);
        log::info!("listening on {}", addr);

        let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
            log::error!("Failed to bind listener: {}", e);
            1
        })?;
        axum::serve(listener, app.into_make_service())
            .await
            .map_err(|e| {
                log::error!("Server error: {}", e);
                1
            })?;
    }

    Ok(())
}
