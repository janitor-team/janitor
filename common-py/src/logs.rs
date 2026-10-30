use crate::io::AsyncReader;
use futures::stream::BoxStream;
use futures::{StreamExt, TryStreamExt};
use janitor::{CodebaseName, RunId};
use pyo3::create_exception;
use pyo3::exceptions::{
    PyFileNotFoundError, PyOSError, PyPermissionError, PyRuntimeError, PyStopAsyncIteration,
    PyTimeoutError, PyValueError,
};
use pyo3::prelude::*;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

create_exception!(
    janitor.logs,
    ServiceUnavailable,
    pyo3::exceptions::PyException
);
create_exception!(
    janitor.logs,
    LogRetrievalError,
    pyo3::exceptions::PyException
);

type LogListing = (String, String, String);

fn convert_logs_error_to_py(err: janitor::logs::Error) -> PyErr {
    match err {
        janitor::logs::Error::ServiceUnavailable => {
            ServiceUnavailable::new_err("Service unavailable")
        }
        janitor::logs::Error::NotFound => PyFileNotFoundError::new_err("Log not found"),
        janitor::logs::Error::PermissionDenied => PyPermissionError::new_err("Permission denied"),
        janitor::logs::Error::Io(e) => PyOSError::new_err(e),
        janitor::logs::Error::LogRetrieval(e) => LogRetrievalError::new_err(e),
        janitor::logs::Error::Timeout => PyTimeoutError::new_err("Operation timed out"),
        janitor::logs::Error::InvalidName(name) => {
            PyValueError::new_err(format!("Invalid name: {}", name))
        }
        janitor::logs::Error::Other(e) => PyRuntimeError::new_err(e),
    }
}

async fn with_timeout<F, T>(timeout: Option<Duration>, fut: F) -> PyResult<T>
where
    F: Future<Output = Result<T, janitor::logs::Error>>,
{
    let r = match timeout {
        Some(timeout) => tokio::time::timeout(timeout, fut)
            .await
            .map_err(|_| PyTimeoutError::new_err("Timeout"))?,
        None => fut.await,
    };
    r.map_err(convert_logs_error_to_py)
}

#[pyclass(subclass)]
pub struct LogFileManager(Arc<janitor::logs::LogFileManager>);

#[pymethods]
impl LogFileManager {
    #[pyo3(signature = (codebase, run_id, name, timeout=None))]
    fn has_log<'a>(
        &self,
        py: Python<'a>,
        codebase: CodebaseName,
        run_id: RunId,
        name: String,
        timeout: Option<Duration>,
    ) -> PyResult<Bound<'a, PyAny>> {
        let z = self.0.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            with_timeout(timeout, z.has_log(&codebase, &run_id, &name)).await
        })
    }

    #[pyo3(signature = (codebase, run_id, name, timeout=None))]
    fn get_log<'a>(
        &self,
        py: Python<'a>,
        codebase: CodebaseName,
        run_id: RunId,
        name: String,
        timeout: Option<Duration>,
    ) -> PyResult<Bound<'a, PyAny>> {
        let z = self.0.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let stream = with_timeout(timeout, z.get_log(&codebase, &run_id, &name)).await?;
            Ok(AsyncReader::new(
                stream.map_err(convert_logs_error_to_py).boxed(),
            ))
        })
    }

    #[pyo3(signature = (codebase, run_id, orig_path, timeout=None, basename=None))]
    fn import_log<'a>(
        &self,
        py: Python<'a>,
        codebase: CodebaseName,
        run_id: RunId,
        orig_path: String,
        timeout: Option<Duration>,
        basename: Option<String>,
    ) -> PyResult<Bound<'a, PyAny>> {
        let z = self.0.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            with_timeout(
                timeout,
                z.import_log(&codebase, &run_id, &orig_path, basename.as_deref()),
            )
            .await
        })
    }

    #[pyo3(signature = (codebase, run_id, name))]
    fn delete_log<'a>(
        &self,
        py: Python<'a>,
        codebase: CodebaseName,
        run_id: RunId,
        name: String,
    ) -> PyResult<Bound<'a, PyAny>> {
        let z = self.0.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            z.delete_log(&codebase, &run_id, &name)
                .await
                .map_err(convert_logs_error_to_py)
        })
    }

    #[pyo3(signature = (codebase, run_id, name))]
    fn get_ctime<'a>(
        &self,
        py: Python<'a>,
        codebase: CodebaseName,
        run_id: RunId,
        name: String,
    ) -> PyResult<Bound<'a, PyAny>> {
        let z = self.0.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            z.get_ctime(&codebase, &run_id, &name)
                .await
                .map_err(convert_logs_error_to_py)
        })
    }

    fn __aenter__<'a>(slf: pyo3::Bound<Self>, py: Python<'a>) -> PyResult<Bound<'a, PyAny>> {
        let slf = slf.clone().unbind();
        pyo3_async_runtimes::tokio::future_into_py(py, async move { Ok(slf) })
    }

    fn __aexit__<'a>(
        &self,
        py: Python<'a>,
        _exc_type: Py<PyAny>,
        _exc_value: Py<PyAny>,
        _traceback: Py<PyAny>,
    ) -> PyResult<Bound<'a, PyAny>> {
        let none = py.None();
        pyo3_async_runtimes::tokio::future_into_py(py, async move { Ok(none) })
    }

    /// Iterate over all logs, yielding (codebase, run_id, name) tuples.
    fn iter_logs(&self) -> LogIterator {
        LogIterator(Arc::new(tokio::sync::Mutex::new(self.0.iter_logs())))
    }
}

