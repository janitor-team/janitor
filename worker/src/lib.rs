// BEHAVIORAL COMPATIBILITY ANALYSIS WITH PYTHON
//
// Critical Notes: The Worker service appears to be implemented entirely in Rust
// using PyO3 for breezyshim integration. There is no equivalent Python implementation
// to compare against - the Python codebase only contains worker_creds.py for
// authentication helpers. The Worker is a pure Rust service that integrates with
// the Python breezy library via PyO3.
//
// Key Compatibility Considerations:
// 1. PyO3 Integration: Uses breezyshim for VCS operations
// 2. API Compatibility: Interfaces with runner/site services via HTTP APIs
// 3. Authentication: Uses worker credentials for service authentication
// 4. Metadata Format: Must match expected JSON structure for other services

use breezyshim::branch::Branch;
use breezyshim::controldir::ControlDirFormat;
use breezyshim::error::Error as BrzError;
use breezyshim::transport::Transport;
use breezyshim::tree::{MutableTree, WorkingTree};
use breezyshim::workingtree::PyWorkingTree;
pub use breezyshim::RevisionId;
use reqwest::header::{HeaderMap, HeaderValue};
use silver_platter::CodemodResult;
use std::collections::HashMap;

use dns_lookup::{getaddrinfo, LookupErrorKind};

use janitor::api::worker::{Metadata, TargetDetails, WorkerFailure};
use janitor::vcs::VcsType;
use janitor_service::metrics::push_to_gateway;
use janitor_types::{Campaign, CodebaseName, RunId};

use url::Url;

pub const DEFAULT_USER_AGENT: &str = concat!("janitor/worker (", env!("CARGO_PKG_VERSION"), ")");

pub mod api_types;
pub mod client;

#[cfg(feature = "debian")]
pub mod debian;

pub mod generic;

pub mod vcs;

pub mod web;

mod tee;

use janitor::shared_web::{BaseAppState, health::{BasicHealthChecker, HealthCheckHandler}};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub output_directory: Option<std::path::PathBuf>,
    pub assignment: Option<janitor::api::worker::Assignment>,
    pub metadata: Option<Metadata>,
    pub health_checker: Option<Arc<BasicHealthChecker>>,
    /// Wall-clock time when the worker process started. Used to
    /// compute `uptime_seconds` in /status — the previous code
    /// reported `SystemTime::now().duration_since(UNIX_EPOCH)`,
    /// which is roughly 1.7 billion seconds and obviously wrong.
    pub started_at: std::time::Instant,
    /// Current stage in the run lifecycle, written by
    /// [`run_worker`] at each transition (`"setup"` → `"codemod"`
    /// → `"build"` → `"push"` → `"finish"`) and exposed via
    /// `WorkerStatusInfo::current_stage`. The active-run page
    /// reads this through the runner's backchannel-status proxy
    /// to pick which log file to auto-open. `None` between runs.
    pub current_stage: Option<String>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            output_directory: None,
            assignment: None,
            metadata: None,
            health_checker: None,
            started_at: std::time::Instant::now(),
            current_stage: None,
        }
    }
}

impl AppState {
    /// Create a new AppState with health checker
    pub fn with_health_checker(mut self) -> Self {
        self.health_checker = Some(Arc::new(
            BasicHealthChecker::with_info(
                "worker".to_string(),
                env!("CARGO_PKG_VERSION").to_string(),
            )
        ));
        self
    }
}

impl BaseAppState for AppState {
    fn service_name(&self) -> &str {
        "worker"
    }
    
    fn service_version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }
    
    fn health_checker(&self) -> Arc<dyn HealthCheckHandler> {
        self.health_checker
            .as_ref()
            .map(|hc| hc.clone() as Arc<dyn HealthCheckHandler>)
            .unwrap_or_else(|| {
                Arc::new(BasicHealthChecker::with_info(
                    "worker".to_string(),
                    env!("CARGO_PKG_VERSION").to_string(),
                ))
            })
    }
}

// Wrapper type to work around orphan rules
#[derive(Clone)]
pub struct WorkerAppState(pub Arc<std::sync::RwLock<AppState>>);

impl BaseAppState for WorkerAppState {
    fn service_name(&self) -> &str {
        "worker"
    }
    
    fn service_version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }
    
    fn health_checker(&self) -> Arc<dyn HealthCheckHandler> {
        match self.0.read() {
            Ok(state) => state.health_checker(),
            Err(_) => {
                // If lock is poisoned, return a basic health checker
                Arc::new(BasicHealthChecker::with_info(
                    "worker".to_string(),
                    env!("CARGO_PKG_VERSION").to_string(),
                ))
            }
        }
    }
}

pub fn is_gce_instance() -> std::io::Result<bool> {
    match getaddrinfo(Some("metadata.google.internal"), None, None) {
        Ok(_) => Ok(true),
        Err(e) => match e.kind() {
            LookupErrorKind::NoName | LookupErrorKind::NoData => Ok(false),
            LookupErrorKind::System => Err(e.into()),
            _ => {
                log::warn!("Unable to detect whether we are on GCE: {}", e);
                Ok(false)
            }
        },
    }
}

pub async fn gce_external_ip() -> Result<Option<String>, reqwest::Error> {
    let url = "http://metadata.google.internal/computeMetadata/v1/instance/network-interfaces/0/access-configs/0/external-ip";
    let mut headers = HeaderMap::new();
    headers.insert("Metadata-Flavor", HeaderValue::from_static("Google"));

    let client = reqwest::Client::new();
    let resp = client.get(url).headers(headers).send().await?;

    match resp.status().as_u16() {
        200 => Ok(Some(resp.text().await?)),
        404 => Ok(None),
        _ => panic!("Unexpected response status: {}", resp.status()),
    }
}

#[derive(Debug)]
pub enum DpkgArchitectureError {
    MissingCommand,
    Other(String),
}

impl std::fmt::Display for DpkgArchitectureError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            DpkgArchitectureError::MissingCommand => write!(
                f,
                "dpkg-architecture command not found; is dpkg-dev installed?"
            ),
            DpkgArchitectureError::Other(ref e) => write!(f, "{}", e),
        }
    }
}

/// Get the architecture dpkg is building for
pub fn get_build_arch() -> Result<String, DpkgArchitectureError> {
    let output = std::process::Command::new("dpkg-architecture")
        .arg("-qDEB_BUILD_ARCH")
        .output();

    let output = match output {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(DpkgArchitectureError::MissingCommand);
        }
        Err(e) => {
            return Err(DpkgArchitectureError::Other(format!(
                "Error running dpkg-architecture: {}",
                e
            )));
        }
    };

    String::from_utf8(output.stdout)
        .map_err(|e| DpkgArchitectureError::Other(format!("Invalid UTF-8 output: {}", e)))
        .map(|s| s.trim().to_owned())
}

pub fn convert_codemod_script_failed(i: i32, command: &str) -> WorkerFailure {
    match i {
        127 => WorkerFailure::new(
            "command-not-found",
            format!("Command {} not found", command),
            ["codemod"],
        ),
        137 => WorkerFailure::new("killed", "Process was killed (by OOM killer?)", ["codemod"]),
        _ => WorkerFailure::new(
            "command-failed",
            format!("Script {} failed to run with code {}", command, i),
            ["codemod"],
        ),
    }
}

fn split_codemod_command(command: &str) -> Result<Vec<String>, WorkerFailure> {
    shlex::split(command).ok_or_else(|| WorkerFailure {
        code: "invalid-command-syntax".to_string(),
        description: format!("Invalid command syntax: {}", command),
        details: None,
        stage: vec!["codemod".to_string()],
        transient: Some(false),
    })
}

type BuildConfig = serde_json::Value;
type ValidateConfig = serde_json::Value;

/// A build target
pub trait Target {
    fn name(&self) -> String;

    fn build(
        &self,
        local_tree: &breezyshim::workingtree::GenericWorkingTree,
        subpath: &std::path::Path,
        output_directory: &std::path::Path,
        config: &BuildConfig,
    ) -> Result<serde_json::Value, WorkerFailure>;

    fn validate(
        &self,
        local_tree: &breezyshim::workingtree::GenericWorkingTree,
        subpath: &std::path::Path,
        config: &ValidateConfig,
    ) -> Result<(), WorkerFailure>;

    fn make_changes(
        &self,
        local_tree: &breezyshim::workingtree::GenericWorkingTree,
        subpath: &std::path::Path,
        argv: &[&str],
        log_directory: &std::path::Path,
        resume_metadata: Option<&serde_json::Value>,
    ) -> Result<Box<dyn silver_platter::CodemodResult>, WorkerFailure>;
}

