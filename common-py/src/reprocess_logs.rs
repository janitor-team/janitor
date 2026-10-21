use crate::config::Config;
use crate::logs::LogFileManager;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

type ProcessFn = janitor::analyze_log::AnalyzeLogFn<Box<dyn std::io::Read>>;
type NewAnalysis = (String, String, Option<Py<PyAny>>);

// One pool per database, shared by all calls.
static POOLS: LazyLock<Mutex<HashMap<String, sqlx::PgPool>>> = LazyLock::new(Default::default);

async fn pool_for(config: &janitor::config::Config) -> Result<sqlx::PgPool, sqlx::Error> {
    let key = config.database_location.clone().unwrap_or_default();
    let cached = POOLS.lock().unwrap().get(&key).cloned();
    if let Some(pool) = cached {
        return Ok(pool);
    }
    let pool = janitor::state::create_pool(config).await?;
    POOLS.lock().unwrap().insert(key, pool.clone());
    Ok(pool)
}

fn process_fn(kind: &str) -> PyResult<ProcessFn> {
    match kind {
        "dist" => Ok(janitor::analyze_log::process_dist_log),
        "build" => Ok(janitor::analyze_log::process_build_log),
        "sbuild" => Ok(janitor::analyze_log::process_sbuild_log),
        _ => Err(PyValueError::new_err(format!("Unknown log kind: {}", kind))),
    }
}

#[pyfunction]
#[pyo3(signature = (config, logfile_manager, codebase, campaign, log_id, command, change_set, duration, result_code, description, failure_details, process_fns, dry_run=false, reschedule=false, log_timeout=None))]
#[allow(clippy::too_many_arguments)]
fn reprocess_run_logs<'a>(
    py: Python<'a>,
    config: PyRef<Config>,
    logfile_manager: PyRef<LogFileManager>,
    codebase: String,
    campaign: String,
    log_id: String,
    command: String,
    change_set: Option<String>,
    duration: chrono::Duration,
    result_code: String,
    description: Option<String>,
    failure_details: Option<Bound<'a, PyAny>>,
    process_fns: Vec<(String, String, String)>,
    dry_run: bool,
    reschedule: bool,
    log_timeout: Option<chrono::Duration>,
) -> PyResult<Bound<'a, PyAny>> {
    let config = config.0.clone();
    let logfile_manager = logfile_manager.0.clone();
    let failure_details: Option<serde_json::Value> = failure_details
        .map(|d| pythonize::depythonize(&d))
        .transpose()?;
    let process_fns = process_fns
        .into_iter()
        .map(|(prefix, logname, kind)| Ok((prefix, logname, process_fn(&kind)?)))
        .collect::<PyResult<Vec<_>>>()?;
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let pool = pool_for(&config)
            .await
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        let analysis = janitor::reprocess_logs::reprocess_run_logs(
            &pool,
            logfile_manager.as_ref(),
            &codebase,
            &campaign,
            &log_id,
            &command,
            change_set.as_deref(),
            duration,
            &result_code,
            description.as_deref().unwrap_or_default(),
            failure_details.as_ref(),
            &process_fns,
            dry_run,
            reschedule,
            log_timeout,
        )
        .await
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        Python::attach(|py| -> PyResult<Option<NewAnalysis>> {
            let Some(analysis) = analysis else {
                return Ok(None);
            };
            let failure_details = analysis
                .failure_details
                .map(|v| pythonize::pythonize(py, &v).map(|d| d.unbind()))
                .transpose()?;
            Ok(Some((analysis.code, analysis.description, failure_details)))
        })
    })
}

pub(crate) fn init(_py: Python, m: &Bound<PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(reprocess_run_logs, m)?)?;
    Ok(())
}
