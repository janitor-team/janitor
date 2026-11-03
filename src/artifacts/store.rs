use futures::stream::{BoxStream, StreamExt, TryStreamExt};
use janitor_types::RunId;
use object_store::buffered::BufWriter;
use object_store::local::LocalFileSystem;
use object_store::path::{Path as ObjectPath, PathPart};
use object_store::{ObjectStore, ObjectStoreExt};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

use crate::artifacts::{ArtifactStream, Error};

/// Number of files transferred concurrently when storing or retrieving a run.
const MAX_CONCURRENT_TRANSFERS: usize = 8;

/// Artifact manager storing each run's artifacts as `<run_id>/<filename>` in an object store.
#[derive(Debug)]
pub struct ArtifactManager {
    store: Arc<dyn ObjectStore>,
    public_base: url::Url,
}

fn run_prefix(run_id: &RunId) -> Result<ObjectPath, Error> {
    let part = PathPart::parse(run_id.as_str()).map_err(|_| Error::InvalidPath)?;
    Ok(ObjectPath::ROOT.join(part))
}

fn artifact_path(run_id: &RunId, filename: &str) -> Result<ObjectPath, Error> {
    let part = PathPart::parse(filename).map_err(|_| Error::InvalidPath)?;
    Ok(run_prefix(run_id)?.join(part))
}

impl ArtifactManager {
    /// Create a manager on top of `store`; `public_base` is the URL under
    /// which the root of the store is publicly reachable.
    pub fn new(store: Arc<dyn ObjectStore>, public_base: url::Url) -> Self {
        Self { store, public_base }
    }

