//! File upload processing for worker results.

use crate::{BuilderResult, WorkerResult};
use axum::extract::Multipart;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;

/// File uploaded by a worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadedFile {
    /// Original filename.
    pub filename: String,
    /// Content type if provided.
    pub content_type: Option<String>,
    /// File size in bytes.
    pub size: u64,
    /// Path where file was stored.
    pub stored_path: PathBuf,
    /// Upload timestamp.
    pub uploaded_at: DateTime<Utc>,
}

/// Complete worker result uploaded via multipart form.
#[derive(Debug, Serialize, Deserialize)]
pub struct UploadedWorkerResult {
    /// Worker result metadata.
    pub worker_result: WorkerResult,
    /// Log files uploaded.
    pub log_files: Vec<UploadedFile>,
    /// Artifact files uploaded.
    pub artifact_files: Vec<UploadedFile>,
    /// Build output files (for builder results).
    pub build_files: Vec<UploadedFile>,
    /// Additional metadata files.
    pub metadata_files: Vec<UploadedFile>,
}

/// Processor for multipart uploads from workers.
pub struct UploadProcessor {
    /// Base directory for storing uploaded files.
    storage_dir: PathBuf,
    /// Maximum file size allowed (in bytes).
    max_file_size: u64,
    /// Maximum total upload size (in bytes).
    max_total_size: u64,
}

impl UploadProcessor {
    /// Create a new upload processor.
    pub fn new(storage_dir: PathBuf, max_file_size: u64, max_total_size: u64) -> Self {
        Self {
            storage_dir,
            max_file_size,
            max_total_size,
        }
    }

