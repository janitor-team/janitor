use clap::Parser;
use std::process::ExitCode;

#[derive(Parser)]
/// Move run logs from one log store to another.
struct Args {
    #[clap(long, default_value = "janitor.conf")]
    /// Path to configuration.
    config: std::path::PathBuf,

    #[clap(long, default_value_t = 100)]
    /// Number of runs to process concurrently.
    concurrency: usize,

    /// Log store to move logs from.
    from_location: String,

    /// Log store to move logs to.
    to_location: String,

    #[clap(flatten)]
    logging: janitor::logging::LoggingArgs,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();

    args.logging.init();

    let config = match janitor::config::read_file(&args.config) {
        Ok(config) => config,
        Err(e) => {
            log::error!("Unable to read config {}: {}", args.config.display(), e);
            return ExitCode::FAILURE;
        }
    };

    let (from, to) = match (
        janitor::logs::create_log_manager(Some(&args.from_location)),
        janitor::logs::create_log_manager(Some(&args.to_location)),
    ) {
        (Ok(from), Ok(to)) => (from, to),
        (Err(e), _) | (_, Err(e)) => {
            log::error!("Unable to open log store: {}", e);
            return ExitCode::FAILURE;
        }
    };

    let db = match janitor::state::create_pool(&config).await {
        Ok(db) => db,
        Err(e) => {
            log::error!("Unable to connect to database: {}", e);
            return ExitCode::FAILURE;
        }
    };

    match janitor::migrate_logs::migrate_all_logs(&db, &from, &to, args.concurrency).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log::error!("{}", e);
            ExitCode::FAILURE
        }
    }
}
