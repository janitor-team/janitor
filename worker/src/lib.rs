use breezyshim::branch::Branch;
use breezyshim::controldir::ControlDirFormat;
use breezyshim::error::Error as BrzError;
use breezyshim::transport::Transport;
use breezyshim::tree::{MutableTree, WorkingTree};
use breezyshim::workingtree::PyWorkingTree;
pub use breezyshim::RevisionId;
use reqwest::header::{HeaderMap, HeaderValue};
use std::collections::HashMap;

use std::net::IpAddr;
use tokio::net::lookup_host;

use janitor::api::worker::{Metadata, TargetDetails, WorkerFailure};
use janitor::prometheus::push_to_gateway;
use janitor::vcs::VcsType;

use url::Url;

pub const DEFAULT_USER_AGENT: &str = concat!("janitor/worker (", env!("CARGO_PKG_VERSION"), ")");

/// Return a copy of `url` with any embedded basic-auth userinfo
/// stripped. Worker credentials are baked into cache / target /
/// resume URLs (via `Credentials::embed_in_url`) so breezy's HTTP
/// git push can authenticate; those URLs MUST NOT reach a log line
/// because the cache endpoint is publicly addressable and the
/// password identifies a specific worker.
pub(crate) fn sanitise_url_for_log(url: &Url) -> Url {
    let mut out = url.clone();
    let _ = out.set_password(None);
    let _ = out.set_username("");
    out
}

pub mod client;

#[cfg(feature = "debian")]
pub mod debian;

pub mod generic;

pub mod vcs;

pub mod web;

mod tee;

/// Result codes that mean a host rate limited this worker specifically,
/// as opposed to some other branch-open failure. Kept in sync with the
/// codes convert_branch_exception() in src/vcs.rs can produce.
const RATE_LIMITED_RESULT_CODES: [&str; 2] =
    ["too-many-requests", "worker-resume-branch-rate-limited"];

/// How long to exclude a host after a rate-limited result with no
/// retry_after of its own.
const DEFAULT_RATE_LIMIT_BACKOFF: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Clone, Default)]
pub struct AppState {
    pub output_directory: Option<std::path::PathBuf>,
    pub assignment: Option<janitor::api::worker::Assignment>,
    pub metadata: Option<Metadata>,
    /// Hosts this worker has itself seen a 429/503 from, and until when to
    /// keep asking the runner to exclude them from this worker's own
    /// assignment requests. Per-process only: does not need to survive a
    /// worker restart, and is never reported to the runner directly.
    pub rate_limited_hosts: HashMap<String, std::time::Instant>,
}

impl AppState {
    /// Record that `host` rate limited this worker just now, so it gets
    /// excluded from this worker's own future assignment requests until
    /// `retry_after` elapses (or DEFAULT_RATE_LIMIT_BACKOFF if not given).
    pub fn record_rate_limited_host(
        &mut self,
        host: String,
        retry_after: Option<std::time::Duration>,
    ) {
        let until = std::time::Instant::now() + retry_after.unwrap_or(DEFAULT_RATE_LIMIT_BACKOFF);
        self.rate_limited_hosts.insert(host, until);
    }

    /// Hosts to exclude from the next assignment request: everything
    /// recorded above whose backoff hasn't expired yet. Expired entries are
    /// dropped.
    pub fn active_rate_limited_hosts(&mut self) -> Vec<String> {
        let now = std::time::Instant::now();
        self.rate_limited_hosts.retain(|_, until| *until > now);
        self.rate_limited_hosts.keys().cloned().collect()
    }
}

/// Pull a retry_after out of a WorkerFailure's `details`, as reported by
/// convert_branch_exception() (a whole number of seconds) or
/// silver_platter::vcs::BranchOpenError::RateLimited (fractional seconds).
/// A missing, zero, or negative value means "no explicit retry_after".
fn retry_after_from_failure_details(
    details: Option<&serde_json::Value>,
) -> Option<std::time::Duration> {
    details
        .and_then(|d| d.get("retry_after"))
        .and_then(|v| v.as_f64())
        .filter(|secs| *secs > 0.0)
        .map(std::time::Duration::from_secs_f64)
}