    /// Process a multipart upload from a worker.
    ///
    /// `codebase` is the codebase name for this run; logs are stored
    /// under `{storage_dir}/{codebase}/{run_id}/{filename}` so the
    /// site's `FileSystemLogFileManager` (which expects the
    /// `{root}/{codebase}/{run_id}/{name}` layout) can read them
    /// straight off a shared volume -- no separate proxy needed.
    /// Other categories (artifacts/build/metadata) keep the
    /// `{storage_dir}/{run_id}/{category}/{filename}` layout.
    pub async fn process_upload(
        &self,
        mut multipart: Multipart,
        run_id: &str,
        codebase: &str,
    ) -> Result<UploadedWorkerResult, UploadError> {
        let mut worker_result: Option<WorkerResult> = None;
        let mut log_files = Vec::new();
        let mut artifact_files = Vec::new();
        let mut build_files = Vec::new();
        let mut metadata_files = Vec::new();
        let mut total_size = 0u64;

        // Per-category directory layout. Logs follow the FS log
        // manager layout so `LOG_URL=file://{storage_dir}` on the
        // site pod can find them.
        let run_dir = self.storage_dir.join(run_id);
        tokio::fs::create_dir_all(&run_dir)
            .await
            .map_err(|e| UploadError::Storage(format!("Failed to create directory: {}", e)))?;
        let logs_dir = self.storage_dir.join(codebase).join(run_id);
        tokio::fs::create_dir_all(&logs_dir)
            .await
            .map_err(|e| UploadError::Storage(format!("Failed to create logs directory: {}", e)))?;

        while let Some(field) = multipart
            .next_field()
            .await
            .map_err(|e| UploadError::Multipart(format!("Failed to read field: {}", e)))?
        {
            let field_name = field
                .name()
                .ok_or_else(|| UploadError::Multipart("Field missing name".to_string()))?
                .to_string();

            // The worker (site/worker/src/client.rs::
            // bundle_results_optimized) names fields `metadata` (the
            // JSON worker result) and `file` (one part per output
            // file). Accept those directly -- matching the Python
            // runner's upload handler -- and keep the typed
            // `worker_result` / `log_*` / `artifact_*` / `build_*` /
            // `metadata_*` prefixes for callers that want to
            // pre-categorise files.
            match field_name.as_str() {
                "worker_result" | "metadata" => {
                    // Process JSON worker result
                    let data = field.bytes().await.map_err(|e| {
                        UploadError::Multipart(format!("Failed to read worker_result: {}", e))
                    })?;

                    // Debug: dump the raw worker payload before parsing so
                    // we can confirm whether the worker actually sends
                    // `branches`/`revision`. Site shows every success run
                    // with empty result_branches; the question is whether
                    // the runner is receiving them and discarding, or the
                    // worker never puts them on the wire. Print the first
                    // 2KB and the parsed branches length.
                    let preview_len = data.len().min(2048);
                    log::info!(
                        "upload {} raw worker_result ({} bytes, first {}): {}",
                        run_id,
                        data.len(),
                        preview_len,
                        String::from_utf8_lossy(&data[..preview_len])
                    );
                    let parsed: crate::WorkerResult =
                        serde_json::from_slice(&data).map_err(|e| {
                            UploadError::Parse(format!("Invalid worker_result JSON: {}", e))
                        })?;
                    log::info!(
                        "upload {} parsed: code={} branches={:?} revision={:?} main_branch_revision={:?}",
                        run_id,
                        parsed.code,
                        parsed.branches.as_ref().map(|b| b.len()),
                        parsed.revision.as_ref().map(|r| r.to_string()),
                        parsed.main_branch_revision.as_ref().map(|r| r.to_string())
                    );
                    worker_result = Some(parsed);
                }
                "file" => {
                    // Route by extension so the page handlers can
                    // keep their "log / artifact / build" split.
                    let filename = field.file_name().unwrap_or_default().to_string();
                    let is_log = filename.ends_with(".log") || filename == "worker.log";
                    if is_log {
                        // Logs land in `{storage_dir}/{codebase}/{run_id}/<name>`
                        // -- the layout the FS log manager expects.
                        let file = self
                            .save_field_flat(field, &logs_dir, &mut total_size)
                            .await?;
                        log_files.push(file);
                    } else {
                        let file = self
                            .save_field_to_file(field, &run_dir, "artifacts", &mut total_size)
                            .await?;
                        artifact_files.push(file);
                    }
                }
                field_name if field_name.starts_with("log_") => {
                    // Process log file
                    let file = self
                        .save_field_to_file(field, &run_dir, "logs", &mut total_size)
                        .await?;
                    log_files.push(file);
                }
                field_name if field_name.starts_with("artifact_") => {
                    // Process artifact file
                    let file = self
                        .save_field_to_file(field, &run_dir, "artifacts", &mut total_size)
                        .await?;
                    artifact_files.push(file);
                }
                field_name if field_name.starts_with("build_") => {
                    // Process build output file
                    let file = self
                        .save_field_to_file(field, &run_dir, "build", &mut total_size)
                        .await?;
                    build_files.push(file);
                }
                field_name if field_name.starts_with("metadata_") => {
                    // Process metadata file
                    let file = self
                        .save_field_to_file(field, &run_dir, "metadata", &mut total_size)
                        .await?;
                    metadata_files.push(file);
                }
                _ => {
                    // Unknown field, skip
                    log::warn!("Skipping unknown field: {}", field_name);
                }
            }

            if total_size > self.max_total_size {
                return Err(UploadError::SizeLimit(format!(
                    "Total upload size {} exceeds limit {}",
                    total_size, self.max_total_size
                )));
            }
        }

        let worker_result = worker_result
            .ok_or_else(|| UploadError::Validation("Missing worker_result field".to_string()))?;

        Ok(UploadedWorkerResult {
            worker_result,
            log_files,
            artifact_files,
            build_files,
            metadata_files,
        })
    }

