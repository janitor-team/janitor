//! Runner crate for the Janitor project.
//!
//! This crate provides functionality for running code quality checks and tests.

#![deny(missing_docs)]

use breezyshim::RevisionId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

// Re-export VcsInfo from the main crate to avoid duplication
pub use janitor::queue::VcsInfo;

// Re-export builder types
pub use builder::{
    get_builder, Builder, BuilderError, CampaignConfig, DebianBuildConfig, DebianBuilder,
    DistroConfig, GenericBuildConfig, GenericBuilder,
};

// Re-export backchannel types
pub use backchannel::{
    Backchannel as BackchannelTrait, Error as BackchannelError, HealthStatus, JenkinsBackchannel,
    PollingBackchannel,
};

// Re-export watchdog types
pub use watchdog::{RunHealthStatus, TerminationReason, Watchdog, WatchdogConfig, WatchdogStats};

/// In-memory active-run tracking, mirroring Python's
/// `QueueProcessor.active_runs`.
pub mod active_runs;

/// API types for runner service.
pub mod api_types;
/// Module for application initialization and orchestration.
pub mod application;
/// Module for worker authentication and security.
pub mod auth;
/// Module for handling backchannel communication with the worker.
pub mod backchannel;
/// Module for build system implementations.
pub mod builder;
/// Runner configuration.
pub mod config;
/// Module for generating configuration files.
pub mod config_generator;
/// Database operations.
pub mod database;
/// Error tracking and logging.
pub mod error_tracking;
/// Module for log file management.
pub mod logs;
/// Module for Prometheus metrics collection.
pub mod metrics;
/// Resume logic for interrupted runs.
pub mod resume;
/// Logging and tracing integration.
pub mod tracing;
/// Module for handling file uploads and multipart forms.
pub mod upload;
/// Module for VCS integration and coordination.
pub mod vcs;
/// Module for monitoring active runs.
pub mod watchdog;
/// Module for the web interface.
pub mod web;

/// Test utilities for the runner module.
pub mod test_utils;

/// Test helpers for database-dependent tests
#[cfg(test)]
pub mod test_helpers;

/// Config migration tests
#[cfg(test)]
mod config_migration_test;

/// Generate environment variables for committing changes.
///
/// # Arguments
/// * `committer` - Optional committer string in the format "Name <email>"
///
/// # Returns
/// A HashMap containing environment variables for committing
pub fn committer_env(committer: Option<&str>) -> HashMap<String, String> {
    let mut env = HashMap::new();
    if let Some(committer) = committer {
        let (user, email) = breezyshim::config::parse_username(committer);
        if !user.is_empty() {
            env.insert("DEBFULLNAME".to_string(), user.to_string());
            env.insert("GIT_COMMITTER_NAME".to_string(), user.to_string());
            env.insert("GIT_AUTHOR_NAME".to_string(), user.to_string());
        }
        if !email.is_empty() {
            env.insert("DEBEMAIL".to_string(), email.to_string());
            env.insert("GIT_COMMITTER_EMAIL".to_string(), email.to_string());
            env.insert("GIT_AUTHOR_EMAIL".to_string(), email.to_string());
            env.insert("EMAIL".to_string(), email.to_string());
        }
        env.insert("COMMITTER".to_string(), committer.to_string());
        env.insert("BRZ_EMAIL".to_string(), committer.to_string());
    }
    env
}

#[cfg(feature = "debian")]
/// Errors that can occur when finding changes files.
#[derive(Debug)]
pub enum FindChangesError {
    /// No changes file was found in the specified directory.
    NoChangesFile(PathBuf),
    /// Inconsistent versions were found in multiple changes files.
    InconsistentVersion(Vec<String>, debversion::Version, debversion::Version),
    /// Inconsistent source names were found in multiple changes files.
    InconsistentSource(Vec<String>, String, String),
    /// Inconsistent distributions were found in multiple changes files.
    InconsistentDistribution(Vec<String>, String, String),
    /// A required field was missing in the changes file.
    MissingChangesFileFields(&'static str),
    /// I/O error when accessing the directory or files.
    IoError(PathBuf, std::io::Error),
    /// Error parsing a changes file.
    ParseError(PathBuf, Box<dyn std::error::Error + Send + Sync>),
    /// Filename cannot be converted to UTF-8.
    InvalidFilename(PathBuf),
}

#[cfg(feature = "debian")]
impl std::fmt::Display for FindChangesError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            FindChangesError::NoChangesFile(path) => {
                write!(f, "No changes file found in {}", path.display())
            }
            FindChangesError::InconsistentVersion(names, found, expected) => write!(
                f,
                "Inconsistent version in changes files {:?}: found {} expected {}",
                names, found, expected
            ),
            FindChangesError::InconsistentSource(names, found, expected) => write!(
                f,
                "Inconsistent source in changes files {:?}: found {} expected {}",
                names, found, expected
            ),
            FindChangesError::InconsistentDistribution(names, found, expected) => write!(
                f,
                "Inconsistent distribution in changes files {:?}: found {} expected {}",
                names, found, expected
            ),
            FindChangesError::MissingChangesFileFields(field) => {
                write!(f, "Missing field {} in changes files", field)
            }
            FindChangesError::IoError(path, err) => {
                write!(f, "I/O error accessing {}: {}", path.display(), err)
            }
            FindChangesError::ParseError(path, err) => {
                write!(f, "Error parsing changes file {}: {}", path.display(), err)
            }
            FindChangesError::InvalidFilename(path) => {
                write!(
                    f,
                    "Invalid filename that cannot be converted to UTF-8: {}",
                    path.display()
                )
            }
        }
    }
}

#[cfg(feature = "debian")]
impl std::error::Error for FindChangesError {}

#[cfg(feature = "debian")]
#[cfg(test)]
#[path = "find_changes_tests.rs"]
mod find_changes_tests;

#[cfg(feature = "debian")]
/// Summary of changes files.
#[derive(Debug)]
pub struct ChangesSummary {
    /// Names of the changes files.
    pub names: Vec<String>,
    /// Source package name.
    pub source: String,
    /// Package version.
    pub version: debversion::Version,
    /// Distribution name.
    pub distribution: String,
    /// Names of binary packages included in the changes.
    pub binary_packages: Vec<String>,
}

