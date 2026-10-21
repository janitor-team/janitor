use crate::{CodebaseName, RunId};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use reqwest::{Client, StatusCode};
use std::io::{self, Cursor, Read};
use std::path::Path;
use std::time::Duration;

use crate::logs::{Error, LogFileManager};

pub struct S3LogFileManager {
    base_url: String,
    _bucket_name: String,
    client: Client,
}

impl S3LogFileManager {
    pub fn new(endpoint_url: &str, bucket_name: Option<&str>) -> Result<Self, Error> {
        let bucket_name = bucket_name.unwrap_or("debian-janitor");
        let base_url = format!("{}/{}/", endpoint_url.trim_end_matches('/'), bucket_name);

        let client = Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| Error::Other(e.to_string()))?;

        Ok(Self {
            base_url,
            _bucket_name: bucket_name.to_string(),
            client,
        })
    }

    fn get_key(&self, codebase: &CodebaseName, run_id: &RunId, name: &str) -> String {
        format!("logs/{}/{}/{}.gz", codebase, run_id, name)
    }

    fn get_url(
        &self,
        codebase: &CodebaseName,
        run_id: &RunId,
        name: &str,
    ) -> Result<String, Error> {
        // The URL parser resolves .. (also %2e%2e), reads \ as / and ends
        // the path at ? or #.
        if name.contains(['/', '\\', '?', '#']) {
            return Err(Error::NotFound);
        }
        Ok(format!(
            "{}{}",
            self.base_url,
            self.get_key(codebase, run_id, name)
        ))
    }
}

#[async_trait]
impl LogFileManager for S3LogFileManager {
    async fn has_log(
        &self,
        codebase: &CodebaseName,
        run_id: &RunId,
        name: &str,
    ) -> Result<bool, Error> {
        let url = match self.get_url(codebase, run_id, name) {
            Ok(url) => url,
            Err(_) => return Ok(false),
        };

        match self.client.head(&url).send().await {
            Ok(resp) => match resp.status() {
                StatusCode::OK => Ok(true),
                StatusCode::NOT_FOUND => Ok(false),
                StatusCode::FORBIDDEN => Ok(false),
                status => Err(Error::Other(format!(
                    "Unexpected response code: {}",
                    status
                ))),
            },
            Err(_e) => Err(Error::ServiceUnavailable),
        }
    }

    async fn get_log(
        &self,
        codebase: &CodebaseName,
        run_id: &RunId,
        name: &str,
    ) -> Result<Box<dyn Read + Send + Sync>, Error> {
        let url = self.get_url(codebase, run_id, name)?;

        let resp = self
            .client
            .get(&url)
            .timeout(Duration::from_secs(300))
            .send()
            .await
            .map_err(|_| Error::ServiceUnavailable)?;

        match resp.status() {
            StatusCode::OK => {
                let bytes = resp.bytes().await.map_err(|_| Error::ServiceUnavailable)?;
                let cursor = Cursor::new(bytes.to_vec());
                let decoder = GzDecoder::new(cursor);
                Ok(Box::new(decoder))
            }
            StatusCode::NOT_FOUND => Err(Error::NotFound),
            StatusCode::FORBIDDEN => Err(Error::PermissionDenied),
            status => Err(Error::Other(format!(
                "Unexpected response code: {}",
                status
            ))),
        }
    }