#[pyclass]
pub struct LogIterator(
    Arc<tokio::sync::Mutex<BoxStream<'static, Result<LogListing, janitor::logs::Error>>>>,
);

#[pymethods]
impl LogIterator {
    fn __aiter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __anext__<'a>(&self, py: Python<'a>) -> PyResult<Bound<'a, PyAny>> {
        let entries = self.0.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            entries
                .lock()
                .await
                .try_next()
                .await
                .map_err(convert_logs_error_to_py)?
                .ok_or_else(|| PyStopAsyncIteration::new_err(()))
        })
    }
}

#[pyclass(extends=LogFileManager)]
pub struct FileSystemLogFileManager;

#[pymethods]
impl FileSystemLogFileManager {
    #[new]
    fn new(log_directory: std::path::PathBuf) -> PyResult<PyClassInitializer<Self>> {
        let z = janitor::logs::LogFileManager::local(log_directory)
            .map_err(convert_logs_error_to_py)?;
        Ok(PyClassInitializer::from(LogFileManager(Arc::new(z)))
            .add_subclass(FileSystemLogFileManager))
    }
}

#[pyclass(extends=LogFileManager)]
pub struct S3LogFileManager;

#[pymethods]
impl S3LogFileManager {
    #[new]
    fn new(location: &str) -> PyResult<PyClassInitializer<Self>> {
        let z =
            janitor::logs::create_log_manager(Some(location)).map_err(convert_logs_error_to_py)?;
        Ok(PyClassInitializer::from(LogFileManager(Arc::new(z))).add_subclass(S3LogFileManager))
    }
}

#[pyclass(extends=LogFileManager)]
pub struct GCSLogFileManager;

#[pymethods]
impl GCSLogFileManager {
    #[new]
    fn new(location: &str) -> PyResult<PyClassInitializer<Self>> {
        let url = url::Url::parse(location).map_err(|e| PyValueError::new_err(e.to_string()))?;
        let bucket_name = url
            .host_str()
            .ok_or_else(|| PyValueError::new_err(format!("missing bucket name in {}", location)))?;
        let z =
            janitor::logs::LogFileManager::gcs(bucket_name).map_err(convert_logs_error_to_py)?;
        Ok(PyClassInitializer::from(LogFileManager(Arc::new(z))).add_subclass(GCSLogFileManager))
    }
}

#[pyfunction]
#[pyo3(signature = (location=None))]
fn get_log_manager(location: Option<&str>) -> PyResult<LogFileManager> {
    let manager = janitor::logs::create_log_manager(location).map_err(convert_logs_error_to_py)?;
    Ok(LogFileManager(Arc::new(manager)))
}

pub(crate) fn init(py: Python, module: &Bound<PyModule>) -> PyResult<()> {
    module.add_class::<LogFileManager>()?;
    module.add_class::<FileSystemLogFileManager>()?;
    module.add_class::<S3LogFileManager>()?;
    module.add_class::<GCSLogFileManager>()?;
    module.add_function(wrap_pyfunction!(get_log_manager, module)?)?;
    module.add("ServiceUnavailable", py.get_type::<ServiceUnavailable>())?;
    module.add("LogRetrievalError", py.get_type::<LogRetrievalError>())?;
    Ok(())
}