/// Pure helper: extract the package name from a `.deb` filename.
/// Debian package filenames are `<name>_<version>_<arch>.deb`, so
/// the package name is everything before the first underscore.
/// Returns `None` for any filename that isn't a recognizable
/// `<name>_..._....deb` form: missing `.deb` suffix, missing
/// underscore, or empty name.
///
/// Used by [`find_changes`] to populate `binary_packages` from a
/// changes file's listing.
pub fn deb_package_name_from_filename(filename: &str) -> Option<String> {
    if !filename.ends_with(".deb") {
        return None;
    }
    // Must contain at least one underscore; otherwise it's not a
    // valid Debian filename and we'd be returning the whole base
    // name as the "package", which is nonsense.
    let (name, _rest) = filename.split_once('_')?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Find and parse Debian changes files in a directory.
///
/// # Arguments
/// * `path` - Directory to search for changes files
///
/// # Returns
/// A summary of the changes files, or an error if not found or inconsistent
pub fn find_changes(path: &Path) -> Result<ChangesSummary, FindChangesError> {
    let mut names: Vec<String> = Vec::new();
    let mut source: Option<String> = None;
    let mut version: Option<debversion::Version> = None;
    let mut distribution: Option<String> = None;
    let mut binary_packages: Vec<String> = Vec::new();

    let read_dir =
        std::fs::read_dir(path).map_err(|e| FindChangesError::IoError(path.to_path_buf(), e))?;

    for entry_result in read_dir {
        let entry = entry_result.map_err(|e| FindChangesError::IoError(path.to_path_buf(), e))?;

        let file_name = entry.file_name();
        let file_name_str = file_name
            .to_str()
            .ok_or_else(|| FindChangesError::InvalidFilename(entry.path()))?;

        if !file_name_str.ends_with(".changes") {
            continue;
        }

        let file_path = entry.path();
        let f = std::fs::File::open(&file_path)
            .map_err(|e| FindChangesError::IoError(file_path.clone(), e))?;

        let changes = debian_control::changes::Changes::read(&f)
            .map_err(|e| FindChangesError::ParseError(file_path, e.into()))?;
        names.push(entry.file_name().to_string_lossy().to_string());
        if let Some(version) = &version {
            if changes.version().as_ref() != Some(version) {
                let found_version = changes
                    .version()
                    .ok_or_else(|| FindChangesError::MissingChangesFileFields("Version"))?;
                return Err(FindChangesError::InconsistentVersion(
                    names,
                    found_version,
                    version.clone(),
                ));
            }
        }
        version = changes.version();
        if let Some(source) = &source {
            if changes.source().as_ref() != Some(source) {
                let found_source = changes
                    .source()
                    .ok_or_else(|| FindChangesError::MissingChangesFileFields("Source"))?;
                return Err(FindChangesError::InconsistentSource(
                    names,
                    found_source,
                    source.to_string(),
                ));
            }
        }
        source = changes.source();

        if let Some(distribution) = &distribution {
            if changes.distribution().as_ref() != Some(distribution) {
                let found_distribution = changes
                    .distribution()
                    .ok_or_else(|| FindChangesError::MissingChangesFileFields("Distribution"))?;
                return Err(FindChangesError::InconsistentDistribution(
                    names,
                    found_distribution,
                    distribution.to_string(),
                ));
            }
        }
        distribution = changes.distribution();

        binary_packages.extend(
            changes
                .files()
                .unwrap_or_default()
                .iter()
                .filter_map(|file| deb_package_name_from_filename(&file.filename)),
        );
    }
    if names.is_empty() {
        return Err(FindChangesError::NoChangesFile(path.to_path_buf()));
    }

    let source = source.ok_or(FindChangesError::MissingChangesFileFields("Source"))?;
    let version = version.ok_or(FindChangesError::MissingChangesFileFields("Version"))?;
    let distribution =
        distribution.ok_or(FindChangesError::MissingChangesFileFields("Distribution"))?;

    Ok(ChangesSummary {
        names,
        source,
        version,
        distribution,
        binary_packages,
    })
}

/// Check if a filename is a log file.
///
/// # Arguments
/// * `name` - Filename to check
///
/// # Returns
/// `true` if the filename is a log file, `false` otherwise
pub fn is_log_filename(name: &str) -> bool {
    let parts = name.split('.').collect::<Vec<_>>();

    // Must have at least one extension and filename must not be empty
    if parts.len() < 2 || parts[0].is_empty() {
        return false;
    }

    // Handle simple .log files (foo.log)
    if parts.last() == Some(&"log") {
        return true;
    }

    // Handle compressed log files (.log.gz, .log.bz2, etc.)
    if parts.len() >= 3 {
        let compression_extensions = ["gz", "bz2", "xz", "lzma", "Z"];
        if let Some(&last_part) = parts.last() {
            if compression_extensions.contains(&last_part) {
                if parts[parts.len() - 2] == "log" {
                    return true;
                }
            }
        }
    }

    // Handle numbered log files (foo.log.1, foo.1.log)
    if parts.len() == 3 {
        let mut rev = parts.iter().rev();
        let last = rev.next().expect("parts.len() == 3 guarantees 3 elements");
        let middle = rev.next().expect("parts.len() == 3 guarantees 3 elements");

        // foo.log.1 pattern
        if last.chars().all(char::is_numeric) && *middle == "log" {
            return true;
        }
    }

    false
}

#[cfg(feature = "debian")]
/// Get the current Debian vendor.
///
/// # Returns
/// The vendor name, or None if it could not be determined
pub fn dpkg_vendor() -> Option<String> {
    std::process::Command::new("dpkg-vendor")
        .arg("--query")
        .arg("vendor")
        .output()
        .map(|output| {
            if output.status.success() {
                Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|_| {
            log::warn!("Failed to determine dpkg vendor");
            Some(String::new())
        })
}

#[cfg(feature = "debian")]
/// Read the source filenames from a changes file.
pub fn changes_filenames(
    changes_location: &Path,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut f = std::fs::File::open(changes_location)?;
    let changes = debian_control::changes::Changes::read(&mut f)?;
    Ok(changes
        .files()
        .unwrap_or_default()
        .iter()
        .map(|file| file.filename.clone())
        .collect())
}

/// Scan a directory for log files.
///
/// # Arguments
/// * `output_directory` - Directory to scan
///
/// # Returns
/// An iterator over log file directory entries, or an empty iterator if the directory cannot be read
pub fn gather_logs(output_directory: &std::path::Path) -> impl Iterator<Item = std::fs::DirEntry> {
    match std::fs::read_dir(output_directory) {
        Ok(entries) => {
            let logs: Vec<_> = entries
                .filter_map(|entry| {
                    let entry = entry.ok()?;
                    let file_type = entry.file_type().ok()?;
                    let file_name = entry.file_name();
                    let file_name_str = file_name.to_str()?;
                    if file_type.is_dir() && is_log_filename(file_name_str) {
                        Some(entry)
                    } else {
                        None
                    }
                })
                .collect();
            logs.into_iter()
        }
        Err(e) => {
            log::error!("Failed to read directory {:?}: {}", output_directory, e);
            Vec::new().into_iter()
        }
    }
}

/// Result of a Janitor run.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JanitorResult {
    /// Unique identifier for the log.
    pub log_id: String,
    /// URL of the branch that was processed.
    pub branch_url: String,
    /// Optional subpath within the repository.
    pub subpath: Option<String>,
    /// Result code.
    pub code: String,
    /// Whether the result is transient.
    pub transient: Option<bool>,
    /// Name of the codebase.
    pub codebase: String,
    /// Name of the campaign.
    pub campaign: String,
    /// Human-readable description of the result.
    pub description: Option<String>,
    /// Result of the codemod.
    pub codemod: Option<serde_json::Value>,
    /// Optional value associated with the result.
    pub value: Option<u64>,
    /// Names of log files.
    pub logfilenames: Vec<String>,

    /// Time when the run started.
    pub start_time: DateTime<Utc>,
    /// Time when the run finished.
    pub finish_time: DateTime<Utc>,

    /// Revision ID of the branch after processing.
    pub revision: Option<RevisionId>,
    /// Revision ID of the main branch.
    pub main_branch_revision: Option<RevisionId>,

    /// Optional changeset ID.
    pub change_set: Option<String>,

    /// Optional tags with revision IDs.
    pub tags: Option<Vec<(String, Option<RevisionId>)>>,
    /// Optional remote repositories.
    pub remotes: Option<HashMap<String, ResultRemote>>,

    /// Optional branches information.
    pub branches: Option<
        Vec<(
            Option<String>,
            Option<String>,
            Option<RevisionId>,
            Option<RevisionId>,
        )>,
    >,

    /// Optional details about the failure.
    pub failure_details: Option<serde_json::Value>,
    /// Optional stages where failure occurred.
    pub failure_stage: Option<Vec<String>>,

    /// Optional information about resuming a previous run.
    pub resume: Option<ResultResume>,

    /// Optional target information.
    pub target: Option<ResultTarget>,

    /// Optional worker name.
    pub worker_name: Option<String>,
    /// Optional VCS type.
    pub vcs_type: Option<String>,
    /// Optional target branch URL.
    pub target_branch_url: Option<String>,
    /// Optional context information.
    pub context: Option<serde_json::Value>,
    /// Optional builder result.
    pub builder_result: Option<BuilderResult>,
}

impl JanitorResult {
    /// Calculate the duration of the run.
    pub fn duration(&self) -> Duration {
        let duration = self.finish_time - self.start_time;
        Duration::from_secs(duration.num_seconds().max(0) as u64)
    }

    /// Convert to JSON representation.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "codebase": self.codebase,
            "campaign": self.campaign,
            "change_set": self.change_set,
            "log_id": self.log_id,
            "description": self.description,
            "code": self.code,
            "failure_details": self.failure_details,
            "failure_stage": self.failure_stage,
            "duration": self.duration().as_secs_f64(),
            "finish_time": self.finish_time.to_rfc3339(),
            "start_time": self.start_time.to_rfc3339(),
            "transient": self.transient,
            "target": self.target.as_ref().map(|t| serde_json::json!({
                "name": t.name,
                "details": t.details
            })).unwrap_or_else(|| serde_json::json!({})),
            "logfilenames": self.logfilenames,
            "codemod": self.codemod,
            "value": self.value,
            "remotes": self.remotes,
            "branch_url": self.branch_url,
            "resume": self.resume.as_ref().map(|r| serde_json::json!({"run_id": r.run_id})),
            "branches": self.branches.as_ref().map(|branches| {
                branches.iter().map(|(fn_name, name, br, r)| {
                    serde_json::json!([
                        fn_name,
                        name,
                        br.as_ref().map(|b| b.to_string()),
                        r.as_ref().map(|r| r.to_string())
                    ])
                }).collect::<Vec<_>>()
            }),
            "tags": self.tags.as_ref().map(|tags| {
                tags.iter().map(|(name, rev)| {
                    serde_json::json!([
                        name,
                        rev.as_ref().map(|r| r.to_string())
                    ])
                }).collect::<Vec<_>>()
            }),
            "revision": self.revision.as_ref().map(|r| r.to_string()),
            "main_branch_revision": self.main_branch_revision.as_ref().map(|r| r.to_string())
        })
    }
}

/// Information about resuming a previous run.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ResultResume {
    /// ID of the run to resume.
    pub run_id: String,
}

/// Target information for a result.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ResultTarget {
    /// Name of the target.
    pub name: String,
    /// Additional details about the target.
    pub details: serde_json::Value,
}

/// Remote repository information for a result.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ResultRemote {
    /// URL of the remote repository.
    pub url: String,
}

fn default_success_code() -> String {
    "success".to_string()
}

/// Result from a worker.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorkerResult {
    /// Result code. The worker's `Metadata` carries this as
    /// `Option<String>` and serializes with
    /// `skip_serializing_if = "Option::is_none"` -- i.e. the
    /// `code` field is absent from the JSON payload on the
    /// success path. Accept the missing-field case here and
    /// default to `"success"` so /finish doesn't reject every
    /// successful run with `missing field 'code'`.
    #[serde(default = "default_success_code")]
    pub code: String,
    /// Human-readable description.
    pub description: Option<String>,
    /// Context information.
    pub context: Option<serde_json::Value>,
    /// Codemod result.
    pub codemod: Option<serde_json::Value>,
    /// Main branch revision ID.
    pub main_branch_revision: Option<RevisionId>,
    /// Current revision ID.
    pub revision: Option<RevisionId>,
    /// Optional value associated with the result.
    pub value: Option<i64>,
    /// Branch information.
    pub branches: Option<
        Vec<(
            Option<String>,
            Option<String>,
            Option<RevisionId>,
            Option<RevisionId>,
        )>,
    >,
    /// Tag information.
    pub tags: Option<Vec<(String, Option<RevisionId>)>>,
    /// Remote repository information.
    pub remotes: Option<HashMap<String, HashMap<String, serde_json::Value>>>,
    /// Failure details.
    pub details: Option<serde_json::Value>,
    /// Failure stage.
    ///
    /// Worker serialises this as `Option<String>` (see
    /// `src/api/worker.rs::Metadata::stage`; `Metadata::update`
    /// writes the joined stage path). The old `Option<Vec<String>>`
    /// type on the runner side broke JSON parsing of every `/finish`
    /// upload with "expected a sequence". Keep the wire format
    /// as-is and let callers split on `/` if they need the pieces.
    pub stage: Option<String>,
    /// Builder result. The worker doesn't actually emit this on the
    /// wire -- it sends [`Self::target`] instead (a
    /// `TargetDetails { name, details }` shape). This field stays
    /// for code paths that already populate it directly (mostly
    /// tests and the in-process resume path); production worker
    /// uploads materialise it via [`Self::resolved_builder_result`]
    /// from the `target` payload.
    #[serde(default)]
    pub builder_result: Option<BuilderResult>,
    /// Wire-shape build metadata as the worker actually emits it
    /// (`{"target": {"name": "debian", "details": {build_version,
    /// build_distribution, source, …}}}` -- see
    /// `worker/src/lib.rs` where `metadata.target` is set right
    /// after `build_target.build(...)`). Renamed-field deserialise
    /// fallback for [`Self::builder_result`]; the finish-run path
    /// calls [`Self::resolved_builder_result`] to fold this back
    /// into a `BuilderResult` so `debian_build` rows actually get
    /// inserted on successful runs.
    #[serde(default)]
    pub target: Option<janitor::api::worker::TargetDetails>,
    /// Start time.
    pub start_time: Option<DateTime<Utc>>,
    /// Finish time.
    pub finish_time: Option<DateTime<Utc>>,
    /// Queue ID.
    pub queue_id: Option<i64>,
    /// Worker name.
    pub worker_name: Option<String>,
    /// Whether the run was refreshed.
    ///
    /// Worker emits `null` for runs where "refreshed" isn't
    /// meaningful (see worker/src/lib.rs where some code-paths
    /// serialise `"refreshed": False` and others leave the option
    /// unset). Accepting `Option<bool>` here matches the wire shape;
    /// callers that previously compared `refreshed == true` should
    /// do `refreshed.unwrap_or(false)`.
    #[serde(default)]
    pub refreshed: Option<bool>,
    /// Target branch URL.
    pub target_branch_url: Option<String>,
    /// Branch URL.
    pub branch_url: Option<String>,
    /// VCS type.
    pub vcs_type: Option<String>,
    /// Subpath within repository.
    pub subpath: Option<String>,
    /// Whether the result is transient.
    pub transient: Option<bool>,
    /// Codebase name.
    pub codebase: Option<String>,
}

