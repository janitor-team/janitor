use crate::logs::LogFileManager;
use buildlog_consultant::common::find_build_failure_description;
use buildlog_consultant::r#match::Error as AnalyzeError;
use futures::{StreamExt, TryStreamExt};
use janitor_types::LogFilename;
use janitor_types::{Campaign, ChangeSetId, CodebaseName, RunId};
use std::io::BufRead;
use tokio::time::timeout;
use tracing::{error, info};

/// How long to wait for a log to be fetched before giving up.
const LOG_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Number of runs that [`reprocess_runs`] processes concurrently.
const REPROCESS_CONCURRENCY: usize = 8;

/// The result of analyzing a log.
pub struct AnalyzedLog {
    /// Result code.
    pub code: String,
    /// Human-readable description of the failure.
    pub description: String,
    /// Phase in which the failure happened.
    pub phase: Option<String>,
    /// Structured details of the failure.
    pub failure_details: Option<serde_json::Value>,
}

/// Analyze the log of a failed dist step.
pub fn process_dist_log<R: std::io::Read>(logf: R) -> Result<AnalyzedLog, AnalyzeError> {
    let lines = std::io::BufReader::new(logf)
        .lines()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AnalyzeError::new(format!("failed to read dist log: {}", e)))?;
    let problem =
        find_build_failure_description(&lines.iter().map(|l| l.as_str()).collect::<Vec<_>>())?
            .and_then(|f| f.problem);
    let (new_code, new_description, new_failure_details) = if let Some(problem) = problem {
        let new_code = if problem.is_universal() {
            problem.kind().to_string()
        } else {
            format!("dist-{}", problem.kind())
        };
        let new_description = problem.to_string();
        let new_failure_details = problem.json();
        (new_code, new_description, Some(new_failure_details))
    } else {
        (
            "dist-command-failed".to_string(),
            "Dist command failed".to_string(),
            None,
        )
    };
    let new_phase = None;
    Ok(AnalyzedLog {
        code: new_code,
        description: new_description,
        phase: new_phase,
        failure_details: new_failure_details,
    })
}

/// Analyze an sbuild log.
pub fn process_sbuild_log<R: std::io::Read>(logf: R) -> Result<AnalyzedLog, AnalyzeError> {
    let bufread = std::io::BufReader::new(logf);

    let sbuildlog = buildlog_consultant::sbuild::SbuildLog::try_from(bufread)
        .map_err(|e| AnalyzeError::new(format!("failed to read build log: {}", e)))?;
    let failure = buildlog_consultant::sbuild::worker_failure_from_sbuild_log(&sbuildlog)?;

    let (new_code, new_failure_details) = if let Some(error) = failure.error.as_ref() {
        let new_code = if let Some(failure_stage) = failure.stage.as_ref() {
            if !error.is_universal() {
                format!("{}-{}", failure_stage, error.kind())
            } else {
                error.kind().to_string()
            }
        } else {
            error.kind().to_string()
        };

        let new_failure_details = error.json();
        (new_code, Some(new_failure_details))
    } else if let Some(failure_stage) = failure.stage.as_ref() {
        let new_code = format!("build-failed-stage-{}", failure_stage);
        let new_failure_details = None;
        (new_code, new_failure_details)
    } else {
        let new_code = "build-failed".to_string();
        let new_failure_details = None;
        (new_code, new_failure_details)
    };

    let new_description = if let Some(error) = failure.error.as_ref() {
        error.to_string()
    } else {
        "Build failed".to_string()
    };

    let new_phase = failure.stage.map(|s| s.to_string());

    Ok(AnalyzedLog {
        code: new_code,
        description: new_description,
        phase: new_phase,
        failure_details: new_failure_details,
    })
}

/// Error reprocessing the logs of a run.
#[derive(Debug)]
pub enum Error {
    /// The log to reanalyze does not exist.
    LogNotFound(LogFilename),
    /// Fetching the log took too long.
    LogTimeout(LogFilename),
    /// Fetching the log failed.
    LogFetch(LogFilename, crate::logs::Error),
    /// The log could not be analyzed.
    Analyze(LogFilename, AnalyzeError),
    /// Updating the run failed.
    Database(sqlx::Error),
    /// Rescheduling the run failed.
    Schedule(janitor::schedule::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::LogNotFound(name) => write!(f, "log {} not found", name),
            Error::LogTimeout(name) => write!(f, "timeout fetching log {}", name),
            Error::LogFetch(name, e) => write!(f, "failed to fetch log {}: {}", name, e),
            Error::Analyze(name, e) => write!(f, "failed to analyze log {}: {}", name, e),
            Error::Database(e) => write!(f, "database error: {}", e),
            Error::Schedule(e) => write!(f, "failed to reschedule: {}", e),
        }
    }
}

