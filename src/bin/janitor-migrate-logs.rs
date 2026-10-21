//! Move the log files of all runs from one log store to another.

use clap::Parser;
use futures::StreamExt;
use janitor::logs::{get_log_manager, Error as LogError, LogFileManager};
use std::io::Write;
use std::num::NonZeroUsize;
use std::process::ExitCode;

const WORKER_LOG_FILENAME: &str = "worker.log";
const BUILD_LOG_FILENAME: &str = "build.log";

#[derive(Parser)]
#[command(about = "Move run logs from one log store to another")]
struct Args {
    #[clap(long, default_value = "janitor.conf")]
    /// Path to configuration.
    config: std::path::PathBuf,

    #[clap(long)]
    /// List what would be moved, but don't change anything.
    dry_run: bool,

    #[clap(long)]
    /// Copy the logs, leaving them in place in the source.
    keep: bool,

    #[clap(long, default_value = "100")]
    /// Number of runs to process concurrently.
    concurrency: NonZeroUsize,

    #[clap(value_parser = clap::builder::NonEmptyStringValueParser::new())]
    /// Location to move logs from.
    from_location: String,

    #[clap(value_parser = clap::builder::NonEmptyStringValueParser::new())]
    /// Location to move logs to.
    to_location: String,

    #[clap(flatten)]
    logging: janitor::logging::LoggingArgs,
}

#[derive(Debug, Clone, Copy, Default)]
struct Options {
    dry_run: bool,
    keep: bool,
}

#[derive(Debug)]
enum Error {
    /// An operation on a specific log file failed.
    Log {
        action: &'static str,
        name: String,
        error: LogError,
    },
    Io(std::io::Error),
    Database(sqlx::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::Log {
                action,
                name,
                error,
            } => write!(f, "failed to {} {}: {}", action, name, error),
            Error::Io(e) => write!(f, "I/O error: {}", e),
            Error::Database(e) => write!(f, "database error: {}", e),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        Error::Database(e)
    }
}

fn log_error<'a>(action: &'static str, name: &'a str) -> impl FnOnce(LogError) -> Error + 'a {
    move |error| Error::Log {
        action,
        name: name.to_string(),
        error,
    }
}

async fn has_log(
    manager: &dyn LogFileManager,
    codebase: &str,
    run_id: &str,
    name: &str,
) -> Result<bool, Error> {
    manager
        .has_log(codebase, run_id, name)
        .await
        .map_err(log_error("check for", name))
}

/// Find the names of the logs that exist for a run.
async fn discover_log_names(
    manager: &dyn LogFileManager,
    codebase: &str,
    run_id: &str,
) -> Result<Vec<String>, Error> {
    let mut names = Vec::new();
    for name in [WORKER_LOG_FILENAME, BUILD_LOG_FILENAME] {
        if has_log(manager, codebase, run_id, name).await? {
            names.push(name.to_string());
        }
    }
    for i in 1.. {
        let name = format!("{}.{}", BUILD_LOG_FILENAME, i);
        if !has_log(manager, codebase, run_id, &name).await? {
            break;
        }
        names.push(name);
    }
    Ok(names)
}

/// The logs of a run that were moved, and those left where they were.
#[derive(Debug, Default, PartialEq, Eq)]
struct Outcome {
    moved: Vec<String>,
    skipped: Vec<String>,
}