/// Information about an active run.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ActiveRun {
    /// Worker name.
    pub worker_name: String,
    /// Optional worker link.
    pub worker_link: Option<String>,
    /// Queue ID.
    pub queue_id: i64,
    /// Unique log ID.
    pub log_id: String,
    /// Start time.
    pub start_time: DateTime<Utc>,
    /// Optional finish time.
    pub finish_time: Option<DateTime<Utc>>,
    /// Optional estimated duration.
    #[serde(with = "serde_with::As::<Option<serde_with::DurationSeconds<f64>>>")]
    pub estimated_duration: Option<Duration>,
    /// Campaign name.
    pub campaign: String,
    /// Optional change set.
    pub change_set: Option<String>,
    /// Command being executed.
    pub command: String,
    /// Backchannel for communication.
    pub backchannel: Backchannel,
    /// VCS information.
    pub vcs_info: VcsInfo,
    /// Codebase name.
    pub codebase: String,
    /// Instigated context.
    pub instigated_context: Option<serde_json::Value>,
    /// Optional resume from run ID.
    pub resume_from: Option<String>,
}

impl ActiveRun {
    /// Calculate current duration of the run.
    pub fn current_duration(&self) -> Duration {
        let now = Utc::now();
        let duration = now - self.start_time;
        Duration::from_secs(duration.num_seconds().max(0) as u64)
    }

    /// Get VCS type.
    pub fn vcs_type(&self) -> Option<&str> {
        self.vcs_info.vcs_type.as_deref()
    }

    /// Get main branch URL.
    pub fn main_branch_url(&self) -> Option<&str> {
        self.vcs_info.branch_url.as_deref()
    }

    /// Get subpath.
    pub fn subpath(&self) -> Option<&str> {
        self.vcs_info.subpath.as_deref()
    }

    /// Create a JanitorResult from this active run.
    pub fn create_result(&self, code: String, description: Option<String>) -> JanitorResult {
        JanitorResult {
            log_id: self.log_id.clone(),
            branch_url: self.vcs_info.branch_url.clone().unwrap_or_default(),
            subpath: self.vcs_info.subpath.clone(),
            code,
            transient: None,
            codebase: self.codebase.clone(),
            campaign: self.campaign.clone(),
            description,
            codemod: None,
            value: None,
            logfilenames: vec![],
            start_time: self.start_time,
            finish_time: Utc::now(),
            revision: None,
            main_branch_revision: None,
            change_set: self.change_set.clone(),
            tags: None,
            remotes: None,
            branches: None,
            failure_details: None,
            failure_stage: None,
            resume: self
                .resume_from
                .as_ref()
                .map(|id| ResultResume { run_id: id.clone() }),
            target: None,
            worker_name: Some(self.worker_name.clone()),
            vcs_type: self.vcs_info.vcs_type.clone(),
            target_branch_url: None,
            context: self.instigated_context.clone(),
            builder_result: None,
        }
    }

    /// Convert to JSON representation.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "queue_id": self.queue_id,
            "id": self.log_id,
            "codebase": self.codebase,
            "change_set": self.change_set,
            "campaign": self.campaign,
            "command": self.command,
            "estimated_duration": self.estimated_duration.map(|d| d.as_secs_f64()),
            "current_duration": self.current_duration().as_secs_f64(),
            "start_time": self.start_time.to_rfc3339(),
            "worker": self.worker_name,
            "worker_link": self.worker_link,
            "vcs": self.vcs_info,
            "backchannel": self.backchannel.to_json(),
            "instigated_context": self.instigated_context,
            "resume_from": self.resume_from
        })
    }

    /// Ping the worker to check if it's still alive.
    pub async fn ping(&self) -> Result<(), PingError> {
        self.backchannel.ping(&self.log_id).await
    }
}

/// Backchannel communication types.
///
/// Serialised with an explicit `type` discriminator: `"jenkins"`,
/// `"polling"`, or `"none"`. The previous `#[serde(untagged)]` form
/// silently mis-classified every Polling backchannel as Jenkins on
/// round-trip through Redis: the Jenkins variant's required fields
/// (`my_url`) are a strict subset of Polling's, with `jenkins:
/// Option<...>` allowed to be missing, so serde tried Jenkins first
/// and matched. Local workers (e.g. tyr/idun) ended up health-checked
/// through `JenkinsBackchannel::get_health_status`, which then timed
/// out hitting `/api/json` on the worker port and converted live runs
/// to `worker-failure`. Tag explicitly so the variant is unambiguous.
///
/// `Deserialize` is implemented manually to also accept the legacy
/// untagged shape -- old Redis entries written before this change look
/// like `{"my_url": "..."}` (Polling) or `{"my_url": "...", "jenkins":
/// ...}` (Jenkins) and would otherwise fail to deserialize.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Backchannel {
    /// Jenkins backchannel.
    Jenkins {
        /// Jenkins URL.
        my_url: String,
        /// Jenkins metadata.
        jenkins: Option<serde_json::Value>,
    },
    /// Polling backchannel.
    Polling {
        /// Worker URL.
        my_url: String,
    },
    /// No backchannel (default).
    None {},
}

impl<'de> Deserialize<'de> for Backchannel {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Accept two shapes:
        //   - new tagged: {"type":"jenkins"|"polling"|"none", ...}
        //   - legacy untagged: {"my_url":"...","jenkins":...} | {"my_url":"..."} | {}
        // Legacy entries already in Redis use the untagged form. The
        // `jenkins` field's presence (not its value -- it can be null)
        // is the discriminator there: Jenkins always serialised it,
        // Polling never had it.
        let v = serde_json::Value::deserialize(deserializer)?;
        let obj = v
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("Backchannel: expected a JSON object"))?;

        if let Some(tag) = obj.get("type").and_then(|t| t.as_str()) {
            return match tag {
                "jenkins" => {
                    let my_url = obj
                        .get("my_url")
                        .and_then(|u| u.as_str())
                        .ok_or_else(|| serde::de::Error::missing_field("my_url"))?
                        .to_string();
                    let jenkins = obj.get("jenkins").cloned();
                    Ok(Backchannel::Jenkins { my_url, jenkins })
                }
                "polling" => {
                    let my_url = obj
                        .get("my_url")
                        .and_then(|u| u.as_str())
                        .ok_or_else(|| serde::de::Error::missing_field("my_url"))?
                        .to_string();
                    Ok(Backchannel::Polling { my_url })
                }
                "none" => Ok(Backchannel::None {}),
                other => Err(serde::de::Error::custom(format!(
                    "Backchannel: unknown type {:?}",
                    other
                ))),
            };
        }

        // Legacy untagged form. Presence of a `jenkins` key (even
        // null) means Jenkins; lone `my_url` means Polling; empty
        // object means None.
        if obj.contains_key("jenkins") {
            let my_url = obj
                .get("my_url")
                .and_then(|u| u.as_str())
                .ok_or_else(|| serde::de::Error::missing_field("my_url"))?
                .to_string();
            let jenkins = obj.get("jenkins").cloned();
            return Ok(Backchannel::Jenkins { my_url, jenkins });
        }
        if let Some(my_url) = obj.get("my_url").and_then(|u| u.as_str()) {
            return Ok(Backchannel::Polling {
                my_url: my_url.to_string(),
            });
        }
        Ok(Backchannel::None {})
    }
}

impl Default for Backchannel {
    fn default() -> Self {
        Backchannel::None {}
    }
}

impl Backchannel {
    /// Ping the worker.
    pub async fn ping(&self, expected_log_id: &str) -> Result<(), PingError> {
        match self {
            Backchannel::None {} => {
                // No ping available
                Err(PingError::Retriable(
                    "No backchannel available for ping".to_string(),
                ))
            }
            Backchannel::Jenkins { my_url, .. } => {
                // Implement Jenkins ping by checking job status
                use reqwest::Client;
                use std::time::Duration;

                let client = Client::builder()
                    .timeout(Duration::from_secs(60))
                    .build()
                    .map_err(|e| {
                        PingError::Retriable(format!("Failed to create HTTP client: {}", e))
                    })?;

                let api_url = format!("{}/api/json", my_url);

                match client.get(&api_url).send().await {
                    Ok(response) => {
                        if response.status() == 404 {
                            Err(PingError::Fatal(format!(
                                "Jenkins job {} has disappeared",
                                my_url
                            )))
                        } else if !response.status().is_success() {
                            Err(PingError::Retriable(format!(
                                "Failed to ping Jenkins {}: HTTP {}",
                                my_url,
                                response.status()
                            )))
                        } else {
                            match response.json::<serde_json::Value>().await {
                                Ok(job) => {
                                    if let Some(result) = job.get("result") {
                                        if result == "FAILURE" {
                                            if let Some(job_id) = job.get("id") {
                                                return Err(PingError::Fatal(format!(
                                                    "Jenkins lists job {} for run {} as failed",
                                                    job_id, expected_log_id
                                                )));
                                            }
                                        }
                                    }
                                    Ok(())
                                }
                                Err(e) => Err(PingError::Retriable(format!(
                                    "Failed to parse Jenkins response from {}: {}",
                                    my_url, e
                                ))),
                            }
                        }
                    }
                    Err(e) => {
                        if e.is_timeout() {
                            Err(PingError::Timeout(format!(
                                "Failed to ping Jenkins {}: {}",
                                my_url, e
                            )))
                        } else {
                            Err(PingError::Retriable(format!(
                                "Failed to ping Jenkins {}: {}",
                                my_url, e
                            )))
                        }
                    }
                }
            }
            Backchannel::Polling { my_url } => {
                // Implement polling ping by checking worker health
                use reqwest::Client;
                use std::time::Duration;

                let client = Client::builder()
                    .timeout(Duration::from_secs(60))
                    .build()
                    .map_err(|e| {
                        PingError::Retriable(format!("Failed to create HTTP client: {}", e))
                    })?;

                let health_url = format!("{}/log-id", my_url);
                log::info!("Pinging URL {} for run {}", health_url, expected_log_id);

                match client.get(&health_url).send().await {
                    Ok(response) => {
                        if !response.status().is_success() {
                            Err(PingError::Retriable(format!(
                                "Failed to ping worker {}: HTTP {}",
                                my_url,
                                response.status()
                            )))
                        } else {
                            match response.text().await {
                                Ok(log_id) => {
                                    let log_id = log_id.trim();
                                    if log_id != expected_log_id {
                                        Err(PingError::Fatal(format!(
                                            "Worker started processing new run {} rather than {}",
                                            log_id, expected_log_id
                                        )))
                                    } else {
                                        Ok(())
                                    }
                                }
                                Err(e) => Err(PingError::Retriable(format!(
                                    "Failed to read response from {}: {}",
                                    my_url, e
                                ))),
                            }
                        }
                    }
                    Err(e) => {
                        if e.is_timeout() {
                            Err(PingError::Timeout(format!(
                                "Failed to ping worker {}: {}",
                                my_url, e
                            )))
                        } else {
                            Err(PingError::Retriable(format!(
                                "Failed to ping worker {}: {}",
                                my_url, e
                            )))
                        }
                    }
                }
            }
        }
    }