impl std::error::Error for Error {}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        Error::Database(e)
    }
}

impl From<janitor::schedule::Error> for Error {
    fn from(e: janitor::schedule::Error) -> Self {
        Error::Schedule(e)
    }
}

/// A run whose logs can be reprocessed.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RunToReprocess {
    /// Id of the run.
    pub id: RunId,
    /// Codebase the run was for.
    pub codebase: CodebaseName,
    /// Campaign the run was for.
    pub campaign: Campaign,
    /// Change set the run belongs to.
    pub change_set: Option<ChangeSetId>,
    /// How long the run took.
    pub duration_seconds: Option<i64>,
    /// Stored result code.
    pub result_code: String,
    /// Stored description.
    pub description: Option<String>,
    /// Stored failure details.
    pub failure_details: Option<serde_json::Value>,
}

const SELECT_RUNS: &str = "SELECT id, codebase, campaign, change_set, \
    EXTRACT(EPOCH FROM finish_time - start_time)::bigint AS duration_seconds, \
    result_code, description, failure_details FROM run";

/// Select the runs to reprocess.
///
/// When `run_ids` is set, exactly those runs are returned and the other
/// filters are ignored. Otherwise all runs with a result code that comes
/// from log analysis are returned, narrowed down by `campaign`,
/// `description_re` and `min_age_days`.
pub async fn select_runs_to_reprocess(
    db: &sqlx::PgPool,
    run_ids: Option<&[RunId]>,
    campaign: Option<&Campaign>,
    description_re: Option<&str>,
    min_age_days: Option<i32>,
) -> Result<Vec<RunToReprocess>, sqlx::Error> {
    let mut query = sqlx::QueryBuilder::new(SELECT_RUNS);
    if let Some(run_ids) = run_ids {
        query.push(" WHERE id = ANY(").push_bind(run_ids).push(")");
        return query.build_query_as().fetch_all(db).await;
    }
    query.push(
        " WHERE (result_code = 'build-failed' OR \
         result_code LIKE 'build-failed-stage-%' OR \
         result_code LIKE 'autopkgtest-%' OR \
         result_code LIKE 'build-%' OR \
         result_code LIKE 'dist-%' OR \
         result_code LIKE 'unpack-%' OR \
         result_code LIKE 'create-session-%' OR \
         result_code LIKE 'missing-%')",
    );
    if let Some(campaign) = campaign {
        query.push(" AND campaign = ").push_bind(campaign);
    }
    if let Some(description_re) = description_re {
        query.push(" AND description ~ ").push_bind(description_re);
    }
    if let Some(min_age_days) = min_age_days {
        query
            .push(" AND finish_time < (NOW() AT TIME ZONE 'UTC') - make_interval(days => ")
            .push_bind(min_age_days)
            .push(")");
    }
    query.build_query_as().fetch_all(db).await
}

/// Reanalyze the logs of a run, and update the run if the analysis changed.
///
/// Returns the new analysis if it differs from what is stored for the run,
/// or `None` if it is unchanged or there is no analyzer for the run's
/// result code.
pub async fn reprocess_run_logs(
    db: &sqlx::PgPool,
    logfile_manager: &LogFileManager,
    run: &RunToReprocess,
    dry_run: bool,
    reschedule: bool,
) -> Result<Option<AnalyzedLog>, Error> {
    let is_dist = run.result_code.starts_with("dist-");
    if run.result_code == "dist-no-tarball" || !(is_dist || run.result_code.starts_with("build-")) {
        return Ok(None);
    }
    let logname = LogFilename::new(if is_dist { "dist.log" } else { "build.log" }.to_string())
        .expect("static log filename is valid");

    let fetch = async {
        logfile_manager
            .get_log(&run.codebase, &run.id, &logname)
            .await?
            .map_ok(|chunk| chunk.to_vec())
            .try_concat()
            .await
    };
    let contents = match timeout(LOG_TIMEOUT, fetch).await {
        Ok(Ok(contents)) => contents,
        Ok(Err(crate::logs::Error::NotFound)) => return Err(Error::LogNotFound(logname)),
        Ok(Err(e)) => return Err(Error::LogFetch(logname, e)),
        Err(_) => return Err(Error::LogTimeout(logname)),
    };
    let new_analysis = if is_dist {
        process_dist_log(contents.as_slice())
    } else {
        process_sbuild_log(contents.as_slice())
    }
    .map_err(|e| Error::Analyze(logname, e))?;

    if new_analysis.code == run.result_code
        && Some(&new_analysis.description) == run.description.as_ref()
        && new_analysis.failure_details == run.failure_details
    {
        return Ok(None);
    }

    info!(
        "{}/{}: Updated {:?}, {:?} ⇒ {:?}, {:?} {:?}",
        run.codebase,
        run.id,
        run.result_code,
        run.description,
        new_analysis.code,
        new_analysis.description,
        new_analysis.phase,
    );
    if dry_run {
        return Ok(Some(new_analysis));
    }
    sqlx::query(
        "UPDATE run SET result_code = $1, description = $2, failure_details = $3 WHERE id = $4",
    )
    .bind(&new_analysis.code)
    .bind(&new_analysis.description)
    .bind(&new_analysis.failure_details)
    .bind(&run.id)
    .execute(db)
    .await?;
    if reschedule && new_analysis.code != run.result_code {
        janitor::schedule::do_schedule(
            db,
            &run.campaign,
            &run.codebase,
            "reschedule",
            run.change_set.as_ref(),
            None,
            false,
            Some("reprocess-build-results"),
            run.duration_seconds.map(chrono::Duration::seconds),
            None,
        )
        .await?;
    }
    Ok(Some(new_analysis))
}