    async fn import_log(
        &self,
        codebase: &CodebaseName,
        run_id: &RunId,
        orig_path: &str,
        _mtime: Option<DateTime<Utc>>,
        basename: Option<&str>,
    ) -> Result<(), Error> {
        let data = tokio::fs::read(orig_path).await?;

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        io::copy(&mut Cursor::new(&data), &mut encoder)?;
        let compressed_data = encoder.finish()?;

        let basename = basename.unwrap_or_else(|| {
            Path::new(orig_path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
        });

        if basename.contains(['/', '\\', '?', '#']) {
            return Err(Error::Other(
                "Basename cannot contain '/', '\\', '?' or '#' characters".to_string(),
            ));
        }

        let key = self.get_key(codebase, run_id, basename);
        let url = format!("{}{}", self.base_url, key);

        let resp = self
            .client
            .put(&url)
            .header("x-amz-acl", "public-read")
            .body(compressed_data)
            .send()
            .await
            .map_err(|_| Error::ServiceUnavailable)?;

        match resp.status() {
            StatusCode::OK | StatusCode::CREATED => Ok(()),
            StatusCode::FORBIDDEN => Err(Error::PermissionDenied),
            StatusCode::SERVICE_UNAVAILABLE => Err(Error::ServiceUnavailable),
            status => Err(Error::Other(format!(
                "Upload failed with status: {}",
                status
            ))),
        }
    }

    async fn delete_log(
        &self,
        codebase: &CodebaseName,
        run_id: &RunId,
        name: &str,
    ) -> Result<(), Error> {
        let url = self.get_url(codebase, run_id, name)?;

        let resp = self
            .client
            .delete(&url)
            .send()
            .await
            .map_err(|_| Error::ServiceUnavailable)?;

        match resp.status() {
            StatusCode::OK | StatusCode::NO_CONTENT => Ok(()),
            StatusCode::NOT_FOUND => Err(Error::NotFound),
            StatusCode::FORBIDDEN => Err(Error::PermissionDenied),
            status => Err(Error::Other(format!(
                "Delete failed with status: {}",
                status
            ))),
        }
    }

    async fn iter_logs(&self) -> Box<dyn Iterator<Item = (String, String, Vec<String>)>> {
        // S3 listing is complex and would require XML parsing
        // For now, return empty iterator - this can be implemented later
        Box::new(std::iter::empty())
    }

    async fn get_ctime(
        &self,
        codebase: &CodebaseName,
        run_id: &RunId,
        name: &str,
    ) -> Result<DateTime<Utc>, Error> {
        let url = self.get_url(codebase, run_id, name)?;

        // Use HEAD request to get object metadata including last-modified
        let response = self
            .client
            .head(&url)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|_| Error::ServiceUnavailable)?;

        if !response.status().is_success() {
            match response.status().as_u16() {
                404 => return Err(Error::NotFound),
                _ => return Err(Error::ServiceUnavailable),
            }
        }

        // Try to get last-modified header as creation time approximation
        if let Some(last_modified) = response.headers().get("last-modified") {
            if let Ok(date_str) = last_modified.to_str() {
                // Parse RFC 2822 format (HTTP date format)
                if let Ok(datetime) = DateTime::parse_from_rfc2822(date_str) {
                    return Ok(datetime.with_timezone(&Utc));
                }
            }
        }

        // If no reliable creation time is available, return an error
        Err(Error::Other(
            "Creation time not available for S3 object".to_string(),
        ))
    }

    async fn health_check(&self) -> Result<(), Error> {
        // Try to access the bucket root
        let resp = self
            .client
            .head(&self.base_url)
            .timeout(Duration::from_secs(10))
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => Ok(()),
            Ok(r) if r.status() == StatusCode::FORBIDDEN => Err(Error::PermissionDenied),
            _ => Err(Error::ServiceUnavailable),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_parent_name_rejected() {
        // Nothing listens on port 1, so a request would give an error.
        let mgr = S3LogFileManager::new("http://127.0.0.1:1", None).unwrap();
        let codebase = CodebaseName::new("foo").unwrap();
        let run_id = RunId::new("run-1").unwrap();
        assert!(mgr.has_log(&codebase, &run_id, "build.log").await.is_err());
        let mut requested = Vec::new();
        for name in [
            "../../../build.log",
            "..\\..\\..\\build.log",
            "%2e%2e/%2e%2e/%2e%2e/build.log",
            ".%2E/.%2E/.%2E/build.log",
            "..?",
            "..#",
            "build.log?acl",
        ] {
            let has_log = mgr.has_log(&codebase, &run_id, name).await;
            let get_log = mgr.get_log(&codebase, &run_id, name).await;
            if !matches!(has_log, Ok(false)) || !matches!(get_log, Err(Error::NotFound)) {
                requested.push(name);
            }
        }
        assert_eq!(requested, Vec::<&str>::new());
    }

    fn refused(result: Result<(), Error>) -> bool {
        matches!(result, Err(Error::Other(e)) if e.starts_with("Basename cannot contain"))
    }

    #[tokio::test]
    async fn test_parent_basename_rejected() {
        // Nothing listens on port 1, so an upload would give an error.
        let mgr = S3LogFileManager::new("http://127.0.0.1:1", None).unwrap();
        let codebase = CodebaseName::new("foo").unwrap();
        let run_id = RunId::new("run-1").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, "").unwrap();
            path.to_str().unwrap().to_string()
        };
        let path = file("x.log");
        let result =
            crate::logs::import_log(&mgr, None, &codebase, &run_id, &path, Some("x.log"), None)
                .await;
        assert!(matches!(result, Err(Error::Other(e)) if e == "All log managers failed"));
        let result = mgr.import_log(&codebase, &run_id, &path, None, None).await;
        assert!(matches!(result, Err(Error::ServiceUnavailable)));
        let mut requested = Vec::new();
        for name in ["..\\..\\..\\x.log", "../x.log", "x.log?acl", "x.log#x"] {
            let result =
                crate::logs::import_log(&mgr, None, &codebase, &run_id, &path, Some(name), None)
                    .await;
            if !refused(result) {
                requested.push(format!("logs::import_log {}", name));
            }
            let result = mgr
                .import_log(&codebase, &run_id, &path, None, Some(name))
                .await;
            if !refused(result) {
                requested.push(format!("basename {}", name));
            }
            if !name.contains('/') {
                let result = mgr
                    .import_log(&codebase, &run_id, &file(name), None, None)
                    .await;
                if !refused(result) {
                    requested.push(format!("file name {}", name));
                }
            }
        }
        assert_eq!(requested, Vec::<String>::new());
    }
}
