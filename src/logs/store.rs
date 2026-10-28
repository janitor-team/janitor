use crate::{CodebaseName, RunId};
use async_compression::tokio::bufread::{GzipDecoder, GzipEncoder};
use chrono::{DateTime, Utc};
use futures::stream::{BoxStream, StreamExt, TryStreamExt};
use object_store::buffered::BufWriter;
use object_store::local::LocalFileSystem;
use object_store::path::{Path as ObjectPath, PathPart};
use object_store::{GetResult, ObjectStore, ObjectStoreExt};
use std::path::Path;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio_util::io::{ReaderStream, StreamReader};

use crate::logs::Error;

/// The decompressed contents of a log, as a stream of chunks.
pub type LogStream = BoxStream<'static, Result<bytes::Bytes, Error>>;

/// Log file manager storing each log gzip-compressed as
/// `<codebase>/<run_id>/<name>.gz` in an object store.
///
/// Logs stored uncompressed as `<codebase>/<run_id>/<name>` can still be read
/// and deleted.
#[derive(Debug)]
pub struct LogFileManager {
    store: Arc<dyn ObjectStore>,
}

fn run_prefix(codebase: &CodebaseName, run_id: &RunId) -> Result<ObjectPath, Error> {
    let codebase =
        PathPart::parse(codebase.as_str()).map_err(|_| Error::InvalidName(codebase.to_string()))?;
    let run_id =
        PathPart::parse(run_id.as_str()).map_err(|_| Error::InvalidName(run_id.to_string()))?;
    Ok(ObjectPath::ROOT.join(codebase).join(run_id))
}

/// The locations a log may be stored at, compressed first.
fn log_paths(
    codebase: &CodebaseName,
    run_id: &RunId,
    name: &str,
) -> Result<[(ObjectPath, bool); 2], Error> {
    let prefix = run_prefix(codebase, run_id)?;
    let plain = PathPart::parse(name).map_err(|_| Error::InvalidName(name.to_string()))?;
    let compressed_name = format!("{}.gz", name);
    let compressed =
        PathPart::parse(&compressed_name).map_err(|_| Error::InvalidName(name.to_string()))?;
    Ok([
        (prefix.clone().join(compressed), true),
        (prefix.join(plain), false),
    ])
}

fn decompressed_stream(result: GetResult) -> LogStream {
    let reader = StreamReader::new(result.into_stream().map_err(std::io::Error::other));
    ReaderStream::new(GzipDecoder::new(reader))
        .map_err(Error::from)
        .boxed()
}

