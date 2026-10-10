use crate::io::Readable;
use chrono::{DateTime, Utc};
use pyo3::create_exception;
use pyo3::exceptions::{
    PyFileNotFoundError, PyOSError, PyPermissionError, PyRuntimeError, PyStopAsyncIteration,
    PyTimeoutError,
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

type DynLogFileManager = Arc<dyn janitor::logs::LogFileManager>;
type LogListing = (String, String, Vec<String>);

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
pub struct LogFileManager(DynLogFileManager);

#[pymethods]
impl LogFileManager {
    #[pyo3(signature = (codebase, run_id, name, timeout=None))]
    fn has_log<'a>(
        &self,
        py: Python<'a>,
        codebase: String,
        run_id: String,
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
        codebase: String,
        run_id: String,
        name: String,
        timeout: Option<Duration>,
    ) -> PyResult<Bound<'a, PyAny>> {
        let z = self.0.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let readable = with_timeout(timeout, z.get_log(&codebase, &run_id, &name)).await?;
            Ok(Readable::new(readable))
        })
    }

    #[pyo3(signature = (codebase, run_id, orig_path, timeout=None, mtime=None, basename=None))]
    #[allow(clippy::too_many_arguments)]
    fn import_log<'a>(
        &self,
        py: Python<'a>,
        codebase: String,
        run_id: String,
        orig_path: String,
        timeout: Option<Duration>,
        mtime: Option<DateTime<Utc>>,
        basename: Option<String>,
    ) -> PyResult<Bound<'a, PyAny>> {
        let z = self.0.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            with_timeout(
                timeout,
                z.import_log(&codebase, &run_id, &orig_path, mtime, basename.as_deref()),
            )
            .await
        })
    }

    #[pyo3(signature = (codebase, run_id, name))]
    fn delete_log<'a>(
        &self,
        py: Python<'a>,
        codebase: String,
        run_id: String,
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
        codebase: String,
        run_id: String,
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

    /// Iterate over all logs, yielding (codebase, run_id, names) tuples.
    fn iter_logs(&self) -> LogIterator {
        LogIterator {
            manager: self.0.clone(),
            entries: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }
}

#[pyclass]
pub struct LogIterator {
    manager: DynLogFileManager,
    entries: Arc<tokio::sync::Mutex<Option<std::vec::IntoIter<LogListing>>>>,
}

#[pymethods]
impl LogIterator {
    fn __aiter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __anext__<'a>(&self, py: Python<'a>) -> PyResult<Bound<'a, PyAny>> {
        let manager = self.manager.clone();
        let entries = self.entries.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let mut entries = entries.lock().await;
            if entries.is_none() {
                let listing = manager.iter_logs().await.collect::<Vec<_>>();
                *entries = Some(listing.into_iter());
            }
            entries
                .as_mut()
                .and_then(|it| it.next())
                .ok_or_else(|| PyStopAsyncIteration::new_err(()))
        })
    }
}

#[pyclass(extends=LogFileManager)]
pub struct FileSystemLogFileManager;

#[pymethods]
impl FileSystemLogFileManager {
    #[new]
    fn new(log_directory: std::path::PathBuf) -> PyResult<(Self, LogFileManager)> {
        let z = janitor::logs::FileSystemLogFileManager::new(log_directory)
            .map_err(convert_logs_error_to_py)?;
        Ok((FileSystemLogFileManager, LogFileManager(Arc::new(z))))
    }
}

#[pyclass(extends=LogFileManager)]
pub struct S3LogFileManager;

#[pymethods]
impl S3LogFileManager {
    #[new]
    #[pyo3(signature = (endpoint_url, bucket_name=None))]
    fn new(endpoint_url: &str, bucket_name: Option<&str>) -> PyResult<(Self, LogFileManager)> {
        let z = janitor::logs::S3LogFileManager::new(endpoint_url, bucket_name)
            .map_err(convert_logs_error_to_py)?;
        Ok((S3LogFileManager, LogFileManager(Arc::new(z))))
    }
}

#[pyclass(extends=LogFileManager)]
pub struct GCSLogFileManager;

#[pymethods]
impl GCSLogFileManager {
    #[new]
    fn new(location: &str) -> PyResult<(Self, LogFileManager)> {
        let url = url::Url::parse(location)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
        let z = pyo3_async_runtimes::tokio::get_runtime()
            .block_on(janitor::logs::GCSLogFileManager::from_url(&url, None))
            .map_err(convert_logs_error_to_py)?;
        Ok((GCSLogFileManager, LogFileManager(Arc::new(z))))
    }
}

#[pyfunction]
#[pyo3(signature = (location=None))]
fn get_log_manager(location: Option<&str>) -> PyResult<LogFileManager> {
    let manager = pyo3_async_runtimes::tokio::get_runtime()
        .block_on(janitor::logs::get_log_manager(location))
        .map_err(convert_logs_error_to_py)?;
    Ok(LogFileManager(Arc::from(manager)))
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