    /// Get health status from the worker.
    pub async fn get_health_status(
        &self,
        expected_log_id: &str,
    ) -> Result<crate::HealthStatus, crate::BackchannelError> {
        match self {
            Backchannel::None {} => Err(crate::BackchannelError::WorkerUnreachable(
                "No backchannel available for health check".to_string(),
            )),
            Backchannel::Jenkins { my_url, .. } => {
                let url = url::Url::parse(my_url).map_err(|_| {
                    crate::BackchannelError::FatalFailure("Invalid URL".to_string())
                })?;
                let jenkins_bc = crate::JenkinsBackchannel::new(url);
                jenkins_bc.get_health_status(expected_log_id).await
            }
            Backchannel::Polling { my_url } => {
                let url = url::Url::parse(my_url).map_err(|_| {
                    crate::BackchannelError::FatalFailure("Invalid URL".to_string())
                })?;
                let polling_bc = crate::PollingBackchannel::new(url);
                polling_bc.get_health_status(expected_log_id).await
            }
        }
    }

    /// Terminate the worker gracefully.
    pub async fn terminate(&self, log_id: &str) -> Result<(), crate::BackchannelError> {
        match self {
            Backchannel::None {} => Err(crate::BackchannelError::WorkerUnreachable(
                "No backchannel available for termination".to_string(),
            )),
            Backchannel::Jenkins { my_url, .. } => {
                let url = url::Url::parse(my_url).map_err(|_| {
                    crate::BackchannelError::FatalFailure("Invalid URL".to_string())
                })?;
                let jenkins_bc = crate::JenkinsBackchannel::new(url);
                jenkins_bc.terminate(log_id).await
            }
            Backchannel::Polling { my_url } => {
                let url = url::Url::parse(my_url).map_err(|_| {
                    crate::BackchannelError::FatalFailure("Invalid URL".to_string())
                })?;
                let polling_bc = crate::PollingBackchannel::new(url);
                polling_bc.terminate(log_id).await
            }
        }
    }

    /// Kill the worker.
    pub async fn kill(&self) -> Result<(), PingError> {
        match self {
            Backchannel::None {} => Err(PingError::NotSupported(
                "No backchannel available for kill".to_string(),
            )),
            Backchannel::Jenkins { .. } => Err(PingError::NotSupported(
                "Jenkins kill not supported - Jenkins jobs cannot be killed via API".to_string(),
            )),
            Backchannel::Polling { my_url } => {
                // Implement polling kill by sending POST request to /kill endpoint
                use reqwest::Client;
                use std::time::Duration;

                let client = Client::builder()
                    .timeout(Duration::from_secs(30))
                    .build()
                    .map_err(|e| {
                        PingError::Retriable(format!("Failed to create HTTP client: {}", e))
                    })?;

                let kill_url = format!("{}/kill", my_url);

                match client
                    .post(&kill_url)
                    .header("Accept", "application/json")
                    .send()
                    .await
                {
                    Ok(response) => {
                        let status = response.status();
                        if status.is_success() {
                            Ok(())
                        } else if status == reqwest::StatusCode::NOT_IMPLEMENTED {
                            // Worker returned 501: kill isn't supported for an
                            // in-progress run.
                            Err(PingError::NotSupported(format!(
                                "kill not supported by worker at {}",
                                my_url
                            )))
                        } else if status == reqwest::StatusCode::GONE {
                            // Worker returned 410: it has no active run (it may
                            // have restarted while this run was in progress).
                            Err(PingError::NoActiveRun(format!(
                                "worker at {} has no active run - it may have restarted",
                                my_url
                            )))
                        } else {
                            Err(PingError::Retriable(format!(
                                "Failed to kill worker at {}: HTTP {}",
                                my_url, status
                            )))
                        }
                    }
                    Err(e) => {
                        if e.is_timeout() {
                            Err(PingError::Timeout(format!(
                                "Timeout killing worker at {}: {}",
                                my_url, e
                            )))
                        } else {
                            Err(PingError::Retriable(format!(
                                "Failed to kill worker at {}: {}",
                                my_url, e
                            )))
                        }
                    }
                }
            }
        }
    }

    /// List available log files.
    pub async fn list_log_files(&self) -> Result<Vec<String>, PingError> {
        match self {
            Backchannel::None {} => Ok(vec![]),
            Backchannel::Jenkins { .. } => Ok(vec!["worker.log".to_string()]),
            Backchannel::Polling { my_url } => {
                // Implement polling list_log_files by querying /logs endpoint
                use reqwest::Client;
                use std::time::Duration;

                let client = Client::builder()
                    .timeout(Duration::from_secs(30))
                    .build()
                    .map_err(|e| {
                        PingError::Retriable(format!("Failed to create HTTP client: {}", e))
                    })?;

                // `my_url` typically ends with `/` (the worker's
                // backchannel URL stores it that way), so a naive
                // `format!("{}/logs", my_url)` builds a double-slash
                // path the worker's router doesn't match (404). Trim
                // any trailing slash before appending so we hit the
                // exact `/logs` route. Also set
                // `Accept: application/json` -- the worker's `/logs`
                // handler does content negotiation and returns HTML
                // by default, which `resp.json()` can't parse.
                let logs_url = format!("{}/logs", my_url.trim_end_matches('/'));

                match client
                    .get(&logs_url)
                    .header(reqwest::header::ACCEPT, "application/json")
                    .send()
                    .await
                {
                    Ok(response) => {
                        if response.status().is_success() {
                            match response.json::<Vec<String>>().await {
                                Ok(log_files) => Ok(log_files),
                                Err(e) => Err(PingError::Retriable(format!(
                                    "Failed to parse log files response from {}: {}",
                                    my_url, e
                                ))),
                            }
                        } else {
                            Err(PingError::Retriable(format!(
                                "Failed to list log files from {}: HTTP {}",
                                my_url,
                                response.status()
                            )))
                        }
                    }
                    Err(e) => {
                        if e.is_timeout() {
                            Err(PingError::Timeout(format!(
                                "Timeout listing log files from {}: {}",
                                my_url, e
                            )))
                        } else {
                            Err(PingError::Retriable(format!(
                                "Failed to list log files from {}: {}",
                                my_url, e
                            )))
                        }
                    }
                }
            }
        }
    }

    /// Get a specific log file.
    pub async fn get_log_file(&self, name: &str) -> Result<Vec<u8>, PingError> {
        match self {
            Backchannel::None {} => {
                Err(PingError::Retriable("No backchannel available".to_string()))
            }
            Backchannel::Jenkins { my_url, .. } => {
                // Jenkins only supports getting "worker.log" via progressiveText endpoint
                if name != "worker.log" {
                    return Err(PingError::NotFound(format!(
                        "Jenkins log file not found: {}",
                        name
                    )));
                }

                use reqwest::Client;
                use std::time::Duration;

                let client = Client::builder()
                    .timeout(Duration::from_secs(60))
                    .build()
                    .map_err(|e| {
                        PingError::Retriable(format!("Failed to create HTTP client: {}", e))
                    })?;

                let log_url = format!("{}/logText/progressiveText", my_url);

                match client.get(&log_url).send().await {
                    Ok(response) => {
                        if response.status().is_success() {
                            match response.bytes().await {
                                Ok(bytes) => Ok(bytes.to_vec()),
                                Err(e) => Err(PingError::Retriable(format!(
                                    "Failed to read log file content from {}: {}",
                                    my_url, e
                                ))),
                            }
                        } else if response.status() == 404 {
                            Err(PingError::NotFound(format!("Log file not found: {}", name)))
                        } else {
                            Err(PingError::Retriable(format!(
                                "Failed to get log file from {}: HTTP {}",
                                my_url,
                                response.status()
                            )))
                        }
                    }
                    Err(e) => {
                        if e.is_timeout() {
                            Err(PingError::Timeout(format!(
                                "Timeout getting log file from {}: {}",
                                my_url, e
                            )))
                        } else {
                            Err(PingError::Retriable(format!(
                                "Failed to get log file from {}: {}",
                                my_url, e
                            )))
                        }
                    }
                }
            }
            Backchannel::Polling { my_url } => {
                // Polling gets log files via /logs/{name} endpoint
                use reqwest::Client;
                use std::time::Duration;

                let client = Client::builder()
                    .timeout(Duration::from_secs(60))
                    .build()
                    .map_err(|e| {
                        PingError::Retriable(format!("Failed to create HTTP client: {}", e))
                    })?;

                // `my_url` is the worker's `--my-url`, which the
                // systemd unit configures with a trailing slash
                // (`http://192.168.49.1:9821/`). Naïve `format!`
                // produces `…//logs/<file>`, and the worker's
                // NormalizePathLayer 404s the double-slash form.
                let log_url = format!("{}/logs/{}", my_url.trim_end_matches('/'), name);

                match client.get(&log_url).send().await {
                    Ok(response) => {
                        if response.status().is_success() {
                            match response.bytes().await {
                                Ok(bytes) => Ok(bytes.to_vec()),
                                Err(e) => Err(PingError::Retriable(format!(
                                    "Failed to read log file content from {}: {}",
                                    my_url, e
                                ))),
                            }
                        } else if response.status() == 404 {
                            Err(PingError::NotFound(format!("Log file not found: {}", name)))
                        } else {
                            Err(PingError::Retriable(format!(
                                "Failed to get log file from {}: HTTP {}",
                                my_url,
                                response.status()
                            )))
                        }
                    }
                    Err(e) => {
                        if e.is_timeout() {
                            Err(PingError::Timeout(format!(
                                "Timeout getting log file from {}: {}",
                                my_url, e
                            )))
                        } else {
                            Err(PingError::Retriable(format!(
                                "Failed to get log file from {}: {}",
                                my_url, e
                            )))
                        }
                    }
                }
            }
        }
    }

