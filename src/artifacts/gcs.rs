use async_trait::async_trait;
use google_cloud_storage::client::{Storage, StorageControl};
use std::collections::HashSet;
use std::fs::File;
use std::path::Path;

use crate::artifacts::{ArtifactManager, Error};

pub struct GCSArtifactManager {
    bucket_name: String,
    storage: Storage,
    control: StorageControl,
}

impl std::fmt::Debug for GCSArtifactManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GCSArtifactManager")
            .field("bucket_name", &self.bucket_name)
            .finish()
    }
}

fn bucket_resource(bucket_name: &str) -> String {
    format!("projects/_/buckets/{}", bucket_name)
}

impl GCSArtifactManager {
    pub async fn from_url(location: &url::Url) -> Result<Self, Error> {
        if location.scheme() != "gs" {
            return Err(Error::Other(format!(
                "Invalid URL scheme: {}",
                location.scheme()
            )));
        }
        let bucket_name = location
            .host_str()
            .ok_or_else(|| Error::Other("Missing bucket name".to_string()))?;

        Self::new(bucket_name.to_string()).await
    }

    pub async fn new(bucket_name: String) -> Result<Self, Error> {
        let storage = Storage::builder()
            .build()
            .await
            .map_err(|e| Error::Other(e.to_string()))?;

        let control = StorageControl::builder()
            .build()
            .await
            .map_err(|e| Error::Other(e.to_string()))?;

        Ok(Self {
            bucket_name,
            storage,
            control,
        })
    }
}

fn map_gcs_error(e: google_cloud_storage::Error) -> Error {
    match e.http_status_code() {
        Some(503) => Error::ServiceUnavailable,
        Some(404) => Error::ArtifactsMissing,
        _ => Error::Other(e.to_string()),
    }
}

#[async_trait]
impl ArtifactManager for GCSArtifactManager {
    async fn store_artifacts(
        &self,
        run_id: &str,
        local_path: &Path,
        names: Option<&[String]>,
    ) -> Result<(), Error> {
        let files_to_upload = match names {
            Some(names) => names.to_vec(),
            None => {
                let mut entries = vec![];
                let mut rd = tokio::fs::read_dir(local_path).await?;
                while let Some(entry) = rd.next_entry().await? {
                    if entry.file_type().await?.is_file() {
                        if let Ok(name) = entry.file_name().into_string() {
                            entries.push(name);
                        }
                    }
                }
                entries
            }
        };

        let bucket = bucket_resource(&self.bucket_name);
        let tasks: Vec<_> = files_to_upload
            .into_iter()
            .map(|name| {
                let file_path = local_path.join(&name);
                let bucket = bucket.clone();
                let storage = self.storage.clone();
                let object_name = format!("{}/{}", run_id, name);
                tokio::spawn(async move {
                    let data = tokio::fs::read(&file_path).await?;
                    storage
                        .write_object(&bucket, &object_name, bytes::Bytes::from(data))
                        .send_buffered()
                        .await
                        .map_err(map_gcs_error)?;
                    Ok::<(), Error>(())
                })
            })
            .collect();

        for task in tasks {
            task.await.map_err(|e| Error::Other(e.to_string()))??;
        }

        Ok(())
    }

    async fn delete_artifacts(&self, run_id: &str) -> Result<(), Error> {
        let prefix = format!("{}/", run_id);
        let bucket = bucket_resource(&self.bucket_name);

        let response = self
            .control
            .list_objects()
            .set_parent(&bucket)
            .set_prefix(&prefix)
            .send()
            .await
            .map_err(map_gcs_error)?;

        for object in response.objects {
            self.control
                .delete_object()
                .set_bucket(&bucket)
                .set_object(&object.name)
                .send()
                .await
                .map_err(map_gcs_error)?;
        }

        Ok(())
    }

    async fn get_artifact(
        &self,
        run_id: &str,
        filename: &str,
    ) -> Result<Box<dyn std::io::Read + Send + Sync>, Error> {
        let object_name = format!("{}/{}", run_id, filename);
        let bucket = bucket_resource(&self.bucket_name);

        let mut resp = self
            .storage
            .read_object(&bucket, &object_name)
            .send()
            .await
            .map_err(map_gcs_error)?;

        let mut contents = Vec::new();
        while let Some(chunk) = resp.next().await.transpose().map_err(map_gcs_error)? {
            contents.extend_from_slice(&chunk);
        }

        Ok(Box::new(std::io::Cursor::new(contents)))
    }

    fn public_artifact_url(&self, run_id: &str, filename: &str) -> url::Url {
        let object_name = format!("{}/{}", run_id, filename);
        let encoded_object_name =
            percent_encoding::utf8_percent_encode(&object_name, percent_encoding::CONTROLS);
        format!(
            "https://storage.googleapis.com/{}/{}",
            self.bucket_name, encoded_object_name
        )
        .parse()
        .unwrap_or_else(|_| {
            url::Url::parse("https://invalid.url").expect("hardcoded URL should be valid")
        })
    }

    async fn retrieve_artifacts(
        &self,
        run_id: &str,
        local_path: &Path,
        filter_fn: Option<&(dyn for<'a> Fn(&'a str) -> bool + Sync + Send)>,
    ) -> Result<(), Error> {
        let prefix = format!("{}/", run_id);
        let bucket = bucket_resource(&self.bucket_name);

        let response = self
            .control
            .list_objects()
            .set_parent(&bucket)
            .set_prefix(&prefix)
            .send()
            .await
            .map_err(map_gcs_error)?;

        for object in response.objects {
            let file_name = object.name.trim_start_matches(&prefix);
            if filter_fn.map_or(true, |f| f(file_name)) {
                let mut content = self.get_artifact(run_id, file_name).await?;
                let mut file = File::create(local_path.join(file_name))?;
                std::io::copy(&mut content, &mut file)?;
            }
        }

        Ok(())
    }

    async fn iter_ids(&self) -> Box<dyn Iterator<Item = String> + Send> {
        let mut ids = HashSet::new();
        let bucket = bucket_resource(&self.bucket_name);

        let response = self
            .control
            .list_objects()
            .set_parent(&bucket)
            .send()
            .await
            .map_err(map_gcs_error)
            .unwrap();

        for object in response.objects {
            if let Some(id) = object.name.split('/').next() {
                ids.insert(id.to_string());
            }
        }

        Box::new(ids.into_iter())
    }
}
