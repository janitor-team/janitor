// Necessary since create_exception!() uses cfg!(feature = "gil-refs"),
// but we don't have that feature.
#![allow(unexpected_cfgs)]
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

mod artifacts;
mod config;
mod io;
mod logs;
mod reprocess_logs;
mod vcs;

#[pyfunction]
fn get_branch_vcs_type(branch: Py<PyAny>) -> PyResult<String> {
    let branch = breezyshim::branch::GenericBranch::from(branch);
    janitor::vcs::get_branch_vcs_type(&branch)
        .map_err(|e| PyValueError::new_err((format!("{:?}", e),)))
        .map(|vcs| vcs.to_string())
}

#[pyfunction]
fn is_authenticated_url(url: &str) -> PyResult<bool> {
    Ok(janitor::vcs::is_authenticated_url(
        &url::Url::parse(url)
            .map_err(|e| PyValueError::new_err((format!("Invalid URL: {}", e),)))?,
    ))
}

#[pyfunction]
fn is_alioth_url(url: &str) -> PyResult<bool> {
    Ok(janitor::vcs::is_alioth_url(&url::Url::parse(url).map_err(
        |e| PyValueError::new_err((format!("Invalid URL: {}", e),)),
    )?))
}

#[pymodule]
pub fn _common(py: Python, m: &Bound<PyModule>) -> PyResult<()> {
    pyo3_log::init();
    m.add_function(wrap_pyfunction!(is_authenticated_url, m)?)?;
    m.add_function(wrap_pyfunction!(is_alioth_url, m)?)?;
    m.add_function(wrap_pyfunction!(get_branch_vcs_type, m)?)?;

    let artifactsm = pyo3::types::PyModule::new(py, "artifacts")?;
    crate::artifacts::init(py, &artifactsm)?;
    m.add_submodule(&artifactsm)?;

    let vcsm = pyo3::types::PyModule::new(py, "vcs")?;
    crate::vcs::init(py, &vcsm)?;
    m.add_submodule(&vcsm)?;

    let logsm = pyo3::types::PyModule::new(py, "logs")?;
    crate::logs::init(py, &logsm)?;
    m.add_submodule(&logsm)?;

    let configm = pyo3::types::PyModule::new(py, "config")?;
    crate::config::init(py, &configm)?;
    m.add_submodule(&configm)?;

    let reprocess_logsm = pyo3::types::PyModule::new(py, "reprocess_logs")?;
    crate::reprocess_logs::init(py, &reprocess_logsm)?;
    m.add_submodule(&reprocess_logsm)?;

    Ok(())
}