    /// Convert to JSON representation.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Backchannel::None {} => serde_json::json!({}),
            Backchannel::Jenkins { my_url, jenkins } => serde_json::json!({
                "my_url": my_url,
                "jenkins": jenkins
            }),
            Backchannel::Polling { my_url } => serde_json::json!({
                "my_url": my_url
            }),
        }
    }

    /// Ask the worker which lifecycle stage it's currently in.
    ///
    /// The worker tracks this in `AppState::current_stage` and exposes
    /// it via `WorkerStatusInfo::current_stage` on `GET /status`.
    /// Mapping the stage onto a default "open this log first" choice
    /// on the active-run page is the whole reason this exists -- opening
    /// `worker.log` while sbuild is mid-build leaves the operator
    /// staring at "Workspace ready, starting", and opening `build.log`
    /// after the build is done leaves them staring at the last apt
    /// line instead of the push/finish trace currently being written
    /// to `worker.log`. Returns `None` when the worker doesn't know
    /// (older worker, or the field is unset between transitions).
    /// Only `Backchannel::Polling` has a worker URL to ask; the other
    /// variants return `None`.
    pub async fn get_current_stage(&self) -> Result<Option<String>, PingError> {
        match self {
            Backchannel::None {} | Backchannel::Jenkins { .. } => Ok(None),
            Backchannel::Polling { my_url } => {
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(10))
                    .build()
                    .map_err(|e| {
                        PingError::Retriable(format!("Failed to create HTTP client: {}", e))
                    })?;
                let status_url = format!("{}/status", my_url.trim_end_matches('/'));
                let resp = client
                    .get(&status_url)
                    .header(reqwest::header::ACCEPT, "application/json")
                    .send()
                    .await
                    .map_err(|e| {
                        if e.is_timeout() {
                            PingError::Timeout(format!(
                                "Timeout fetching status from {}: {}",
                                my_url, e
                            ))
                        } else {
                            PingError::Retriable(format!(
                                "Failed to fetch status from {}: {}",
                                my_url, e
                            ))
                        }
                    })?;
                if !resp.status().is_success() {
                    return Err(PingError::Retriable(format!(
                        "Worker {} /status returned HTTP {}",
                        my_url,
                        resp.status()
                    )));
                }
                let body: serde_json::Value = resp.json().await.map_err(|e| {
                    PingError::Retriable(format!("Failed to parse /status from {}: {}", my_url, e))
                })?;
                Ok(body
                    .get("current_stage")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()))
            }
        }
    }
}

/// Builder result types.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind")]
pub enum BuilderResult {
    /// Generic build result.
    #[serde(rename = "generic")]
    Generic,
    /// Debian build result.
    #[serde(rename = "debian")]
    Debian {
        /// Source package name.
        source: Option<String>,
        /// Build version.
        build_version: Option<String>,
        /// Build distribution.
        build_distribution: Option<String>,
        /// Changes filenames.
        changes_filenames: Option<Vec<String>>,
        /// Lintian result.
        lintian_result: Option<serde_json::Value>,
        /// Binary packages.
        binary_packages: Option<Vec<String>>,
    },
}

/// Runner-side view of the wire-shape `target.details` payload for
/// `target.name == "debian"`. All fields are optional because the
/// worker only fills the ones it has: the `.changes`-derived fields
/// come from parsing the artifacts directory on upload, and
/// `lintian` is only present when sbuild + lintian actually ran.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DebianTargetDetails {
    /// Source package name from the produced `.changes` file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Source-package version from the produced `.changes` file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_version: Option<String>,
    /// Distribution the build targeted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_distribution: Option<String>,
    /// Names of the `.changes` files produced. Multi-arch builds
    /// produce one per arch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes_filenames: Option<Vec<String>>,
    /// Binary package names listed in the produced `.changes` file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_packages: Option<Vec<String>>,
    /// Raw lintian output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lintian: Option<serde_json::Value>,
}

impl BuilderResult {
    /// Build a `BuilderResult` from the worker's wire-shape
    /// [`janitor::api::worker::TargetDetails`].
    ///
    /// The worker emits build metadata as
    /// `{"target": {"name": "debian", "details": {"lintian": …}}}`
    /// -- only `lintian` lives on the wire, mirroring Python's
    /// `worker.debian.build.build()` which `return {"lintian":
    /// lintian_result}`. The other fields on `BuilderResult::Debian`
    /// (`source`, `build_version`, `build_distribution`,
    /// `changes_filenames`, `binary_packages`) come from parsing
    /// the `.changes` files in the artifacts directory at upload
    /// time -- see [`crate::find_changes`] and
    /// [`crate::upload::FileUploadProcessor::extract_builder_result`].
    /// Python does the same two-step in
    /// `DebianResult.from_json` + `from_directory`.
    ///
    /// This function only handles the wire-only fields; it leaves
    /// the directory-derived fields as `None` for the caller to
    /// fill in. Returns `None` for unknown target kinds.
    pub fn from_target_details(td: &janitor::api::worker::TargetDetails) -> Option<Self> {
        match td.name.as_str() {
            "generic" => Some(BuilderResult::Generic),
            "debian" => {
                // Pull every wire-shape field through. Extra fields
                // not present in the JSON deserialise as None, which
                // is the same as the older "wire-only-fields-only"
                // behaviour for callers that don't emit them.
                // `extract_debian_builder_result` (the directory-derived
                // path in `upload.rs`) reconstructs the same fields
                // from `.changes` parsing -- both paths converge on
                // the same `BuilderResult::Debian` shape.
                let details: DebianTargetDetails =
                    serde_json::from_value(td.details.clone()).unwrap_or_default();
                Some(BuilderResult::Debian {
                    source: details.source,
                    build_version: details.build_version,
                    build_distribution: details.build_distribution,
                    changes_filenames: details.changes_filenames,
                    binary_packages: details.binary_packages,
                    lintian_result: details.lintian,
                })
            }
            _ => None,
        }
    }

    /// Get the kind of builder result.
    pub fn kind(&self) -> &'static str {
        match self {
            BuilderResult::Generic => "generic",
            BuilderResult::Debian { .. } => "debian",
        }
    }

    /// Convert to JSON.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            BuilderResult::Generic => serde_json::json!({}),
            BuilderResult::Debian {
                source,
                build_version,
                build_distribution,
                changes_filenames,
                lintian_result,
                binary_packages,
            } => serde_json::json!({
                "source": source,
                "build_version": build_version,
                "build_distribution": build_distribution,
                "changes_filenames": changes_filenames,
                "lintian": lintian_result,
                "binary_packages": binary_packages
            }),
        }
    }

    /// Get artifact filenames.
    pub fn artifact_filenames(&self) -> Vec<String> {
        match self {
            BuilderResult::Generic => vec![],
            BuilderResult::Debian {
                changes_filenames, ..
            } => changes_filenames.clone().unwrap_or_default(),
        }
    }
}

/// Ping failure types.
#[derive(Debug)]
pub enum PingError {
    /// Timeout while pinging.
    Timeout(String),
    /// Fatal failure that's not retriable.
    Fatal(String),
    /// Retriable failure.
    Retriable(String),
    /// Specifically: the requested resource (log file, etc.) does
    /// not exist on the worker. Distinguished from Fatal so that the
    /// web layer can return 404 instead of 500.
    NotFound(String),
    /// The operation is not supported by this kind of backchannel.
    /// The runner web layer maps this to a 501 ("kill not supported
    /// for this type of run") instead of a generic 500.
    NotSupported(String),
    /// The worker has no active run to kill -- it may have restarted
    /// while the run was in progress. The runner web layer maps this
    /// to a 410 (Gone) rather than a generic 500.
    NoActiveRun(String),
}

impl std::fmt::Display for PingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PingError::Timeout(msg) => write!(f, "Ping timeout: {}", msg),
            PingError::Fatal(msg) => write!(f, "Fatal ping failure: {}", msg),
            PingError::Retriable(msg) => write!(f, "Ping failure: {}", msg),
            PingError::NotFound(msg) => write!(f, "Not found: {}", msg),
            PingError::NotSupported(msg) => write!(f, "Not supported: {}", msg),
            PingError::NoActiveRun(msg) => write!(f, "No active run: {}", msg),
        }
    }
}

impl std::error::Error for PingError {}

/// A queue item representing work to be done.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QueueItem {
    /// Unique identifier for the queue item.
    pub id: i64,
    /// Context information for the run.
    pub context: Option<serde_json::Value>,
    /// Command to execute.
    pub command: String,
    /// Estimated duration of the run.
    #[serde(with = "serde_with::As::<Option<serde_with::DurationSeconds<f64>>>")]
    pub estimated_duration: Option<Duration>,
    /// Campaign name.
    pub campaign: String,
    /// Whether to refresh the run.
    pub refresh: bool,
    /// Who requested this run.
    pub requester: Option<String>,
    /// Optional change set identifier.
    pub change_set: Option<String>,
    /// Name of the codebase.
    pub codebase: String,
}

impl QueueItem {
    /// Convert to JSON representation.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "context": self.context,
            "command": self.command,
            "estimated_duration": self.estimated_duration.map(|d| d.as_secs_f64()),
            "campaign": self.campaign,
            "refresh": self.refresh,
            "requester": self.requester,
            "change_set": self.change_set,
            "codebase": self.codebase
        })
    }
}