impl LogFileManager {
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        Self { store }
    }

    /// Create a manager storing logs in a local directory, creating it if necessary.
    pub fn local(path: impl AsRef<Path>) -> Result<Self, Error> {
        std::fs::create_dir_all(path.as_ref())?;
        let store = LocalFileSystem::new_with_prefix(path)?.with_automatic_cleanup(true);
        Ok(Self::new(Arc::new(store)))
    }

    /// Create a manager storing logs in memory, for use in tests.
    pub fn in_memory() -> Self {
        Self::new(Arc::new(object_store::memory::InMemory::new()))
    }

    /// Create a manager storing logs in a Google Cloud Storage bucket.
    ///
    /// Credentials are picked up from the environment, falling back to
    /// application default credentials and the instance metadata server.
    #[cfg(feature = "gcs")]
    pub fn gcs(bucket_name: &str) -> Result<Self, Error> {
        let store = object_store::gcp::GoogleCloudStorageBuilder::from_env()
            .with_bucket_name(bucket_name)
            .build()?;
        Ok(Self::new(Arc::new(store)))
    }

    /// Create a manager storing logs in an S3 bucket, given as
    /// `s3://bucket[/prefix]`.
    ///
    /// The endpoint, region and credentials are picked up from the `AWS_*`
    /// environment variables.
    pub fn s3(location: &str) -> Result<Self, Error> {
        let url = url::Url::parse(location)
            .map_err(|e| Error::Other(format!("Invalid S3 URL {}: {}", location, e)))?;
        let prefix = ObjectPath::from_url_path(url.path()).map_err(object_store::Error::from)?;
        let store = object_store::aws::AmazonS3Builder::from_env()
            .with_url(location)
            .with_allow_http(true)
            .build()?;
        Ok(Self::new(Arc::new(object_store::prefix::PrefixStore::new(
            store, prefix,
        ))))
    }

    async fn find_log(
        &self,
        codebase: &CodebaseName,
        run_id: &RunId,
        name: &str,
    ) -> Result<(ObjectPath, object_store::ObjectMeta), Error> {
        for (location, _) in log_paths(codebase, run_id, name)? {
            match self.store.head(&location).await {
                Ok(meta) => return Ok((location, meta)),
                Err(object_store::Error::NotFound { .. }) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(Error::NotFound)
    }

    /// Check if a log exists.
    pub async fn has_log(
        &self,
        codebase: &CodebaseName,
        run_id: &RunId,
        name: &str,
    ) -> Result<bool, Error> {
        match self.find_log(codebase, run_id, name).await {
            Ok(_) => Ok(true),
            Err(Error::NotFound) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Get the decompressed contents of a log.
    pub async fn get_log(
        &self,
        codebase: &CodebaseName,
        run_id: &RunId,
        name: &str,
    ) -> Result<LogStream, Error> {
        for (location, compressed) in log_paths(codebase, run_id, name)? {
            let result = match self.store.get(&location).await {
                Ok(result) => result,
                Err(object_store::Error::NotFound { .. }) => continue,
                Err(e) => return Err(e.into()),
            };
            return Ok(if compressed {
                decompressed_stream(result)
            } else {
                result.into_stream().map_err(Error::from).boxed()
            });
        }
        Err(Error::NotFound)
    }

    /// Store the file at `orig_path` compressed, under `basename` or else its
    /// own file name.
    pub async fn import_log(
        &self,
        codebase: &CodebaseName,
        run_id: &RunId,
        orig_path: &str,
        basename: Option<&str>,
    ) -> Result<(), Error> {
        let basename = match basename {
            Some(basename) => basename,
            None => Path::new(orig_path)
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| Error::InvalidName(orig_path.to_string()))?,
        };
        let [(location, _), _] = log_paths(codebase, run_id, basename)?;

        let file = tokio::fs::File::open(orig_path).await?;
        let mut encoder = GzipEncoder::new(tokio::io::BufReader::new(file));
        let mut writer = BufWriter::new(self.store.clone(), location);
        if let Err(e) = tokio::io::copy(&mut encoder, &mut writer).await {
            writer.abort().await?;
            return Err(e.into());
        }
        writer.shutdown().await?;
        Ok(())
    }

    /// Delete a log.
    pub async fn delete_log(
        &self,
        codebase: &CodebaseName,
        run_id: &RunId,
        name: &str,
    ) -> Result<(), Error> {
        // Look the log up first: some stores report success when deleting a
        // missing object.
        let (location, _) = self.find_log(codebase, run_id, name).await?;
        self.store.delete(&location).await?;
        Ok(())
    }

    /// List all logs as `(codebase, run_id, name)`.
    pub fn iter_logs(&self) -> BoxStream<'static, Result<(String, String, String), Error>> {
        self.store
            .list(None)
            .map_err(Error::from)
            .try_filter_map(|meta| {
                let parts: Vec<_> = meta.location.parts().collect();
                let entry = match parts.as_slice() {
                    [codebase, run_id, name] => {
                        let name = name.as_ref();
                        Some((
                            codebase.as_ref().to_string(),
                            run_id.as_ref().to_string(),
                            name.strip_suffix(".gz").unwrap_or(name).to_string(),
                        ))
                    }
                    _ => {
                        log::warn!("Ignoring unexpected object {} in log store", meta.location);
                        None
                    }
                };
                futures::future::ready(Ok(entry))
            })
            .boxed()
    }

    /// Get the time a log was stored.
    pub async fn get_ctime(
        &self,
        codebase: &CodebaseName,
        run_id: &RunId,
        name: &str,
    ) -> Result<DateTime<Utc>, Error> {
        let (_, meta) = self.find_log(codebase, run_id, name).await?;
        Ok(meta.last_modified)
    }

    /// Check that the store can be listed.
    pub async fn health_check(&self) -> Result<(), Error> {
        match self.store.list(None).next().await {
            Some(Err(e)) => Err(e.into()),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, LogFileManager) {
        let td = TempDir::new().unwrap();
        let mgr = LogFileManager::local(td.path()).unwrap();
        (td, mgr)
    }

    fn codebase() -> CodebaseName {
        CodebaseName::new("codebase").unwrap()
    }

    fn run_id(id: &str) -> RunId {
        RunId::new(id).unwrap()
    }

    fn write_source(content: &[u8]) -> (TempDir, String) {
        let td = TempDir::new().unwrap();
        let path = td.path().join("build.log");
        std::fs::write(&path, content).unwrap();
        let path = path.to_str().unwrap().to_string();
        (td, path)
    }

    async fn read_log(mgr: &LogFileManager, run: &str, name: &str) -> Result<Vec<u8>, Error> {
        mgr.get_log(&codebase(), &run_id(run), name)
            .await?
            .map_ok(|chunk| chunk.to_vec())
            .try_concat()
            .await
    }

    #[tokio::test]
    async fn test_has_log_not_found() {
        let (_td, mgr) = setup();
        assert!(!mgr
            .has_log(&codebase(), &run_id("run-1"), "build.log")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn test_get_log_not_found() {
        let (_td, mgr) = setup();
        let result = read_log(&mgr, "run-1", "build.log").await;
        assert!(matches!(result, Err(Error::NotFound)), "got {result:?}");
    }

    #[tokio::test]
    async fn test_import_and_get_log() {
        let (td, mgr) = setup();
        let (_src, path) = write_source(b"log content");

        mgr.import_log(&codebase(), &run_id("run-1"), &path, None)
            .await
            .unwrap();

        // Stored compressed, in the same layout as before.
        let stored = std::fs::read(td.path().join("codebase/run-1/build.log.gz")).unwrap();
        assert_eq!(&stored[..2], b"\x1f\x8b");
        assert!(mgr
            .has_log(&codebase(), &run_id("run-1"), "build.log")
            .await
            .unwrap());
        assert_eq!(
            read_log(&mgr, "run-1", "build.log").await.unwrap(),
            b"log content"
        );
    }

    #[tokio::test]
    async fn test_import_large_log() {
        let mgr = LogFileManager::in_memory();
        let content: Vec<u8> = (0..20 * 1024 * 1024)
            .map(|i| b"abcdefghij\n"[i % 11])
            .collect();
        let (_src, path) = write_source(&content);

        mgr.import_log(&codebase(), &run_id("run-1"), &path, None)
            .await
            .unwrap();

        assert!(read_log(&mgr, "run-1", "build.log").await.unwrap() == content);
    }

    #[tokio::test]
    async fn test_import_with_custom_basename() {
        let (_td, mgr) = setup();
        let (_src, path) = write_source(b"content");

        mgr.import_log(&codebase(), &run_id("run-1"), &path, Some("custom.log"))
            .await
            .unwrap();

        assert!(mgr
            .has_log(&codebase(), &run_id("run-1"), "custom.log")
            .await
            .unwrap());
        assert!(!mgr
            .has_log(&codebase(), &run_id("run-1"), "build.log")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn test_get_uncompressed_log() {
        let (td, mgr) = setup();
        let run_dir = td.path().join("codebase/run-1");
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::write(run_dir.join("build.log"), b"plain").unwrap();

        assert!(mgr
            .has_log(&codebase(), &run_id("run-1"), "build.log")
            .await
            .unwrap());
        assert_eq!(
            read_log(&mgr, "run-1", "build.log").await.unwrap(),
            b"plain"
        );

        mgr.delete_log(&codebase(), &run_id("run-1"), "build.log")
            .await
            .unwrap();
        assert!(!run_dir.join("build.log").exists());
    }

    #[tokio::test]
    async fn test_delete_log() {
        let (_td, mgr) = setup();
        let (_src, path) = write_source(b"content");
        mgr.import_log(&codebase(), &run_id("run-1"), &path, None)
            .await
            .unwrap();

        mgr.delete_log(&codebase(), &run_id("run-1"), "build.log")
            .await
            .unwrap();

        assert!(!mgr
            .has_log(&codebase(), &run_id("run-1"), "build.log")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn test_delete_log_not_found() {
        let mgr = LogFileManager::in_memory();
        let result = mgr
            .delete_log(&codebase(), &run_id("run-1"), "build.log")
            .await;
        assert!(matches!(result, Err(Error::NotFound)), "got {result:?}");
    }

    #[tokio::test]
    async fn test_get_ctime() {
        let (_td, mgr) = setup();
        let (_src, path) = write_source(b"content");
        let before = Utc::now() - chrono::Duration::seconds(1);

        mgr.import_log(&codebase(), &run_id("run-1"), &path, None)
            .await
            .unwrap();

        let ctime = mgr
            .get_ctime(&codebase(), &run_id("run-1"), "build.log")
            .await
            .unwrap();
        assert!(ctime >= before, "{ctime} < {before}");
    }

    #[tokio::test]
    async fn test_get_ctime_not_found() {
        let (_td, mgr) = setup();
        let result = mgr
            .get_ctime(&codebase(), &run_id("run-1"), "build.log")
            .await;
        assert!(matches!(result, Err(Error::NotFound)), "got {result:?}");
    }

    #[tokio::test]
    async fn test_iter_logs() {
        let (_td, local) = setup();
        for mgr in [local, LogFileManager::in_memory()] {
            let (_src, path) = write_source(b"content");
            for (run, name) in [
                ("run-1", "build.log"),
                ("run-1", "worker.log"),
                ("run-2", "build.log"),
            ] {
                mgr.import_log(&codebase(), &run_id(run), &path, Some(name))
                    .await
                    .unwrap();
            }

            let mut logs: Vec<_> = mgr.iter_logs().try_collect().await.unwrap();
            logs.sort();
            let expected: Vec<_> = [
                ("codebase", "run-1", "build.log"),
                ("codebase", "run-1", "worker.log"),
                ("codebase", "run-2", "build.log"),
            ]
            .into_iter()
            .map(|(c, r, n)| (c.to_string(), r.to_string(), n.to_string()))
            .collect();
            assert_eq!(logs, expected, "{:?}", mgr);
        }
    }

    #[tokio::test]
    async fn test_iter_logs_empty() {
        let (_td, mgr) = setup();
        let logs: Vec<_> = mgr.iter_logs().try_collect().await.unwrap();
        assert_eq!(logs, vec![]);
    }

    #[tokio::test]
    async fn test_health_check() {
        let (_td, mgr) = setup();
        mgr.health_check().await.unwrap();
    }

    #[tokio::test]
    async fn test_invalid_name_rejected() {
        let (_td, mgr) = setup();
        // Codebase and run id can't contain '/' at all; the log name is
        // checked by the manager.
        assert!(CodebaseName::new("code/base").is_err());
        assert!(RunId::new("run/1").is_err());
        for name in ["build/log", "..", "."] {
            let result = mgr.has_log(&codebase(), &run_id("run-1"), name).await;
            assert!(
                matches!(result, Err(Error::InvalidName(_))),
                "got {result:?}"
            );
        }
    }
}