/// Move the named logs of a run, leaving those the destination already has.
async fn migrate_run_logs(
    from_manager: &dyn LogFileManager,
    to_manager: &dyn LogFileManager,
    codebase: &str,
    run_id: &str,
    names: &[String],
    options: Options,
) -> Result<Outcome, Error> {
    let mut outcome = Outcome::default();
    for name in names {
        if name.contains('/') {
            log::warn!("Skipping {} of {}: the name contains '/'", name, run_id);
            continue;
        }
        if has_log(to_manager, codebase, run_id, name).await? {
            // Never import over, or remove, a log the destination has.
            if has_log(from_manager, codebase, run_id, name).await? {
                log::info!(
                    "{} of {} is already in the destination, leaving it",
                    name,
                    run_id
                );
                outcome.skipped.push(name.clone());
            }
            continue;
        }
        if options.dry_run {
            if has_log(from_manager, codebase, run_id, name).await? {
                outcome.moved.push(name.clone());
            }
            continue;
        }
        let mut log = match from_manager.get_log(codebase, run_id, name).await {
            Ok(log) => log,
            Err(LogError::NotFound) => continue,
            Err(e) => return Err(log_error("fetch", name)(e)),
        };
        let tmp = tokio::task::spawn_blocking(move || -> std::io::Result<_> {
            let mut tmp = tempfile::NamedTempFile::new()?;
            std::io::copy(&mut log, &mut tmp)?;
            tmp.flush()?;
            Ok(tmp)
        })
        .await
        .map_err(std::io::Error::other)??;
        let path = tmp.path().to_str().ok_or_else(|| {
            std::io::Error::other(format!("non-UTF-8 temporary path {:?}", tmp.path()))
        })?;
        to_manager
            .import_log(codebase, run_id, path, None, Some(name))
            .await
            .map_err(log_error("import", name))?;
        if !options.keep {
            from_manager
                .delete_log(codebase, run_id, name)
                .await
                .map_err(log_error("delete", name))?;
        }
        outcome.moved.push(name.clone());
    }
    Ok(outcome)
}

/// Process a single run, recording its log names if none are stored.
async fn process_run(
    pool: &sqlx::PgPool,
    from_manager: &dyn LogFileManager,
    to_manager: &dyn LogFileManager,
    codebase: &str,
    run_id: &str,
    logfilenames: Option<Vec<String>>,
    options: Options,
) -> Result<Outcome, Error> {
    let logfilenames = match logfilenames {
        Some(names) if !names.is_empty() => names,
        _ => {
            let names = discover_log_names(from_manager, codebase, run_id).await?;
            if !options.dry_run {
                sqlx::query("UPDATE run SET logfilenames = $1 WHERE id = $2")
                    .bind(&names)
                    .bind(run_id)
                    .execute(pool)
                    .await?;
            }
            names
        }
    };
    let outcome = migrate_run_logs(
        from_manager,
        to_manager,
        codebase,
        run_id,
        &logfilenames,
        options,
    )
    .await?;
    if options.dry_run {
        log::info!("Would process {} ({:?})", run_id, outcome.moved);
    } else {
        log::info!("Processed {} ({:?})", run_id, outcome.moved);
    }
    Ok(outcome)
}

/// Process all runs; returns the number of runs, failed runs and skipped logs.
async fn process_all_runs(
    pool: &sqlx::PgPool,
    from_manager: &dyn LogFileManager,
    to_manager: &dyn LogFileManager,
    concurrency: usize,
    options: Options,
) -> Result<(usize, usize, usize), Error> {
    let rows: Vec<(String, String, Option<Vec<String>>)> =
        sqlx::query_as("SELECT codebase, id, logfilenames FROM run")
            .fetch_all(pool)
            .await?;
    let total = rows.len();
    let (failed, skipped) = futures::stream::iter(rows)
        .map(|(codebase, run_id, logfilenames)| async move {
            match process_run(
                pool,
                from_manager,
                to_manager,
                &codebase,
                &run_id,
                logfilenames,
                options,
            )
            .await
            {
                Ok(outcome) => (0, outcome.skipped.len()),
                Err(e) => {
                    log::error!("Error processing run {}: {}", run_id, e);
                    (1, 0)
                }
            }
        })
        .buffer_unordered(concurrency)
        .fold((0, 0), |(failed, skipped), (f, s)| {
            std::future::ready((failed + f, skipped + s))
        })
        .await;
    Ok((total, failed, skipped))
}

