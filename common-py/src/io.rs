use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use std::io::Read;

#[pyclass]
pub(crate) struct Readable(Box<dyn std::io::Read + Send + Sync>);

impl Readable {
    pub fn new(read: Box<dyn std::io::Read + Send + Sync>) -> Self {
        Self(read)
    }
}

fn read_bytes(source: &mut dyn std::io::Read, size: Option<usize>) -> std::io::Result<Vec<u8>> {
    match size {
        // Python's file-object convention: read() / read(None) means "read until EOF",
        // not "read one buffer" - std::io::Read::read() is allowed to return fewer bytes
        // than the buffer without that meaning EOF.
        None => {
            let mut buf = Vec::new();
            source.read_to_end(&mut buf)?;
            Ok(buf)
        }
        Some(n) => {
            let mut buf = vec![0; n];
            let read_n = source.read(&mut buf)?;
            buf.truncate(read_n);
            Ok(buf)
        }
    }
}

#[pymethods]
impl Readable {
    #[pyo3(signature = (size=None))]
    fn read(&mut self, py: Python, size: Option<usize>) -> PyResult<Py<PyAny>> {
        let buf = read_bytes(&mut self.0, size).map_err(PyRuntimeError::new_err)?;
        Ok(PyBytes::new(py, &buf).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_bytes_none_reads_past_one_buffer() {
        let data = vec![7u8; 5000];
        let mut cursor = std::io::Cursor::new(data.clone());
        assert_eq!(read_bytes(&mut cursor, None).unwrap(), data);
    }

    #[test]
    fn test_read_bytes_some_respects_requested_size() {
        let data = vec![7u8; 100];
        let mut cursor = std::io::Cursor::new(data);
        assert_eq!(read_bytes(&mut cursor, Some(10)).unwrap().len(), 10);
    }
}
