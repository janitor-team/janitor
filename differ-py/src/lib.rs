use pyo3::exceptions::{PyRuntimeError, PyTimeoutError, PyValueError};
use pyo3::prelude::*;

#[pyfunction]
#[pyo3(signature = (old_binaries, new_binaries, timeout = None, memory_limit = None, diffoscope_command = None))]
fn run_diffoscope<'a>(
    py: Python<'a>,
    old_binaries: Vec<(String, String)>,
    new_binaries: Vec<(String, String)>,
    timeout: Option<f64>,
    memory_limit: Option<u64>,
    diffoscope_command: Option<String>,
) -> PyResult<Bound<'a, PyAny>> {
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let old_binaries = old_binaries
            .iter()
            .map(|(path, hash)| (path.as_str(), hash.as_str()))
            .collect::<Vec<_>>();
        let new_binaries = new_binaries
            .iter()
            .map(|(path, hash)| (path.as_str(), hash.as_str()))
            .collect::<Vec<_>>();

        let o = janitor_differ::diffoscope::run_diffoscope(
            old_binaries.as_slice(),
            new_binaries.as_slice(),
            timeout,
            memory_limit,
            diffoscope_command.as_deref(),
        )
        .await
        .map_err(|e| match e {
            janitor_differ::diffoscope::DiffoscopeError::Timeout => {
                PyTimeoutError::new_err("Diffoscope timed out")
            }
            janitor_differ::diffoscope::DiffoscopeError::Io(e) => e.into(),
            janitor_differ::diffoscope::DiffoscopeError::Other(e) => PyRuntimeError::new_err(e),
            janitor_differ::diffoscope::DiffoscopeError::Serde(e) => {
                PyValueError::new_err(e.to_string())
            }
            janitor_differ::diffoscope::DiffoscopeError::Py(e) => e,
        })?;
        Ok(Python::attach(|py| {
            pythonize::pythonize(py, &o).unwrap().unbind()
        }))
    })
}

#[pyfunction]
fn filter_boring_udiff(
    udiff: &str,
    old_version: &str,
    new_version: &str,
    display_version: &str,
) -> PyResult<String> {
    let o = janitor_differ::diffoscope::filter_boring_udiff(
        udiff,
        old_version,
        new_version,
        display_version,
    )
    .map_err(|e| PyValueError::new_err(e.to_string()))?;
    Ok(o)
}

#[pymodule]
pub fn _differ(m: &Bound<PyModule>) -> PyResult<()> {
    pyo3_log::init();

    m.add_function(wrap_pyfunction!(run_diffoscope, m)?)?;
    m.add_function(wrap_pyfunction!(filter_boring_udiff, m)?)?;

    Ok(())
}