/// Decide what changer_result to feed the build stage given the
/// codemod outcome and the workspace state. Pulled out of `run_worker`
/// so the dispatch is unit-testable without standing up a real
/// `Workspace`.
///
/// Inputs come straight off `silver_platter::Workspace` accessors
/// (`any_branch_changes`, `changes_since_main`, `resume_branch`,
/// `main_branch`); `local_revision` is the local tree's last revision
/// and `main_revision` is `Some(rev)` only when the workspace has a
/// main branch.
fn decide_changer_result(
    codemod_result: Result<Box<dyn CodemodResult>, WorkerFailure>,
    force_build: bool,
    any_branch_changes: bool,
    changes_since_main: bool,
    has_resume_branch: bool,
    main_revision: Option<RevisionId>,
    local_revision: RevisionId,
) -> Result<Box<dyn CodemodResult>, WorkerFailure> {
    let no_change_build = || -> Box<dyn CodemodResult> {
        Box::new(silver_platter::codemod::CommandResult {
            description: Some("No change build".to_owned()),
            context: None,
            tags: Vec::new(),
            value: Some(0),
            old_revision: local_revision.clone(),
            new_revision: local_revision.clone(),
            title: None,
            commit_message: None,
            serialized_context: None,
            target_branch_url: None,
        })
    };
    match codemod_result {
        Ok(r) => {
            if !any_branch_changes {
                if force_build {
                    // force_build → build even without VCS changes
                    // (mirrors the Err(nothing-to-do) branch below).
                    Ok(no_change_build())
                } else {
                    Err(WorkerFailure {
                        code: "nothing-to-do".to_owned(),
                        description: "Nothing to do.".to_owned(),
                        stage: vec!["codemod".to_owned()],
                        transient: Some(false),
                        details: None,
                    })
                }
            } else {
                Ok(r)
            }
        }
        Err(e) if e.code.as_str() == "nothing-to-do" => {
            if changes_since_main {
                // This should only ever happen if we were resuming
                let main_revision = main_revision.ok_or_else(|| WorkerFailure {
                    code: "main-branch-missing".to_owned(),
                    description: "Main branch is missing when checking for changes".to_string(),
                    stage: vec!["codemod".to_owned()],
                    transient: Some(false),
                    details: None,
                })?;
                if !has_resume_branch {
                    return Err(WorkerFailure {
                        code: "unexpected-existing-changes".to_owned(),
                        description: format!(
                            "Found existing changes despite not having resumed. Mainline: {}, local: {}",
                            main_revision, local_revision
                        ),
                        stage: vec!["codemod".to_owned()],
                        transient: Some(false),
                        details: None,
                    });
                }
                Err(WorkerFailure {
                    code: "nothing-new-to-do".to_owned(),
                    description: e.description.clone(),
                    stage: vec!["codemod".to_owned()],
                    transient: Some(false),
                    details: None,
                })
            } else if force_build {
                Ok(no_change_build())
            } else {
                Err(e)
            }
        }
        Err(e) => Err(e),
    }
}

#[cfg(feature = "cli")]
pub fn py_to_serde_json(obj: &pyo3::Bound<pyo3::PyAny>) -> pyo3::PyResult<serde_json::Value> {
    use pyo3::prelude::*;
    if obj.is_none() {
        Ok(serde_json::Value::Null)
    } else if let Ok(b) = obj.cast::<pyo3::types::PyBool>() {
        Ok(serde_json::Value::Bool(b.is_true()))
    } else if let Ok(i) = obj.cast::<pyo3::types::PyInt>() {
        // Try to extract as i64 first, then as u64, then as f64
        if let Ok(val) = i.extract::<i64>() {
            Ok(serde_json::Value::Number(serde_json::Number::from(val)))
        } else if let Ok(val) = i.extract::<u64>() {
            Ok(serde_json::Value::Number(serde_json::Number::from(val)))
        } else if let Ok(val) = i.extract::<f64>() {
            Ok(serde_json::Value::Number(
                serde_json::Number::from_f64(val).unwrap_or_else(|| serde_json::Number::from(0)),
            ))
        } else {
            Err(pyo3::exceptions::PyTypeError::new_err(
                ("integer too large",),
            ))
        }
    } else if let Ok(f) = obj.cast::<pyo3::types::PyFloat>() {
        let float_val = f.value();
        if let Some(num) = serde_json::Number::from_f64(float_val) {
            Ok(serde_json::Value::Number(num))
        } else {
            // Handle special float values (NaN, Infinity, -Infinity)
            Err(pyo3::exceptions::PyValueError::new_err(
                format!("Cannot convert float value {} to JSON", float_val),
            ))
        }
    } else if let Ok(s) = obj.cast::<pyo3::types::PyString>() {
        Ok(serde_json::Value::String(s.to_string_lossy().to_string()))
    } else if let Ok(l) = obj.cast::<pyo3::types::PyList>() {
        Ok(serde_json::Value::Array(
            l.iter()
                .map(|x| py_to_serde_json(&x))
                .collect::<PyResult<Vec<_>>>()?,
        ))
    } else if let Ok(d) = obj.cast::<pyo3::types::PyDict>() {
        let mut ret = serde_json::Map::new();
        for (k, v) in d.iter() {
            let k = k.extract::<String>()?;
            let v = py_to_serde_json(&v)?;
            ret.insert(k, v);
        }
        Ok(serde_json::Value::Object(ret))
    } else {
        Err(pyo3::exceptions::PyTypeError::new_err(
            ("unsupported type",),
        ))
    }
}

#[cfg(feature = "cli")]
pub fn serde_json_to_py(value: &serde_json::Value) -> pyo3::Py<pyo3::PyAny> {
    use pyo3::prelude::*;
    use pyo3::IntoPyObject;
    Python::attach(|py| match value {
        serde_json::Value::Null => py.None(),
        serde_json::Value::Bool(b) => pyo3::types::PyBool::new(py, *b).to_owned().into_any().unbind(),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into_pyobject(py).unwrap().into_any().unbind()
            } else if let Some(u) = n.as_u64() {
                u.into_pyobject(py).unwrap().into_any().unbind()
            } else if let Some(f) = n.as_f64() {
                pyo3::types::PyFloat::new(py, f).into_any().unbind()
            } else {
                py.None()
            }
        }
        serde_json::Value::String(s) => {
            pyo3::types::PyString::new(py, s.as_str()).into_any().unbind()
        }
        serde_json::Value::Array(a) => {
            pyo3::types::PyList::new(
                py,
                a.iter().map(serde_json_to_py).collect::<Vec<_>>(),
            )
            .unwrap()
            .into_any()
            .unbind()
        }
        serde_json::Value::Object(o) => {
            let ret = pyo3::types::PyDict::new(py);
            for (k, v) in o.into_iter() {
                if let Err(e) = ret.set_item(k, serde_json_to_py(v)) {
                    log::warn!("Failed to set dict item {}: {}", k, e);
                }
            }
            ret.into_any().unbind()
        }
    })
}

