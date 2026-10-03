use pyo3::prelude::*;
use pyo3_filelike::PyBinaryFile;

/// `(code, description, phase, failure_details)`, matching what the site
/// handlers already unpack.
type AnalyzedLogTuple = (String, String, Option<String>, Option<Py<PyAny>>);

fn to_py(py: Python, r: janitor::analyze_log::AnalyzedLog) -> PyResult<AnalyzedLogTuple> {
    // A dict, not a string: the pool installs a json.dumps codec, so handing
    // over a string would store a double-encoded JSON literal.
    let failure_details = r
        .failure_details
        .map(|v| pythonize::pythonize(py, &v).map(|b| b.unbind()))
        .transpose()?;
    Ok((r.code, r.description, r.phase, failure_details))
}

/// Drain the Python file object here, so a read error becomes an exception
/// rather than an empty log the analyser would misreport as a build failure.
fn read_all(logf: Py<PyAny>) -> PyResult<Vec<u8>> {
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut PyBinaryFile::from(logf), &mut buf)?;
    Ok(buf)
}

#[pyfunction]
fn process_dist_log(py: Python, logf: Py<PyAny>) -> PyResult<AnalyzedLogTuple> {
    let buf = read_all(logf)?;
    let r = py.detach(|| janitor::analyze_log::process_dist_log(std::io::Cursor::new(buf)));
    to_py(py, r)
}

#[pyfunction]
fn process_build_log(py: Python, logf: Py<PyAny>) -> PyResult<AnalyzedLogTuple> {
    let buf = read_all(logf)?;
    let r = py.detach(|| janitor::analyze_log::process_build_log(std::io::Cursor::new(buf)));
    to_py(py, r)
}

#[pyfunction]
fn process_sbuild_log(py: Python, logf: Py<PyAny>) -> PyResult<AnalyzedLogTuple> {
    let buf = read_all(logf)?;
    let r = py.detach(|| janitor::analyze_log::process_sbuild_log(std::io::Cursor::new(buf)));
    to_py(py, r)
}

pub(crate) fn init_module(_py: Python, m: &Bound<PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(process_dist_log, m)?)?;
    m.add_function(wrap_pyfunction!(process_build_log, m)?)?;
    m.add_function(wrap_pyfunction!(process_sbuild_log, m)?)?;
    Ok(())
}