    /// Create a manager storing artifacts in a local directory, creating it if necessary.
    pub fn local(path: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(path)?;
        let path = path.canonicalize()?;
        let store = LocalFileSystem::new_with_prefix(&path)
            .map_err(std::io::Error::other)?
            .with_automatic_cleanup(true);
        let public_base = url::Url::from_directory_path(&path).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{} can not be represented as a URL", path.display()),
            )
        })?;
        Ok(Self::new(Arc::new(store), public_base))
    }

    /// Create a manager storing artifacts in memory, for use in tests.
    pub fn in_memory() -> Self {
        Self::new(
            Arc::new(object_store::memory::InMemory::new()),
            "memory:///".parse().expect("valid URL"),
        )
    }

    /// Create a manager storing artifacts in a Google Cloud Storage bucket.
    ///
    /// Credentials are picked up from the environment, falling back to
    /// application default credentials and the instance metadata server.
    #[cfg(feature = "gcs")]
    pub fn gcs(bucket_name: &str) -> Result<Self, Error> {
        let store = object_store::gcp::GoogleCloudStorageBuilder::from_env()
            .with_bucket_name(bucket_name)
            .build()?;
        let public_base = format!("https://storage.googleapis.com/{}/", bucket_name)
            .parse()
            .map_err(|e| Error::Other(format!("invalid bucket name {}: {}", bucket_name, e)))?;
        Ok(Self::new(Arc::new(store), public_base))
    }

    async fn list_run(&self, run_id: &RunId) -> Result<Vec<ObjectPath>, Error> {
        let prefix = run_prefix(run_id)?;
        let objects: Vec<_> = self
            .store
            .list(Some(&prefix))
            .map_ok(|meta| meta.location)
            .try_collect()
            .await?;
        if objects.is_empty() {
            return Err(Error::ArtifactsMissing);
        }
        Ok(objects)
    }

    async fn upload_file(&self, local_file: &Path, location: &ObjectPath) -> Result<(), Error> {
        let mut file = tokio::fs::File::open(local_file).await?;
        let mut writer = BufWriter::new(self.store.clone(), location.clone());
        if let Err(e) = tokio::io::copy(&mut file, &mut writer).await {
            writer.abort().await?;
            return Err(e.into());
        }
        writer.shutdown().await?;
        Ok(())
    }

    /// Store artifacts from a local directory.
    ///
    /// Only the files listed in `names` are stored, or all regular files in
    /// `local_path` if `names` is None.
    pub async fn store_artifacts(
        &self,
        run_id: &RunId,
        local_path: &Path,
        names: Option<&[String]>,
    ) -> Result<(), Error> {
        let names = match names {
            Some(names) => names.to_vec(),
            None => {
                let mut entries = vec![];
                let mut rd = tokio::fs::read_dir(local_path).await?;
                while let Some(entry) = rd.next_entry().await? {
                    if !entry.file_type().await?.is_file() {
                        continue;
                    }
                    let name = entry.file_name().into_string().map_err(|name| {
                        Error::Other(format!("non-UTF-8 artifact filename: {:?}", name))
                    })?;
                    entries.push(name);
                }
                entries
            }
        };

        let uploads = names
            .iter()
            .map(|name| Ok((local_path.join(name), artifact_path(run_id, name)?)))
            .collect::<Result<Vec<_>, Error>>()?;

        futures::stream::iter(uploads)
            .map(|(local_file, location)| async move {
                self.upload_file(&local_file, &location).await
            })
            .buffer_unordered(MAX_CONCURRENT_TRANSFERS)
            .try_collect::<()>()
            .await
    }

    /// Get the contents of a single artifact.
    pub async fn get_artifact(
        &self,
        run_id: &RunId,
        filename: &str,
    ) -> Result<ArtifactStream, Error> {
        let location = artifact_path(run_id, filename)?;
        let result = self.store.get(&location).await?;
        Ok(result.into_stream().map_err(Error::from).boxed())
    }

    /// Get the public URL for an artifact.
    pub fn public_artifact_url(&self, run_id: &RunId, filename: &str) -> url::Url {
        let mut url = self.public_base.clone();
        url.path_segments_mut()
            .expect("public base URL can be a base")
            .pop_if_empty()
            .push(run_id.as_str())
            .push(filename);
        url
    }

    /// Retrieve the artifacts of a run to a local directory, optionally
    /// only those whose name matches `filter_fn`.
    pub async fn retrieve_artifacts(
        &self,
        run_id: &RunId,
        local_path: &Path,
        filter_fn: Option<&(dyn for<'a> Fn(&'a str) -> bool + Sync + Send)>,
    ) -> Result<(), Error> {
        let objects = self.list_run(run_id).await?;
        let wanted = objects.into_iter().filter(|location| {
            let name = location.filename().expect("listed objects have a name");
            filter_fn.is_none_or(|f| f(name))
        });

        futures::stream::iter(wanted)
            .map(|location| async move {
                let name = location.filename().expect("listed objects have a name");
                let mut stream = self.store.get(&location).await?.into_stream();
                let mut file = tokio::fs::File::create(local_path.join(name)).await?;
                while let Some(chunk) = stream.try_next().await? {
                    file.write_all(&chunk).await?;
                }
                file.flush().await?;
                Ok::<(), Error>(())
            })
            .buffer_unordered(MAX_CONCURRENT_TRANSFERS)
            .try_collect::<()>()
            .await
    }

    /// List the ids of all runs that have artifacts.
    pub fn iter_ids(&self) -> BoxStream<'static, Result<String, Error>> {
        // list_with_delimiter would only fetch the run prefixes, but it
        // collects the full listing before returning; walk all objects instead
        // so ids are yielded as they are found.
        let mut seen = HashSet::new();
        self.store
            .list(None)
            .map_err(Error::from)
            .try_filter_map(move |meta| {
                let id = meta
                    .location
                    .parts()
                    .next()
                    .map(|part| part.as_ref().to_string())
                    .filter(|id| seen.insert(id.clone()));
                futures::future::ready(Ok(id))
            })
            .boxed()
    }

    /// Delete all artifacts for a run.
    pub async fn delete_artifacts(&self, run_id: &RunId) -> Result<(), Error> {
        let objects = self.list_run(run_id).await?;
        self.store
            .delete_stream(futures::stream::iter(objects.into_iter().map(Ok)).boxed())
            .try_collect::<Vec<_>>()
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup() -> (TempDir, ArtifactManager) {
        let td = TempDir::new().unwrap();
        let mgr = ArtifactManager::local(td.path()).unwrap();
        (td, mgr)
    }

    fn create_source_dir(files: &[(&str, &[u8])]) -> TempDir {
        let td = TempDir::new().unwrap();
        for (name, content) in files {
            fs::write(td.path().join(name), content).unwrap();
        }
        td
    }

    fn run_id(id: &str) -> RunId {
        RunId::new(id).unwrap()
    }

    fn sorted_listing(path: &Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(path)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn test_store_and_retrieve() {
        let (td, mgr) = setup();
        let source = create_source_dir(&[("hello.txt", b"hello world"), ("data.bin", b"\x00\x01")]);

        mgr.store_artifacts(&run_id("run-1"), source.path(), None)
            .await
            .unwrap();

        // Keep the on-disk layout compatible with existing artifact directories.
        assert_eq!(
            fs::read(td.path().join("run-1").join("hello.txt")).unwrap(),
            b"hello world"
        );

        let retrieve_dir = TempDir::new().unwrap();
        mgr.retrieve_artifacts(&run_id("run-1"), retrieve_dir.path(), None)
            .await
            .unwrap();

        assert_eq!(
            sorted_listing(retrieve_dir.path()),
            vec!["data.bin", "hello.txt"]
        );
        assert_eq!(
            fs::read_to_string(retrieve_dir.path().join("hello.txt")).unwrap(),
            "hello world"
        );
        assert_eq!(
            fs::read(retrieve_dir.path().join("data.bin")).unwrap(),
            b"\x00\x01"
        );
    }

    /// Files larger than the BufWriter capacity go through a multipart upload.
    #[tokio::test]
    async fn test_store_and_retrieve_large_file() {
        let (_td, mgr) = setup();
        let content: Vec<u8> = (0..25 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        let source = create_source_dir(&[("big.deb", &content)]);

        mgr.store_artifacts(&run_id("run-1"), source.path(), None)
            .await
            .unwrap();

        let retrieve_dir = TempDir::new().unwrap();
        mgr.retrieve_artifacts(&run_id("run-1"), retrieve_dir.path(), None)
            .await
            .unwrap();
        assert!(fs::read(retrieve_dir.path().join("big.deb")).unwrap() == content);
    }

    #[tokio::test]
    async fn test_store_twice_is_idempotent() {
        let (_td, mgr) = setup();
        let source = create_source_dir(&[("file.txt", b"content")]);

        mgr.store_artifacts(&run_id("run-1"), source.path(), None)
            .await
            .unwrap();
        mgr.store_artifacts(&run_id("run-1"), source.path(), None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_store_skips_subdirectories() {
        let (_td, mgr) = setup();
        let source = create_source_dir(&[("file.txt", b"content")]);
        fs::create_dir(source.path().join("subdir")).unwrap();

        mgr.store_artifacts(&run_id("run-1"), source.path(), None)
            .await
            .unwrap();

        let retrieve_dir = TempDir::new().unwrap();
        mgr.retrieve_artifacts(&run_id("run-1"), retrieve_dir.path(), None)
            .await
            .unwrap();
        assert_eq!(sorted_listing(retrieve_dir.path()), vec!["file.txt"]);
    }

    #[tokio::test]
    async fn test_store_with_names_filter() {
        let (_td, mgr) = setup();
        let source = create_source_dir(&[("keep.txt", b"yes"), ("skip.txt", b"no")]);

        mgr.store_artifacts(
            &run_id("run-1"),
            source.path(),
            Some(&["keep.txt".to_string()]),
        )
        .await
        .unwrap();

        let retrieve_dir = TempDir::new().unwrap();
        mgr.retrieve_artifacts(&run_id("run-1"), retrieve_dir.path(), None)
            .await
            .unwrap();

        assert_eq!(sorted_listing(retrieve_dir.path()), vec!["keep.txt"]);
    }

    #[tokio::test]
    async fn test_store_rejects_path_traversal() {
        let (_td, mgr) = setup();
        let source = create_source_dir(&[]);

        let result = mgr
            .store_artifacts(&run_id("run-1"), source.path(), Some(&["..".to_string()]))
            .await;
        assert!(matches!(result, Err(Error::InvalidPath)), "got {result:?}");
    }

    #[tokio::test]
    async fn test_store_missing_file() {
        let (_td, mgr) = setup();
        let source = create_source_dir(&[]);

        let result = mgr
            .store_artifacts(
                &run_id("run-1"),
                source.path(),
                Some(&["missing.txt".to_string()]),
            )
            .await;
        assert!(matches!(result, Err(Error::IoError(_))), "got {result:?}");
    }

    #[tokio::test]
    async fn test_retrieve_nonexistent() {
        let (_td, mgr) = setup();
        let retrieve_dir = TempDir::new().unwrap();
        let result = mgr
            .retrieve_artifacts(&run_id("nonexistent"), retrieve_dir.path(), None)
            .await;
        assert!(matches!(result, Err(Error::ArtifactsMissing)));
    }

    #[tokio::test]
    async fn test_retrieve_with_filter() {
        let (_td, mgr) = setup();
        let source = create_source_dir(&[("keep.txt", b"yes"), ("skip.txt", b"no")]);

        mgr.store_artifacts(&run_id("run-1"), source.path(), None)
            .await
            .unwrap();

        let retrieve_dir = TempDir::new().unwrap();
        mgr.retrieve_artifacts(
            &run_id("run-1"),
            retrieve_dir.path(),
            Some(&|name: &str| name == "keep.txt"),
        )
        .await
        .unwrap();

        assert_eq!(sorted_listing(retrieve_dir.path()), vec!["keep.txt"]);
    }

    #[tokio::test]
    async fn test_delete_artifacts() {
        let (_td, mgr) = setup();
        let source = create_source_dir(&[("file.txt", b"content")]);

        mgr.store_artifacts(&run_id("run-1"), source.path(), None)
            .await
            .unwrap();
        mgr.delete_artifacts(&run_id("run-1")).await.unwrap();

        let retrieve_dir = TempDir::new().unwrap();
        let result = mgr
            .retrieve_artifacts(&run_id("run-1"), retrieve_dir.path(), None)
            .await;
        assert!(matches!(result, Err(Error::ArtifactsMissing)));
        let ids: Vec<String> = mgr.iter_ids().try_collect().await.unwrap();
        assert_eq!(ids, Vec::<String>::new());
    }

    #[tokio::test]
    async fn test_delete_nonexistent() {
        let (_td, mgr) = setup();
        let result = mgr.delete_artifacts(&run_id("nonexistent")).await;
        assert!(matches!(result, Err(Error::ArtifactsMissing)));
    }

    #[tokio::test]
    async fn test_iter_ids() {
        let (_td, local) = setup();
        for mgr in [local, ArtifactManager::in_memory()] {
            let source = create_source_dir(&[("a.txt", b"a"), ("b.txt", b"b"), ("c.txt", b"c")]);

            // Ids that are prefixes of each other must still be told apart.
            for id in ["run-1", "run-1-a", "run-10"] {
                mgr.store_artifacts(&run_id(id), source.path(), None)
                    .await
                    .unwrap();
            }

            let mut ids: Vec<String> = mgr.iter_ids().try_collect().await.unwrap();
            ids.sort();
            assert_eq!(ids, vec!["run-1", "run-1-a", "run-10"], "{:?}", mgr);
        }
    }

    #[tokio::test]
    async fn test_iter_ids_empty() {
        let (_td, mgr) = setup();
        let ids: Vec<String> = mgr.iter_ids().try_collect().await.unwrap();
        assert_eq!(ids, Vec::<String>::new());
    }

    #[tokio::test]
    async fn test_get_artifact() {
        let (_td, mgr) = setup();
        let source = create_source_dir(&[("file.txt", b"hello")]);

        mgr.store_artifacts(&run_id("run-1"), source.path(), None)
            .await
            .unwrap();

        let content: Vec<u8> = mgr
            .get_artifact(&run_id("run-1"), "file.txt")
            .await
            .unwrap()
            .map_ok(|chunk| chunk.to_vec())
            .try_concat()
            .await
            .unwrap();
        assert_eq!(content, b"hello");
    }

    #[tokio::test]
    async fn test_get_artifact_missing() {
        let (_td, mgr) = setup();
        let result = mgr.get_artifact(&run_id("run-1"), "file.txt").await;
        assert!(matches!(result, Err(Error::ArtifactsMissing)));
    }

    #[test]
    fn test_public_artifact_url_local() {
        let (td, mgr) = setup();
        let expected = url::Url::from_file_path(
            td.path()
                .canonicalize()
                .unwrap()
                .join("run-1")
                .join("file.txt"),
        )
        .unwrap();
        assert_eq!(
            mgr.public_artifact_url(&run_id("run-1"), "file.txt"),
            expected
        );
    }

    #[test]
    fn test_public_artifact_url_encodes_filename() {
        let mgr = ArtifactManager::new(
            Arc::new(object_store::memory::InMemory::new()),
            "https://storage.googleapis.com/bucket/".parse().unwrap(),
        );
        assert_eq!(
            mgr.public_artifact_url(&run_id("run-1"), "foo bar_1.0~1_amd64.deb")
                .as_str(),
            "https://storage.googleapis.com/bucket/run-1/foo%20bar_1.0~1_amd64.deb"
        );
    }
}