pub fn run_worker(
    codebase: &CodebaseName,
    campaign: &Campaign,
    main_branch_url: Option<&url::Url>,
    run_id: &RunId,
    build_config: &serde_json::Value,
    env: HashMap<String, String>,
    command: &str,
    output_directory: &std::path::Path,
    metadata: &mut Metadata,
    target_repo_url: &Url,
    vendor: &str,
    target: &str,
    vcs_type: Option<VcsType>,
    subpath: &std::path::Path,
    resume_branch_url: Option<&Url>,
    cached_branch_url: Option<&Url>,
    mut resume_codemod_result: Option<&serde_json::Value>,
    resume_branches: Option<Vec<(&str, &str, Option<RevisionId>, Option<RevisionId>)>>,
    possible_transports: &mut Option<Vec<Transport>>,
    force_build: Option<bool>,
    tee: Option<bool>,
    additional_colocated_branches: Option<HashMap<String, String>>,
    skip_setup_validation: Option<bool>,
    default_empty: Option<bool>,
    // Optional handle on the worker's shared AppState so this
    // function can update `current_stage` at each transition
    // ("setup" → "codemod" → "build" → "push" → "finish"). The
    // stage is exposed via /status and the active-run page reads
    // it to decide which log to auto-open. None for in-process
    // callers / tests that don't run a backchannel.
    app_state: Option<std::sync::Arc<std::sync::RwLock<AppState>>>,
) -> Result<(), WorkerFailure> {
    // Helper closure: update `current_stage` if we have a handle
    // on AppState. Failures (poisoned lock) are logged and
    // swallowed — stage tracking is observability, not
    // correctness, so a failed update mustn't abort the run.
    let set_stage = |stage: &str| {
        if let Some(ref s) = app_state {
            match s.write() {
                Ok(mut guard) => guard.current_stage = Some(stage.to_string()),
                Err(_) => log::warn!("AppState lock poisoned; can't record stage {}", stage),
            }
        }
    };
    set_stage("setup");
    let force_build = force_build.unwrap_or(false);
    let tee = tee.unwrap_or(false);
    let skip_setup_validation = skip_setup_validation.unwrap_or(false);
    let worker_log_path = output_directory.join("worker.log");
    log::debug!("Writing worker log to {}", worker_log_path.display());
    let copy_output = crate::tee::CopyOutput::new(&worker_log_path, tee).map_err(|e| {
        WorkerFailure::new(
            "worker-log-creation-failed",
            format!(
                "Failed to create worker log file at {}: {}",
                worker_log_path.display(),
                e
            ),
            ["setup"],
        )
    })?;
    let argv = split_codemod_command(command)?;
    let command = argv.iter().map(|s| s.as_str()).collect::<Vec<_>>();
    metadata.command = Some(command.iter().map(|s| s.to_string()).collect());
    metadata.codebase = Some(codebase.clone());

    let build_target: Box<dyn Target> = match target {
        #[cfg(feature = "debian")]
        "debian" => Box::new(crate::debian::DebianTarget::new(env)),
        "generic" => Box::new(crate::generic::GenericTarget::new(env)),
        n => {
            return Err(WorkerFailure::new(
                "target-unsupported",
                format!("The target {} is not supported", n),
                ["setup"],
            )
            .with_transient(false));
        }
    };

    let main_branch: Option<breezyshim::branch::GenericBranch>;
    let empty_format: Option<ControlDirFormat>;

    if let Some(main_branch_url) = main_branch_url {
        log::info!("Opening branch at {}", main_branch_url);
        main_branch = match janitor::vcs::open_branch_ext(
            main_branch_url,
            possible_transports.as_mut(),
            None,
        ) {
            Ok(b) => Some(b),
            Err(janitor::vcs::BranchOpenFailure {
                ref code,
                description,
                retry_after,
            }) => {
                // Surface the underlying breezy/transport error to
                // worker.log so the per-run page shows *why* the
                // open failed, not just the WorkerFailure code.
                log::error!(
                    "Failed to open branch {}: {} ({})",
                    main_branch_url,
                    description,
                    code
                );
                return Err(WorkerFailure::new(code.clone(), description, ["setup"])
                    .with_transient(code.contains("temporarily"))
                    .with_details(serde_json::json!({
                        "url": main_branch_url,
                        "retry_after": retry_after.map(|r| r.num_seconds()),
                    })));
            }
        };
        let branch_ref = main_branch.as_ref().ok_or_else(|| WorkerFailure {
            code: "main-branch-missing".to_owned(),
            description: "Main branch reference is missing".to_string(),
            details: None,
            transient: None,
            stage: vec!["setup".to_owned()],
        })?;
        metadata.branch_url = Some(branch_ref.get_user_url());
        metadata.vcs_type = Some(janitor::vcs::get_branch_vcs_type(branch_ref).map_err(|e| {
            WorkerFailure::new(
                "vcs-type-unsupported",
                format!("Unable to determine VCS type: {}", e),
                ["setup"],
            )
            .with_transient(false)
        })?);
        metadata.subpath = Some(subpath.to_string_lossy().to_string());
        empty_format = None;
    } else {
        let vcs_type = vcs_type.expect("vcs_type is required when main_branch_url is not given");
        main_branch = None;
        metadata.branch_url = None;
        metadata.vcs_type = Some(vcs_type);
        metadata.subpath = Some("".to_string());
        empty_format = if let Some(f) =
            breezyshim::controldir::FORMAT_REGISTRY.make_controldir(&vcs_type.to_string())
        {
            Some(f)
        } else {
            return Err(WorkerFailure::new(
                "vcs-type-unsupported",
                format!("Unable to find format for vcs type {}", vcs_type),
                ["setup"],
            )
            .with_transient(false)
            .with_details(serde_json::json!({"vcs_type": vcs_type})));
        };
    }

    let cached_branch = if let Some(cached_branch_url) = cached_branch_url {
        let probers =
            silver_platter::probers::select_probers(vcs_type.map(|v| v.to_string()).as_deref());
        match silver_platter::vcs::open_branch(
            cached_branch_url,
            possible_transports.as_mut(),
            Some(
                probers
                    .iter()
                    .map(|p| p.as_ref())
                    .collect::<Vec<_>>()
                    .as_slice(),
            ),
            None,
        ) {
            Ok(b) => {
                let display = silver_platter::vcs::full_branch_url(&b);
                log::info!("Using cached branch {}", display);
                Some(b)
            }
            Err(silver_platter::vcs::BranchOpenError::Missing { url, description }) => {
                log::info!("Cached branch URL {} missing: {}", url, description);
                None
            }
            Err(
                silver_platter::vcs::BranchOpenError::Unavailable { url, description }
                | silver_platter::vcs::BranchOpenError::TemporarilyUnavailable { url, description },
            ) => {
                log::info!("Cached branch URL {} unavailable: {}", url, description);
                None
            }
            Err(
                silver_platter::vcs::BranchOpenError::Unsupported { .. }
                | silver_platter::vcs::BranchOpenError::Other(..)
                | silver_platter::vcs::BranchOpenError::RateLimited { .. },
            ) => {
                log::info!("Error accessing cached branch URL {}", cached_branch_url);
                None
            }
        }
    } else {
        None
    };

    let resume_branch = if let Some(resume_branch_url) = resume_branch_url {
        log::info!("Using resume branch: {}", resume_branch_url);
        let probers =
            silver_platter::probers::select_probers(vcs_type.map(|v| v.to_string()).as_deref());
        match silver_platter::vcs::open_branch(
            resume_branch_url,
            possible_transports.as_mut(),
            Some(
                probers
                    .iter()
                    .map(|p| p.as_ref())
                    .collect::<Vec<_>>()
                    .as_slice(),
            ),
            None,
        ) {
            Err(silver_platter::vcs::BranchOpenError::TemporarilyUnavailable {
                url,
                description,
            }) => {
                log::info!(
                    "Resume branch URL {} temporarily unavailable: {}",
                    url,
                    description
                );
                return Err(WorkerFailure::new(
                    "worker-resume-branch-temporarily-unavailable",
                    description,
                    ["setup"],
                )
                .with_transient(true)
                .with_details(serde_json::json!({
                    "url": url,
                })));
            }
            Err(silver_platter::vcs::BranchOpenError::RateLimited {
                url,
                description,
                retry_after,
            }) => {
                log::info!("Resume branch URL {} rate limited: {}", url, description);
                return Err(WorkerFailure::new(
                    "worker-resume-branch-rate-limited",
                    description,
                    ["setup"],
                )
                .with_transient(true)
                .with_details(serde_json::json!({
                    "url": url,
                    "retry_after": retry_after,
                })));
            }
            Err(silver_platter::vcs::BranchOpenError::Unavailable { url, description }) => {
                log::info!("Resume branch URL {} unavailable: {}", url, description);
                return Err(WorkerFailure::new(
                    "worker-resume-branch-unavailable",
                    description,
                    ["setup"],
                )
                .with_transient(false)
                .with_details(serde_json::json!({
                    "url": url
                })));
            }
            Err(silver_platter::vcs::BranchOpenError::Missing { url, description }) => {
                log::info!("Resume branch URL {} missing: {}", url, description);
                return Err(WorkerFailure::new(
                    "worker-resume-branch-missing",
                    description,
                    ["setup"],
                )
                .with_transient(false)
                .with_details(serde_json::json!({
                    "url": url
                })));
            }
            Err(silver_platter::vcs::BranchOpenError::Unsupported { .. }) => {
                return Err(WorkerFailure::new(
                    "worker-resume-branch-unsupported",
                    "Unsupported resume branch URL",
                    ["setup"],
                )
                .with_transient(false));
            }
            Err(silver_platter::vcs::BranchOpenError::Other(..)) => {
                return Err(WorkerFailure::new(
                    "worker-resume-branch-error",
                    "Error opening resume branch",
                    ["setup"],
                )
                .with_transient(false));
            }
            Ok(b) => Some(b),
        }
    } else {
        None
    };

    let mut roles: HashMap<String, String> =
        additional_colocated_branches.clone().unwrap_or_default();

    if let Some(main_branch) = main_branch.as_ref() {
        match main_branch.name() {
            Some(name) => {
                roles.insert(name, "main".to_string());
            }
            None => {
                log::warn!("Branch name is None, using empty string");
                roles.insert("".to_string(), "main".to_string());
            }
        }
    } else {
        roles.insert("".to_string(), "main".to_string());
    }

    let directory_name = codebase.to_string();

    let mut ws_builder = silver_platter::workspace::Workspace::builder();
    if let Some(main_branch) = main_branch {
        ws_builder = ws_builder.main_branch(main_branch);
    }

    if let Some(resume_branch) = resume_branch {
        ws_builder = ws_builder.resume_branch(resume_branch);
    }

    if let Some(cached_branch) = cached_branch {
        ws_builder = ws_builder.cached_branch(cached_branch);
    }

    let ws_path = output_directory.join(directory_name);
    log::debug!("Workspace path: {}", ws_path.display());
    ws_builder = ws_builder.path(ws_path);
    if let Some(additional_colocated_branches) = additional_colocated_branches.as_ref() {
        ws_builder =
            ws_builder.additional_colocated_branches(additional_colocated_branches.clone());
    }
    if let Some(empty_format) = empty_format {
        ws_builder = ws_builder.format(&empty_format);
    }
    if let Some(resume_branches) = resume_branches.as_ref() {
        ws_builder = ws_builder.resume_branch_additional_colocated_branches(
            resume_branches
                .iter()
                .filter_map(|(role, name, _, _)| {
                    if role != &"main" {
                        Some((name.to_string(), role.to_string()))
                    } else {
                        None
                    }
                })
                .collect(),
        );
    }

    let ws = match ws_builder.build() {
        Ok(ws) => ws,
        Err(silver_platter::workspace::Error::BrzError(e @ BrzError::IncompleteRead(..))) => {
            return Err(
                WorkerFailure::new("incomplete-read", e.to_string(), ["setup", "clone"])
                    .with_transient(true),
            );
        }
        Err(silver_platter::workspace::Error::BrzError(BrzError::MalformedTransform(msg))) => {
            return Err(WorkerFailure::new(
                "malformed-transform",
                format!("Malformed transform: {:?}", msg),
                ["setup", "clone"],
            )
            .with_transient(false));
        }
        Err(silver_platter::workspace::Error::BrzError(
            e @ BrzError::TransformRenameFailed(..),
        )) => {
            return Err(WorkerFailure::new(
                "transform-rename-failed",
                e.to_string(),
                ["setup", "clone"],
            )
            .with_transient(false));
        }
        Err(silver_platter::workspace::Error::BrzError(e @ BrzError::ImmortalLimbo(..))) => {
            return Err(WorkerFailure::new(
                "transform-immortal-limbo",
                e.to_string(),
                ["setup", "clone"],
            )
            .with_transient(false));
        }
        Err(silver_platter::workspace::Error::BrzError(BrzError::UnexpectedHttpStatus {
            url,
            code,
            extra,
            headers,
        })) => {
            if code == 502 {
                return Err(WorkerFailure::new(
                    "bad-gateway",
                    if let Some(ref extra) = extra {
                        format!("Bad gateway from {}: {}", url, extra)
                    } else {
                        format!("Bad gateway from {}", url)
                    },
                    ["setup", "clone"],
                )
                .with_transient(true));
            } else {
                return Err(WorkerFailure::new(
                    format!("http-{}", code),
                    if let Some(ref extra) = extra {
                        format!(
                            "Unexpected HTTP status code {} from {}: {}",
                            code, url, extra
                        )
                    } else {
                        format!("Unexpected HTTP status code {} from {}", code, url)
                    },
                    ["setup", "clone"],
                )
                .with_details(serde_json::json!({
                    "status-code": code,
                    "url": url,
                    "extra": extra,
                    "headers": headers,
                })));
            }
        }
        Err(silver_platter::workspace::Error::BrzError(BrzError::TransportError(msg))) => {
            if msg.contains("No space left on device") {
                return Err(
                    WorkerFailure::new("no-space-on-device", msg, ["setup", "clone"])
                        .with_transient(true),
                );
            }
            if msg.contains("Too many open files") {
                return Err(
                    WorkerFailure::new("too-many-open-files", msg, ["setup", "clone"])
                        .with_transient(true),
                );
            }
            if msg.contains("Temporary failure in name resolution") {
                return Err(WorkerFailure::new(
                    "temporary-transport-error",
                    msg,
                    ["setup", "clone"],
                )
                .with_transient(true));
            }
            return Err(
                WorkerFailure::new("transport-error", msg, ["setup", "clone"])
                    .with_transient(false),
            );
        }
        Err(silver_platter::workspace::Error::BrzError(BrzError::RemoteGitError(ref msg))) => {
            return Err(
                WorkerFailure::new("git-error", msg.clone(), ["setup", "clone"])
                    .with_transient(false),
            );
        }
        Err(silver_platter::workspace::Error::BrzError(e @ BrzError::Timeout)) => {
            return Err(
                WorkerFailure::new("timeout", e.to_string(), ["setup", "clone"])
                    .with_transient(true),
            );
        }
        Err(silver_platter::workspace::Error::BrzError(BrzError::MissingNestedTree(ref msg))) => {
            return Err(WorkerFailure::new(
                "requires-nested-tree-support",
                msg.display().to_string(),
                ["setup", "clone"],
            )
            .with_transient(false));
        }
        Err(e) => {
            return Err(
                WorkerFailure::new("unexpected-error", e.to_string(), ["setup", "clone"])
                    .with_transient(false),
            );
        }
    };

    log::info!("Workspace ready - starting.");
    set_stage("codemod");

    let has_changes = ws.local_tree().has_changes().map_err(|e| {
        WorkerFailure::new(
            "tree-check-failed",
            format!("Failed to check for changes in tree: {}", e),
            ["setup"],
        )
        .with_transient(false)
    })?;
    if has_changes {
        return Err(WorkerFailure::new(
            "unexpected-changes-in-tree",
            "The working tree has unexpected changes after initial clone",
            ["setup", "clone"],
        )
        .with_transient(false));
    }

    if !skip_setup_validation {
        build_target.validate(ws.local_tree(), subpath, build_config)?;
    }

    metadata.revision = Some(if let Some(main_branch) = ws.main_branch() {
        main_branch.last_revision()
    } else {
        breezyshim::RevisionId::null()
    });
    metadata.main_branch_revision = metadata.revision.clone();

    metadata.codemod = Some(serde_json::Value::Null);

    if ws.resume_branch().is_none() {
        // If the resume branch was discarded for whatever reason, then we
        // don't need to pass in the codemod result.
        resume_codemod_result = None;
    }

    if let Some(main_branch) = ws.main_branch() {
        metadata.add_remote("origin".to_string(), main_branch.get_user_url());
    }

    let r = build_target.make_changes(
        ws.local_tree(),
        subpath,
        &command,
        output_directory,
        resume_codemod_result,
    );
    metadata.revision = Some(ws.local_tree().branch().last_revision());

    let local_revision = ws.local_tree().last_revision().unwrap_or_else(|e| {
        log::warn!("Failed to get last revision: {:?}", e);
        RevisionId::null()
    });
    let main_revision = ws.main_branch().map(|b| b.last_revision());
    let changer_result = decide_changer_result(
        r,
        force_build,
        ws.any_branch_changes(),
        ws.changes_since_main(),
        ws.resume_branch().is_some(),
        main_revision,
        local_revision,
    )?;

    metadata.refreshed = Some(ws.refreshed());
    metadata.value = changer_result.value().map(|i| i as u64);
    metadata.codemod = Some(changer_result.context());
    metadata.target_branch_url = changer_result.target_branch_url();
    metadata.description = changer_result.description();

    let mut result_branches = vec![];
    for (name, base_revision, revision) in ws.changed_branches() {
        let role = match roles.get(&name) {
            Some(role) => role,
            None => {
                log::warn!("Unable to find role for branch {}", name);
                continue;
            }
        };
        if base_revision == revision {
            continue;
        }
        result_branches.push((role.clone(), name, base_revision, revision));
    }

    let result_branch_roles = result_branches
        .iter()
        .map(|(role, _, _, _)| role)
        .collect::<Vec<_>>();
    assert_eq!(
        result_branch_roles.len(),
        result_branch_roles
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        "Duplicate result branches: {:?}",
        result_branches
    );

    let actual_vcs_type =
        janitor::vcs::get_branch_vcs_type(&ws.local_tree().branch()).map_err(|e| {
            WorkerFailure::new(
                "vcs-type-unsupported",
                format!("Unable to determine VCS type: {}", e),
                ["result-push"],
            )
            .with_transient(false)
        })?;

    let vcs_type = match vcs_type {
        None => actual_vcs_type,
        Some(expected) if expected == actual_vcs_type => expected,
        Some(expected) => {
            return Err(WorkerFailure::new(
                "vcs-type-mismatch",
                format!("Expected VCS {}, got {}", expected, actual_vcs_type),
                ["result-push"],
            )
            .with_transient(false));
        }
    };

    crate::vcs::import_branches(
        vcs_type,
        target_repo_url,
        &ws.local_tree().branch(),
        campaign,
        run_id,
        &result_branches,
        changer_result.tags(),
        false,
        None,
        possible_transports,
    )
    .map_err(|e| push_error_to_worker_failure(e, vec!["result-push".to_string()]))?;

    for (f, n, br, r) in result_branches.iter() {
        metadata.add_branch(f.to_string(), n.to_string(), br.clone(), r.clone());
    }
    for (n, r) in changer_result.tags().iter() {
        metadata.add_tag(n.to_string(), r.clone());
    }

    let should_build = if force_build {
        true
    } else {
        result_branches
            .iter()
            .any(|(role, _name, _br, _r)| role == "main")
    };

    let target_details = if should_build {
        set_stage("build");
        build_target.build(ws.local_tree(), subpath, output_directory, build_config)?
    } else {
        serde_json::Value::Null
    };

    metadata.target = Some(TargetDetails {
        name: build_target.name(),
        details: target_details,
    });

    let vendor = vendor.to_string();
    let cache = match (cached_branch_url, ws.main_branch()) {
        (Some(url), Some(main_branch)) => Some(crate::vcs::CacheBranch {
            url: url.clone(),
            revision: main_branch.last_revision(),
            tag_selector: Box::new(move |tag_name: String| -> bool {
                tag_name.starts_with(&format!("{}/", vendor)) || tag_name.starts_with("upstream/")
            }),
        }),
        _ => None,
    };

    log::info!("Pushing result branch to {}", target_repo_url);

    crate::vcs::import_branches(
        vcs_type,
        target_repo_url,
        &ws.local_tree().branch(),
        campaign,
        run_id,
        &result_branches,
        changer_result.tags(),
        true,
        cache,
        possible_transports,
    )
    .map_err(|e| push_error_to_worker_failure(e, vec!["result-sym".to_owned()]))?;

    std::mem::drop(copy_output);

    log::info!("All done.");
    Ok(())
}

