use crate::RunId;
use async_trait::async_trait;
use std::fs;
use std::fs::File;
use std::io::{self, ErrorKind};
use std::path::{Component, Path, PathBuf};

use crate::artifacts::{ArtifactManager, Error};

#[derive(Debug)]
pub struct LocalArtifactManager {
    path: PathBuf,
}

impl LocalArtifactManager {
    pub fn new(path: &Path) -> io::Result<Self> {
        if !path.is_dir() {
            fs::create_dir_all(path)?;
        }
        Ok(Self {
            path: path.canonicalize()?,
        })
    }
}

#[async_trait]
impl ArtifactManager for LocalArtifactManager {
    async fn store_artifacts(
        &self,
        run_id: &RunId,
        local_path: &Path,
        names: Option<&[String]>,
    ) -> Result<(), Error> {
        let run_dir = self.path.join(run_id);
        fs::create_dir(&run_dir).or_else(|e| {
            if e.kind() == ErrorKind::AlreadyExists {
                Ok(())
            } else {
                Err(e)
            }
        })?;
        let names = match names {
            Some(names) => names.to_vec(),
            None => fs::read_dir(local_path)
                .map_err(Error::IoError)?
                .filter_map(|entry| entry.ok().and_then(|e| e.file_name().into_string().ok()))
                .collect::<Vec<_>>(),
        };
        for name in names {
            fs::copy(local_path.join(&name), run_dir.join(&name))?;
        }
        Ok(())
    }

    async fn get_artifact(
        &self,
        run_id: &RunId,
        filename: &str,
    ) -> Result<Box<dyn std::io::Read + Send + Sync>, Error> {
        let mut components = Path::new(filename).components();
        if !matches!(
            (components.next(), components.next()),
            (Some(Component::Normal(_)), None)
        ) {
            return Err(io::Error::from(ErrorKind::NotFound).into());
        }
        let path = self.path.join(run_id).join(filename);
        Ok(Box::new(File::open(path)?))
    }

    fn public_artifact_url(&self, run_id: &RunId, filename: &str) -> url::Url {
        url::Url::from_file_path(self.path.join(run_id).join(filename)).unwrap_or_else(|_| {
            url::Url::parse("file:///invalid/path").expect("hardcoded URL should be valid")
        })
    }