/// Reprocess the logs of `runs`, logging any failures.
pub async fn reprocess_runs(
    db: &sqlx::PgPool,
    logfile_manager: &LogFileManager,
    runs: Vec<RunToReprocess>,
    dry_run: bool,
    reschedule: bool,
) {
    futures::stream::iter(runs)
        .for_each_concurrent(REPROCESS_CONCURRENCY, |run| async move {
            if let Err(e) = reprocess_run_logs(db, logfile_manager, &run, dry_run, reschedule).await
            {
                error!("{}/{}: {}", run.codebase, run.id, e);
            }
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{TestDatabase, TestSchema};

    #[test]
    fn test_process_sbuild_log() {
        let sep = format!("+{}+", "-".repeat(78));
        let log = format!(
            "{sep}\n| Build |\n{sep}\n\
             sl.c:41:10: fatal error: curses.h: No such file or directory\n\
             make: *** [Makefile:13: sl] Error 1\n\
             {sep}\n| Summary |\n{sep}\nFail-Stage: build\n"
        );
        let analysis = process_sbuild_log(log.as_bytes()).unwrap();
        assert_eq!(
            (
                analysis.code.as_str(),
                analysis.description.as_str(),
                analysis.phase.as_deref()
            ),
            (
                "build-missing-c-header",
                "Missing C header: curses.h",
                Some("build")
            )
        );
    }

    #[test]
    fn test_process_logs_invalid_utf8() {
        let log = &b"hello\ncaf\xe9\n"[..];
        assert_eq!(
            process_sbuild_log(log).err().unwrap().to_string(),
            "failed to read build log: stream did not contain valid UTF-8"
        );
        assert_eq!(
            process_dist_log(log).err().unwrap().to_string(),
            "failed to read dist log: stream did not contain valid UTF-8"
        );
    }

    async fn setup_db() -> Option<TestDatabase> {
        let db = TestDatabase::new_optional_with_schema(TestSchema::Core)
            .await
            .unwrap()?;
        sqlx::query(
            "INSERT INTO change_set (id, campaign) VALUES \
             ('cs', 'lintian-fixes'), ('cs2', 'multiarch-hints')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        Some(db)
    }

    async fn insert_run(
        db: &sqlx::PgPool,
        id: &str,
        campaign: &str,
        result_code: &str,
        description: Option<&str>,
        age_days: i32,
    ) {
        sqlx::query("INSERT INTO codebase (name) VALUES ($1) ON CONFLICT DO NOTHING")
            .bind(id)
            .execute(db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO run (id, command, codebase, campaign, change_set, logfilenames, \
             result_code, description, start_time, finish_time) VALUES \
             ($1, '', $1, $2, CASE WHEN $2 = 'lintian-fixes' THEN 'cs' ELSE 'cs2' END, \
              '{build.log}', $3, $4, \
              (NOW() AT TIME ZONE 'UTC') - make_interval(days => $5, mins => 30), \
              (NOW() AT TIME ZONE 'UTC') - make_interval(days => $5))",
        )
        .bind(id)
        .bind(campaign)
        .bind(result_code)
        .bind(description)
        .bind(age_days)
        .execute(db)
        .await
        .unwrap();
    }

    async fn store_log(mgr: &LogFileManager, run_id: &str, name: &str, content: &[u8]) {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join(name);
        std::fs::write(&path, content).unwrap();
        mgr.import_log(
            &CodebaseName::new(run_id).unwrap(),
            &RunId::new(run_id).unwrap(),
            path.to_str().unwrap(),
            None,
        )
        .await
        .unwrap();
    }

    async fn select_ids(
        db: &sqlx::PgPool,
        campaign: Option<&str>,
        description_re: Option<&str>,
        min_age_days: Option<i32>,
    ) -> Vec<String> {
        let campaign = campaign.map(|c| Campaign::new(c).unwrap());
        let mut ids: Vec<String> =
            select_runs_to_reprocess(db, None, campaign.as_ref(), description_re, min_age_days)
                .await
                .unwrap()
                .into_iter()
                .map(|r| r.id.to_string())
                .collect();
        ids.sort();
        ids
    }

    #[tokio::test]
    async fn test_select_runs_to_reprocess() {
        let Some(db) = setup_db().await else {
            return;
        };
        let pool = db.pool();
        insert_run(
            pool,
            "old",
            "lintian-fixes",
            "build-failed",
            Some("sbuild timed out"),
            10,
        )
        .await;
        insert_run(
            pool,
            "other-campaign",
            "multiarch-hints",
            "build-failed",
            None,
            0,
        )
        .await;
        insert_run(
            pool,
            "patch",
            "lintian-fixes",
            "unpack-patch-application-failed",
            None,
            0,
        )
        .await;
        insert_run(pool, "success", "lintian-fixes", "success", None, 0).await;

        assert_eq!(
            select_ids(pool, None, None, None).await,
            vec!["old", "other-campaign", "patch"]
        );
        assert_eq!(
            select_ids(pool, Some("multiarch-hints"), None, None).await,
            vec!["other-campaign"]
        );
        assert_eq!(
            select_ids(pool, None, Some("timed out"), None).await,
            vec!["old"]
        );
        assert_eq!(select_ids(pool, None, None, Some(5)).await, vec!["old"]);

        let run_ids = [RunId::new("success").unwrap()];
        let runs = select_runs_to_reprocess(pool, Some(&run_ids), None, Some("nomatch"), None)
            .await
            .unwrap();
        assert_eq!(
            runs.iter().map(|r| r.id.to_string()).collect::<Vec<_>>(),
            vec!["success"]
        );
    }

    async fn reprocess(
        db: &sqlx::PgPool,
        logs: &LogFileManager,
        run_id: &str,
        dry_run: bool,
    ) -> Result<Option<AnalyzedLog>, Error> {
        let run_ids = [RunId::new(run_id).unwrap()];
        let runs = select_runs_to_reprocess(db, Some(&run_ids), None, None, None)
            .await
            .unwrap();
        reprocess_run_logs(db, logs, &runs[0], dry_run, false).await
    }

    async fn stored_result(db: &sqlx::PgPool, run_id: &str) -> (String, Option<String>) {
        sqlx::query_as("SELECT result_code, description FROM run WHERE id = $1")
            .bind(run_id)
            .fetch_one(db)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_reprocess_run_logs() {
        let Some(db) = setup_db().await else {
            return;
        };
        let pool = db.pool();
        let logs = LogFileManager::in_memory();
        insert_run(
            pool,
            "changed",
            "lintian-fixes",
            "build-old-code",
            Some("old"),
            0,
        )
        .await;
        store_log(&logs, "changed", "build.log", b"nothing to see here\n").await;
        insert_run(
            pool,
            "unchanged",
            "lintian-fixes",
            "build-failed",
            Some("Build failed"),
            0,
        )
        .await;
        store_log(&logs, "unchanged", "build.log", b"nothing to see here\n").await;
        insert_run(pool, "no-log", "lintian-fixes", "build-failed", None, 0).await;
        insert_run(
            pool,
            "no-analyzer",
            "lintian-fixes",
            "unpack-failed",
            None,
            0,
        )
        .await;

        let analysis = reprocess(pool, &logs, "changed", true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (analysis.code.as_str(), analysis.description.as_str()),
            ("build-failed", "Build failed")
        );
        assert_eq!(
            stored_result(pool, "changed").await,
            ("build-old-code".to_string(), Some("old".to_string()))
        );

        reprocess(pool, &logs, "changed", false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            stored_result(pool, "changed").await,
            ("build-failed".to_string(), Some("Build failed".to_string()))
        );

        assert!(reprocess(pool, &logs, "unchanged", false)
            .await
            .unwrap()
            .is_none());
        assert!(reprocess(pool, &logs, "no-analyzer", false)
            .await
            .unwrap()
            .is_none());
        assert!(matches!(
            reprocess(pool, &logs, "no-log", false).await,
            Err(Error::LogNotFound(name)) if name.as_str() == "build.log"
        ));
    }
}