fn derive_branch_name(url: &url::Url) -> String {
    let url_str = breezyshim::urlutils::split_segment_parameters(url)
        .0
        .to_string()
        .trim_end_matches('/')
        .to_string();

    url_str
        .rsplit_once('/')
        .map(|(_, name)| name.to_string())
        .unwrap_or_else(|| url_str)
}

pub enum SingleItemError {
    AssignmentFailure(String),
    EmptyQueue,
    ResultUploadFailure(String),
    WorkerPanicked(String),
}

impl From<client::AssignmentError> for SingleItemError {
    fn from(e: client::AssignmentError) -> Self {
        match e {
            client::AssignmentError::EmptyQueue => SingleItemError::EmptyQueue,
            client::AssignmentError::Failure(e) => SingleItemError::AssignmentFailure(e),
        }
    }
}

impl From<client::UploadFailure> for SingleItemError {
    fn from(e: client::UploadFailure) -> Self {
        SingleItemError::ResultUploadFailure(e.to_string())
    }
}

pub async fn process_single_item(
    client: &crate::client::Client,
    my_url: Option<&Url>,
    node_name: &str,
    jenkins_build_url: Option<&Url>,
    prometheus: Option<&Url>,
    codebase: Option<&CodebaseName>,
    campaign: Option<&Campaign>,
    tee: bool,
    output_directory_base: Option<&std::path::Path>,
    state: std::sync::Arc<std::sync::RwLock<AppState>>,
) -> Result<(), SingleItemError> {
    let assignment = client
        .get_assignment(my_url, node_name, jenkins_build_url, codebase, campaign)
        .await?;

    state
        .write()
        .map_err(|_| {
            SingleItemError::WorkerPanicked("Failed to acquire write lock on state".to_string())
        })?
        .assignment = Some(assignment.clone());

    log::debug!("Got back assignment: {:?}", &assignment);

    let force_build = assignment.force_build;
    let (resume_result, resume_branch_url, resume_branches) =
        if let Some(resume) = &assignment.resume {
            (
                Some(resume.result.clone()),
                Some(resume.branch_url.clone()),
                Some(resume.branches.clone()),
            )
        } else {
            (None, None, None)
        };
    let build_environment = assignment.build.environment.clone().unwrap_or_default();

    let start_time = chrono::Utc::now();

    let possible_transports = vec![];

    let mut env = assignment.env.clone();

    env.extend(build_environment.clone());

    log::debug!("Environment: {:?}", env);

    let vendor = build_environment
        .get("DEB_VENDOR")
        .map_or("debian", |x| x.as_str())
        .to_string();

    let output_directory = if let Some(output_directory_base) = output_directory_base {
        tempfile::TempDir::with_prefix_in("janitor-worker-", output_directory_base).map_err(
            |e| {
                SingleItemError::WorkerPanicked(format!(
                    "Failed to create temporary directory: {}",
                    e
                ))
            },
        )?
    } else {
        tempfile::TempDir::with_prefix("janitor-worker-").map_err(|e| {
            SingleItemError::WorkerPanicked(format!("Failed to create temporary directory: {}", e))
        })?
    };

    let assignment_ = assignment.clone();
    let state_clone = state.clone();

    let output_directory_ = output_directory.path().to_path_buf();

    // Publish the per-run output directory to AppState so the
    // worker's `/logs` and `/logs/{filename}` endpoints can serve
    // the live worker.log / codemod.log / build.log being written
    // by this run. Without this, /logs reports "Log directory not
    // created yet" the entire time a run is in flight, and the
    // runner's `GET /log/{run_id}/{filename}` proxy returns 404 to
    // anyone trying to tail logs from /cupboard/active-runs.
    if let Ok(mut s) = state.write() {
        s.output_directory = Some(output_directory_.clone());
    }

    let metadata = match tokio::task::spawn_blocking(move || {
        let mut metadata = Metadata {
            queue_id: Some(assignment.queue_id),
            start_time: Some(start_time),
            branch_url: assignment_.branch.url.clone(),
            vcs_type: Some(assignment.branch.vcs_type),
            ..Metadata::default()
        };

        // Update metadata in appstate while working for real-time monitoring
        {
            let mut app_state = match state_clone.write() {
                Ok(state) => state,
                Err(_) => {
                    log::error!("Failed to acquire write lock on state for metadata update");
                    return Err(SingleItemError::WorkerPanicked(
                        "Failed to acquire write lock on state".to_string(),
                    ));
                }
            };
            app_state.metadata = Some(metadata.clone());
        }

        let result = run_worker(
            &assignment_.codebase,
            &assignment_.campaign,
            assignment_.branch.url.as_ref(),
            &assignment_.id,
            &assignment_.build.config,
            env,
            &assignment_.codemod.command,
            output_directory_.as_path(),
            &mut metadata,
            &assignment_.target_repository.url,
            &vendor,
            &assignment_.build.target,
            Some(assignment_.branch.vcs_type),
            assignment_.branch.subpath.as_ref(),
            resume_branch_url.as_ref(),
            assignment_.branch.cached_url.as_ref(),
            resume_result.as_ref(),
            resume_branches.as_ref().map(|x| {
                {
                    x.iter().map(|(role, name, base, revision)| {
                        (role.as_str(), name.as_str(), base.clone(), revision.clone())
                    })
                }
                .collect::<Vec<_>>()
            }),
            &mut Some(possible_transports),
            Some(force_build),
            Some(tee),
            assignment.branch.additional_colocated_branches.map(|p| {
                p.into_iter()
                    .map(|k| (k.to_string(), k.to_string()))
                    .collect()
            }),
            Some(assignment_.skip_setup_validation),
            Some(assignment_.branch.default_empty),
            // Hand the AppState down so `run_worker` can record
            // its current stage; the active-run page polls this
            // through the runner's `/active-runs/{id}` proxy and
            // uses it to decide which log file to auto-open.
            Some(state_clone.clone()),
        );

        match result {
            Ok(_) => {
                metadata.code = None;
                if let Some(description) = metadata.description.as_ref() {
                    log::info!("{}", description);
                }
                log::info!("Worker finished successfully");
            }
            Err(
                ref e @ WorkerFailure {
                    ref code,
                    ref description,
                    ref stage,
                    ..
                },
            ) => {
                metadata.update(e);
                log::info!("Worker failed in {:?} ({}): {}", stage, code, description);
                // This is a failure for the worker, but returning 0 will cause
                // jenkins to mark the job having failed, which is not really
                // true.  We're happy if we get to successfully POST to /finish
            }
        }
        let finish_time = chrono::Utc::now();
        metadata.finish_time = Some(finish_time);
        log::info!("Elapsed time: {}", finish_time - start_time);

        // Update final metadata in appstate
        {
            if let Ok(mut app_state) = state_clone.write() {
                app_state.metadata = Some(metadata.clone());
            } else {
                log::error!("Failed to acquire write lock on state for final metadata update");
                // Don't fail the operation just because we couldn't update the state
            }
        }

        Ok::<_, SingleItemError>(metadata)
    })
    .await
    {
        Ok(result) => result?,
        Err(e) => {
            // The build task panicked. Report the run as a worker failure;
            // returning an error here instead skips the upload, and the
            // runner then keeps the run active until the watchdog gives up.
            let description = format!("Worker task panicked: {}", e);
            log::error!("{}", description);
            Metadata {
                queue_id: Some(assignment.queue_id),
                start_time: Some(start_time),
                finish_time: Some(chrono::Utc::now()),
                branch_url: assignment.branch.url.clone(),
                vcs_type: Some(assignment.branch.vcs_type),
                code: Some("worker-panic".to_string()),
                description: Some(description),
                ..Metadata::default()
            }
        }
    };

    let result = client
        .upload_results(&assignment.id, &metadata, Some(output_directory.path()))
        .await?;

    log::info!("Results uploaded");

    log::debug!("Result: {:?}", result);

    // Clear the run assignment + output_directory now that results
    // are uploaded. Without this, /status keeps reporting
    // `processing` with the *previous* run's id between iterations
    // of the worker loop — feeding the runner's watchdog stale data
    // and producing the "advanced from … to …" / "different-run"
    // race we saw in production — and /logs/{filename} keeps
    // pointing at the just-removed tempdir. The metadata is left
    // intact so post-run debugging pages can still show what just
    // finished.
    if let Ok(mut s) = state.write() {
        s.assignment = None;
        s.output_directory = None;
    }

    if let Some(prometheus) = prometheus {
        if let Err(e) = push_to_gateway(
            prometheus,
            "janitor.worker",
            maplit::hashmap! {
                "run_id" => assignment.id.as_str(),
                "campaign" => assignment.campaign.as_str(),
            },
        )
        .await
        {
            log::warn!("Failed to push metrics to Prometheus gateway: {}", e);
        }
    }

    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum CachePushError {
    /// Worth trying again on the next run.
    Transient,
    /// The next run would fail the same way, so the run fails.
    Permanent,
}

/// Sorts a cache push error, giving None for errors not seen before.
fn cache_push_error_kind(e: &BrzError) -> Option<CachePushError> {
    match e {
        BrzError::InvalidHttpResponse(..)
        | BrzError::IncompleteRead(..)
        | BrzError::UnexpectedHttpStatus { .. }
        | BrzError::TransportError(..)
        | BrzError::TransportNotPossible(..)
        | BrzError::RemoteGitError(..)
        | BrzError::ConnectionError(..) => Some(CachePushError::Transient),
        // e.g. a TLS failure
        BrzError::Socket(..) => Some(CachePushError::Permanent),
        _ => None,
    }
}

fn push_error_to_worker_failure(e: BrzError, stage: Vec<String>) -> WorkerFailure {
    match e {
        BrzError::UnexpectedHttpStatus {
            code: 502,
            url,
            extra,
            ..
        } => WorkerFailure::new(
            "bad-gateway",
            if let Some(extra) = extra.as_ref() {
                format!("Bad gateway from {}: {}", url, extra)
            } else {
                format!("Bad gateway from {}", url)
            },
            stage,
        )
        .with_transient(true)
        .with_details(serde_json::json!({
            "url": url,
            "extra":  extra,
        })),
        BrzError::UnexpectedHttpStatus {
            code, url, extra, ..
        } => WorkerFailure::new(
            format!("http-{}", code),
            if let Some(extra) = extra.as_ref() {
                format!("Unexpected HTTP status {} from {}: {}", code, url, extra)
            } else {
                format!("Unexpected HTTP status {} from {}", code, url)
            },
            stage,
        )
        .with_details(serde_json::json!({
            "status-code": code,
            "url": url,
            "extra": extra,
        })),
        BrzError::ConnectionError(msg) => {
            if msg.contains("Temporary failure in name resolution") {
                WorkerFailure::new(
                    "failed-temporarily",
                    format!("Failed to push result branch: {}", msg),
                    stage,
                )
                .with_transient(true)
            } else {
                WorkerFailure::new(
                    "push-failed",
                    format!("Failed to push result branch: {}", msg),
                    stage,
                )
            }
        }
        BrzError::InvalidHttpResponse(..)
        | BrzError::IncompleteRead(..)
        | BrzError::TransportError(..) => WorkerFailure::new(
            "push-failed",
            format!("Failed to push result branch: {}", e),
            stage,
        ),
        BrzError::RemoteGitError(msg) if msg == "missing necessary objects" => {
            WorkerFailure::new("git-missing-necessary-objects", msg, stage)
        }
        BrzError::RemoteGitError(msg) if msg == "failed to updated ref" => {
            WorkerFailure::new("git-ref-update-failed", msg, stage)
        }
        BrzError::RemoteGitError(msg) => WorkerFailure::new("git-error", msg, stage),
        e => WorkerFailure::new("unexpected-error", e.to_string(), stage),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WorkerFailure;
    use std::path::PathBuf;
    // Both globs above export a `Branch`; import the trait anonymously so
    // method resolution works without the name clash.
    use breezyshim::branch::Branch as _;
    use breezyshim::controldir;
    use janitor::api::worker::*;
    use serial_test::serial;
    use std::path::Path;

    use test_log::test;

    #[test]
    fn test_derive_branch_name() {
        assert_eq!(
            derive_branch_name(&url::Url::parse("https://example.com/foo/bar").unwrap()),
            "bar"
        );
        assert_eq!(
            derive_branch_name(&url::Url::parse("https://example.com/foo/bar/").unwrap()),
            "bar"
        );
        assert_eq!(
            derive_branch_name(&url::Url::parse("https://example.com/").unwrap()),
            "example.com"
        );
    }

    #[test]
    fn test_split_codemod_command() {
        assert_eq!(
            split_codemod_command("lintian-brush --modern 'a b'"),
            Ok(vec![
                "lintian-brush".to_string(),
                "--modern".to_string(),
                "a b".to_string()
            ])
        );
        assert_eq!(
            split_codemod_command("echo 'oops"),
            Err(WorkerFailure {
                code: "invalid-command-syntax".to_string(),
                description: "Invalid command syntax: echo 'oops".to_string(),
                stage: vec!["codemod".to_string()],
                details: None,
                transient: Some(false),
            })
        );
    }

    #[test]
    fn test_convert_codemod_script_failed() {
        assert_eq!(
            convert_codemod_script_failed(127, "foobar"),
            WorkerFailure::new("command-not-found", "Command foobar not found", ["codemod"])
        );
        assert_eq!(
            convert_codemod_script_failed(137, "foobar"),
            WorkerFailure::new("killed", "Process was killed (by OOM killer?)", ["codemod"])
        );
        assert_eq!(
            convert_codemod_script_failed(1, "foobar"),
            WorkerFailure::new(
                "command-failed",
                "Script foobar failed to run with code 1",
                ["codemod"]
            )
        );
    }

    #[test]
    fn test_cache_push_error_kind() {
        let errors = [
            BrzError::ConnectionError("Connection refused".to_string()),
            BrzError::TransportError("Broken pipe".to_string()),
            BrzError::Socket(std::io::Error::from_raw_os_error(1), None),
            BrzError::NoSuchFile(PathBuf::from("/srv/cache")),
        ];
        assert_eq!(
            errors.iter().map(cache_push_error_kind).collect::<Vec<_>>(),
            vec![
                Some(CachePushError::Transient),
                Some(CachePushError::Transient),
                Some(CachePushError::Permanent),
                None,
            ]
        );
    }

    #[serial]
    #[test]
    #[ignore = "Requires Git/Breezy system dependencies and permissions"]
    fn test_run_worker_existing_git() {
        test_run_worker_existing(VcsType::Git);
    }

    #[serial]
    #[test]
    #[ignore = "Requires Bazaar/Breezy system dependencies and permissions"]
    fn test_run_worker_existing_bzr() {
        test_run_worker_existing(VcsType::Bzr);
    }

    fn test_run_worker_existing(vcs_type: VcsType) {
        let env = breezyshim::testing::TestEnv::new();
        let tmp_path = env.working_dir.as_path();
        let wt = breezyshim::controldir::create_standalone_workingtree(
            &tmp_path.join("main"),
            &breezyshim::controldir::FORMAT_REGISTRY
                .make_controldir(&vcs_type.to_string())
                .unwrap(),
        )
        .unwrap();
        std::fs::write(
            tmp_path.join("main").join("Makefile"),
            r#"
all:

test:

check:

"#,
        )
        .unwrap();
        wt.add(&[Path::new("Makefile")]).unwrap();
        let old_revid = wt.build_commit().message("Add makefile").commit().unwrap();
        std::fs::create_dir(tmp_path.join("target")).unwrap();
        let output_dir = tmp_path.join("output");
        std::fs::create_dir(&output_dir).unwrap();
        let mut metadata = Metadata::default();
        run_worker(
            &CodebaseName::new("mycodebase").unwrap(),
            &Campaign::new("mycampaign").unwrap(),
            Some(&wt.branch().get_user_url()),
            &RunId::new("run-id").unwrap(),
            &serde_json::json!({
                "chroot": serde_json::Value::Null,
                "dep_server_url": serde_json::Value::Null,
            }),
            HashMap::new(),
            "sh -c 'echo foo > bar'",
            &output_dir,
            &mut metadata,
            &Url::from_directory_path(tmp_path.join("target")).unwrap(),
            "foo",
            "generic",
            Some(vcs_type),
            Path::new(""),
            None,
            None,
            None,
            None,
            &mut None,
            None,
            None,
            None,
            None,
            None,
            None, // app_state — tests don't share AppState
        )
        .unwrap();
        let found_logfiles = std::fs::read_dir(&output_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_str().unwrap().to_string())
            .collect::<std::collections::HashSet<_>>();

        assert_eq!(
            found_logfiles,
            [
                "codemod.log",
                "worker.log",
                "build.log",
                "test.log",
                "mycodebase"
            ]
            .iter()
            .map(|s| s.to_string())
            .collect::<std::collections::HashSet<_>>()
        );
        let (b, branch_name) = match vcs_type {
            VcsType::Git => {
                let cd = controldir::open(tmp_path.join("target").as_path(), None).unwrap();
                let b = cd.open_branch(Some("mycampaign/main")).unwrap();
                let branch_name = "master";
                (b, branch_name)
            }
            VcsType::Bzr => {
                let b = controldir::open(tmp_path.join("target/mycampaign").as_path(), None)
                    .unwrap()
                    .open_branch(None)
                    .unwrap();
                let branch_name = "";
                (b, branch_name)
            }
            other => unreachable!("test only parameterizes over git/bzr, got {other:?}"),
        };
        assert_eq!(
            metadata,
            Metadata {
                branch_url: Some(wt.branch().get_user_url()),
                transient: None,
                stage: None,
                branches: vec![(
                    "main".to_string(),
                    Some(branch_name.to_string()),
                    Some(old_revid.clone()),
                    Some(b.last_revision())
                )],
                campaign: None,
                code: None,
                failure_details: None,
                finish_time: None,
                codebase: Some(CodebaseName::new("mycodebase").unwrap()),
                codemod: Some(serde_json::Value::Null),
                queue_id: None,
                start_time: None,
                command: Some(vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    "echo foo > bar".to_owned()
                ]),
                description: Some("".to_string()),
                main_branch_revision: Some(old_revid),
                refreshed: Some(false),
                remotes: maplit::hashmap! {
                    "origin".to_string() =>  Remote {
                        url: wt.branch().get_user_url()
                    }
                },
                revision: Some(b.last_revision()),
                subpath: Some("".to_string()),
                tags: vec![],
                target: Some(janitor::api::worker::TargetDetails {
                    name: "generic".to_string(),
                    details: serde_json::json!({}),
                }),
                target_branch_url: None,
                value: None,
                vcs_type: Some(vcs_type),
            }
        );
        std::mem::drop(env);
    }

    #[serial]
    #[test]
    fn test_run_worker_push_failure_git() {
        test_run_worker_push_failure(VcsType::Git);
    }

    #[serial]
    #[test]
    fn test_run_worker_push_failure_bzr() {
        test_run_worker_push_failure(VcsType::Bzr);
    }

    fn test_run_worker_push_failure(vcs_type: VcsType) {
        let env = breezyshim::testing::TestEnv::new();
        let tmp_path = env.working_dir.as_path();
        let wt = breezyshim::controldir::create_standalone_workingtree(
            &tmp_path.join("main"),
            vcs_type.to_string().as_str(),
        )
        .unwrap();
        wt.build_commit()
            .message("Initial commit")
            .allow_pointless(true)
            .commit()
            .unwrap();
        // A file where the VCS store should be, so the push fails.
        std::fs::write(tmp_path.join("target"), "").unwrap();
        let output_dir = tmp_path.join("output");
        std::fs::create_dir(&output_dir).unwrap();
        let mut metadata = Metadata::default();
        let failure = run_worker(
            &CodebaseName::new("mycodebase").unwrap(),
            &Campaign::new("mycampaign").unwrap(),
            Some(&wt.branch().get_user_url()),
            &RunId::new("run-id").unwrap(),
            &serde_json::json!({
                "chroot": serde_json::Value::Null,
                "dep_server_url": serde_json::Value::Null,
            }),
            HashMap::new(),
            "sh -c 'echo foo > bar'",
            &output_dir,
            &mut metadata,
            &Url::from_directory_path(tmp_path.join("target")).unwrap(),
            "foo",
            "generic",
            Some(vcs_type),
            Path::new(""),
            None,
            None,
            None,
            None,
            &mut None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(failure.code, "unexpected-error");
        assert_eq!(failure.stage, vec!["result-push".to_string()]);
        assert_eq!(metadata.branches, vec![]);
        assert_eq!(metadata.tags, vec![]);
        std::mem::drop(env);
    }

    /*

    #[test]
    fn test_run_worker_new_git() {
        test_run_worker_new(tempfile::tempdir().unwrap().path(), "git");
    }

    #[test]
    fn test_run_worker_new_bzr() {
        test_run_worker_new(tempfile::tempdir().unwrap().path(), "bzr");
    }

    fn test_run_worker_new(tmp_path: &std::path::Path, vcs_type: &str) {
        std::fs::create_dir(tmp_path.join("target")).unwrap();
        let output_dir = tmp_path.join("output");
        std::fs::create_dir(&output_dir).unwrap();
        let metadata = Metadata::default();
        run_worker(
            "mycodebase",
            "mycampaign",
            "run-id",
            &["sh", "-c", "echo all check test: > Makefile"],
            &metadata,
            None,
            &serde_json::Value::Null,
            "generic",
            &output_dir,
            tmp_path.join("target").to_str().unwrap(),
            "foo",
            vcs_type);

        assert {e.name for e in os.scandir(output_dir)} == {
            "codemod.log",
            "worker.log",
            "build.log",
            "test.log",
            "mycodebase",
        }
        match vcs_type {
            "git" => {
                cd = ControlDir.open(str(tmp_path / "target"))
                b = cd.open_branch(name="mycampaign/main")
                tags = b.tags.get_tag_dict()
                assert tags == {"run/run-id/main": b.last_revision()}
            }
            "bzr" => {
                b = ControlDir.open(str(tmp_path / "target" / "mycampaign")).open_branch()
                tags = b.tags.get_tag_dict()
                assert tags == {"run-id": b.last_revision()}
            }
        }
        assert metadata.json() == {
            "branch_url": None,
            "branches": [["main", "", None, b.last_revision().decode("utf-8")]],
            "codebase": "mycodebase",
            "codemod": None,
            "command": ["sh", "-c", "echo all check test: > Makefile"],
            "description": "",
            "main_branch_revision": "null:",
            "refreshed": False,
            "remotes": {},
            "revision": b.last_revision().decode("utf-8"),
            "subpath": "",
            "tags": [],
            "target": {"details": {}, "name": "generic"},
            "target_branch_url": None,
            "value": None,
            "vcs_type": vcs_type,
        }
    }

    #[serial]
    #[test]
    fn test_run_worker_build_failure_git() {
        test_run_worker_build_failure(tempfile::tempdir().unwrap().path(), "git");
    }

    #[serial]
    #[test]
    fn test_run_worker_build_failure_bzr() {
        test_run_worker_build_failure(tempfile::tempdir().unwrap().path(), "bzr");
    }

    fn test_run_worker_build_failure(path: &Path, vcs_type: &str) {
        std::fs::create_dir(path.join("target")).unwrap();
        let output_dir = path.join("output");
        std::fs::create_dir(&output_dir).unwrap();
        let metadata = Metadata::default();

        with pytest.raises(_WorkerFailure, match=".*no-build-tools.*"):
            run_worker(
                codebase="mycodebase",
                campaign="mycampaign",
                run_id="run-id",
                command=["sh", "-c", "echo foo > bar"],
                metadata=metadata,
                main_branch_url=None,
                build_config={},
                target="generic",
                output_directory=output_dir,
                target_repo_url=str(tmp_path / "target"),
                vendor="foo",
                vcs_type=vcs_type,
                env={},
            )
        assert {e.name for e in os.scandir(output_dir)} == {
            "codemod.log",
            "worker.log",
            "mycodebase",
        }
        if vcs_type == "git":
            repo = ControlDir.open(str(tmp_path / "target")).open_repository()
            assert list(repo._git.get_refs().keys()) == [b"refs/tags/run/run-id/main"]  # type: ignore
            run_id_revid = repo.lookup_foreign_revision_id(  # type: ignore
                repo._git.get_refs()[b"refs/tags/run/run-id/main"]  # type: ignore
            )
        elif vcs_type == "bzr":
            b = ControlDir.open(str(tmp_path / "target" / "mycampaign")).open_branch()
            tags = b.tags.get_tag_dict()
            assert list(tags.keys()) == ["run-id"]
            run_id_revid = tags["run-id"]
        assert metadata.json() == {
            "branch_url": None,
            "branches": [["main", "", None, run_id_revid.decode("utf-8")]],
            "codebase": "mycodebase",
            "codemod": None,
            "command": ["sh", "-c", "echo foo > bar"],
            "description": "",
            "main_branch_revision": "null:",
            "refreshed": False,
            "remotes": {},
            "revision": run_id_revid.decode("utf-8"),
            "subpath": "",
            "tags": [],
            "target": {"details": None, "name": "generic"},
            "target_branch_url": None,
            "value": None,
            "vcs_type": vcs_type,
        }
    }
    */

    /// Codemod succeeded with VCS changes — return the codemod's own
    /// result unchanged regardless of force_build.
    #[test]
    fn decide_changer_result_ok_with_changes_passes_through() {
        let original = Box::new(silver_platter::codemod::CommandResult {
            description: Some("real codemod".to_owned()),
            value: Some(42),
            ..Default::default()
        });
        let out = decide_changer_result(
            Ok(original),
            /*force_build=*/ false,
            /*any_branch_changes=*/ true,
            /*changes_since_main=*/ false,
            /*has_resume_branch=*/ false,
            /*main_revision=*/ Some(RevisionId::null()),
            /*local_revision=*/ RevisionId::null(),
        );
        match out {
            Ok(r) => {
                assert_eq!(r.description().as_deref(), Some("real codemod"));
                assert_eq!(r.value(), Some(42));
            }
            Err(e) => panic!("ok-with-changes should succeed; got {:?}", e),
        }
    }

    /// Codemod returned Ok but produced no VCS changes; without
    /// force_build this is a `nothing-to-do` failure (the historical
    /// behavior).
    #[test]
    fn decide_changer_result_ok_no_changes_no_force_build_is_nothing_to_do() {
        let original = Box::new(silver_platter::codemod::CommandResult::default());
        let out = decide_changer_result(
            Ok(original),
            /*force_build=*/ false,
            /*any_branch_changes=*/ false,
            /*changes_since_main=*/ false,
            /*has_resume_branch=*/ false,
            /*main_revision=*/ Some(RevisionId::null()),
            /*local_revision=*/ RevisionId::null(),
        );
        match out {
            Ok(_) => panic!("must bail with nothing-to-do"),
            Err(err) => {
                assert_eq!(err.code, "nothing-to-do");
                assert_eq!(err.stage, vec!["codemod"]);
            }
        }
    }

    /// Regression for the control-campaign success path. Codemod
    /// returned Ok with no VCS changes (typical for the `control`
    /// suite running `brz up`), and `force_build=true` says we still
    /// want a build out of this. Must produce a synthetic
    /// `No change build` result and let the build stage proceed —
    /// not bail with `nothing-to-do`.
    #[test]
    fn decide_changer_result_ok_no_changes_force_build_yields_no_change_build() {
        let local_rev = RevisionId::null();
        let original = Box::new(silver_platter::codemod::CommandResult::default());
        let out = decide_changer_result(
            Ok(original),
            /*force_build=*/ true,
            /*any_branch_changes=*/ false,
            /*changes_since_main=*/ false,
            /*has_resume_branch=*/ false,
            /*main_revision=*/ Some(local_rev.clone()),
            local_rev,
        );
        match out {
            Ok(r) => {
                assert_eq!(r.description().as_deref(), Some("No change build"));
                assert_eq!(r.value(), Some(0));
            }
            Err(e) => panic!("force_build should produce a synthetic ok result; got {:?}", e),
        }
    }

    /// Same shape as above, but on the `Err(nothing-to-do)` side —
    /// the original force_build branch that the new Ok-side mirrors.
    /// Kept as a regression so refactors that touch one branch don't
    /// silently diverge from the other.
    #[test]
    fn decide_changer_result_err_nothing_to_do_force_build_yields_no_change_build() {
        let nothing_to_do = WorkerFailure {
            code: "nothing-to-do".to_owned(),
            description: "no changes".to_owned(),
            stage: vec!["codemod".to_owned()],
            transient: Some(false),
            details: None,
        };
        let out = decide_changer_result(
            Err(nothing_to_do),
            /*force_build=*/ true,
            /*any_branch_changes=*/ false,
            /*changes_since_main=*/ false,
            /*has_resume_branch=*/ false,
            /*main_revision=*/ Some(RevisionId::null()),
            /*local_revision=*/ RevisionId::null(),
        );
        match out {
            Ok(r) => assert_eq!(r.description().as_deref(), Some("No change build")),
            Err(e) => panic!("force_build should turn nothing-to-do into a build; got {:?}", e),
        }
    }

    /// `nothing-to-do` with pre-existing main-branch changes and no
    /// resume branch → `unexpected-existing-changes`. The error
    /// description must include both revisions so the failure_details
    /// triage page is useful.
    #[test]
    fn decide_changer_result_err_nothing_to_do_changes_since_main_no_resume_is_unexpected() {
        let nothing_to_do = WorkerFailure {
            code: "nothing-to-do".to_owned(),
            description: "ignored".to_owned(),
            stage: vec!["codemod".to_owned()],
            transient: Some(false),
            details: None,
        };
        let main_rev = RevisionId::from(b"main-rev".to_vec());
        let local_rev = RevisionId::from(b"local-rev".to_vec());
        let out = decide_changer_result(
            Err(nothing_to_do),
            /*force_build=*/ false,
            /*any_branch_changes=*/ false,
            /*changes_since_main=*/ true,
            /*has_resume_branch=*/ false,
            Some(main_rev),
            local_rev,
        );
        match out {
            Ok(_) => panic!("must report unexpected-existing-changes"),
            Err(err) => {
                assert_eq!(err.code, "unexpected-existing-changes");
                assert!(
                    err.description.contains("main-rev")
                        && err.description.contains("local-rev"),
                    "description should include both revisions, got: {}",
                    err.description
                );
            }
        }
    }

    /// Same as above but `has_resume_branch=true` → `nothing-new-to-do`
    /// (resume scenario, the codemod re-ran but didn't add anything
    /// beyond what the resume branch already had).
    #[test]
    fn decide_changer_result_err_nothing_to_do_changes_since_main_with_resume_is_nothing_new() {
        let nothing_to_do = WorkerFailure {
            code: "nothing-to-do".to_owned(),
            description: "carried-through".to_owned(),
            stage: vec!["codemod".to_owned()],
            transient: Some(false),
            details: None,
        };
        let out = decide_changer_result(
            Err(nothing_to_do),
            /*force_build=*/ false,
            /*any_branch_changes=*/ false,
            /*changes_since_main=*/ true,
            /*has_resume_branch=*/ true,
            Some(RevisionId::null()),
            RevisionId::null(),
        );
        match out {
            Ok(_) => panic!("must report nothing-new-to-do"),
            Err(err) => {
                assert_eq!(err.code, "nothing-new-to-do");
                assert_eq!(err.description, "carried-through");
            }
        }
    }

    /// Non-`nothing-to-do` errors propagate untouched.
    #[test]
    fn decide_changer_result_err_other_propagates() {
        let other = WorkerFailure {
            code: "build-failed".to_owned(),
            description: "boom".to_owned(),
            stage: vec!["build".to_owned()],
            transient: Some(true),
            details: None,
        };
        let out = decide_changer_result(
            Err(other),
            /*force_build=*/ true, // even with force_build set
            /*any_branch_changes=*/ false,
            /*changes_since_main=*/ false,
            /*has_resume_branch=*/ false,
            Some(RevisionId::null()),
            RevisionId::null(),
        );
        match out {
            Ok(_) => panic!("non-nothing-to-do must propagate"),
            Err(err) => {
                assert_eq!(err.code, "build-failed");
                assert_eq!(err.transient, Some(true));
            }
        }
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod lib_tests;
