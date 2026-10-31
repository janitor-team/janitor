use crate::analyze_log::AnalyzeLogFn;
use crate::logs::LogFileManager;
use crate::{Campaign, ChangeSetId, CodebaseName, RunId};
use futures::TryStreamExt;
use sqlx::Executor;
use tokio::time::timeout;
use tracing::{error, info};

/// Reprocess run logs.
pub async fn reprocess_run_logs(
    db: &sqlx::PgPool,
    logfile_manager: &LogFileManager,
    codebase: &CodebaseName,
    campaign: &Campaign,
    log_id: &RunId,
    change_set: Option<&ChangeSetId>,
    duration: chrono::Duration,
    result_code: &str,
    description: Option<&str>,
    failure_details: Option<&serde_json::Value>,
    process_fns: &[(String, String, AnalyzeLogFn<Box<dyn std::io::Read>>)],
    dry_run: bool,
    reschedule: bool,
    log_timeout: Option<chrono::Duration>,
) -> Result<Option<crate::analyze_log::AnalyzedLog>, crate::schedule::Error> {
    if ["dist-no-tarball"].contains(&result_code) {
        return Ok(None);
    }
    let log_timeout = log_timeout.unwrap_or_else(|| chrono::Duration::minutes(5));
    let mut process_fn_iter = process_fns.iter();
    let new_analysis = loop {
        let Some((prefix, logname, f)) = process_fn_iter.next() else {
            return Ok(None);
        };
        if !result_code.starts_with(prefix) {
            continue;
        }
        let fetch = async {
            logfile_manager
                .get_log(codebase, log_id, logname)
                .await?
                .map_ok(|chunk| chunk.to_vec())
                .try_concat()
                .await
        };
        match timeout(log_timeout.to_std().unwrap(), fetch).await {
            Ok(r) => match r {
                Ok(contents) => match f(Box::new(std::io::Cursor::new(contents))) {
                    Ok(analysis) => break analysis,
                    Err(e) => {
                        error!(
                            "{}/{}: Failed to analyze log {}: {}",
                            codebase, log_id, logname, e
                        );
                        return Ok(None);
                    }
                },
                Err(crate::logs::Error::NotFound) => {
                    return Ok(None);
                }
                Err(e) => {
                    error!(
                        "{}/{}: Failed to fetch log {}: {:?}",
                        codebase, log_id, logname, e
                    );
                    return Ok(None);
                }
            },
            Err(_) => {
                error!("{}/{}: Timeout fetching log {}", codebase, log_id, logname);
                return Ok(None);
            }
        }
    };

    if new_analysis.code != result_code
        || Some(new_analysis.description.as_str()) != description
        || new_analysis.failure_details.as_ref() != failure_details
    {
        info!(
            "{}/{}: Updated {:?}, {:?} ⇒ {:?}, {:?} {:?}",
            codebase,
            log_id,
            result_code,
            description,
            new_analysis.code,
            new_analysis.description,
            new_analysis.phase,
        );
        if !dry_run {
            let query = sqlx::query(
                "UPDATE run SET result_code = $1, description = $2, failure_details = $3 WHERE id = $4")
                .bind(&new_analysis.code)
                .bind(&new_analysis.description)
                .bind(&new_analysis.failure_details)
                .bind(log_id);

            db.execute(query).await?;
            if reschedule && new_analysis.code != result_code {
                crate::schedule::do_schedule(
                    db,
                    campaign,
                    codebase,
                    "reschedule",
                    change_set,
                    None,
                    false,
                    Some("reprocess-build-results"),
                    Some(duration),
                    None,
                )
                .await?;
            }
        }
        return Ok(Some(new_analysis));
    }

    Ok(None)
}