    async fn retrieve_artifacts(
        &self,
        run_id: &RunId,
        local_path: &Path,
        filter_fn: Option<&(dyn for<'a> Fn(&'a str) -> bool + Sync + Send)>,
    ) -> Result<(), Error> {
        let run_path = self.path.join(run_id);
        if !run_path.is_dir() {
            return Err(Error::ArtifactsMissing);
        }

        for entry in fs::read_dir(run_path)? {
            let entry = entry?;
            let name = entry.file_name();
            let filter_fn = filter_fn.unwrap_or(&|_| true);
            if let Some(name_str) = name.to_str() {
                if filter_fn(name_str) {
                    fs::copy(entry.path(), local_path.join(&name))?;
                }
            }
        }
        Ok(())
    }

    async fn iter_ids(&self) -> Box<dyn Iterator<Item = String> + Send> {
        let entries = fs::read_dir(&self.path)
            .ok()
            .map(|dir| {
                dir.filter_map(|entry| {
                    entry.ok().and_then(|e| {
                        e.file_type().ok().and_then(|ft| {
                            if ft.is_dir() {
                                e.file_name().into_string().ok()
                            } else {
                                None
                            }
                        })
                    })
                })
                .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Box::new(entries.into_iter())
    }

    async fn delete_artifacts(&self, run_id: &RunId) -> Result<(), Error> {
        let run_path = self.path.join(run_id);
        if !run_path.is_dir() {
            return Err(Error::ArtifactsMissing);
        }
        fs::remove_dir_all(run_path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::ArtifactManager;
    use tempfile::TempDir;

    fn setup() -> (TempDir, LocalArtifactManager) {
        let td = TempDir::new().unwrap();
        let mgr = LocalArtifactManager::new(td.path()).unwrap();
        (td, mgr)
    }

    fn create_source_dir(files: &[(&str, &[u8])]) -> TempDir {
        let td = TempDir::new().unwrap();
        for (name, content) in files {
            fs::write(td.path().join(name), content).unwrap();
        }
        td
    }

    #[tokio::test]
    async fn test_store_and_retrieve() {
        let (_td, mgr) = setup();
        let source = create_source_dir(&[("hello.txt", b"hello world"), ("data.bin", b"\x00\x01")]);

        mgr.store_artifacts(&RunId::new("run-1").unwrap(), source.path(), None)
            .await
            .unwrap();

        let retrieve_dir = TempDir::new().unwrap();
        mgr.retrieve_artifacts(&RunId::new("run-1").unwrap(), retrieve_dir.path(), None)
            .await
            .unwrap();

        assert_eq!(
            fs::read_to_string(retrieve_dir.path().join("hello.txt")).unwrap(),
            "hello world"
        );
        assert_eq!(
            fs::read(retrieve_dir.path().join("data.bin")).unwrap(),
            b"\x00\x01"
        );
    }

    #[tokio::test]
    async fn test_store_twice_is_idempotent() {
        let (_td, mgr) = setup();
        let source = create_source_dir(&[("file.txt", b"content")]);

        mgr.store_artifacts(&RunId::new("run-1").unwrap(), source.path(), None)
            .await
            .unwrap();
        // Storing again should not error
        mgr.store_artifacts(&RunId::new("run-1").unwrap(), source.path(), None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_store_with_names_filter() {
        let (_td, mgr) = setup();
        let source = create_source_dir(&[("keep.txt", b"yes"), ("skip.txt", b"no")]);

        mgr.store_artifacts(
            &RunId::new("run-1").unwrap(),
            source.path(),
            Some(&["keep.txt".to_string()]),
        )
        .await
        .unwrap();

        let retrieve_dir = TempDir::new().unwrap();
        mgr.retrieve_artifacts(&RunId::new("run-1").unwrap(), retrieve_dir.path(), None)
            .await
            .unwrap();

        assert!(retrieve_dir.path().join("keep.txt").exists());
        assert!(!retrieve_dir.path().join("skip.txt").exists());
    }

    #[tokio::test]
    async fn test_retrieve_nonexistent() {
        let (_td, mgr) = setup();
        let retrieve_dir = TempDir::new().unwrap();
        let result = mgr
            .retrieve_artifacts(
                &RunId::new("nonexistent").unwrap(),
                retrieve_dir.path(),
                None,
            )
            .await;
        assert!(matches!(result, Err(Error::ArtifactsMissing)));
    }

    #[tokio::test]
    async fn test_delete_artifacts() {
        let (_td, mgr) = setup();
        let source = create_source_dir(&[("file.txt", b"content")]);

        mgr.store_artifacts(&RunId::new("run-1").unwrap(), source.path(), None)
            .await
            .unwrap();
        mgr.delete_artifacts(&RunId::new("run-1").unwrap())
            .await
            .unwrap();

        let retrieve_dir = TempDir::new().unwrap();
        let result = mgr
            .retrieve_artifacts(&RunId::new("run-1").unwrap(), retrieve_dir.path(), None)
            .await;
        assert!(matches!(result, Err(Error::ArtifactsMissing)));
    }

    #[tokio::test]
    async fn test_delete_nonexistent() {
        let (_td, mgr) = setup();
        let result = mgr
            .delete_artifacts(&RunId::new("nonexistent").unwrap())
            .await;
        assert!(matches!(result, Err(Error::ArtifactsMissing)));
    }

    #[tokio::test]
    async fn test_iter_ids() {
        let (_td, mgr) = setup();
        let source = create_source_dir(&[("file.txt", b"content")]);

        mgr.store_artifacts(&RunId::new("run-1").unwrap(), source.path(), None)
            .await
            .unwrap();
        mgr.store_artifacts(&RunId::new("run-2").unwrap(), source.path(), None)
            .await
            .unwrap();

        let mut ids: Vec<String> = mgr.iter_ids().await.collect();
        ids.sort();
        assert_eq!(ids, vec!["run-1", "run-2"]);
    }

    #[tokio::test]
    async fn test_iter_ids_empty() {
        let (_td, mgr) = setup();
        let ids: Vec<String> = mgr.iter_ids().await.collect();
        assert_eq!(ids, Vec::<String>::new());
    }

    #[tokio::test]
    async fn test_get_artifact() {
        let (_td, mgr) = setup();
        let source = create_source_dir(&[("file.txt", b"hello")]);

        mgr.store_artifacts(&RunId::new("run-1").unwrap(), source.path(), None)
            .await
            .unwrap();

        let mut reader = mgr
            .get_artifact(&RunId::new("run-1").unwrap(), "file.txt")
            .await
            .unwrap();
        let mut content = String::new();
        reader.read_to_string(&mut content).unwrap();
        assert_eq!(content, "hello");
    }

    #[tokio::test]
    async fn test_get_artifact_outside_run_dir() {
        let (td, mgr) = setup();
        let source = create_source_dir(&[("file.txt", b"hello")]);
        mgr.store_artifacts("run-1", source.path(), None)
            .await
            .unwrap();
        let outside = td.path().join("outside.txt");
        fs::write(&outside, b"outside").unwrap();

        for name in ["../outside.txt", outside.to_str().unwrap()] {
            let kind = match mgr.get_artifact("run-1", name).await {
                Err(Error::IoError(e)) => Some(e.kind()),
                _ => None,
            };
            assert_eq!(kind, Some(ErrorKind::NotFound), "{}", name);
        }
    }

    #[tokio::test]
    async fn test_public_artifact_url() {
        let (_td, mgr) = setup();
        let url = mgr.public_artifact_url(&RunId::new("run-1").unwrap(), "file.txt");
        assert_eq!(url.scheme(), "file");
    }

    #[tokio::test]
    async fn test_retrieve_with_filter() {
        let (_td, mgr) = setup();
        let source = create_source_dir(&[("keep.txt", b"yes"), ("skip.txt", b"no")]);

        mgr.store_artifacts(&RunId::new("run-1").unwrap(), source.path(), None)
            .await
            .unwrap();

        let retrieve_dir = TempDir::new().unwrap();
        mgr.retrieve_artifacts(
            &RunId::new("run-1").unwrap(),
            retrieve_dir.path(),
            Some(&|name: &str| name == "keep.txt"),
        )
        .await
        .unwrap();

        assert!(retrieve_dir.path().join("keep.txt").exists());
        assert!(!retrieve_dir.path().join("skip.txt").exists());
    }
}
