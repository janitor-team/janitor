use clap::Parser;
use janitor_types::{Campaign, CodebaseName};
use std::time::Instant;

#[derive(Parser)]
struct Args {
    #[clap(long)]
    /// Create branches but don't push or propose anything.
    dry_run: bool,

    #[clap(long)]
    /// Prometheus push gateway to export to.
    prometheus: Option<String>,

    #[clap(long, default_value = "janitor.conf")]
    /// Path to configuration.
    config: std::path::PathBuf,

    #[clap(long)]
    /// Restrict to a specific campaign.
    campaign: Option<Campaign>,

    /// Codebase to process.
    codebases: Vec<CodebaseName>,

    #[clap(long)]
    /// Bucket to use.
    bucket: Option<String>,

    #[clap(long)]
    /// Requester to use.
    requester: Option<String>,

    #[clap(long)]
    /// Refresh the queue.
    refresh: bool,

    #[clap(flatten)]
    logging: janitor::logging::LoggingArgs,
}

#[tokio::main]
async fn main() -> Result<(), i32> {
    let args = Args::parse();
    let start_time = Instant::now();

    // LoggingArgs::init() handles `--gcp-logging` itself when the
    // crate is built with `feature = "gcp"` (see
    // src/logging.rs::LoggingArgs). The duplicate bin-level flag
    // that used to live here was a stub that only logged an info
    // line.
    args.logging.init();

    log::info!("Starting janitor scheduler");
    log::info!("Reading configuration from: {:?}", args.config);

    let config = janitor::config::read_file(&args.config).map_err(|e| {
        log::error!("Failed to read configuration: {}", e);
        1
    })?;

    log::info!("Connecting to database");
    let db = janitor::state::create_pool(&config).await.map_err(|e| {
        log::error!("Failed to connect to database: {}", e);
        1
    })?;

    // Log configuration summary
    if let Some(ref campaign) = args.campaign {
        log::info!("Filtering by campaign: {}", campaign);
    }
    if !args.codebases.is_empty() {
        log::info!("Processing {} specific codebases", args.codebases.len());
    }
    if args.dry_run {
        log::warn!("DRY RUN MODE: No actual scheduling will occur");
    }

    log::info!("Finding candidates with publish policy");
    let todo = janitor::schedule::iter_schedule_requests_from_candidates(
        &db,
        if args.codebases.is_empty() {
            None
        } else {
            Some(args.codebases.as_slice())
        },
        args.campaign.as_ref(),
    )
    .await
    .map_err(|e| {
        log::error!("Failed to find candidates: {}", e);
        1
    })?
    .collect::<Vec<_>>();

    if todo.is_empty() {
        log::info!("No candidates found for scheduling");
        return Ok(());
    }

    log::info!("Determined schedule for {} candidates", todo.len());

    // Log some statistics about what we're scheduling
    let campaigns: std::collections::HashSet<_> = todo.iter().map(|t| t.campaign()).collect();
    let codebases: std::collections::HashSet<_> = todo.iter().map(|t| t.codebase()).collect();
    log::info!(
        "Scheduling across {} campaigns and {} codebases",
        campaigns.len(),
        codebases.len()
    );

    if args.dry_run {
        log::info!("DRY RUN: Would schedule {} items", todo.len());
        for item in &todo {
            log::debug!(
                "Would schedule: {} on {} (campaign: {}, value: {})",
                item.command(),
                item.codebase(),
                item.campaign(),
                item.value()
            );
        }
    } else {
        log::info!("Adding {} items to queue", todo.len());

        janitor::schedule::bulk_add_to_queue(
            &db,
            &todo,
            args.dry_run,
            0.0, // default_offset
            args.bucket.as_deref(),
            args.requester.as_deref(),
            args.refresh,
        )
        .await
        .map_err(|e| {
            log::error!("Failed to add items to queue: {}", e);
            1
        })?;

        log::info!("Successfully scheduled {} items", todo.len());
    }

    let duration = start_time.elapsed();
    log::info!("Scheduling completed in {:.2}s", duration.as_secs_f64());

    // Prometheus metrics export if configured
    if let Some(prometheus_gateway) = args.prometheus {
        log::info!(
            "Exporting metrics to Prometheus gateway: {}",
            prometheus_gateway
        );

        // Match the Python scheduler's metric exactly so existing dashboards
        // and the aiohttp-openmetrics exporter keep working: gauge name
        // `job_last_success_unixtime` pushed under job `janitor.schedule`.
        let success_gauge = prometheus::Gauge::new(
            "job_last_success_unixtime",
            "Last time a batch job successfully finished",
        )
        .map_err(|e| {
            log::error!("Failed to create Prometheus gauge: {}", e);
            1
        })?;
        let registry = prometheus::Registry::new();
        registry
            .register(Box::new(success_gauge.clone()))
            .map_err(|e| {
                log::error!("Failed to register Prometheus gauge: {}", e);
                1
            })?;

        success_gauge.set(chrono::Utc::now().timestamp() as f64);

        let metric_families = registry.gather();
        if let Err(e) = prometheus::push_metrics(
            "janitor.schedule",
            prometheus::labels! {},
            &prometheus_gateway,
            metric_families,
            None,
        ) {
            log::warn!("Failed to push metrics to Prometheus: {}", e);
            // Don't fail the entire run for metrics issues
        } else {
            log::info!("Metrics pushed to Prometheus successfully");
        }
    }

    log::info!("Scheduler finished successfully");
    Ok(())
}