/// Queue assignment result.
#[derive(Debug)]
pub struct QueueAssignment {
    /// The assigned queue item.
    pub queue_item: QueueItem,
    /// VCS information for the codebase.
    pub vcs_info: VcsInfo,
}

/// Application state for the runner.
#[derive(Clone)]
pub struct AppState {
    /// Database connection pool.
    pub database: Arc<database::RunnerDatabase>,
    /// Redis-backed store of active runs (see [`active_runs::ActiveRunStore`]).
    pub active_runs: active_runs::ActiveRunStore,
    /// VCS management system.
    pub vcs_manager: Arc<vcs::RunnerVcsManager>,
    /// Log file management system.
    pub log_manager: Arc<dyn logs::LogFileManager>,
    /// Artifact storage management system.
    pub artifact_manager: Arc<dyn janitor::artifacts::ArtifactManager>,
    /// Error tracking system.
    pub error_tracker: Arc<error_tracking::ErrorTracker>,
    /// Metrics collector.
    pub metrics: Arc<metrics::MetricsCollector>,
    /// Configuration.
    pub config: Arc<janitor::config::Config>,
    /// Upload processor for multipart forms.
    pub upload_processor: Arc<upload::UploadProcessor>,
    /// Worker authentication service.
    pub auth_service: Arc<auth::WorkerAuthService>,
    /// Security service for rate limiting and access control.
    pub security_service: Arc<auth::SecurityService>,
    /// Resume service for handling interrupted runs.
    pub resume_service: Arc<resume::ResumeService>,
    /// Health checker for the service.
    pub health_checker: Arc<HealthChecker>,
    /// Public base URL of our apt archive -- supplied via the
    /// `--public-apt-archive-location` CLI flag. When set, the
    /// runner expands each campaign's `extra_build_distribution`
    /// into a fully formed `deb [trusted=yes] {url} {dist} main`
    /// line for the worker; when None, no extra apt sources are
    /// sent.
    pub public_apt_archive_location: Option<String>,
}

/// Overall health of a component or the runner service as a whole,
/// as returned by the `/health` endpoint. Distinct from
/// [`backchannel::HealthStatus`], which describes a *worker*'s
/// state as reported through its backchannel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceHealthStatus {
    /// Fully healthy.
    Healthy,
    /// Degraded but still operational.
    Degraded,
    /// Not usable.
    Unhealthy,
}

/// Health of a single named component (database, logs, etc).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentHealth {
    /// Component name (e.g. `"database"`).
    pub name: String,
    /// Component status.
    pub status: ServiceHealthStatus,
    /// Error string, if the component is unhealthy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Aggregated health report as returned by `/health`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthReport {
    /// Overall status derived from the component statuses.
    pub status: ServiceHealthStatus,
    /// Service name.
    pub service: String,
    /// Service version.
    pub version: String,
    /// Per-component results.
    pub checks: Vec<ComponentHealth>,
    /// When the check ran.
    pub timestamp: DateTime<Utc>,
}

/// Health probe for the runner service. Runs a `SELECT 1` against
/// the database and calls the log manager's health-check hook; the
/// VCS and artifact managers don't have cheap probes, so their
/// presence is treated as healthy.
pub struct HealthChecker {
    database: Arc<database::RunnerDatabase>,
    log_manager: Arc<dyn logs::LogFileManager>,
    _vcs_manager: Arc<vcs::RunnerVcsManager>,
    _artifact_manager: Arc<dyn janitor::artifacts::ArtifactManager>,
}

impl HealthChecker {
    /// Build a health checker wired to the runner's collaborators.
    pub fn new(
        database: Arc<database::RunnerDatabase>,
        vcs_manager: Arc<vcs::RunnerVcsManager>,
        log_manager: Arc<dyn logs::LogFileManager>,
        artifact_manager: Arc<dyn janitor::artifacts::ArtifactManager>,
    ) -> Self {
        Self {
            database,
            log_manager,
            _vcs_manager: vcs_manager,
            _artifact_manager: artifact_manager,
        }
    }

    /// Full report across every component.
    pub async fn report(&self) -> HealthReport {
        let db = match sqlx::query("SELECT 1")
            .fetch_one(self.database.pool())
            .await
        {
            Ok(_) => ComponentHealth {
                name: "database".to_string(),
                status: ServiceHealthStatus::Healthy,
                error: None,
            },
            Err(e) => ComponentHealth {
                name: "database".to_string(),
                status: ServiceHealthStatus::Unhealthy,
                error: Some(e.to_string()),
            },
        };

        let logs = match self.log_manager.health_check().await {
            Ok(()) => ComponentHealth {
                name: "logs".to_string(),
                status: ServiceHealthStatus::Healthy,
                error: None,
            },
            Err(e) => ComponentHealth {
                name: "logs".to_string(),
                status: ServiceHealthStatus::Unhealthy,
                error: Some(e.to_string()),
            },
        };

        let vcs = ComponentHealth {
            name: "vcs".to_string(),
            status: ServiceHealthStatus::Healthy,
            error: None,
        };
        let artifacts = ComponentHealth {
            name: "artifacts".to_string(),
            status: ServiceHealthStatus::Healthy,
            error: None,
        };

        let checks = vec![db, vcs, logs, artifacts];
        let overall = if checks
            .iter()
            .any(|c| c.status == ServiceHealthStatus::Unhealthy)
        {
            ServiceHealthStatus::Unhealthy
        } else if checks
            .iter()
            .any(|c| c.status == ServiceHealthStatus::Degraded)
        {
            ServiceHealthStatus::Degraded
        } else {
            ServiceHealthStatus::Healthy
        };

        HealthReport {
            status: overall,
            service: "janitor-runner".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            checks,
            timestamp: Utc::now(),
        }
    }

