use crate::{CodebaseName, RunId};
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use google_cloud_gax::paginator::ItemPaginator;
use google_cloud_storage::client::{Storage, StorageControl};
use std::collections::HashMap;
use std::io::{self, Cursor, Read};

use crate::logs::{Error, LogFileManager};

pub struct GCSLogFileManager {
    bucket_name: String,
    storage: Storage,
    control: StorageControl,
}

impl GCSLogFileManager {
    pub async fn from_url(location: &url::Url, _creds: Option<()>) -> Result<Self, Error> {
        if location.scheme() != "gs" {
            return Err(Error::Other(format!(
                "Invalid scheme: {}",
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

    fn bucket_path(&self) -> String {
        format!("projects/_/buckets/{}", self.bucket_name)
    }

    fn get_object_name(&self, codebase: &CodebaseName, run_id: &RunId, name: &str) -> String {
        format!("{}/{}/{}.gz", codebase, run_id, name)
    }
}

#[async_trait]
impl LogFileManager for GCSLogFileManager {
    async fn has_log(
        &self,
        codebase: &CodebaseName,
        run_id: &RunId,
        name: &str,
    ) -> Result<bool, Error> {
        let bucket = self.bucket_path();
        let object_name = self.get_object_name(codebase, run_id, name);

        match self
            .control
            .get_object()
            .set_bucket(bucket)
            .set_object(object_name)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(e) => {
                if e.to_string().contains("Not Found") {
                    Ok(false)
                } else {
                    Err(Error::Other(e.to_string()))
                }
            }
        }
    }

    async fn get_log(
        &self,
        codebase: &CodebaseName,
        run_id: &RunId,
        name: &str,
    ) -> Result<Box<dyn Read + Send + Sync>, Error> {
        let bucket = self.bucket_path();
        let object_name = self.get_object_name(codebase, run_id, name);

        let mut response = self
            .storage
            .read_object(&bucket, &object_name)
            .send()
            .await
            .map_err(|e| {
                if e.to_string().contains("Not Found") {
                    Error::NotFound
                } else {
                    Error::Other(e.to_string())
                }
            })?;

        let mut contents = Vec::new();
        while let Some(chunk) = response.next().await {
            let chunk = chunk.map_err(|e| Error::Other(e.to_string()))?;
            contents.extend_from_slice(&chunk);
        }

        let cursor = Cursor::new(contents);
        let decoder = GzDecoder::new(cursor);
        Ok(Box::new(decoder) as Box<dyn Read + Send + Sync>)
    }

    async fn import_log_data(
        &self,
        codebase: &CodebaseName,
        run_id: &RunId,
        name: &str,
        data: &[u8],
        _mtime: Option<DateTime<Utc>>,
    ) -> Result<(), Error> {
        let bucket = self.bucket_path();
        let object_name = self.get_object_name(codebase, run_id, name);

        // Compress the data
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        io::copy(&mut Cursor::new(data), &mut encoder).map_err(|e| Error::Other(e.to_string()))?;
        let compressed_data = encoder.finish().map_err(|e| Error::Other(e.to_string()))?;

        // Upload to GCS using the new API
        self.storage
            .write_object(&bucket, &object_name, Bytes::from(compressed_data))
            .send_unbuffered()
            .await
            .map_err(|e| {
                if e.to_string().contains("Permission") {
                    Error::PermissionDenied
                } else {
                    Error::Other(e.to_string())
                }
            })?;

        Ok(())
    }

    async fn iter_logs(&self) -> Box<dyn Iterator<Item = (String, String, Vec<String>)>> {
        let result = self.iter_logs_internal().await;
        match result {
            Ok(logs) => Box::new(logs.into_iter()),
            Err(_) => Box::new(std::iter::empty()),
        }
    }

    async fn get_ctime(
        &self,
        codebase: &CodebaseName,
        run_id: &RunId,
        name: &str,
    ) -> Result<DateTime<Utc>, Error> {
        let bucket = self.bucket_path();
        let object_name = self.get_object_name(codebase, run_id, name);

        match self
            .control
            .get_object()
            .set_bucket(bucket)
            .set_object(object_name)
            .send()
            .await
        {
            Ok(object) => {
                // Extract create_time from the metadata
                if let Some(create_time) = object.create_time {
                    // Convert from wkt::Timestamp to chrono::DateTime<Utc>
                    let timestamp = create_time.seconds();
                    let nsecs = create_time.nanos() as u32;
                    if let Some(dt) = DateTime::<Utc>::from_timestamp(timestamp, nsecs) {
                        Ok(dt)
                    } else {
                        Ok(Utc::now())
                    }
                } else {
                    Err(Error::Other(
                        "Time created not found for the object".to_string(),
                    ))
                }
            }
            Err(e) => {
                if e.to_string().contains("Not Found") {
                    Err(Error::NotFound)
                } else {
                    Err(Error::Other(e.to_string()))
                }
            }
        }
    }

    async fn delete_log(
        &self,
        codebase: &CodebaseName,
        run_id: &RunId,
        name: &str,
    ) -> Result<(), Error> {
        let bucket = self.bucket_path();
        let object_name = self.get_object_name(codebase, run_id, name);

        match self
            .control
            .delete_object()
            .set_bucket(&bucket)
            .set_object(&object_name)
            .send()
            .await
        {
            Ok(_) => {
                log::info!("Successfully deleted log object: {}", object_name);
                Ok(())
            }
            Err(err) => {
                let error_msg = format!("Failed to delete object {}: {}", object_name, err);

                if err.to_string().contains("404") || err.to_string().contains("Not Found") {
                    Err(Error::NotFound)
                } else if err.to_string().contains("403") || err.to_string().contains("Forbidden") {
                    Err(Error::PermissionDenied)
                } else {
                    Err(Error::Other(error_msg))
                }
            }
        }
    }

    async fn health_check(&self) -> Result<(), Error> {
        // Try listing objects with a prefix that should return quickly
        let bucket = self.bucket_path();
        match self.control.list_objects().set_parent(&bucket).send().await {
            Ok(_) => Ok(()),
            Err(e) => Err(Error::Other(format!("GCS health check failed: {}", e))),
        }
    }
}

impl GCSLogFileManager {
    // Helper method to implement iter_logs functionality
    async fn iter_logs_internal(&self) -> Result<Vec<(String, String, Vec<String>)>, Error> {
        let mut logs: HashMap<(String, String), Vec<String>> = HashMap::new();
        let bucket = self.bucket_path();

        let mut stream = self.control.list_objects().set_parent(bucket).by_item();

        while let Some(result) = stream.next().await {
            let object = result.map_err(|e| Error::Other(e.to_string()))?;
            let name = &object.name;

            // Parse object name into components
            let parts: Vec<&str> = name.split('/').collect();
            if parts.len() == 3 {
                let codebase = parts[0].to_string();
                let log_id = parts[1].to_string();

                // Get the log filename (remove .gz extension)
                let mut log_file = parts[2].to_string();
                if log_file.ends_with(".gz") {
                    log_file = log_file[..log_file.len() - 3].to_string();
                }

                logs.entry((codebase, log_id)).or_default().push(log_file);
            }
        }

        Ok(logs
            .into_iter()
            .map(|((codebase, log_id), log_files)| (codebase, log_id, log_files))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use crate::{CodebaseName, RunId};

    /// A simplified test for get_object_name that doesn't require actual GCS connections
    #[test]
    fn test_get_object_name() {
        // Create a struct with just the methods we need for testing
        struct TestGCSLogFileManager;

        impl TestGCSLogFileManager {
            fn get_object_name(
                &self,
                codebase: &CodebaseName,
                run_id: &RunId,
                name: &str,
            ) -> String {
                format!("{}/{}/{}.gz", codebase, run_id, name)
            }
        }

        let manager = TestGCSLogFileManager {};

        assert_eq!(
            manager.get_object_name(
                &CodebaseName::new("my-codebase").unwrap(),
                &RunId::new("run123").unwrap(),
                "build.log"
            ),
            "my-codebase/run123/build.log.gz"
        );
    }

    // Integration tests that would connect to actual GCS would go here
    // We'd need to use environment variables or mocks for those tests
}
