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

/// Process a single run, recording its log names if they were not known.
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
        Some(names) => names,
        None => {
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

    let from_manager = match get_log_manager(Some(&args.from_location)).await {
        Ok(manager) => manager,
        Err(e) => {
            log::error!("Unable to open {}: {}", args.from_location, e);
            return ExitCode::FAILURE;
        }
    };
    let to_manager = match get_log_manager(Some(&args.to_location)).await {
        Ok(manager) => manager,
        Err(e) => {
            log::error!("Unable to open {}: {}", args.to_location, e);
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
    let (total, failed, skipped) = match process_all_runs(
        &pool,
        from_manager.as_ref(),
        to_manager.as_ref(),
        args.concurrency.get(),
        options,
    )
    .await
    {
        Ok(counts) => counts,
        Err(e) => {
            log::error!("Error: {}", e);
            return ExitCode::FAILURE;
        }
    };
    if skipped > 0 {
        log::info!("Left {} logs that are already in the destination", skipped);
    }
    if failed > 0 {
        log::error!("Failed to process {} of {} runs", failed, total);
        ExitCode::FAILURE
    } else {
        log::info!("Processed {} runs", total);
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use janitor::logs::FileSystemLogFileManager;
    use std::io::Read;
    use tempfile::TempDir;

    const CODEBASE: &str = "codebase";
    const RUN_ID: &str = "run-1";

    fn setup() -> (TempDir, FileSystemLogFileManager) {
        let td = TempDir::new().unwrap();
        let mgr = FileSystemLogFileManager::new(td.path()).unwrap();
        (td, mgr)
    }

    fn write_log(td: &TempDir, name: &str, content: &str) {
        let dir = td.path().join(CODEBASE).join(RUN_ID);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), content).unwrap();
    }

    async fn read_log(mgr: &dyn LogFileManager, name: &str) -> String {
        let mut content = String::new();
        mgr.get_log(CODEBASE, RUN_ID, name)
            .await
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        content
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[tokio::test]
    async fn test_move() {
        let (from_td, from) = setup();
        let (_to_td, to) = setup();
        write_log(&from_td, "worker.log", "worker\n");
        write_log(&from_td, "build.log", "build\n");

        let todo = names(&["worker.log", "build.log"]);
        let moved = migrate_run_logs(&from, &to, CODEBASE, RUN_ID, &todo, Options::default())
            .await
            .unwrap();

        assert_eq!(moved.moved, todo);
        assert!(moved.skipped.is_empty());
        assert_eq!(read_log(&to, "worker.log").await, "worker\n");
        assert_eq!(read_log(&to, "build.log").await, "build\n");
        assert!(!from.has_log(CODEBASE, RUN_ID, "worker.log").await.unwrap());
        assert!(!from.has_log(CODEBASE, RUN_ID, "build.log").await.unwrap());
    }

    #[tokio::test]
    async fn test_keep() {
        let (from_td, from) = setup();
        let (_to_td, to) = setup();
        write_log(&from_td, "worker.log", "worker\n");

        let options = Options {
            keep: true,
            ..Default::default()
        };
        let todo = names(&["worker.log"]);
        let moved = migrate_run_logs(&from, &to, CODEBASE, RUN_ID, &todo, options)
            .await
            .unwrap();

        assert_eq!(moved.moved, todo);
        assert!(moved.skipped.is_empty());
        assert_eq!(read_log(&to, "worker.log").await, "worker\n");
        assert_eq!(read_log(&from, "worker.log").await, "worker\n");
    }

    #[tokio::test]
    async fn test_dry_run() {
        let (from_td, from) = setup();
        let (to_td, to) = setup();
        write_log(&from_td, "worker.log", "worker\n");

        let options = Options {
            dry_run: true,
            ..Default::default()
        };
        let todo = names(&["worker.log", "build.log"]);
        let moved = migrate_run_logs(&from, &to, CODEBASE, RUN_ID, &todo, options)
            .await
            .unwrap();

        assert_eq!(moved.moved, names(&["worker.log"]));
        assert_eq!(read_log(&from, "worker.log").await, "worker\n");
        assert_eq!(std::fs::read_dir(to_td.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn test_discover() {
        let (td, mgr) = setup();
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

    #[tokio::test]
    async fn test_discover_none() {
        let (_td, mgr) = setup();
        assert!(discover_log_names(&mgr, CODEBASE, RUN_ID)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn test_missing_skipped() {
        let (from_td, from) = setup();
        let (_to_td, to) = setup();
        write_log(&from_td, "build.log", "build\n");

        let todo = names(&["worker.log", "build.log"]);
        let moved = migrate_run_logs(&from, &to, CODEBASE, RUN_ID, &todo, Options::default())
            .await
            .unwrap();

        assert_eq!(moved.moved, names(&["build.log"]));
        assert!(!to.has_log(CODEBASE, RUN_ID, "worker.log").await.unwrap());
        assert_eq!(read_log(&to, "build.log").await, "build\n");
    }

    #[tokio::test]
    async fn test_import_failure_keeps_source() {
        let (from_td, from) = setup();
        write_log(&from_td, "worker.log", "worker\n");
        // A destination that is a regular file can't hold any logs.
        let to_td = TempDir::new().unwrap();
        let to_path = to_td.path().join("file");
        std::fs::write(&to_path, "").unwrap();
        let to = FileSystemLogFileManager::new(&to_path).unwrap();

        let todo = names(&["worker.log"]);
        let err = migrate_run_logs(&from, &to, CODEBASE, RUN_ID, &todo, Options::default())
            .await
            .unwrap_err();

        assert!(
            matches!(&err, Error::Log { action: "import", name, .. } if name == "worker.log"),
            "{:?}",
            err
        );
        assert_eq!(read_log(&from, "worker.log").await, "worker\n");
    }

    #[tokio::test]
    async fn test_move_compressed() {
        let (_from_td, from) = setup();
        let (_to_td, to) = setup();
        let orig = TempDir::new().unwrap();
        let orig_path = orig.path().join("build.log");
        std::fs::write(&orig_path, "build\n").unwrap();
        from.import_log(CODEBASE, RUN_ID, orig_path.to_str().unwrap(), None, None)
            .await
            .unwrap();

        let todo = names(&["build.log"]);
        let moved = migrate_run_logs(&from, &to, CODEBASE, RUN_ID, &todo, Options::default())
            .await
            .unwrap();

        assert_eq!(moved.moved, todo);
        assert!(moved.skipped.is_empty());
        assert_eq!(read_log(&to, "build.log").await, "build\n");
        assert!(!from.has_log(CODEBASE, RUN_ID, "build.log").await.unwrap());
    }

    /// Every file below a directory, with its content.
    fn snapshot(dir: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
        let mut files = Vec::new();
        let mut todo = vec![dir.to_path_buf()];
        while let Some(dir) = todo.pop() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    todo.push(path);
                } else {
                    let content = std::fs::read(&path).unwrap();
                    files.push((path, content));
                }
            }
        }
        files.sort();
        files
    }

    async fn import(mgr: &FileSystemLogFileManager, name: &str, content: &str) {
        let orig = TempDir::new().unwrap();
        let orig_path = orig.path().join(name);
        std::fs::write(&orig_path, content).unwrap();
        mgr.import_log(CODEBASE, RUN_ID, orig_path.to_str().unwrap(), None, None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_destination_has_log() {
        let (from_td, from) = setup();
        let (to_td, to) = setup();
        write_log(&from_td, "worker.log", "worker\n");
        write_log(&from_td, "build.log", "build\n");
        write_log(&from_td, "build.log.1", "build 1\n");
        write_log(&to_td, "worker.log", "other worker\n");
        import(&to, "build.log", "other build\n").await;
        let before = (snapshot(from_td.path()), snapshot(to_td.path()));

        let todo = names(&["worker.log", "build.log"]);
        for dry_run in [true, false] {
            let options = Options {
                dry_run,
                ..Default::default()
            };
            let outcome = migrate_run_logs(&from, &to, CODEBASE, RUN_ID, &todo, options)
                .await
                .unwrap();

            assert!(outcome.moved.is_empty());
            assert_eq!(outcome.skipped, todo);
            assert_eq!((snapshot(from_td.path()), snapshot(to_td.path())), before);
        }

        // A log that the destination does not have is still moved.
        let todo = names(&["worker.log", "build.log", "build.log.1"]);
        let outcome = migrate_run_logs(&from, &to, CODEBASE, RUN_ID, &todo, Options::default())
            .await
            .unwrap();

        assert_eq!(outcome.moved, names(&["build.log.1"]));
        assert_eq!(outcome.skipped, names(&["worker.log", "build.log"]));
        assert_eq!(read_log(&to, "build.log.1").await, "build 1\n");
        assert_eq!(read_log(&to, "worker.log").await, "other worker\n");
        assert_eq!(read_log(&to, "build.log").await, "other build\n");
        assert!(!from.has_log(CODEBASE, RUN_ID, "build.log.1").await.unwrap());
        assert_eq!(read_log(&from, "worker.log").await, "worker\n");
        assert_eq!(read_log(&from, "build.log").await, "build\n");
    }

    #[tokio::test]
    async fn test_destination_only() {
        let (_from_td, from) = setup();
        let (to_td, to) = setup();
        write_log(&to_td, "worker.log", "worker\n");

        let todo = names(&["worker.log"]);
        let before = snapshot(to_td.path());
        let outcome = migrate_run_logs(&from, &to, CODEBASE, RUN_ID, &todo, Options::default())
            .await
            .unwrap();

        assert_eq!(outcome, Outcome::default());
        assert_eq!(read_log(&to, "worker.log").await, "worker\n");
        // read_log prefers the plain name, so a .gz beside it is invisible.
        assert_eq!(snapshot(to_td.path()), before);
    }

    #[tokio::test]
    async fn test_same_directory() {
        let (td, from) = setup();
        let other = TempDir::new().unwrap();
        let link = other.path().join("link");
        std::os::unix::fs::symlink(td.path(), &link).unwrap();
        write_log(&td, "worker.log", "worker\n");
        import(&from, "build.log", "build\n").await;
        let before = snapshot(td.path());
        assert_eq!(before.len(), 2);

        let todo = names(&["worker.log", "build.log"]);
        for to_path in [td.path(), link.as_path()] {
            let to = FileSystemLogFileManager::new(to_path).unwrap();
            for keep in [false, true] {
                let options = Options {
                    keep,
                    ..Default::default()
                };
                let outcome = migrate_run_logs(&from, &to, CODEBASE, RUN_ID, &todo, options)
                    .await
                    .unwrap();

                assert!(outcome.moved.is_empty());
                assert_eq!(outcome.skipped, todo);
                assert_eq!(snapshot(td.path()), before);
            }
        }
    }

    #[test]
    fn test_parse_args() {
        let args = Args::try_parse_from(["janitor-migrate-logs", "/from", "gs://to"]).unwrap();
        assert_eq!(args.config, std::path::PathBuf::from("janitor.conf"));
        assert_eq!(args.from_location, "/from");
        assert_eq!(args.to_location, "gs://to");
        assert_eq!(args.concurrency.get(), 100);
        assert!(!args.dry_run);
        assert!(!args.keep);

        let args = Args::try_parse_from([
            "janitor-migrate-logs",
            "--config=other.conf",
            "--dry-run",
            "--keep",
            "--concurrency=5",
            "/from",
            "/to",
        ])
        .unwrap();
        assert_eq!(args.config, std::path::PathBuf::from("other.conf"));
        assert_eq!(args.concurrency.get(), 5);
        assert!(args.dry_run);
        assert!(args.keep);
    }

    #[test]
    fn test_parse_args_invalid() {
        assert!(Args::try_parse_from(["janitor-migrate-logs"]).is_err());
        assert!(Args::try_parse_from(["janitor-migrate-logs", "/from"]).is_err());
        assert!(
            Args::try_parse_from(["janitor-migrate-logs", "--concurrency=0", "/from", "/to"])
                .is_err()
        );
        assert!(Args::try_parse_from(["janitor-migrate-logs", "/from", ""]).is_err());
        assert!(Args::try_parse_from(["janitor-migrate-logs", "", "/to"]).is_err());
    }

    #[cfg(feature = "testing")]
    async fn setup_database(logfilenames: &str) -> Option<janitor::test_utils::TestDatabase> {
        let db = janitor::test_utils::TestDatabase::new_optional()
            .await
            .unwrap()?;
        janitor::schema::setup_test_database(&db.pool)
            .await
            .unwrap();
        // The current schema forbids NULL; older databases have such rows.
        for stmt in [
            "ALTER TABLE run ALTER COLUMN logfilenames DROP NOT NULL".to_string(),
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

    #[cfg(feature = "testing")]
    #[tokio::test]
    async fn test_process_all_runs_records_log_names() {
        let Some(db) = setup_database("NULL").await else {
            return;
        };
        let pool = &db.pool;
        let query = "SELECT logfilenames FROM run WHERE id = 'run-1'";

        let (from_td, from) = setup();
        let (to_td, to) = setup();
        write_log(&from_td, "worker.log", "worker\n");
        write_log(&from_td, "build.log", "build\n");

        let options = Options {
            dry_run: true,
            ..Default::default()
        };
        let result = process_all_runs(pool, &from, &to, 100, options)
            .await
            .unwrap();

        assert_eq!(result, (1, 0, 0));
        let stored: Option<Vec<String>> = sqlx::query_scalar(query).fetch_one(pool).await.unwrap();
        assert_eq!(stored, None);
        assert_eq!(read_log(&from, "worker.log").await, "worker\n");
        assert_eq!(read_log(&from, "build.log").await, "build\n");
        assert_eq!(std::fs::read_dir(to_td.path()).unwrap().count(), 0);

        let result = process_all_runs(pool, &from, &to, 100, Options::default())
            .await
            .unwrap();

        assert_eq!(result, (1, 0, 0));
        let stored: Vec<String> = sqlx::query_scalar(query).fetch_one(pool).await.unwrap();
        assert_eq!(stored, names(&["worker.log", "build.log"]));
        assert_eq!(read_log(&to, "build.log").await, "build\n");
        assert!(!from.has_log(CODEBASE, RUN_ID, "worker.log").await.unwrap());
    }

    #[cfg(feature = "testing")]
    #[tokio::test]
    async fn test_process_all_runs_counts_failures() {
        let Some(db) = setup_database("ARRAY['worker.log']").await else {
            return;
        };
        let (from_td, from) = setup();
        write_log(&from_td, "worker.log", "worker\n");
        let to_td = TempDir::new().unwrap();
        let to_path = to_td.path().join("file");
        std::fs::write(&to_path, "").unwrap();
        let to = FileSystemLogFileManager::new(&to_path).unwrap();

        let result = process_all_runs(&db.pool, &from, &to, 100, Options::default())
            .await
            .unwrap();

        assert_eq!(result, (1, 1, 0));
        assert_eq!(read_log(&from, "worker.log").await, "worker\n");
    }

    #[cfg(feature = "testing")]
    #[tokio::test]
    async fn test_process_all_runs_same_directory() {
        let Some(db) = setup_database("ARRAY['worker.log', 'build.log']").await else {
            return;
        };
        let (td, from) = setup();
        let to = FileSystemLogFileManager::new(td.path()).unwrap();
        write_log(&td, "worker.log", "worker\n");
        import(&from, "build.log", "build\n").await;
        let before = snapshot(td.path());

        let result = process_all_runs(&db.pool, &from, &to, 100, Options::default())
            .await
            .unwrap();

        assert_eq!(result, (1, 0, 2));
        assert_eq!(snapshot(td.path()), before);
    }

    #[test]
    fn test_error_display() {
        assert_eq!(
            Error::Log {
                action: "import",
                name: "worker.log".to_string(),
                error: LogError::NotFound,
            }
            .to_string(),
            "failed to import worker.log: Not found"
        );
        assert_eq!(
            Error::Io(std::io::Error::other("boom")).to_string(),
            "I/O error: boom"
        );
        assert!(Error::Database(sqlx::Error::RowNotFound)
            .to_string()
            .starts_with("database error: "));
    }

    #[cfg(feature = "testing")]
    #[tokio::test]
    async fn test_process_all_runs_obeys_the_stored_names() {
        let Some(db) = setup_database("ARRAY['build.log']").await else {
            return;
        };
        let (from_td, from) = setup();
        let (_to_td, to) = setup();
        write_log(&from_td, "worker.log", "worker\n");
        write_log(&from_td, "build.log", "build\n");

        let result = process_all_runs(&db.pool, &from, &to, 100, Options::default())
            .await
            .unwrap();

        assert_eq!(result, (1, 0, 0));
        assert_eq!(read_log(&to, "build.log").await, "build\n");
        // worker.log is in the source but not in the stored list, so it stays.
        assert!(!to.has_log(CODEBASE, RUN_ID, "worker.log").await.unwrap());
        assert!(from.has_log(CODEBASE, RUN_ID, "worker.log").await.unwrap());
    }
}
