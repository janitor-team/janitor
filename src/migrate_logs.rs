//! Move run logs from one log store to another.

use crate::logs::LogFileManager;
use crate::{CodebaseName, RunId};
use futures::{StreamExt, TryStreamExt};
use tokio::io::AsyncWriteExt;

#[derive(Debug)]
pub enum Error {
    Database(sqlx::Error),
    Log(crate::logs::Error),
    Io(std::io::Error),
}

impl From<sqlx::Error> for Error {
    fn from(err: sqlx::Error) -> Self {
        Error::Database(err)
    }
}

impl From<crate::logs::Error> for Error {
    fn from(err: crate::logs::Error) -> Self {
        Error::Log(err)
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Io(err)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::Database(err) => write!(f, "Database error: {}", err),
            Error::Log(err) => write!(f, "Log error: {}", err),
            Error::Io(err) => write!(f, "I/O error: {}", err),
        }
    }
}

impl std::error::Error for Error {}

/// Move the logs of a single run from `from` to `to`.
pub async fn migrate_run_logs(
    from: &LogFileManager,
    to: &LogFileManager,
    codebase: &CodebaseName,
    run_id: &RunId,
    logfilenames: &[String],
) -> Result<(), Error> {
    log::info!("Processing {} ({:?})", run_id, logfilenames);
    let td = tempfile::tempdir()?;
    for name in logfilenames {
        let mut stream = match from.get_log(codebase, run_id, name).await {
            Ok(stream) => stream,
            Err(crate::logs::Error::NotFound) => continue,
            Err(e) => return Err(e.into()),
        };
        let path = td.path().join(name);
        let mut f = tokio::fs::File::create(&path).await?;
        while let Some(chunk) = stream.try_next().await? {
            f.write_all(&chunk).await?;
        }
        f.flush().await?;
        drop(f);
        let path = path
            .to_str()
            .ok_or_else(|| crate::logs::Error::InvalidName(name.clone()))?;
        to.import_log(codebase, run_id, path, None).await?;
        from.delete_log(codebase, run_id, name).await?;
    }
    Ok(())
}

/// Move the logs of all runs from `from` to `to`, processing up to
/// `concurrency` runs at a time.
pub async fn migrate_all_logs(
    pool: &sqlx::PgPool,
    from: &LogFileManager,
    to: &LogFileManager,
    concurrency: usize,
) -> Result<(), Error> {
    let runs: Vec<(CodebaseName, RunId, Vec<String>)> =
        sqlx::query_as("SELECT codebase, id, logfilenames FROM run")
            .fetch_all(pool)
            .await?;
    futures::stream::iter(runs)
        .map(|(codebase, run_id, logfilenames)| async move {
            migrate_run_logs(from, to, &codebase, &run_id, &logfilenames).await
        })
        .buffer_unordered(concurrency)
        .try_collect::<()>()
        .await
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    use super::*;

    async fn read_log(
        mgr: &LogFileManager,
        codebase: &CodebaseName,
        run_id: &RunId,
        name: &str,
    ) -> Vec<u8> {
        mgr.get_log(codebase, run_id, name)
            .await
            .unwrap()
            .map_ok(|chunk| chunk.to_vec())
            .try_concat()
            .await
            .unwrap()
    }

    async fn store_log(
        mgr: &LogFileManager,
        codebase: &CodebaseName,
        run_id: &RunId,
        name: &str,
        content: &[u8],
    ) {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join(name);
        std::fs::write(&path, content).unwrap();
        mgr.import_log(codebase, run_id, path.to_str().unwrap(), None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_migrate_all_logs() {
        let Some(db) = crate::test_utils::TestDatabase::new_optional_with_schema(
            crate::test_utils::TestSchema::Core,
        )
        .await
        .unwrap() else {
            return;
        };
        sqlx::query(
            "INSERT INTO codebase (name, branch_url, url, vcs_type) \
             VALUES ('cb', 'https://example.invalid/cb', 'https://example.invalid/cb', 'git')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query("INSERT INTO change_set (id, campaign) VALUES ('cs', 'lintian-fixes')")
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO run (id, codebase, suite, change_set, logfilenames, result_code,
                              start_time, finish_time) VALUES
             ('r1', 'cb', 'lintian-fixes', 'cs', '{worker.log,missing.log}', 'success',
              '2024-01-01 00:00:00', '2024-01-01 00:00:01'),
             ('r2', 'cb', 'lintian-fixes', 'cs', '{worker.log,build.log}', 'some-failure',
              '2024-01-02 00:00:00', '2024-01-02 00:00:01')",
        )
        .execute(db.pool())
        .await
        .unwrap();

        let codebase = CodebaseName::new("cb").unwrap();
        let r1 = RunId::new("r1").unwrap();
        let r2 = RunId::new("r2").unwrap();
        let from = LogFileManager::in_memory();
        let to = LogFileManager::in_memory();
        store_log(&from, &codebase, &r1, "worker.log", b"r1 worker").await;
        store_log(&from, &codebase, &r2, "worker.log", b"r2 worker").await;
        store_log(&from, &codebase, &r2, "build.log", b"r2 build").await;
        store_log(&from, &codebase, &r2, "unlisted.log", b"r2 unlisted").await;

        migrate_all_logs(db.pool(), &from, &to, 10).await.unwrap();

        let mut migrated: Vec<_> = to.iter_logs().try_collect().await.unwrap();
        migrated.sort();
        assert_eq!(
            migrated,
            vec![
                ("cb".to_string(), "r1".to_string(), "worker.log".to_string()),
                ("cb".to_string(), "r2".to_string(), "build.log".to_string()),
                ("cb".to_string(), "r2".to_string(), "worker.log".to_string()),
            ]
        );
        assert_eq!(
            read_log(&to, &codebase, &r1, "worker.log").await,
            b"r1 worker"
        );
        assert_eq!(
            read_log(&to, &codebase, &r2, "worker.log").await,
            b"r2 worker"
        );
        assert_eq!(
            read_log(&to, &codebase, &r2, "build.log").await,
            b"r2 build"
        );

        let remaining: Vec<_> = from.iter_logs().try_collect().await.unwrap();
        assert_eq!(
            remaining,
            vec![(
                "cb".to_string(),
                "r2".to_string(),
                "unlisted.log".to_string()
            )]
        );
    }
}