pub async fn is_gce_instance() -> bool {
    match lookup_host("metadata.google.internal").await {
        Ok(lookup_result) => {
            for addr in lookup_result {
                if let IpAddr::V4(ipv4) = addr.ip() {
                    if ipv4.is_private() {
                        return true;
                    }
                }
            }
            false
        }
        Err(_) => false,
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

    Ok(String::from_utf8(output.stdout).unwrap().trim().to_owned())
}

pub fn convert_codemod_script_failed(i: i32, command: &str) -> WorkerFailure {
    match i {
        127 => WorkerFailure {
            code: "command-not-found".to_string(),
            description: format!("Command {} not found", command),
            details: None,
            stage: vec!["codemod".to_string()],
            transient: None,
        },
        137 => WorkerFailure {
            code: "killed".to_string(),
            description: "Process was killed (by OOM killer?)".to_string(),
            details: None,
            stage: vec!["codemod".to_string()],
            transient: None,
        },
        _ => WorkerFailure {
            code: "command-failed".to_string(),
            description: format!("Script {} failed to run with code {}", command, i),
            details: None,
            stage: vec!["codemod".to_string()],
            transient: None,
        },
    }
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

/// Translate the result of opening the resume branch into either the
/// opened branch or a WorkerFailure, mirroring the resume branch onto
/// metadata.branch_url on a rate limit hit.
///
/// Pulled out of run_worker so the rate-limited arm - the one
/// process_single_item() actually cares about, since it reads
/// metadata.branch_url to pick which host to pass to
/// record_rate_limited_host() - can be exercised directly in a test
/// without opening a real branch over the network.
fn handle_resume_branch_open(
    result: Result<breezyshim::branch::GenericBranch, silver_platter::vcs::BranchOpenError>,
    metadata: &mut Metadata,
) -> Result<Option<breezyshim::branch::GenericBranch>, WorkerFailure> {
    match result {
        Err(silver_platter::vcs::BranchOpenError::TemporarilyUnavailable { url, description }) => {
            let display = sanitise_url_for_log(&url);
            log::info!(
                "Resume branch URL {} temporarily unavailable: {}",
                display,
                description
            );
            Err(WorkerFailure {
                code: "worker-resume-branch-temporarily-unavailable".to_owned(),
                description,
                stage: vec!["setup".to_owned()],
                transient: Some(true),
                details: Some(serde_json::json!({
                    "url": display,
                })),
            })
        }
        Err(silver_platter::vcs::BranchOpenError::RateLimited {
            url,
            description,
            retry_after,
        }) => {
            let display = sanitise_url_for_log(&url);
            log::info!(
                "Resume branch URL {} rate limited: {}",
                display,
                description
            );
            // The resume branch, not the main branch set earlier in
            // run_worker, is what actually got rate limited here.
            metadata.branch_url = Some(display.clone());
            Err(WorkerFailure {
                code: "worker-resume-branch-rate-limited".to_owned(),
                description,
                stage: vec!["setup".to_owned()],
                transient: Some(true),
                details: Some(serde_json::json!({
                    "url": display,
                    "retry_after": retry_after,
                })),
            })
        }
        Err(silver_platter::vcs::BranchOpenError::Unavailable {
            url, description, ..
        }) => {
            let display = sanitise_url_for_log(&url);
            log::info!("Resume branch URL {} unavailable: {}", display, description);
            Err(WorkerFailure {
                code: "worker-resume-branch-unavailable".to_owned(),
                description,
                stage: vec!["setup".to_owned()],
                transient: Some(false),
                details: Some(serde_json::json!({
                    "url": display
                })),
            })
        }
        Err(silver_platter::vcs::BranchOpenError::Missing { url, description }) => {
            let display = sanitise_url_for_log(&url);
            log::info!("Resume branch URL {} missing: {}", display, description);
            Err(WorkerFailure {
                code: "worker-resume-branch-missing".to_owned(),
                description,
                stage: vec!["setup".to_owned()],
                transient: Some(false),
                details: Some(serde_json::json!({
                    "url": display
                })),
            })
        }
        Err(silver_platter::vcs::BranchOpenError::Unsupported { .. }) => Err(WorkerFailure {
            code: "worker-resume-branch-unsupported".to_owned(),
            description: "Unsupported resume branch URL".to_owned(),
            stage: vec!["setup".to_owned()],
            transient: Some(false),
            details: None,
        }),
        Err(silver_platter::vcs::BranchOpenError::Other(..)) => Err(WorkerFailure {
            code: "worker-resume-branch-error".to_owned(),
            description: "Error opening resume branch".to_owned(),
            stage: vec!["setup".to_owned()],
            transient: Some(false),
            details: None,
        }),
        Ok(b) => Ok(Some(b)),
    }
}

pub fn run_worker(
    codebase: &str,
    campaign: &str,
    main_branch_url: Option<&url::Url>,
    run_id: &str,
    build_config: &serde_json::Value,
    env: HashMap<String, String>,
    command: Vec<&str>,
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
) -> Result<(), WorkerFailure> {
    let force_build = force_build.unwrap_or(false);
    let tee = tee.unwrap_or(false);
    let skip_setup_validation = skip_setup_validation.unwrap_or(false);
    let worker_log_path = output_directory.join("worker.log");
    log::debug!("Writing worker log to {}", worker_log_path.display());
    let copy_output = crate::tee::CopyOutput::new(&worker_log_path, tee).unwrap();
    metadata.command = Some(command.iter().map(|s| s.to_string()).collect());
    metadata.codebase = Some(codebase.to_string());

    let build_target: Box<dyn Target> = match target {
        "debian" => Box::new(crate::debian::DebianTarget::new(env)),
        "generic" => Box::new(crate::generic::GenericTarget::new(env)),
        n => {
            return Err(WorkerFailure {
                code: "target-unsupported".to_owned(),
                description: format!("The target {} is not supported", n),
                transient: Some(false),
                stage: vec!["setup".to_owned()],
                details: None,
            });
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
                return Err(WorkerFailure {
                    code: code.clone(),
                    description,
                    stage: vec!["setup".to_owned()],
                    transient: Some(code.contains("temporarily") || code == "too-many-requests"),
                    details: Some(serde_json::json!({
                        "url": main_branch_url,
                        "retry_after": retry_after.map(|r| r.num_seconds()),
                    })),
                })
            }
        };
        metadata.branch_url = Some(main_branch.as_ref().unwrap().get_user_url());
        metadata.vcs_type = Some(
            janitor::vcs::get_branch_vcs_type(main_branch.as_ref().unwrap()).map_err(|e| {
                WorkerFailure {
                    code: "vcs-type-unsupported".to_owned(),
                    description: format!("Unable to determine VCS type: {}", e),
                    stage: vec!["setup".to_owned()],
                    transient: Some(false),
                    details: None,
                }
            })?,
        );
        metadata.subpath = Some(subpath.to_string_lossy().to_string());
        empty_format = None;
    } else {
        assert!(vcs_type.is_some());
        main_branch = None;
        metadata.branch_url = None;
        metadata.vcs_type = vcs_type;
        metadata.subpath = Some("".to_string());
        empty_format = if let Some(f) =
            breezyshim::controldir::FORMAT_REGISTRY.make_controldir(&vcs_type.unwrap().to_string())
        {
            Some(f)
        } else {
            return Err(WorkerFailure {
                code: "vcs-type-unsupported".to_string(),
                description: format!("Unable to find format for vcs type {}", vcs_type.unwrap()),
                stage: vec!["setup".to_owned()],
                transient: Some(false),
                details: Some(serde_json::json!({"vcs_type": vcs_type})),
            });
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
                let sanitised = sanitise_url_for_log(&display);
                log::info!("Using cached branch {}", sanitised);
                Some(b)
            }
            Err(silver_platter::vcs::BranchOpenError::Missing { url, description }) => {
                log::info!(
                    "Cached branch URL {} missing: {}",
                    sanitise_url_for_log(&url),
                    description
                );
                None
            }
            Err(
                silver_platter::vcs::BranchOpenError::Unavailable {
                    url, description, ..
                }
                | silver_platter::vcs::BranchOpenError::TemporarilyUnavailable { url, description },
            ) => {
                log::info!(
                    "Cached branch URL {} unavailable: {}",
                    sanitise_url_for_log(&url),
                    description
                );
                None
            }
            Err(
                silver_platter::vcs::BranchOpenError::Unsupported { .. }
                | silver_platter::vcs::BranchOpenError::Other(..)
                | silver_platter::vcs::BranchOpenError::RateLimited { .. },
            ) => {
                log::info!(
                    "Error accessing cached branch URL {}",
                    sanitise_url_for_log(cached_branch_url)
                );
                None
            }
        }
    } else {
        None
    };

    let resume_branch = if let Some(resume_branch_url) = resume_branch_url {
        log::info!(
            "Using resume branch: {}",
            sanitise_url_for_log(resume_branch_url)
        );
        let probers =
            silver_platter::probers::select_probers(vcs_type.map(|v| v.to_string()).as_deref());
        let result = silver_platter::vcs::open_branch(
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
        );
        handle_resume_branch_open(result, metadata)?
    } else {
        None
    };

    let mut roles: HashMap<String, String> =
        additional_colocated_branches.clone().unwrap_or_default();

    if let Some(main_branch) = main_branch.as_ref() {
        roles.insert(main_branch.name().unwrap(), "main".to_string());
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
            return Err(WorkerFailure {
                code: "incomplete-read".to_owned(),
                description: e.to_string(),
                stage: vec!["setup".to_owned(), "clone".to_owned()],
                transient: Some(true),
                details: None,
            });
        }
        Err(silver_platter::workspace::Error::BrzError(BrzError::MalformedTransform(msg))) => {
            return Err(WorkerFailure {
                code: "malformed-transform".to_owned(),
                description: format!("Malformed transform: {:?}", msg),
                stage: vec!["setup".to_owned(), "clone".to_owned()],
                transient: Some(false),
                details: None,
            });
        }
        Err(silver_platter::workspace::Error::BrzError(
            e @ BrzError::TransformRenameFailed(..),
        )) => {
            return Err(WorkerFailure {
                code: "transform-rename-failed".to_owned(),
                description: e.to_string(),
                stage: vec!["setup".to_owned(), "clone".to_owned()],
                transient: Some(false),
                details: None,
            });
        }
        Err(silver_platter::workspace::Error::BrzError(e @ BrzError::ImmortalLimbo(..))) => {
            return Err(WorkerFailure {
                code: "transform-immortal-limbo".to_owned(),
                description: e.to_string(),
                stage: vec!["setup".to_owned(), "clone".to_owned()],
                transient: Some(false),
                details: None,
            });
        }
        Err(silver_platter::workspace::Error::BrzError(BrzError::UnexpectedHttpStatus {
            url,
            code,
            extra,
            headers,
        })) => {
            if code == 502 {
                return Err(WorkerFailure {
                    code: "bad-gateway".to_owned(),
                    description: if let Some(ref extra) = extra {
                        format!("Bad gateway from {}: {}", url, extra)
                    } else {
                        format!("Bad gateway from {}", url)
                    },
                    stage: vec!["setup".to_owned(), "clone".to_owned()],
                    transient: Some(true),
                    details: None,
                });
            } else {
                return Err(WorkerFailure {
                    code: format!("http-{}", code),
                    description: if let Some(ref extra) = extra {
                        format!(
                            "Unexpected HTTP status code {} from {}: {}",
                            code, url, extra
                        )
                    } else {
                        format!("Unexpected HTTP status code {} from {}", code, url)
                    },
                    stage: vec!["setup".to_owned(), "clone".to_owned()],
                    details: Some(serde_json::json!({
                        "status-code": code,
                        "url": url,
                        "extra": extra,
                        "headers": headers,
                    })),
                    transient: None,
                });
            }
        }
        Err(silver_platter::workspace::Error::BrzError(BrzError::TransportError(msg))) => {
            if msg.contains("No space left on device") {
                return Err(WorkerFailure {
                    code: "no-space-on-device".to_owned(),
                    description: msg,
                    stage: vec!["setup".to_owned(), "clone".to_owned()],
                    transient: Some(true),
                    details: None,
                });
            }
            if msg.contains("Too many open files") {
                return Err(WorkerFailure {
                    code: "too-many-open-files".to_owned(),
                    description: msg,
                    stage: vec!["setup".to_owned(), "clone".to_owned()],
                    transient: Some(true),
                    details: None,
                });
            }
            if msg.contains("Temporary failure in name resolution") {
                return Err(WorkerFailure {
                    code: "temporary-transport-error".to_owned(),
                    description: msg,
                    stage: vec!["setup".to_owned(), "clone".to_owned()],
                    transient: Some(true),
                    details: None,
                });
            }
            return Err(WorkerFailure {
                code: "transport-error".to_owned(),
                description: msg,
                stage: vec!["setup".to_owned(), "clone".to_owned()],
                transient: Some(false),
                details: None,
            });
        }
        Err(silver_platter::workspace::Error::BrzError(BrzError::RemoteGitError(ref msg))) => {
            return Err(WorkerFailure {
                code: "git-error".to_owned(),
                description: msg.clone(),
                stage: vec!["setup".to_owned(), "clone".to_owned()],
                transient: Some(false),
                details: None,
            });
        }
        Err(silver_platter::workspace::Error::BrzError(e @ BrzError::Timeout)) => {
            return Err(WorkerFailure {
                code: "timeout".to_owned(),
                description: e.to_string(),
                stage: vec!["setup".to_owned(), "clone".to_owned()],
                transient: Some(true),
                details: None,
            });
        }
        Err(silver_platter::workspace::Error::BrzError(BrzError::MissingNestedTree(ref msg))) => {
            return Err(WorkerFailure {
                code: "requires-nested-tree-support".to_owned(),
                description: msg.display().to_string(),
                stage: vec!["setup".to_owned(), "clone".to_owned()],
                transient: Some(false),
                details: None,
            });
        }
        Err(e) => {
            return Err(WorkerFailure {
                code: "unexpected-error".to_owned(),
                description: e.to_string(),
                stage: vec!["setup".to_owned(), "clone".to_owned()],
                transient: Some(false),
                details: None,
            });
        }
    };

    log::info!("Workspace ready - starting.");

    if ws.local_tree().has_changes().unwrap() {
        return Err(WorkerFailure {
            code: "unexpected-changes-in-tree".to_owned(),
            description: "The working tree has unexpected changes after initial clone".to_owned(),
            stage: vec!["setup".to_owned(), "clone".to_owned()],
            details: None,
            transient: Some(false),
        });
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

    let changer_result = match r {
        Ok(r) => {
            if !ws.any_branch_changes() {
                return Err(WorkerFailure {
                    code: "nothing-to-do".to_owned(),
                    description: "Nothing to do.".to_owned(),
                    stage: vec!["codemod".to_owned()],
                    transient: Some(false),
                    details: None,
                });
            }
            r
        }
        Err(
            ref e @ WorkerFailure {
                ref code,
                ref description,
                ..
            },
        ) if code.as_str() == "nothing-to-do" => {
            if ws.changes_since_main() {
                // This should only ever happen if we were resuming
                assert!(
                    ws.resume_branch().is_some(),
                    "Found existing changes despite not having resumed. Mainline: {}, local: {}",
                    ws.main_branch().unwrap().last_revision(),
                    ws.local_tree().branch().last_revision()
                );
                return Err(WorkerFailure {
                    code: "nothing-new-to-do".to_owned(),
                    description: description.clone(),
                    stage: vec!["codemod".to_owned()],
                    transient: Some(false),
                    details: None,
                });
            } else if force_build {
                Box::new(silver_platter::codemod::CommandResult {
                    description: Some("No change build".to_owned()),
                    context: None,
                    tags: Vec::new(),
                    value: Some(0),
                    old_revision: ws.local_tree().last_revision().unwrap(),
                    new_revision: ws.local_tree().last_revision().unwrap(),
                    title: None,
                    commit_message: None,
                    serialized_context: None,
                    target_branch_url: None,
                })
            } else {
                return Err(e.clone());
            }
        }
        Err(e) => return Err(e),
    };

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

    for (f, n, br, r) in result_branches.iter() {
        metadata.add_branch(f.to_string(), n.to_string(), br.clone(), r.clone());
    }
    for (n, r) in changer_result.tags().iter() {
        metadata.add_tag(n.to_string(), r.clone());
    }

    let actual_vcs_type =
        janitor::vcs::get_branch_vcs_type(&ws.local_tree().branch()).map_err(|e| {
            WorkerFailure {
                code: "vcs-type-unsupported".to_owned(),
                description: format!("Unable to determine VCS type: {}", e),
                stage: vec!["result-push".to_owned()],
                transient: Some(false),
                details: None,
            }
        })?;

    let vcs_type = if vcs_type.is_none() {
        actual_vcs_type
    } else if Some(actual_vcs_type) != vcs_type {
        return Err(WorkerFailure {
            code: "vcs-type-mismatch".to_owned(),
            description: format!(
                "Expected VCS {}, got {}",
                vcs_type.unwrap(),
                actual_vcs_type
            ),
            stage: vec!["result-push".to_owned()],
            transient: Some(false),
            details: None,
        });
    } else {
        vcs_type.unwrap()
    };

    match vcs_type {
        VcsType::Git => &crate::vcs::GitVcs as &dyn crate::vcs::Vcs,
        VcsType::Bzr => &crate::vcs::BzrVcs,
    }
    .import_branches(
        target_repo_url,
        &ws.local_tree().branch(),
        campaign,
        run_id,
        &result_branches,
        changer_result.tags(),
        false,
    )
    .map_err(|e| push_error_to_worker_failure(e, vec!["result-push".to_string()]))?;

    let should_build = if force_build {
        true
    } else {
        result_branches
            .iter()
            .any(|(role, _name, _br, _r)| role == "main")
    };

    let target_details = if should_build {
        build_target.build(ws.local_tree(), subpath, output_directory, build_config)?
    } else {
        serde_json::Value::Null
    };

    metadata.target = Some(TargetDetails {
        name: build_target.name(),
        details: target_details,
    });

    log::info!(
        "Pushing result branch to {}",
        sanitise_url_for_log(target_repo_url)
    );

    match vcs_type {
        VcsType::Git => &crate::vcs::GitVcs as &dyn crate::vcs::Vcs,
        VcsType::Bzr => &crate::vcs::BzrVcs,
    }
    .import_branches(
        target_repo_url,
        &ws.local_tree().branch(),
        campaign,
        run_id,
        &result_branches,
        changer_result.tags(),
        true,
    )
    .map_err(|e| push_error_to_worker_failure(e, vec!["result-sym".to_owned()]))?;

    if let Some(cached_branch_url) = cached_branch_url.as_ref() {
        // TODO(jelmer): integrate into import_branches_git / import_branches_bzr
        log::info!("Pushing packaging branch cache to {}", cached_branch_url);

        let vendor = vendor.to_string();

        let tag_selector = move |tag_name: String| -> bool {
            tag_name.starts_with(&format!("{}/", vendor)) || tag_name.starts_with("upstream/")
        };

        if let Some(main_branch) = ws.main_branch() {
            match crate::vcs::push_branch(
                &ws.local_tree().branch(),
                cached_branch_url,
                Some(vcs_type),
                true,
                Some(main_branch.last_revision()),
                Some(Box::new(tag_selector)),
                possible_transports,
            ) {
                Err(
                    e @ BrzError::InvalidHttpResponse(..)
                    | e @ BrzError::IncompleteRead(..)
                    | e @ BrzError::UnexpectedHttpStatus { .. }
                    | e @ BrzError::TransportError(..)
                    | e @ BrzError::TransportNotPossible(..)
                    | e @ BrzError::RemoteGitError(..),
                ) => {
                    log::warn!("unable to push to cache URL {}: {}", cached_branch_url, e);
                }
                Err(e) => {
                    panic!(
                        "Unexpected error pushing to cache URL {}: {}",
                        cached_branch_url, e
                    );
                }
                Ok(_) => {
                    log::info!("Pushed packaging branch cache to {}", cached_branch_url);
                }
            }
        }
    }

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
    JobPanicked(String),
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
    codebase: Option<&str>,
    campaign: Option<&str>,
    tee: bool,
    output_directory_base: Option<&std::path::Path>,
    state: std::sync::Arc<std::sync::RwLock<AppState>>,
) -> Result<(), SingleItemError> {
    let exclude_hosts = state.write().unwrap().active_rate_limited_hosts();
    let assignment = client
        .get_assignment(
            my_url,
            node_name,
            jenkins_build_url,
            codebase,
            campaign,
            &exclude_hosts,
        )
        .await?;

    state.write().unwrap().assignment = Some(assignment.clone());

    log::debug!("Got back assignment: {:?}", &assignment);

    let force_build = assignment.force_build;
    let (resume_result, resume_branch_url, resume_branches) =
        if let Some(resume) = &assignment.resume {
            let resume_result = resume.result.clone();
            let resume_branch_url = resume
                .branch_url
                .to_string()
                .trim_end_matches('/')
                .parse()
                .unwrap();
            let resume_branches = resume.branches.clone();
            (
                Some(resume_result),
                Some(resume_branch_url),
                Some(resume_branches),
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
        tempfile::TempDir::with_prefix_in("janitor-worker-", output_directory_base)
    } else {
        tempfile::TempDir::with_prefix("janitor-worker-")
    }
    .unwrap();

    let assignment_ = assignment.clone();

    let output_directory_ = output_directory.path().to_path_buf();

    // Embed the worker's Basic-auth credentials into the
    // target_repository URL so the push subprocess (breezy/dulwich)
    // authenticates against git-store's public listener. The runner
    // only knows the bare URL; only the worker has its own creds, so
    // splicing them in here keeps creds in one place
    // (WORKER_NAME/WORKER_PASSWORD on the worker) instead of leaking
    // them into the runner-side config. No-op for `Credentials::None`,
    // so in-cluster setups using cluster-internal URLs without auth
    // still work.
    let target_repo_url_with_creds = client
        .credentials()
        .embed_in_url(&assignment.target_repository.url);
    // The cached_url and resume_branch_url usually point at the same
    // git-store as target_repository (the runner derives them from
    // the same `public_vcs_location`), so they need the same Basic
    // auth. Splice creds into both.
    let cached_branch_url_with_creds = assignment
        .branch
        .cached_url
        .as_ref()
        .map(|u| client.credentials().embed_in_url(u));
    let resume_branch_url_with_creds = resume_branch_url
        .as_ref()
        .map(|u| client.credentials().embed_in_url(u));

    let metadata = tokio::task::spawn_blocking(move || {
        let mut metadata = Metadata {
            queue_id: Some(assignment.queue_id),
            start_time: Some(start_time),
            branch_url: assignment_.branch.url.clone(),
            vcs_type: Some(assignment.branch.vcs_type),
            ..Metadata::default()
        };

        // TODO: Update metadata in appstate while working

        let result = run_worker(
            &assignment_.codebase,
            &assignment_.campaign,
            assignment_.branch.url.as_ref(),
            &assignment_.id,
            &assignment_.build.config,
            env,
            shlex::split(&assignment_.codemod.command)
                .unwrap()
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>(),
            output_directory_.as_path(),
            &mut metadata,
            &target_repo_url_with_creds,
            &vendor,
            &assignment_.build.target,
            Some(assignment_.branch.vcs_type),
            assignment_.branch.subpath.as_ref(),
            resume_branch_url_with_creds.as_ref(),
            cached_branch_url_with_creds.as_ref(),
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

        metadata
    })
    .await
    .map_err(|e| SingleItemError::JobPanicked(e.to_string()))?;

    if let Some(code) = metadata.code.as_deref() {
        if RATE_LIMITED_RESULT_CODES.contains(&code) {
            if let Some(host) = metadata.branch_url.as_ref().and_then(|u| u.host_str()) {
                let retry_after =
                    retry_after_from_failure_details(metadata.failure_details.as_ref());
                log::info!(
                    "Rate limited by {} ({}); excluding it from this worker's own assignment requests",
                    host,
                    code
                );
                state
                    .write()
                    .unwrap()
                    .record_rate_limited_host(host.to_string(), retry_after);
            }
        }
    }

    let result = client
        .upload_results(&assignment.id, &metadata, Some(output_directory.path()))
        .await?;

    log::info!("Results uploaded");

    log::debug!("Result: {:?}", result);

    if let Some(prometheus) = prometheus {
        push_to_gateway(
            prometheus,
            "janitor.worker",
            maplit::hashmap! {
                "run_id" => assignment.id.as_str(),
                "campaign" => assignment.campaign.as_str(),
            },
            prometheus::default_registry(),
        )
        .await
        .unwrap();
    }

    Ok(())
}

fn push_error_to_worker_failure(e: BrzError, stage: Vec<String>) -> WorkerFailure {
    match e {
        BrzError::UnexpectedHttpStatus {
            code: 502,
            url,
            extra,
            ..
        } => WorkerFailure {
            code: "bad-gateway".to_string(),
            description: if let Some(extra) = extra.as_ref() {
                format!("Bad gateway from {}: {}", url, extra)
            } else {
                format!("Bad gateway from {}", url)
            },
            stage,
            transient: Some(true),
            details: Some(serde_json::json!({
                "url": url,
                "extra":  extra,
            })),
        },
        BrzError::UnexpectedHttpStatus {
            code, url, extra, ..
        } => WorkerFailure {
            code: format!("http-{}", code),
            description: if let Some(extra) = extra.as_ref() {
                format!("Unexpected HTTP status {} from {}: {}", code, url, extra)
            } else {
                format!("Unexpected HTTP status {} from {}", code, url)
            },
            stage,
            details: Some(serde_json::json!({
                "status-code": code,
                "url": url,
                "extra": extra,
            })),
            transient: None,
        },
        BrzError::ConnectionError(msg) => {
            if msg.contains("Temporary failure in name resolution") {
                WorkerFailure {
                    code: "failed-temporarily".to_string(),
                    description: format!("Failed to push result branch: {}", msg),
                    stage,
                    transient: Some(true),
                    details: None,
                }
            } else {
                WorkerFailure {
                    code: "push-failed".to_string(),
                    description: format!("Failed to push result branch: {}", msg),
                    stage,
                    details: None,
                    transient: None,
                }
            }
        }
        BrzError::InvalidHttpResponse(..)
        | BrzError::IncompleteRead(..)
        | BrzError::TransportError(..) => WorkerFailure {
            code: "push-failed".to_string(),
            description: format!("Failed to push result branch: {}", e),
            stage,
            details: None,
            transient: None,
        },
        BrzError::RemoteGitError(msg) if msg == "missing necessary objects" => WorkerFailure {
            code: "git-missing-necessary-objects".to_string(),
            description: msg,
            stage,
            details: None,
            transient: None,
        },
        BrzError::RemoteGitError(msg) if msg == "failed to updated ref" => WorkerFailure {
            code: "git-ref-update-failed".to_string(),
            description: msg,
            stage,
            details: None,
            transient: None,
        },
        BrzError::RemoteGitError(msg) => WorkerFailure {
            code: "git-error".to_string(),
            description: msg,
            stage,
            details: None,
            transient: None,
        },
        e => WorkerFailure {
            code: "unexpected-error".to_string(),
            description: e.to_string(),
            stage,
            details: None,
            transient: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WorkerFailure;
    use breezyshim::controldir;
    use janitor::api::worker::*;
    use serial_test::serial;
    use std::path::{Path, PathBuf};
    use std::str::FromStr;
    use test_log::test;

    /// Regression for worker credential leaks. The cache push and
    /// branch-open log lines used to print URLs with embedded
    /// HTTP-basic creds (e.g. `http://frigg:<pass>@.../git/foo`)
    /// because `Credentials::embed_in_url` rewrites the userinfo
    /// component and the worker handed that URL straight to
    /// `log::info!` / `log::warn!`. The cache endpoint is public,
    /// so the userinfo must be stripped before any URL reaches
    /// the journal.
    #[test]
    fn test_sanitise_url_for_log_drops_userinfo() {
        let with_creds: Url = "http://frigg:secret-token@janitor.local/git/imath"
            .parse()
            .unwrap();
        let cleaned = sanitise_url_for_log(&with_creds);
        let s = cleaned.to_string();
        assert!(!s.contains("frigg"), "username must be stripped: {}", s);
        assert!(
            !s.contains("secret-token"),
            "password must be stripped: {}",
            s
        );
        assert_eq!(s, "http://janitor.local/git/imath");
    }

    #[test]
    fn test_sanitise_url_for_log_passes_through_clean_urls() {
        // URLs that never had userinfo must be returned as-is so we
        // don't accidentally rewrite them (path/query preserved).
        let clean: Url = "https://salsa.debian.org/jelmer/foo.git?branch=master"
            .parse()
            .unwrap();
        let cleaned = sanitise_url_for_log(&clean);
        assert_eq!(cleaned.as_str(), clean.as_str());
    }

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
    fn test_convert_codemod_script_failed() {
        assert_eq!(
            convert_codemod_script_failed(127, "foobar"),
            WorkerFailure {
                code: "command-not-found".to_string(),
                description: "Command foobar not found".to_string(),
                stage: vec!["codemod".to_string()],
                details: None,
                transient: None,
            }
        );
        assert_eq!(
            convert_codemod_script_failed(137, "foobar"),
            WorkerFailure {
                code: "killed".to_string(),
                description: "Process was killed (by OOM killer?)".to_string(),
                stage: vec!["codemod".to_string()],

                details: None,
                transient: None,
            }
        );
        assert_eq!(
            convert_codemod_script_failed(1, "foobar"),
            WorkerFailure {
                code: "command-failed".to_string(),
                description: "Script foobar failed to run with code 1".to_string(),
                stage: vec!["codemod".to_string()],
                details: None,
                transient: None,
            }
        );
    }

    #[test]
    fn test_app_state_rate_limited_hosts() {
        let mut state = AppState::default();
        assert_eq!(state.active_rate_limited_hosts(), Vec::<String>::new());

        state.record_rate_limited_host(
            "salsa.debian.org".to_string(),
            Some(std::time::Duration::from_secs(60)),
        );
        assert_eq!(
            state.active_rate_limited_hosts(),
            vec!["salsa.debian.org".to_string()]
        );

        // An already-expired backoff is excluded and dropped.
        state.record_rate_limited_host(
            "expired.example".to_string(),
            Some(std::time::Duration::from_secs(0)),
        );
        assert_eq!(
            state.active_rate_limited_hosts(),
            vec!["salsa.debian.org".to_string()]
        );
        assert!(!state.rate_limited_hosts.contains_key("expired.example"));
    }

    #[test]
    fn test_app_state_record_rate_limited_host_defaults_backoff() {
        let mut state = AppState::default();
        state.record_rate_limited_host("example.com".to_string(), None);
        assert_eq!(
            state.active_rate_limited_hosts(),
            vec!["example.com".to_string()]
        );
    }

    #[test]
    fn test_handle_resume_branch_open_rate_limited_records_resume_branch_host() {
        // Regression for the resume-branch-rate-limited path recording the
        // wrong host. metadata.branch_url starts out holding the main
        // branch's URL, same as run_worker leaves it after opening the
        // main branch earlier in the function. If handle_resume_branch_open
        // doesn't overwrite it on a rate limit hit, process_single_item()
        // goes on to call record_rate_limited_host() with the main
        // branch's host instead of the one that was actually rate limited.
        let mut metadata = Metadata {
            branch_url: Some(Url::parse("https://main.example/repo").unwrap()),
            ..Metadata::default()
        };
        let err = silver_platter::vcs::BranchOpenError::RateLimited {
            url: Url::parse("https://ratelimited.example/repo").unwrap(),
            description: "429: Too Many Requests".to_string(),
            retry_after: Some(30.0),
        };

        let result = handle_resume_branch_open(Err(err), &mut metadata);

        let failure = match result {
            Err(failure) => failure,
            Ok(_) => panic!("rate limit must surface as a WorkerFailure"),
        };
        assert_eq!(failure.code, "worker-resume-branch-rate-limited");

        // What process_single_item() actually reads, verbatim: RESULT_CODES
        // check -> metadata.branch_url.host_str() -> record_rate_limited_host().
        metadata.update(&failure);
        let mut state = AppState::default();
        if RATE_LIMITED_RESULT_CODES.contains(&metadata.code.as_deref().unwrap()) {
            if let Some(host) = metadata.branch_url.as_ref().and_then(|u| u.host_str()) {
                state.record_rate_limited_host(host.to_string(), None);
            }
        }
        assert_eq!(
            state.active_rate_limited_hosts(),
            vec!["ratelimited.example".to_string()],
            "must record the branch that was actually rate limited, not the main branch"
        );
    }

    #[test]
    fn test_retry_after_from_failure_details() {
        // convert_branch_exception()'s style: a whole number of seconds.
        assert_eq!(
            retry_after_from_failure_details(Some(&serde_json::json!({"retry_after": 30}))),
            Some(std::time::Duration::from_secs(30))
        );
        // silver_platter's RateLimited style: fractional seconds.
        assert_eq!(
            retry_after_from_failure_details(Some(&serde_json::json!({"retry_after": 30.5}))),
            Some(std::time::Duration::from_secs_f64(30.5))
        );
        // No retry_after, no details, and a non-positive retry_after all
        // mean "fall back to the caller's default".
        assert_eq!(
            retry_after_from_failure_details(Some(&serde_json::json!({"url": "x"}))),
            None
        );
        assert_eq!(retry_after_from_failure_details(None), None);
        assert_eq!(
            retry_after_from_failure_details(Some(&serde_json::json!({"retry_after": 0}))),
            None
        );
        assert_eq!(
            retry_after_from_failure_details(Some(&serde_json::json!({"retry_after": -5}))),
            None
        );
    }

    #[serial]
    #[test]
    fn test_run_worker_existing_git() {
        test_run_worker_existing(tempfile::tempdir().unwrap().path(), VcsType::Git);
    }

    #[serial]
    #[test]
    fn test_run_worker_existing_bzr() {
        test_run_worker_existing(tempfile::tempdir().unwrap().path(), VcsType::Bzr);
    }

    fn test_run_worker_existing(tmp_path: &std::path::Path, vcs_type: VcsType) {
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
            "mycodebase",
            "mycampaign",
            Some(&wt.branch().get_user_url()),
            "run-id",
            &serde_json::json!({
                "chroot": serde_json::Value::Null,
                "dep_server_url": serde_json::Value::Null,
            }),
            HashMap::new(),
            vec!["sh", "-c", "echo foo > bar"],
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
                codebase: Some("mycodebase".to_string()),
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

    #[test]
    fn test_run_worker_build_failure_git() {
        test_run_worker_build_failure(tempfile::tempdir().unwrap().path(), "git");
    }

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
}