    /// Save a multipart field straight into `dir/<filename>` --
    /// no category subfolder. Used for log files which already get
    /// a per-codebase/per-run directory upstream.
    async fn save_field_flat<'a>(
        &self,
        field: axum::extract::multipart::Field<'a>,
        dir: &PathBuf,
        total_size: &mut u64,
    ) -> Result<UploadedFile, UploadError> {
        let filename = field.file_name().unwrap_or("unknown").to_string();
        let content_type = field.content_type().map(|ct| ct.to_string());
        let safe_filename = sanitize_filename(&filename);
        let file_path = dir.join(&safe_filename);
        let file_size =
            stream_field_to_disk(field, &file_path, self.max_file_size, &filename).await?;
        *total_size += file_size;
        log::info!(
            "Uploaded log: {} ({} bytes) -> {:?}",
            filename,
            file_size,
            file_path
        );
        Ok(UploadedFile {
            filename,
            content_type,
            size: file_size,
            stored_path: file_path,
            uploaded_at: Utc::now(),
        })
    }

    /// Save a multipart field to a file.
    async fn save_field_to_file<'a>(
        &self,
        field: axum::extract::multipart::Field<'a>,
        run_dir: &PathBuf,
        category: &str,
        total_size: &mut u64,
    ) -> Result<UploadedFile, UploadError> {
        let filename = field.file_name().unwrap_or("unknown").to_string();

        let content_type = field.content_type().map(|ct| ct.to_string());

        let category_dir = run_dir.join(category);
        tokio::fs::create_dir_all(&category_dir)
            .await
            .map_err(|e| {
                UploadError::Storage(format!("Failed to create {} directory: {}", category, e))
            })?;

        let safe_filename = sanitize_filename(&filename);
        let file_path = category_dir.join(&safe_filename);

        let file_size =
            stream_field_to_disk(field, &file_path, self.max_file_size, &filename).await?;

        *total_size += file_size;

        log::info!(
            "Uploaded file: {} ({} bytes) -> {:?}",
            filename,
            file_size,
            file_path
        );

        Ok(UploadedFile {
            filename,
            content_type,
            size: file_size,
            stored_path: file_path,
            uploaded_at: Utc::now(),
        })
    }

    /// Process uploaded worker result and extract builder result if present.
    ///
    /// The wire reality is that the worker emits `target: {name,
    /// details}` and leaves `builder_result` unset, so checking
    /// `worker_result.builder_result` alone misses every successful
    /// run -- `find_changes` never fires and the `debian_build` row
    /// lands with NULL `version`/`source`/`distribution` (Postgres
    /// rejects it on the NOT NULL constraint, the whole /finish
    /// transaction rolls back, and the run vanishes). Look at
    /// `target.name` whenever `builder_result` is missing so the
    /// per-run extraction still runs.
    pub fn extract_builder_result(
        &self,
        uploaded: &UploadedWorkerResult,
    ) -> Result<Option<BuilderResult>, UploadError> {
        if let Some(ref builder_result) = uploaded.worker_result.builder_result {
            return match builder_result {
                BuilderResult::Debian { .. } => self.extract_debian_builder_result(uploaded),
                BuilderResult::Generic => Ok(Some(BuilderResult::Generic)),
            };
        }
        match uploaded
            .worker_result
            .target
            .as_ref()
            .map(|t| t.name.as_str())
        {
            Some("debian") => self.extract_debian_builder_result(uploaded),
            Some("generic") => Ok(Some(BuilderResult::Generic)),
            _ => Ok(None),
        }
    }

    /// Extract Debian-specific builder result from uploaded files.
    ///
    /// Two-step:
    ///
    /// 1. `lintian_result` comes from the worker's
    ///    `target.details.lintian` blob -- that's the only build
    ///    metadata the worker actually emits (see
    ///    `worker/src/debian/build.rs::DebianBuildResult`, which
    ///    serialises `{"lintian": ...}` and nothing else).
    /// 2. Everything else -- `source`, `build_version`,
    ///    `build_distribution`, `binary_packages`, full
    ///    `changes_filenames` -- is reconstructed by parsing the
    ///    `.changes` files in the artifacts directory via
    ///    [`crate::find_changes`]. Without this step the
    ///    `debian_build` row landed with NULL
    ///    distribution/version/source on every successful run and
    ///    the per-run page rendered "this run did not produce a
    ///    build".
    ///
    /// Failures of the changes-file parse are logged and degraded
    /// to "no source/version/binaries known" rather than aborting
    /// the upload -- a malformed .changes shouldn't prevent the
    /// run row from being written; the lintian piece is still
    /// useful and the run is genuinely successful.
    fn extract_debian_builder_result(
        &self,
        uploaded: &UploadedWorkerResult,
    ) -> Result<Option<BuilderResult>, UploadError> {
        let lintian_result = uploaded
            .worker_result
            .target
            .as_ref()
            .filter(|t| t.name == "debian")
            .and_then(|t| t.details.get("lintian").cloned());

        // Locate the artifacts directory. Each uploaded file in
        // `artifact_files` lives under `<stored_root>/<run_id>/...`
        // -- the parent of the first artifact is the dir we want.
        // Fall back to `build_files`, then to None when nothing
        // landed (e.g. failure-stage uploads with no artefacts).
        let artifacts_dir: Option<std::path::PathBuf> = uploaded
            .artifact_files
            .first()
            .or_else(|| uploaded.build_files.first())
            .and_then(|f| f.stored_path.parent().map(|p| p.to_path_buf()));

        let mut source: Option<String> = None;
        let mut build_version: Option<String> = None;
        let mut build_distribution: Option<String> = None;
        let mut changes_filenames: Option<Vec<String>> = None;
        let mut binary_packages: Option<Vec<String>> = None;

        if let Some(ref dir) = artifacts_dir {
            match crate::find_changes(dir) {
                Ok(summary) => {
                    source = Some(summary.source);
                    build_version = Some(summary.version.to_string());
                    build_distribution = Some(summary.distribution);
                    if !summary.names.is_empty() {
                        changes_filenames = Some(summary.names);
                    }
                    if !summary.binary_packages.is_empty() {
                        binary_packages = Some(summary.binary_packages);
                    }
                }
                Err(crate::FindChangesError::NoChangesFile(_)) => {
                    // Codemod-stage / setup-stage failures often
                    // leave no `.changes` behind -- that's expected,
                    // not an error worth bubbling up.
                    log::debug!(
                        "no .changes file under {}; leaving source/version unset",
                        dir.display()
                    );
                }
                Err(e) => {
                    log::warn!("failed to parse .changes under {}: {}", dir.display(), e);
                }
            }
        }

        // If there's no lintian blob and no changes-derived data
        // either, return None -- we have nothing useful to record.
        if lintian_result.is_none()
            && source.is_none()
            && build_version.is_none()
            && changes_filenames.is_none()
        {
            return Ok(None);
        }

        Ok(Some(BuilderResult::Debian {
            source,
            build_version,
            build_distribution,
            changes_filenames,
            lintian_result,
            binary_packages,
        }))
    }

    /// Get storage statistics.
    pub async fn get_storage_stats(&self) -> Result<StorageStats, UploadError> {
        let mut total_files = 0;
        let mut total_size = 0;
        let mut categories = HashMap::new();

        if self.storage_dir.exists() {
            let mut entries = tokio::fs::read_dir(&self.storage_dir).await.map_err(|e| {
                UploadError::Storage(format!("Failed to read storage directory: {}", e))
            })?;

            while let Some(entry) = entries.next_entry().await.map_err(|e| {
                UploadError::Storage(format!("Failed to read directory entry: {}", e))
            })? {
                if entry
                    .file_type()
                    .await
                    .map_err(|e| UploadError::Storage(e.to_string()))?
                    .is_dir()
                {
                    // This is a run directory
                    let run_stats = self.get_run_storage_stats(&entry.path()).await?;
                    total_files += run_stats.total_files;
                    total_size += run_stats.total_size;

                    for (category, count) in run_stats.files_by_category {
                        *categories.entry(category).or_insert(0) += count;
                    }
                }
            }
        }

        Ok(StorageStats {
            total_files,
            total_size,
            files_by_category: categories,
        })
    }

    /// Get storage statistics for a specific run.
    async fn get_run_storage_stats(
        &self,
        run_dir: &PathBuf,
    ) -> Result<RunStorageStats, UploadError> {
        let mut total_files = 0;
        let mut total_size = 0;
        let mut files_by_category = HashMap::new();

        let mut entries = tokio::fs::read_dir(run_dir)
            .await
            .map_err(|e| UploadError::Storage(format!("Failed to read run directory: {}", e)))?;

        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| UploadError::Storage(format!("Failed to read directory entry: {}", e)))?
        {
            if entry
                .file_type()
                .await
                .map_err(|e| UploadError::Storage(e.to_string()))?
                .is_dir()
            {
                let category = entry.file_name().to_string_lossy().to_string();
                let category_stats = self.get_category_storage_stats(&entry.path()).await?;

                total_files += category_stats.0;
                total_size += category_stats.1;
                files_by_category.insert(category, category_stats.0);
            }
        }

        Ok(RunStorageStats {
            total_files,
            total_size,
            files_by_category,
        })
    }

    /// Get storage statistics for a category directory.
    async fn get_category_storage_stats(
        &self,
        category_dir: &PathBuf,
    ) -> Result<(u64, u64), UploadError> {
        let mut file_count = 0;
        let mut total_size = 0;

        let mut entries = tokio::fs::read_dir(category_dir).await.map_err(|e| {
            UploadError::Storage(format!("Failed to read category directory: {}", e))
        })?;

        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| UploadError::Storage(format!("Failed to read directory entry: {}", e)))?
        {
            if entry
                .file_type()
                .await
                .map_err(|e| UploadError::Storage(e.to_string()))?
                .is_file()
            {
                let metadata = entry.metadata().await.map_err(|e| {
                    UploadError::Storage(format!("Failed to read file metadata: {}", e))
                })?;

                file_count += 1;
                total_size += metadata.len();
            }
        }

        Ok((file_count, total_size))
    }
}