/// Move the logs of all runs; fails if the source is missing or any run fails.
async fn migrate(
    pool: &sqlx::PgPool,
    from_location: &str,
    to_location: &str,
    concurrency: usize,
    options: Options,
) -> Result<usize, String> {
    let remote = ["gs://", "http://", "https://"]
        .iter()
        .any(|scheme| from_location.starts_with(scheme));
    if !remote && !std::path::Path::new(from_location).is_dir() {
        return Err(format!("Source {} does not exist", from_location));
    }
    let from_manager = get_log_manager(Some(from_location))
        .await
        .map_err(|e| format!("Unable to open {}: {}", from_location, e))?;
    let to_manager = get_log_manager(Some(to_location))
        .await
        .map_err(|e| format!("Unable to open {}: {}", to_location, e))?;
    let (total, failed, skipped) = process_all_runs(
        pool,
        from_manager.as_ref(),
        to_manager.as_ref(),
        concurrency,
        options,
    )
    .await
    .map_err(|e| e.to_string())?;
    if skipped > 0 {
        log::info!("Left {} logs that are already in the destination", skipped);
    }
    if failed > 0 {
        return Err(format!("Failed to process {} of {} runs", failed, total));
    }
    Ok(total)
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

    let pool = match janitor::state::create_pool(&config).await {
        Ok(pool) => pool,
        Err(e) => {
            log::error!("Unable to connect to database: {}", e);
            return ExitCode::FAILURE;
        }
    };

    let options = Options {
        dry_run: args.dry_run,
        keep: args.keep,
    };
    match migrate(
        &pool,
        &args.from_location,
        &args.to_location,
        args.concurrency.get(),
        options,
    )
    .await
    {
        Ok(total) => {
            log::info!("Processed {} runs", total);
            ExitCode::SUCCESS
        }
        Err(e) => {
            log::error!("{}", e);
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use janitor::logs::FileSystemLogFileManager;
    use tempfile::TempDir;

    const CODEBASE: &str = "codebase";
    const RUN_ID: &str = "run-1";

    fn write_log(td: &TempDir, name: &str, content: &str) {
        let dir = td.path().join(CODEBASE).join(RUN_ID);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), content).unwrap();
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[tokio::test]
    async fn test_discover() {
        let td = TempDir::new().unwrap();
        let mgr = FileSystemLogFileManager::new(td.path()).unwrap();
        for name in ["worker.log", "build.log", "build.log.1", "build.log.2"] {
            write_log(&td, name, "content\n");
        }
        // Not found, since build.log.3 is missing.
        write_log(&td, "build.log.4", "content\n");

        assert_eq!(
            discover_log_names(&mgr, CODEBASE, RUN_ID).await.unwrap(),
            names(&["worker.log", "build.log", "build.log.1", "build.log.2"])
        );
    }

    #[cfg(feature = "testing")]
    mod database {
        use super::*;
        use std::io::Read;

        async fn setup_database(logfilenames: &str) -> Option<janitor::test_utils::TestDatabase> {
            let db = janitor::test_utils::TestDatabase::new_optional()
                .await
                .unwrap()?;
            janitor::schema::setup_test_database(&db.pool)
                .await
                .unwrap();
            for stmt in [
                "INSERT INTO codebase (name) VALUES ('codebase')".to_string(),
                "INSERT INTO change_set (id, campaign) VALUES ('cs', 'lintian-fixes')".to_string(),
                format!(
                    "INSERT INTO run (id, codebase, suite, change_set, result_code, logfilenames) \
                     VALUES ('run-1', 'codebase', 'lintian-fixes', 'cs', 'success', {})",
                    logfilenames
                ),
            ] {
                sqlx::query(sqlx::AssertSqlSafe(stmt))
                    .execute(&db.pool)
                    .await
                    .unwrap();
            }
            Some(db)
        }

        /// Every file below a directory, relative to it, with its content.
        fn files(dir: &std::path::Path) -> Vec<(String, Vec<u8>)> {
            let mut files = Vec::new();
            let mut todo = vec![dir.to_path_buf()];
            while let Some(d) = todo.pop() {
                for entry in std::fs::read_dir(d).unwrap() {
                    let path = entry.unwrap().path();
                    if path.is_dir() {
                        todo.push(path);
                    } else {
                        let rel = path.strip_prefix(dir).unwrap().display().to_string();
                        files.push((rel, std::fs::read(&path).unwrap()));
                    }
                }
            }
            files.sort();
            files
        }

        async fn read_log(location: &TempDir, name: &str) -> String {
            let mgr = FileSystemLogFileManager::new(location.path()).unwrap();
            let mut content = String::new();
            mgr.get_log(CODEBASE, RUN_ID, name)
                .await
                .unwrap()
                .read_to_string(&mut content)
                .unwrap();
            content
        }

        fn location(td: &TempDir) -> &str {
            td.path().to_str().unwrap()
        }

        async fn stored_names(pool: &sqlx::PgPool) -> Vec<String> {
            sqlx::query_scalar("SELECT logfilenames FROM run WHERE id = 'run-1'")
                .fetch_one(pool)
                .await
                .unwrap()
        }

        #[tokio::test]
        async fn test_migrate_stores_names_and_moves() {
            let Some(db) = setup_database("'{}'").await else {
                return;
            };
            let from = TempDir::new().unwrap();
            let to = TempDir::new().unwrap();
            write_log(&from, "worker.log", "worker\n");
            write_log(&from, "build.log", "build\n");
            let before = files(from.path());

            let dry_run = Options {
                dry_run: true,
                ..Default::default()
            };
            let total = migrate(&db.pool, location(&from), location(&to), 100, dry_run).await;

            assert_eq!(total, Ok(1));
            assert_eq!(stored_names(&db.pool).await, Vec::<String>::new());
            assert_eq!(files(from.path()), before);
            assert_eq!(files(to.path()), vec![]);

            let total = migrate(
                &db.pool,
                location(&from),
                location(&to),
                100,
                Options::default(),
            )
            .await;

            assert_eq!(total, Ok(1));
            assert_eq!(
                stored_names(&db.pool).await,
                names(&["worker.log", "build.log"])
            );
            assert_eq!(files(from.path()), vec![]);
            assert_eq!(read_log(&to, "worker.log").await, "worker\n");
            assert_eq!(read_log(&to, "build.log").await, "build\n");
        }

        #[tokio::test]
        async fn test_migrate_leaves_logs_the_destination_has() {
            let Some(db) = setup_database("ARRAY['worker.log', 'build.log']").await else {
                return;
            };
            let from = TempDir::new().unwrap();
            let to = TempDir::new().unwrap();
            write_log(&from, "worker.log", "worker\n");
            write_log(&from, "build.log", "build\n");
            write_log(&to, "worker.log", "other worker\n");

            let total = migrate(
                &db.pool,
                location(&from),
                location(&to),
                100,
                Options::default(),
            )
            .await;

            assert_eq!(total, Ok(1));
            assert_eq!(
                files(from.path()),
                vec![(
                    "codebase/run-1/worker.log".to_string(),
                    b"worker\n".to_vec()
                )]
            );
            assert_eq!(read_log(&to, "worker.log").await, "other worker\n");
            assert_eq!(read_log(&to, "build.log").await, "build\n");
        }

        #[tokio::test]
        async fn test_migrate_failed_run() {
            let Some(db) = setup_database("ARRAY['worker.log']").await else {
                return;
            };
            let from = TempDir::new().unwrap();
            write_log(&from, "worker.log", "worker\n");
            let before = files(from.path());
            // A destination that is a regular file can't hold any logs.
            let to = TempDir::new().unwrap();
            let to_path = to.path().join("file");
            std::fs::write(&to_path, "").unwrap();

            let total = migrate(
                &db.pool,
                location(&from),
                to_path.to_str().unwrap(),
                100,
                Options::default(),
            )
            .await;

            assert_eq!(total, Err("Failed to process 1 of 1 runs".to_string()));
            assert_eq!(files(from.path()), before);
        }

        #[tokio::test]
        async fn test_migrate_skips_name_with_slash() {
            let Some(db) = setup_database("ARRAY['../run-2/worker.log']").await else {
                return;
            };
            // An HTTP log store that has every log.
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), Default::default());
            gz.write_all(b"run-2 worker\n").unwrap();
            let body = gz.finish().unwrap();
            let app = axum::Router::new().fallback(move || std::future::ready(body.clone()));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let from = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, app).await });
            let to = TempDir::new().unwrap();
            std::fs::create_dir_all(to.path().join(CODEBASE).join("run-2")).unwrap();

            let keep = Options {
                keep: true,
                ..Default::default()
            };
            let total = migrate(&db.pool, &from, location(&to), 100, keep).await;

            assert_eq!(total, Ok(1));
            assert_eq!(files(to.path()), vec![]);
        }

        #[tokio::test]
        async fn test_migrate_missing_source() {
            let Some(db) = setup_database("ARRAY['worker.log']").await else {
                return;
            };
            let td = TempDir::new().unwrap();
            let missing = td.path().join("missing");
            let missing = missing.to_str().unwrap();

            let total = migrate(&db.pool, missing, location(&td), 100, Options::default()).await;

            assert_eq!(total, Err(format!("Source {} does not exist", missing)));
        }
    }
}
