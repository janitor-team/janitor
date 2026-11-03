use clap::Parser;
use janitor_types::{Campaign, CodebaseName};

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

    args.logging.init();

    log::info!("Reading configuration");

    let config = janitor::config::read_file(&args.config).unwrap();

    let db = janitor::state::create_pool(&config).await.unwrap();

    log::info!("Finding candidates with policy");
    log::info!("Determining schedule for candidates");
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
        log::error!("Error: {}", e);
        1
    })?
    .collect::<Vec<_>>();

    log::info!("Adding {} items to queue", todo.len());

    janitor::schedule::bulk_add_to_queue(
        &db,
        &todo,
        args.dry_run,
        0.0,
        args.bucket.as_deref(),
        args.requester.as_deref(),
        args.refresh,
    )
    .await
    .unwrap();
    Ok(())
}