    /// Readiness probe: overall status must be [`ServiceHealthStatus::Healthy`].
    pub async fn is_ready(&self) -> bool {
        self.report().await.status == ServiceHealthStatus::Healthy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_jenkins_kill_returns_not_supported() {
        // The web layer pattern-matches on PingError::NotSupported
        // to return a 501 instead of a 500 -- this guards the
        // contract: Jenkins backchannels must yield NotSupported,
        // not Fatal/Retriable, when killed.
        let bc = Backchannel::Jenkins {
            my_url: "http://jenkins.example.invalid/".parse().unwrap(),
            jenkins: Some(serde_json::json!({})),
        };
        match bc.kill().await {
            Err(PingError::NotSupported(msg)) => {
                assert!(msg.contains("Jenkins"));
            }
            other => panic!("expected PingError::NotSupported, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_none_backchannel_kill_returns_not_supported() {
        // Backchannel::None is a placeholder used when no worker
        // transport has been wired up. Killing it must report
        // NotSupported so the web layer returns 501, mirroring how
        // Python's base Backchannel.kill raises NotImplementedError.
        let bc = Backchannel::None {};
        match bc.kill().await {
            Err(PingError::NotSupported(_)) => {}
            other => panic!("expected PingError::NotSupported, got {:?}", other),
        }
    }

    #[test]
    fn test_ping_error_not_supported_displays() {
        let err = PingError::NotSupported("kill not wired".to_string());
        assert_eq!(format!("{}", err), "Not supported: kill not wired");
    }

    /// New tagged form must round-trip cleanly: serialize emits
    /// `"type"`, deserialize reads it back to the right variant.
    #[test]
    fn test_backchannel_tagged_roundtrip_polling() {
        let bc = Backchannel::Polling {
            my_url: "http://192.168.1.196:9821".to_string(),
        };
        let s = serde_json::to_string(&bc).unwrap();
        assert!(
            s.contains("\"type\":\"polling\""),
            "expected tagged serialisation; got: {}",
            s
        );
        let back: Backchannel = serde_json::from_str(&s).unwrap();
        match back {
            Backchannel::Polling { my_url } => {
                assert_eq!(my_url, "http://192.168.1.196:9821");
            }
            other => panic!("expected Polling, got {:?}", other),
        }
    }

    #[test]
    fn test_backchannel_tagged_roundtrip_jenkins() {
        let bc = Backchannel::Jenkins {
            my_url: "https://jenkins.debian.net/job/janitor-worker/1234/".to_string(),
            jenkins: Some(serde_json::json!({"crumb": "abc"})),
        };
        let s = serde_json::to_string(&bc).unwrap();
        assert!(
            s.contains("\"type\":\"jenkins\""),
            "expected tagged serialisation; got: {}",
            s
        );
        let back: Backchannel = serde_json::from_str(&s).unwrap();
        match back {
            Backchannel::Jenkins { my_url, jenkins } => {
                assert_eq!(
                    my_url,
                    "https://jenkins.debian.net/job/janitor-worker/1234/"
                );
                assert_eq!(jenkins, Some(serde_json::json!({"crumb": "abc"})));
            }
            other => panic!("expected Jenkins, got {:?}", other),
        }
    }

    #[test]
    fn test_backchannel_tagged_roundtrip_none() {
        let bc = Backchannel::None {};
        let s = serde_json::to_string(&bc).unwrap();
        assert_eq!(s, r#"{"type":"none"}"#);
        let back: Backchannel = serde_json::from_str(&s).unwrap();
        assert!(
            matches!(back, Backchannel::None {}),
            "expected None variant, got {:?}",
            back
        );
    }

    /// Legacy untagged Polling shape (`{"my_url":"..."}`) must
    /// deserialize as Polling, NOT Jenkins. This is the bug that took
    /// out tyr/idun: Redis entries written by an older runner came
    /// back as Jenkins because both variants accepted just `my_url`,
    /// and Jenkins was tried first under `#[serde(untagged)]`.
    #[test]
    fn test_backchannel_legacy_polling_no_jenkins_field() {
        let json = r#"{"my_url":"http://192.168.1.196:9821"}"#;
        let bc: Backchannel = serde_json::from_str(json).unwrap();
        match bc {
            Backchannel::Polling { my_url } => {
                assert_eq!(my_url, "http://192.168.1.196:9821");
            }
            other => panic!(
                "legacy {{my_url:...}} must decode as Polling, got {:?}",
                other
            ),
        }
    }

    /// Legacy untagged Jenkins shape (with `jenkins` key, even null)
    /// must deserialize as Jenkins.
    #[test]
    fn test_backchannel_legacy_jenkins_with_null_jenkins_field() {
        let json = r#"{"my_url":"https://jenkins.debian.net/job/x/1/","jenkins":null}"#;
        let bc: Backchannel = serde_json::from_str(json).unwrap();
        match bc {
            Backchannel::Jenkins { my_url, jenkins } => {
                assert_eq!(my_url, "https://jenkins.debian.net/job/x/1/");
                assert!(jenkins.is_none() || jenkins == Some(serde_json::Value::Null));
            }
            other => panic!(
                "legacy {{my_url, jenkins:null}} must decode as Jenkins, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn test_backchannel_legacy_jenkins_with_object_jenkins_field() {
        let json = r#"{"my_url":"https://jenkins.debian.net/job/x/1/","jenkins":{"crumb":"c"}}"#;
        let bc: Backchannel = serde_json::from_str(json).unwrap();
        match bc {
            Backchannel::Jenkins { jenkins, .. } => {
                assert_eq!(jenkins, Some(serde_json::json!({"crumb": "c"})));
            }
            other => panic!("expected Jenkins, got {:?}", other),
        }
    }

    /// Empty object (legacy `Backchannel::None`) decodes as None.
    #[test]
    fn test_backchannel_legacy_none_empty_object() {
        let bc: Backchannel = serde_json::from_str("{}").unwrap();
        assert!(
            matches!(bc, Backchannel::None {}),
            "empty object must decode as None, got {:?}",
            bc
        );
    }

    /// End-to-end Redis round-trip simulation for the bug report:
    /// a Polling backchannel, when serialized via the OLD untagged
    /// derive (just `{"my_url":"..."}`) and re-read by the NEW
    /// deserializer, must come back as Polling. The data shape is
    /// what's already sitting in production Redis right now.
    #[test]
    fn test_backchannel_legacy_polling_redis_data_decodes_as_polling() {
        // Exact shape produced by the old `#[serde(untagged)]` derive
        // serializing `Backchannel::Polling { my_url: ... }`.
        let legacy_redis_payload = r#"{"my_url":"http://idun.local:8080"}"#;
        let bc: Backchannel = serde_json::from_str(legacy_redis_payload).unwrap();
        match bc {
            Backchannel::Polling { my_url } => {
                assert_eq!(my_url, "http://idun.local:8080");
            }
            other => panic!(
                "legacy Polling redis payload must decode as Polling \
                 (not Jenkins as before the fix); got {:?}",
                other
            ),
        }
    }

    #[test]
    fn test_committer_env() {
        let committer = Some("John Doe <john@example.com>");

        let expected = maplit::hashmap! {
            "DEBFULLNAME".to_string() => "John Doe".to_string(),
            "GIT_COMMITTER_NAME".to_string() => "John Doe".to_string(),
            "GIT_AUTHOR_NAME".to_string() => "John Doe".to_string(),
            "DEBEMAIL".to_string() => "john@example.com".to_string(),
            "GIT_COMMITTER_EMAIL".to_string() => "john@example.com".to_string(),
            "GIT_AUTHOR_EMAIL".to_string() => "john@example.com".to_string(),
            "EMAIL".to_string() => "john@example.com".to_string(),
            "COMMITTER".to_string() => "John Doe <john@example.com>".to_string(),
            "BRZ_EMAIL".to_string() => "John Doe <john@example.com>".to_string(),
        };

        assert_eq!(committer_env(committer), expected);
    }

    #[test]
    fn test_active_run_creation() {
        let vcs_info = VcsInfo {
            vcs_type: Some("git".to_string()),
            branch_url: Some("https://github.com/example/repo.git".to_string()),
            subpath: None,
        };

        let active_run = ActiveRun {
            worker_name: "test-worker".to_string(),
            worker_link: None,
            queue_id: 123,
            log_id: "log-456".to_string(),
            start_time: Utc::now(),
            finish_time: None,
            estimated_duration: Some(Duration::from_secs(300)),
            campaign: "test-campaign".to_string(),
            change_set: None,
            command: "test command".to_string(),
            backchannel: Backchannel::default(),
            vcs_info,
            codebase: "test-codebase".to_string(),
            instigated_context: None,
            resume_from: None,
        };

        assert_eq!(active_run.vcs_type(), Some("git"));
        assert_eq!(
            active_run.main_branch_url(),
            Some("https://github.com/example/repo.git")
        );

        let result =
            active_run.create_result("success".to_string(), Some("Test completed".to_string()));
        assert_eq!(result.code, "success");
        assert_eq!(result.description, Some("Test completed".to_string()));
        assert_eq!(result.codebase, "test-codebase");
    }

    #[test]
    fn test_builder_result_serialization() {
        let generic = BuilderResult::Generic;
        assert_eq!(generic.kind(), "generic");
        assert_eq!(generic.artifact_filenames(), Vec::<String>::new());

        let debian = BuilderResult::Debian {
            source: Some("test-package".to_string()),
            build_version: Some("1.0.0".to_string()),
            build_distribution: Some("bullseye".to_string()),
            changes_filenames: Some(vec!["test.changes".to_string()]),
            lintian_result: None,
            binary_packages: Some(vec!["test-bin".to_string()]),
        };

        assert_eq!(debian.kind(), "debian");
        assert_eq!(debian.artifact_filenames(), vec!["test.changes"]);
    }

    /// Regression for the wire-format mismatch between worker
    /// (`metadata.target = TargetDetails { name: "debian", details:
    /// {build_version, …} }`) and runner (`builder_result:
    /// BuilderResult` with `#[serde(tag="kind")]`). Without
    /// `from_target_details` every successful Debian run came in
    /// with `builder_result: None`, no `debian_build` row was
    /// inserted, and the per-run page rendered "this run did not
    /// produce a build".
    #[test]
    fn test_builder_result_from_target_details_debian() {
        let td = janitor::api::worker::TargetDetails {
            name: "debian".to_string(),
            details: serde_json::json!({
                "source": "okio",
                "build_version": "1.16.0-3~jan+lint1",
                "build_distribution": "lintian-fixes",
                "changes_filenames": ["okio_1.16.0-3~jan+lint1_amd64.changes"],
                "binary_packages": ["libokio-java", "libokio-java-doc"],
                // missing fields (lintian_result) tolerated.
            }),
        };
        let br = BuilderResult::from_target_details(&td).expect("debian target should convert");
        match br {
            BuilderResult::Debian {
                source,
                build_version,
                build_distribution,
                changes_filenames,
                binary_packages,
                lintian_result,
            } => {
                assert_eq!(source.as_deref(), Some("okio"));
                assert_eq!(build_version.as_deref(), Some("1.16.0-3~jan+lint1"));
                assert_eq!(build_distribution.as_deref(), Some("lintian-fixes"));
                assert_eq!(
                    changes_filenames,
                    Some(vec!["okio_1.16.0-3~jan+lint1_amd64.changes".to_string()])
                );
                assert_eq!(
                    binary_packages,
                    Some(vec![
                        "libokio-java".to_string(),
                        "libokio-java-doc".to_string()
                    ])
                );
                assert!(lintian_result.is_none());
            }
            other => panic!("expected Debian variant, got {:?}", other),
        }
    }

    #[test]
    fn test_builder_result_from_target_details_generic_and_unknown() {
        let generic = janitor::api::worker::TargetDetails {
            name: "generic".to_string(),
            details: serde_json::json!({}),
        };
        assert!(matches!(
            BuilderResult::from_target_details(&generic),
            Some(BuilderResult::Generic)
        ));
        let unknown = janitor::api::worker::TargetDetails {
            name: "ocaml-future".to_string(),
            details: serde_json::Value::Null,
        };
        assert!(BuilderResult::from_target_details(&unknown).is_none());
    }

    /// `WorkerResult` must accept the wire shape the worker
    /// actually emits: a `target` field instead of `builder_result`.
    #[test]
    fn test_worker_result_deserializes_target_field() {
        let body = serde_json::json!({
            "code": "success",
            "description": "ok",
            "target": {
                "name": "debian",
                "details": {
                    "source": "x",
                    "build_version": "1.0",
                    "build_distribution": "sid"
                }
            }
        });
        let wr: WorkerResult = serde_json::from_value(body).unwrap();
        assert!(wr.builder_result.is_none());
        let target = wr.target.expect("target should be parsed");
        let br = BuilderResult::from_target_details(&target).unwrap();
        match br {
            BuilderResult::Debian { build_version, .. } => {
                assert_eq!(build_version.as_deref(), Some("1.0"));
            }
            other => panic!("expected Debian, got {:?}", other),
        }
    }

    #[test]
    fn test_committer_env_no_committer() {
        let committer = None;

        let expected = maplit::hashmap! {};

        assert_eq!(committer_env(committer), expected);
    }

    #[test]
    fn is_log_filename_test() {
        assert!(is_log_filename("foo.log"));
        assert!(is_log_filename("foo.log.1"));
        assert!(is_log_filename("foo.1.log"));
        assert!(!is_log_filename("foo.1"));
        assert!(!is_log_filename("foo.1.log.1"));
        assert!(!is_log_filename("foo.1.notlog"));
        assert!(!is_log_filename("foo.log.notlog"));
    }

    #[test]
    fn test_dpkg_vendor() {
        let vendor = dpkg_vendor();
        assert!(vendor.is_some());
    }

    #[test]
    fn test_committer_env_name_only() {
        // Committer with name but no email
        let env = committer_env(Some("John Doe"));
        assert_eq!(env.get("DEBFULLNAME"), Some(&"John Doe".to_string()));
        assert_eq!(env.get("COMMITTER"), Some(&"John Doe".to_string()));
        assert_eq!(env.get("BRZ_EMAIL"), Some(&"John Doe".to_string()));
    }

    #[test]
    fn test_is_log_filename_extensions() {
        // Standard .log files
        assert!(is_log_filename("build.log"));
        assert!(is_log_filename("worker.log"));
        assert!(is_log_filename("a.log"));

        // Rotated logs
        assert!(is_log_filename("build.log.1"));
        assert!(is_log_filename("build.log.42"));

        // Nested number format (e.g., build.1.log)
        assert!(is_log_filename("build.1.log"));

        // Not log files
        assert!(!is_log_filename("build.txt"));
        assert!(!is_log_filename("build.log.bak"));
        assert!(!is_log_filename("build"));
        // ".log" is a hidden file with empty basename, not a log
        assert!(!is_log_filename(".log"));
    }

    /// Compressed log files: build.log.gz, build.log.bz2, build.log.xz,
    /// build.log.lzma, build.log.Z. These were unhandled by the
    /// previous test pass; the function handles them so tests should
    /// lock that in.
    #[test]
    fn test_is_log_filename_compressed() {
        assert!(is_log_filename("build.log.gz"));
        assert!(is_log_filename("worker.log.bz2"));
        assert!(is_log_filename("worker.log.xz"));
        assert!(is_log_filename("worker.log.lzma"));
        // The capital-Z form (legacy compress(1)) should also work.
        assert!(is_log_filename("worker.log.Z"));
    }

    /// Nearby false positives that look compressed but aren't:
    /// unsupported compression suffixes (.zst, .lz4) and the
    /// double-compressed form should not match.
    #[test]
    fn test_is_log_filename_compressed_negatives() {
        // Unknown compression suffixes
        assert!(!is_log_filename("worker.log.zst"));
        assert!(!is_log_filename("worker.log.lz4"));
        // Compression suffix without `log` in the second-to-last position
        assert!(!is_log_filename("worker.txt.gz"));
        assert!(!is_log_filename("output.gz"));
    }

    /// Empty input and degenerate inputs should be rejected.
    #[test]
    fn test_is_log_filename_degenerate() {
        assert!(!is_log_filename(""));
        assert!(!is_log_filename("log"));
        // Single-component name with no extension.
        assert!(!is_log_filename("logfile"));
    }

    /// Standard Debian package filename:
    /// <name>_<version>_<arch>.deb -> name only.
    #[test]
    fn test_deb_package_name_standard() {
        assert_eq!(
            deb_package_name_from_filename("hello_2.10-3_amd64.deb"),
            Some("hello".to_string())
        );
    }

    /// Source package names with hyphens and digits round-trip.
    #[test]
    fn test_deb_package_name_with_hyphens() {
        assert_eq!(
            deb_package_name_from_filename("python3-foo_1.0.0-1_all.deb"),
            Some("python3-foo".to_string())
        );
        assert_eq!(
            deb_package_name_from_filename("libfoo-bar2_3.14_amd64.deb"),
            Some("libfoo-bar2".to_string())
        );
    }

    /// Non-.deb files (changes, dsc, buildinfo, source tarballs)
    /// must return None -- find_changes uses this to filter only
    /// binary packages out of the changes file listing.
    #[test]
    fn test_deb_package_name_non_deb_returns_none() {
        assert_eq!(deb_package_name_from_filename("hello.changes"), None);
        assert_eq!(deb_package_name_from_filename("hello.dsc"), None);
        assert_eq!(
            deb_package_name_from_filename("hello_2.10-3_amd64.buildinfo"),
            None
        );
        assert_eq!(
            deb_package_name_from_filename("hello_2.10.orig.tar.gz"),
            None
        );
    }

    /// Filenames without an underscore are not valid Debian
    /// package filenames and must return None -- otherwise we'd
    /// silently emit nonsense like `nounderscore.deb` as a
    /// package name.
    #[test]
    fn test_deb_package_name_no_underscore_returns_none() {
        assert_eq!(deb_package_name_from_filename("nounderscore.deb"), None);
        assert_eq!(deb_package_name_from_filename(".deb"), None);
    }

    /// Empty package name (`_version_arch.deb`) returns None.
    #[test]
    fn test_deb_package_name_empty_name_returns_none() {
        assert_eq!(deb_package_name_from_filename("_1.0_amd64.deb"), None);
    }

    /// Empty input returns None.
    #[test]
    fn test_deb_package_name_empty_input() {
        assert_eq!(deb_package_name_from_filename(""), None);
    }

    #[test]
    fn test_gather_logs_with_files() {
        let td = tempfile::tempdir().unwrap();
        // gather_logs looks for *directories* that match is_log_filename
        std::fs::create_dir(td.path().join("build.log")).unwrap();
        std::fs::create_dir(td.path().join("worker.log")).unwrap();
        std::fs::create_dir(td.path().join("not-a-log")).unwrap();
        // Regular file should be ignored
        std::fs::write(td.path().join("output.log"), "content").unwrap();

        let logs: Vec<_> = gather_logs(td.path()).collect();
        let mut names: Vec<String> = logs
            .iter()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec!["build.log", "worker.log"]);
    }

    #[test]
    fn test_gather_logs_empty_directory() {
        let td = tempfile::tempdir().unwrap();
        let logs: Vec<_> = gather_logs(td.path()).collect();
        assert_eq!(logs.len(), 0);
    }

    #[test]
    fn test_janitor_result_serde() {
        let result = JanitorResult {
            log_id: "log-123".to_string(),
            branch_url: "https://example.com/repo".to_string(),
            subpath: Some("debian/".to_string()),
            code: "success".to_string(),
            transient: Some(false),
            codebase: "mycodebase".to_string(),
            campaign: "lintian-fixes".to_string(),
            description: Some("Fixed 3 lintian issues".to_string()),
            codemod: Some(serde_json::json!({"applied": 3})),
            value: Some(30),
            logfilenames: vec!["build.log".to_string(), "worker.log".to_string()],
            start_time: chrono::Utc::now(),
            finish_time: chrono::Utc::now(),
            revision: Some(breezyshim::RevisionId::from(b"rev-1".to_vec())),
            main_branch_revision: Some(breezyshim::RevisionId::from(b"rev-0".to_vec())),
            change_set: Some("cs-1".to_string()),
            tags: None,
            remotes: None,
            branches: None,
            failure_details: None,
            failure_stage: None,
            resume: None,
            target: None,
            worker_name: None,
            vcs_type: None,
            target_branch_url: None,
            context: None,
            builder_result: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        let roundtripped: JanitorResult = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped.log_id, "log-123");
        assert_eq!(roundtripped.code, "success");
        assert_eq!(roundtripped.codebase, "mycodebase");
        assert_eq!(roundtripped.campaign, "lintian-fixes");
        assert_eq!(roundtripped.value, Some(30));
        assert_eq!(roundtripped.logfilenames, vec!["build.log", "worker.log"]);
    }

    #[test]
    fn test_janitor_result_with_failure() {
        let result = JanitorResult {
            log_id: "log-456".to_string(),
            branch_url: "https://example.com/repo".to_string(),
            subpath: None,
            code: "build-failed".to_string(),
            transient: Some(true),
            codebase: "failcodebase".to_string(),
            campaign: "fresh-releases".to_string(),
            description: Some("Build failed".to_string()),
            codemod: None,
            value: None,
            logfilenames: vec![],
            start_time: chrono::Utc::now(),
            finish_time: chrono::Utc::now(),
            revision: None,
            main_branch_revision: None,
            change_set: None,
            tags: None,
            remotes: None,
            branches: None,
            failure_details: Some(serde_json::json!({"error": "compilation failed"})),
            failure_stage: Some(vec!["build".to_string()]),
            resume: None,
            target: None,
            worker_name: None,
            vcs_type: None,
            target_branch_url: None,
            context: None,
            builder_result: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        let roundtripped: JanitorResult = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped.code, "build-failed");
        assert_eq!(roundtripped.transient, Some(true));
        assert!(roundtripped.failure_details.is_some());
        assert_eq!(roundtripped.failure_stage, Some(vec!["build".to_string()]));
    }

    #[test]
    fn test_janitor_result_with_resume() {
        let result = JanitorResult {
            log_id: "log-789".to_string(),
            branch_url: "https://example.com/repo".to_string(),
            subpath: None,
            code: "success".to_string(),
            transient: None,
            codebase: "test".to_string(),
            campaign: "test".to_string(),
            description: Some("OK".to_string()),
            codemod: Some(serde_json::json!({})),
            value: None,
            logfilenames: vec![],
            start_time: chrono::Utc::now(),
            finish_time: chrono::Utc::now(),
            revision: None,
            main_branch_revision: None,
            change_set: None,
            tags: None,
            remotes: Some(maplit::hashmap! {
                "origin".to_string() => ResultRemote {
                    url: "https://example.com/origin".to_string(),
                }
            }),
            branches: Some(vec![(
                Some("main".to_string()),
                Some("refs/heads/main".to_string()),
                None,
                None,
            )]),
            failure_details: None,
            failure_stage: None,
            resume: Some(ResultResume {
                run_id: "prev-run".to_string(),
            }),
            target: Some(ResultTarget {
                name: "debian".to_string(),
                details: serde_json::json!({"dist": "unstable"}),
            }),
            worker_name: None,
            vcs_type: None,
            target_branch_url: None,
            context: None,
            builder_result: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        let roundtripped: JanitorResult = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped.resume.as_ref().unwrap().run_id, "prev-run");
        assert_eq!(roundtripped.target.as_ref().unwrap().name, "debian");
        assert!(roundtripped.remotes.is_some());
        assert!(roundtripped.branches.is_some());
    }

    #[test]
    fn test_find_changes_error_display() {
        let err = FindChangesError::NoChangesFile(std::path::PathBuf::from("/tmp/output"));
        assert_eq!(err.to_string(), "No changes file found in /tmp/output");

        let err = FindChangesError::MissingChangesFileFields("Source");
        assert_eq!(err.to_string(), "Missing field Source in changes files");
    }

    // The worker emits `stage` as a single string and either omits
    // `refreshed` entirely or sends `null`. Earlier the runner declared
    // `stage: Option<Vec<String>>` and required `refreshed: bool`,
    // which made every /finish upload silently fail to parse.
    #[test]
    fn test_worker_result_accepts_string_stage() {
        let payload = serde_json::json!({
            "code": "build-failed",
            "description": "boom",
            "stage": "build",
        });
        let wr: WorkerResult = serde_json::from_value(payload).unwrap();
        assert_eq!(wr.stage.as_deref(), Some("build"));
    }

    #[test]
    fn test_worker_result_accepts_missing_refreshed() {
        let payload = serde_json::json!({
            "code": "success",
        });
        let wr: WorkerResult = serde_json::from_value(payload).unwrap();
        assert_eq!(wr.refreshed, None);
    }

    #[test]
    fn test_worker_result_accepts_null_refreshed() {
        let payload = serde_json::json!({
            "code": "success",
            "refreshed": null,
        });
        let wr: WorkerResult = serde_json::from_value(payload).unwrap();
        assert_eq!(wr.refreshed, None);
    }

    #[test]
    fn test_worker_result_accepts_explicit_refreshed() {
        let payload = serde_json::json!({
            "code": "success",
            "refreshed": true,
        });
        let wr: WorkerResult = serde_json::from_value(payload).unwrap();
        assert_eq!(wr.refreshed, Some(true));
    }
}
