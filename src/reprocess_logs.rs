use crate::analyze_log::AnalyzeLogFn;
use crate::logs::LogFileManager;
use sqlx::Executor;
use tokio::time::timeout;
use tracing::{error, info};

/// Reprocess run logs.
pub async fn reprocess_run_logs(
    db: &sqlx::PgPool,
    logfile_manager: &dyn LogFileManager,
    codebase: &str,
    campaign: &str,
    log_id: &str,
    _command: &str,
    change_set: Option<&str>,
    duration: chrono::Duration,
    result_code: &str,
    description: &str,
    failure_details: &serde_json::Value,
    process_fns: &[(String, String, AnalyzeLogFn<Box<dyn std::io::Read>>)],
    dry_run: bool,
    reschedule: bool,
    log_timeout: Option<chrono::Duration>,
) -> Option<crate::analyze_log::AnalyzedLog> {
    if ["dist-no-tarball"].contains(&result_code) {
        return None;
    }
    let log_timeout = log_timeout.unwrap_or_else(|| chrono::Duration::minutes(5));
    let mut process_fn_iter = process_fns.iter();
    let new_analysis = loop {
        let (prefix, logname, f) = process_fn_iter.next()?;
        if !result_code.starts_with(prefix) {
            continue;
        }
        match timeout(
            log_timeout.to_std().unwrap(),
            logfile_manager.get_log(codebase, log_id, logname),
        )
        .await
        {
            Ok(r) => match r {
                Ok(logf) => {
                    break f(logf);
                }
                Err(crate::logs::Error::NotFound) => {
                    return None;
                }
                Err(e) => {
                    error!(
                        "{}/{}: Failed to fetch log {}: {:?}",
                        codebase, log_id, logname, e
                    );
                    return None;
                }
            },
            Err(_) => {
                error!("{}/{}: Timeout fetching log {}", codebase, log_id, logname);
                return None;
            }
        }
    };

    if new_analysis.code != result_code
        || new_analysis.description != description
        || new_analysis.failure_details.as_ref() != Some(failure_details)
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

            db.execute(query).await.unwrap();
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
                .await
                .unwrap();
            }
        }
        return Some(new_analysis);
    }

    None
}