/// Storage statistics.
#[derive(Debug, Serialize)]
pub struct StorageStats {
    /// Total number of files.
    pub total_files: u64,
    /// Total size in bytes.
    pub total_size: u64,
    /// Files by category.
    pub files_by_category: HashMap<String, u64>,
}

/// Storage statistics for a specific run.
#[derive(Debug)]
struct RunStorageStats {
    total_files: u64,
    total_size: u64,
    files_by_category: HashMap<String, u64>,
}

/// Errors that can occur during upload processing.
#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    /// Multipart parsing error.
    #[error("Multipart error: {0}")]
    Multipart(String),

    /// JSON parsing error.
    #[error("Parse error: {0}")]
    Parse(String),

    /// File storage error.
    #[error("Storage error: {0}")]
    Storage(String),

    /// Size limit exceeded.
    #[error("Size limit exceeded: {0}")]
    SizeLimit(String),

    /// Validation error.
    #[error("Validation error: {0}")]
    Validation(String),
}

/// Stream a multipart field to disk in chunks instead of buffering
/// it in memory. axum's `Field::bytes()` accumulates the entire field
/// before yielding, which on a Java package's multi-hundred-MB
/// orig.tar.gz balloons RSS by exactly the file size. `Field` itself
/// implements `Stream<Item=Result<Bytes>>` via `chunk()`, so we pull
/// chunks as they arrive and write them straight to the destination
/// file. Enforces `max_file_size` while reading so we abort early on
/// oversize uploads instead of after holding the whole thing.
async fn stream_field_to_disk<'a>(
    mut field: axum::extract::multipart::Field<'a>,
    file_path: &std::path::Path,
    max_file_size: u64,
    display_name: &str,
) -> Result<u64, UploadError> {
    let mut file = tokio::fs::File::create(file_path).await.map_err(|e| {
        UploadError::Storage(format!("Failed to create file {}: {}", display_name, e))
    })?;
    let mut written: u64 = 0;
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|e| UploadError::Multipart(format!("Failed to read file data: {}", e)))?
    {
        written += chunk.len() as u64;
        if written > max_file_size {
            // Drop the partial file rather than leaving a truncated
            // artifact on disk that downstream parsers might mistake
            // for a real one.
            let _ = tokio::fs::remove_file(file_path).await;
            return Err(UploadError::SizeLimit(format!(
                "File {} size {} exceeds limit {}",
                display_name, written, max_file_size
            )));
        }
        file.write_all(&chunk).await.map_err(|e| {
            UploadError::Storage(format!("Failed to write file {}: {}", display_name, e))
        })?;
    }
    file.flush().await.map_err(|e| {
        UploadError::Storage(format!("Failed to flush file {}: {}", display_name, e))
    })?;
    Ok(written)
}

/// Sanitize a filename for safe storage.
fn sanitize_filename(filename: &str) -> String {
    // Replace potentially dangerous characters
    filename
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect::<String>()
        .trim_matches('.')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_filename() {
        assert_eq!(sanitize_filename("normal.txt"), "normal.txt");
        assert_eq!(sanitize_filename("with/slash.txt"), "with_slash.txt");
        assert_eq!(sanitize_filename("with:colon.txt"), "with_colon.txt");
        assert_eq!(sanitize_filename("..dangerous"), "dangerous");
        assert_eq!(sanitize_filename("dangerous.."), "dangerous");
    }
}
