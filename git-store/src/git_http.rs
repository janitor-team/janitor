//! Git HTTP smart protocol and range-query handlers.

use crate::api_types::{GitPerson, LogEntry, LogResponse, RevisionInfoEntry};
use crate::error::{GitStoreError, Result};
use axum::{
    body::Body,
    extract::{Path, Query, Request, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
};
use futures_util::StreamExt;
use serde::Deserialize;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt},
    process::Command,
};
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Default)]
pub struct AuthContext {
    pub worker_name: Option<String>,
    pub allow_writes: bool,
}

/// `old` and `new` are validated manually so missing values give a
/// Python-compatible 400 "need both old and new" rather than axum's
/// default 422 rejection body.
#[derive(Debug, Deserialize)]
pub struct DiffQuery {
    old: Option<String>,
    new: Option<String>,
    path: Option<String>,
}

pub async fn git_diff(
    State(state): State<crate::web::AppState>,
    Path(codebase): Path<String>,
    Query(params): Query<DiffQuery>,
) -> Result<Response> {
    let (old, new) = match (params.old, params.new) {
        (Some(o), Some(n)) => (o, n),
        _ => {
            return Err(GitStoreError::BadRequest(
                "need both old and new".to_string(),
            ));
        }
    };

    let repo_path = state.repo_manager.repo_path(&codebase);

    // Python order: missing repo dir before SHA validity.
    if !repo_path.is_dir() {
        return Err(GitStoreError::LocalRepositoryUnavailable(codebase));
    }

    if !is_valid_hexsha(&old) || !is_valid_hexsha(&new) {
        return Err(GitStoreError::BadRequest(
            "invalid shas specified".to_string(),
        ));
    }

    let mut cmd = Command::new("git");
    cmd.arg("diff")
        .arg(&old)
        .arg(&new)
        .current_dir(&repo_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    if let Some(path) = params.path {
        cmd.arg("--").arg(path);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| GitStoreError::Other(anyhow::anyhow!("spawn git diff: {}", e)))?;

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| GitStoreError::Other(anyhow::anyhow!("no stdout on git diff")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| GitStoreError::Other(anyhow::anyhow!("no stderr on git diff")))?;

    // Peek the first chunk before writing headers so an immediate
    // failure (bad object, broken repo) still surfaces as 500 rather
    // than a 200 with a truncated/empty body.
    let timeout = std::time::Duration::from_secs(state.config.git.git_timeout);
    let mut first_chunk = vec![0u8; 8192];
    let n = match tokio::time::timeout(timeout, stdout.read(&mut first_chunk)).await {
        Ok(Ok(n)) => n,
        Ok(Err(e)) => {
            let _ = child.kill().await;
            return Err(GitStoreError::Other(anyhow::anyhow!(
                "read git diff stdout: {}",
                e
            )));
        }
        Err(_) => {
            let _ = child.kill().await;
            return Err(GitStoreError::DiffTimeout);
        }
    };
    first_chunk.truncate(n);

    if n == 0 {
        // No output: wait for exit so we can report an accurate error.
        match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) if status.success() => {
                // Empty diff is legitimate (identical trees).
                return Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/x-diff")
                    .body(Body::empty())
                    .map_err(GitStoreError::HttpLibError);
            }
            Ok(Ok(_)) => {
                let msg = read_stderr_lossy(stderr).await;
                warn!("git diff failed: {}", msg);
                return Err(GitStoreError::GitDiffFailed(msg));
            }
            Ok(Err(e)) => {
                return Err(GitStoreError::Other(anyhow::anyhow!(
                    "wait git diff: {}",
                    e
                )));
            }
            Err(_) => {
                let _ = child.kill().await;
                return Err(GitStoreError::DiffTimeout);
            }
        }
    }

    // Stream the rest. A child whose stdout/wait tasks live past the
    // handler needs to be reaped by someone, so spawn a reaper that
    // drains stderr for logging and awaits exit.
    tokio::spawn(async move {
        let msg = read_stderr_lossy(stderr).await;
        match child.wait().await {
            Ok(status) if !status.success() => {
                warn!("git diff exited non-zero: {}", msg);
            }
            Err(e) => warn!("git diff wait failed: {}", e),
            _ => {}
        }
    });

    let first = futures_util::stream::once(async move {
        Ok::<_, std::io::Error>(bytes::Bytes::from(first_chunk))
    });
    let rest = tokio_util::io::ReaderStream::new(stdout);
    let combined = first.chain(rest);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/x-diff")
        .body(Body::from_stream(combined))
        .map_err(GitStoreError::HttpLibError)
}

/// Drain git's stderr for logging; silently ignores read errors so a
/// best-effort message is better than none.
async fn read_stderr_lossy<R: tokio::io::AsyncRead + Unpin>(mut reader: R) -> String {
    let mut buf = Vec::new();
    let _ = tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut buf).await;
    String::from_utf8_lossy(&buf).into_owned()
}

/// True for a 40-char lowercase or uppercase hex string. Matches
/// dulwich's `valid_hexsha` semantics used by Python.
fn is_valid_hexsha(s: &str) -> bool {
    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// `git cat-file -e <rev>`: exits 0 if the object is reachable.
async fn ensure_revision_present(
    repo_path: &std::path::Path,
    codebase: &str,
    revision: &str,
    timeout_secs: u64,
) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.arg("cat-file")
        .arg("-e")
        .arg(revision)
        .current_dir(repo_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let status = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), cmd.status())
        .await
        .map_err(|_| GitStoreError::Timeout)??;
    if status.success() {
        Ok(())
    } else {
        Err(GitStoreError::RevisionNotFound {
            codebase: codebase.to_string(),
            revision: revision.to_string(),
        })
    }
}

/// `old` and `new` are validated manually so missing values give a
/// Python-compatible 400 "need both old and new" rather than axum's
/// default 422 rejection body.
#[derive(Debug, Deserialize)]
pub struct RevisionInfoQuery {
    old: Option<String>,
    new: Option<String>,
}

/// All-zeros SHA: "walk from the root of `new`, don't exclude anything".
const ZERO_SHA: &str = "0000000000000000000000000000000000000000";

/// Return the commits in `old..new` (newest-first) as a JSON array of
/// `{commit-id, revision-id, link, message}`, matching what
/// `py/janitor/site/cupboard/review.py::get_revision_info` expects.
pub async fn revision_info(
    State(state): State<crate::web::AppState>,
    Path(codebase): Path<String>,
    Query(params): Query<RevisionInfoQuery>,
) -> Result<Response> {
    let (old, new) = match (params.old, params.new) {
        (Some(o), Some(n)) => (o, n),
        _ => {
            return Err(GitStoreError::BadRequest(
                "need both old and new".to_string(),
            ));
        }
    };

    let repo_path = state.repo_manager.repo_path(&codebase);
    // Python order: check repo dir before SHA validity.
    if !repo_path.is_dir() {
        return Err(GitStoreError::LocalRepositoryUnavailable(codebase));
    }
    if !is_valid_hexsha(&old) || !is_valid_hexsha(&new) {
        return Err(GitStoreError::BadRequest(
            "invalid shas specified".to_string(),
        ));
    }

    // Mirror Python's `MissingCommitError` branch: on a missing commit
    // return 404 with a JSON body of `{}` so callers can `.json()` the
    // response uniformly.
    let precheck: Result<()> = async {
        ensure_revision_present(&repo_path, &codebase, &new, state.config.git.git_timeout).await?;
        if old != ZERO_SHA {
            ensure_revision_present(&repo_path, &codebase, &old, state.config.git.git_timeout)
                .await?;
        }
        Ok(())
    }
    .await;
    match precheck {
        Ok(()) => {}
        Err(GitStoreError::RevisionNotFound { .. }) => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .map_err(GitStoreError::HttpLibError);
        }
        Err(e) => return Err(e),
    }

    // NUL-separated records where each record is `sha\n<message>` so
    // commit messages can contain any characters without ambiguating
    // the parse. Direction: newest-first (git log default), matching
    // Python's dulwich walker.
    const SEP: u8 = 0;
    let mut cmd = Command::new("git");
    cmd.arg("log").arg("--format=%H%n%B%x00");
    let range = if old == ZERO_SHA {
        new.clone()
    } else {
        format!("{}..{}", old, new)
    };
    cmd.arg(&range).current_dir(&repo_path).kill_on_drop(true);

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(state.config.git.git_timeout),
        cmd.output(),
    )
    .await
    .map_err(|_| GitStoreError::Timeout)??;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!("git log {}: {}", range, stderr);
        return Err(GitStoreError::GitError(git2::Error::from_str(&stderr)));
    }

    let mut entries: Vec<RevisionInfoEntry> = Vec::new();
    for chunk in output.stdout.split(|b| *b == SEP) {
        if chunk.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }
        let s = String::from_utf8_lossy(chunk);
        let trimmed = s.trim_start();
        let mut lines = trimmed.lines();
        let sha = match lines.next() {
            Some(l) if !l.is_empty() => l.to_string(),
            _ => continue,
        };
        let message = lines.collect::<Vec<_>>().join("\n").trim().to_string();
        entries.push(RevisionInfoEntry {
            commit_id: sha.clone(),
            revision_id: format!("git-v1:{}", sha),
            link: format!("/git/{}/commit/{}/", codebase, sha),
            message,
        });
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_string(&entries).map_err(|e| GitStoreError::Other(e.into()))?,
        ))
        .map_err(GitStoreError::HttpLibError)
}

#[derive(Debug, Deserialize)]
pub struct LogQuery {
    /// Exclusive lower bound. Absent means "from the root of `new`".
    old: Option<String>,
    /// Inclusive upper bound.
    new: String,
}

/// `git log old..new` (or `git log new` when `old` is absent),
/// oldest first, with author info and message.
pub async fn git_log(
    State(state): State<crate::web::AppState>,
    Path(codebase): Path<String>,
    Query(params): Query<LogQuery>,
) -> Result<Response> {
    crate::repository::RepositoryManager::validate_sha(&params.new)?;
    if let Some(old) = params.old.as_deref() {
        if !old.is_empty() {
            crate::repository::RepositoryManager::validate_sha(old)?;
        }
    }

    let repo_path = state.repo_manager.repo_path(&codebase);
    if !repo_path.exists() {
        return Err(GitStoreError::LocalRepositoryUnavailable(codebase));
    }

    // NUL-terminated record format so commit messages can contain
    // anything (including blank lines) without ambiguating the parse.
    const SEP: u8 = 0;
    let mut cmd = Command::new("git");
    cmd.arg("log")
        .arg("--reverse")
        .arg("--format=%H%n%an%n%ae%n%at%n%B%x00");
    let range = match params.old.as_deref() {
        Some(old) if !old.is_empty() => format!("{}..{}", old, params.new),
        _ => params.new.clone(),
    };
    cmd.arg(&range).current_dir(&repo_path).kill_on_drop(true);

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(state.config.git.git_timeout),
        cmd.output(),
    )
    .await
    .map_err(|_| GitStoreError::Timeout)??;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!("git log {}: {}", range, stderr);
        return Err(GitStoreError::GitError(git2::Error::from_str(&stderr)));
    }

    let mut commits = Vec::new();
    for chunk in output.stdout.split(|b| *b == SEP) {
        if chunk.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }
        // git appends '\n' after each record, so chunks 2..N start
        // with a bridging newline; trim before reading the SHA.
        let s = String::from_utf8_lossy(chunk);
        let trimmed = s.trim_start();
        let mut lines = trimmed.lines();
        let sha = match lines.next() {
            Some(l) if !l.is_empty() => l.to_string(),
            _ => continue,
        };
        let author_name = lines.next().unwrap_or("").to_string();
        let author_email = lines.next().unwrap_or("").to_string();
        let author_ts: i64 = lines.next().and_then(|l| l.parse().ok()).unwrap_or(0);
        let message = lines.collect::<Vec<_>>().join("\n").trim().to_string();
        commits.push(LogEntry {
            sha,
            author: GitPerson {
                name: author_name,
                email: author_email,
                timestamp: author_ts,
            },
            message,
        });
    }

    let body = LogResponse { commits };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_string(&body).map_err(|e| GitStoreError::Other(e.into()))?,
        ))
        .map_err(GitStoreError::HttpLibError)
}

async fn extract_auth_context(
    headers: &HeaderMap,
    state: &crate::web::AppState,
) -> Result<AuthContext> {
    if state.role == crate::web::AppRole::Admin {
        return Ok(AuthContext {
            allow_writes: true,
            worker_name: None,
        });
    }

    let worker_name = crate::web::is_worker_request(&state.db_manager, headers).await?;
    Ok(AuthContext {
        allow_writes: worker_name.is_some(),
        worker_name,
    })
}

fn validate_git_command(
    service: Option<&String>,
    path_info: &str,
    auth_context: &AuthContext,
) -> Result<()> {
    if path_info.contains("..") || path_info.contains("//") || path_info.starts_with('/') {
        warn!("Dangerous path detected: {}", path_info);
        return Err(GitStoreError::HttpError("Invalid path".to_string()));
    }

    if let Some(service) = service {
        validate_git_service(service, auth_context)?;
    }

    Ok(())
}

fn validate_git_service(service: &str, auth_context: &AuthContext) -> Result<()> {
    match service {
        "git-upload-pack" => Ok(()),
        "git-receive-pack" => {
            // Return 401 rather than 403 so clients with URL userinfo
            // retry once challenged. Without the 401, breezy/dulwich
            // treat 403 as terminal.
            if auth_context.allow_writes {
                info!(
                    "receive-pack allowed for {}",
                    auth_context.worker_name.as_deref().unwrap_or("admin")
                );
                Ok(())
            } else {
                Err(GitStoreError::AuthenticationFailed)
            }
        }
        // Python `_git_check_service` raises HTTPForbidden here.
        _ => Err(GitStoreError::PermissionDenied),
    }
}

/// Copy the request headers into the CGI env dict as `HTTP_*`,
/// filtering out:
/// * hop-by-hop headers (RFC 7230 §6.1) that describe the connection
///   to the proxy, not the end-to-end request;
/// * `Content-Encoding`, because axum has already decompressed the
///   body and git would otherwise try to decode a second time.
///
/// Pulled out so it can be unit-tested without spawning a real
/// subprocess.
fn forward_request_headers(headers: &HeaderMap, env_vars: &mut HashMap<String, String>) {
    for (name, value) in headers.iter() {
        if is_hop_by_hop(name.as_str()) {
            continue;
        }
        if name.as_str().eq_ignore_ascii_case("content-encoding") {
            continue;
        }
        if let Ok(value_str) = value.to_str() {
            let env_name = format!("HTTP_{}", name.as_str().replace('-', "_").to_uppercase());
            env_vars.insert(env_name, value_str.to_string());
        }
    }
}

/// RFC 7230 §6.1 hop-by-hop header names. These describe the single
/// connection to the proxy and must not be forwarded to the CGI
/// program or echoed back from it.
fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

/// Mirror the regex set in `dulwich.web.HTTPGitApplication.services`
/// so we don't shell out to `git http-backend` for arbitrary URLs.
/// `method` is the request method and `subpath` is the path after the
/// `/{codebase}/` prefix (without a leading slash).
fn is_git_http_subpath(method: &axum::http::Method, subpath: &str) -> bool {
    use axum::http::Method;
    match (method.as_str(), subpath) {
        ("POST", "git-upload-pack") | ("POST", "git-receive-pack") => true,
        (m, s) if m == Method::GET.as_str() => {
            matches!(
                s,
                "HEAD"
                    | "info/refs"
                    | "objects/info/alternates"
                    | "objects/info/http-alternates"
                    | "objects/info/packs"
            ) || is_loose_object_path(s)
                || is_pack_path(s)
        }
        _ => false,
    }
}

/// `objects/<2hex>/<38hex>` -- a loose git object. Pure hex check.
fn is_loose_object_path(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("objects/") else {
        return false;
    };
    let Some((prefix, suffix)) = rest.split_once('/') else {
        return false;
    };
    prefix.len() == 2
        && suffix.len() == 38
        && prefix.chars().all(|c| c.is_ascii_hexdigit())
        && suffix.chars().all(|c| c.is_ascii_hexdigit())
}

/// `objects/pack/pack-<40|64 hex>.pack` or `.idx`.
fn is_pack_path(s: &str) -> bool {
    let Some(name) = s.strip_prefix("objects/pack/") else {
        return false;
    };
    let (stem, ext) = match name.rsplit_once('.') {
        Some(parts) => parts,
        None => return false,
    };
    if ext != "pack" && ext != "idx" {
        return false;
    }
    // `pack-<hex>` is the convention; the leading `pack-` prefix is
    // not strictly required by dulwich's regex (`\w+-`) but every
    // real-world file matches it.
    let Some(hex) = stem.rsplit_once('-').map(|(_, h)| h) else {
        return false;
    };
    (hex.len() == 40 || hex.len() == 64) && hex.chars().all(|c| c.is_ascii_hexdigit())
}

/// Strip breezy's `,branch=<role>` segment parameter (both the
/// literal and URL-encoded forms). Breezy sends these for colocated
/// branches but git-store keys codebases on the bare name.
fn strip_branch_segment(segment: &str) -> &str {
    for marker in [",branch=", "%2Cbranch%3D", "%2cbranch%3d"] {
        if let Some(idx) = segment.find(marker) {
            return &segment[..idx];
        }
    }
    segment
}

const GIT_BACKEND_TERM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const GIT_BACKEND_REAP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Signal the whole process group. A missing group (ESRCH) is treated
/// as success: it just means the backend already exited. Any other
/// errno is logged and swallowed -- we never want to panic trying to
/// clean up a child.
#[cfg(unix)]
fn signal_backend_group(raw_pid: u32, signal: nix::sys::signal::Signal) {
    use nix::sys::signal::killpg;
    use nix::unistd::Pid;
    match killpg(Pid::from_raw(raw_pid as i32), signal) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
        Err(e) => warn!(
            "git http-backend: failed to {} process group {}: {}",
            signal, raw_pid, e
        ),
    }
}

/// Wait up to `limit` for the backend to exit. Returns `true` if it
/// did.
async fn wait_backend(process: &mut tokio::process::Child, limit: std::time::Duration) -> bool {
    tokio::time::timeout(limit, process.wait()).await.is_ok()
}

/// Scope guard: on drop, SIGTERM the http-backend process group,
/// wait up to `GIT_BACKEND_TERM_TIMEOUT`, then SIGKILL and wait up
/// to `GIT_BACKEND_REAP_TIMEOUT`. SIGTERM first lets git remove
/// its own ref lockfiles.
struct BackendReaper(Option<tokio::process::Child>);

impl BackendReaper {
    fn new(process: tokio::process::Child) -> Self {
        Self(Some(process))
    }

    /// `None` unless the backend has exited.
    fn exit_status(&mut self) -> Option<std::process::ExitStatus> {
        self.0.as_mut().and_then(|p| p.try_wait().ok().flatten())
    }
}

/// Whether an empty reply that still carries 200 means the request body
/// never arrived in full.
fn is_incomplete_request_body(
    body_truncated: bool,
    body_empty: bool,
    backend_failed: bool,
    status_code: StatusCode,
) -> bool {
    body_truncated && body_empty && backend_failed && status_code == StatusCode::OK
}

impl Drop for BackendReaper {
    fn drop(&mut self) {
        let Some(mut process) = self.0.take() else {
            return;
        };
        if matches!(process.try_wait(), Ok(Some(_))) {
            return;
        }
        tokio::spawn(async move { reap_backend(&mut process).await });
    }
}

async fn reap_backend(process: &mut tokio::process::Child) {
    let Some(raw_pid) = process.id() else {
        // Already reaped by someone else; just surface any error.
        if let Err(e) = process.wait().await {
            warn!("git http-backend wait failed: {}", e);
        }
        return;
    };

    #[cfg(unix)]
    {
        use nix::sys::signal::Signal;
        signal_backend_group(raw_pid, Signal::SIGTERM);
        if wait_backend(process, GIT_BACKEND_TERM_TIMEOUT).await {
            return;
        }
        signal_backend_group(raw_pid, Signal::SIGKILL);
        if !wait_backend(process, GIT_BACKEND_REAP_TIMEOUT).await {
            warn!(
                "git http-backend session {} still alive {}s after SIGKILL",
                raw_pid,
                GIT_BACKEND_REAP_TIMEOUT.as_secs()
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = process.start_kill();
        let _ = wait_backend(process, GIT_BACKEND_REAP_TIMEOUT).await;
    }
}

/// Write-error kinds treated as git closing the pipe.
fn is_expected_stdin_close(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::UnexpectedEof
    )
}

/// Delegate smart-protocol requests to `git http-backend`.
pub async fn git_backend(
    State(state): State<crate::web::AppState>,
    req: Request,
) -> Result<Response> {
    let uri = req.uri().clone();
    let method = req.method().clone();
    let headers = req.headers().clone();
    let body = req.into_body();

    // Strip optional `/git/` prefix so this handler serves both the
    // admin and public route trees.
    let path_trimmed = uri
        .path()
        .trim_start_matches('/')
        .strip_prefix("git/")
        .unwrap_or_else(|| uri.path().trim_start_matches('/'));
    let path_segments: Vec<&str> = path_trimmed.split('/').collect();

    if path_segments.is_empty() || path_segments[0].is_empty() {
        return Err(GitStoreError::HttpError(
            "Missing codebase in path".to_string(),
        ));
    }

    let codebase = strip_branch_segment(path_segments[0]);
    let subpath = if path_segments.len() > 1 {
        path_segments[1..].join("/")
    } else {
        String::new()
    };

    // Only accept paths that `dulwich.web.HTTPGitApplication.services`
    // declares. Catches typos / probes before shelling out to
    // git-http-backend. Mirrors Python's regex-mounted routes.
    if !is_git_http_subpath(&method, &subpath) {
        return Err(GitStoreError::GitPathNotFound(subpath));
    }

    debug!("git http-backend: {} {} {}", method, codebase, subpath);

    let auth_context = extract_auth_context(&headers, &state).await?;

    if !state.db_manager.codebase_exists(codebase).await? {
        warn!("codebase {} not found in database", codebase);
        return Err(GitStoreError::RepositoryNotFound(codebase.to_string()));
    }

    let repo_path = state.repo_manager.repo_path(codebase);
    if !repo_path.exists() {
        info!("auto-creating repository for {}", codebase);
        state.repo_manager.open_or_create(codebase)?;
    }

    // Extract request information
    let method_str = method.as_str();

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let query_string = uri.query().unwrap_or("");

    // Parse query for service parameter
    let query_params: HashMap<String, String> =
        serde_urlencoded::from_str(query_string).unwrap_or_default();

    let service = query_params.get("service");

    // Validate Git command with enhanced security checks
    validate_git_command(service, &subpath, &auth_context)?;

    // Setup Git HTTP backend process
    let mut cmd = Command::new("git");
    if auth_context.allow_writes {
        cmd.args(["-c", "http.receivepack=1"]);
    }
    cmd.arg("http-backend");

    // Setup environment variables for Git HTTP backend
    let mut env_vars = HashMap::new();
    env_vars.insert("GIT_HTTP_EXPORT_ALL".to_string(), "true".to_string());
    env_vars.insert("REQUEST_METHOD".to_string(), method_str.to_string());
    env_vars.insert("CONTENT_TYPE".to_string(), content_type.to_string());
    env_vars.insert("QUERY_STRING".to_string(), query_string.to_string());

    // Set the repository path
    let full_path = repo_path.join(subpath.trim_start_matches('/'));
    env_vars.insert(
        "PATH_TRANSLATED".to_string(),
        full_path.display().to_string(),
    );
    env_vars.insert(
        "GIT_PROJECT_ROOT".to_string(),
        repo_path.display().to_string(),
    );
    // git-http-backend refuses to run without PATH_INFO even when
    // GIT_PROJECT_ROOT is set.
    env_vars.insert(
        "PATH_INFO".to_string(),
        format!("/{}", subpath.trim_start_matches('/')),
    );

    forward_request_headers(&headers, &mut env_vars);

    for (key, value) in env_vars {
        cmd.env(key, value);
    }

    // No `kill_on_drop`: SIGKILL races git's last writes and
    // truncates the response for dulwich/breezy clients. Reaped via
    // the scope guard below instead.
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Own session/group so grandchildren that inherit the
    // stdout/stderr pipes are reachable via killpg.
    #[cfg(unix)]
    cmd.process_group(0);

    let mut process = cmd
        .spawn()
        .map_err(|e| GitStoreError::Other(anyhow::anyhow!("failed to spawn git: {}", e)))?;

    // Feed the request body to git's stdin, closing it on EOF. A failing
    // read and a failing write are handled separately.
    let body_truncated = Arc::new(AtomicBool::new(false));
    if let Some(mut stdin) = process.stdin.take() {
        let mut body_stream = body.into_data_stream();
        let body_truncated = Arc::clone(&body_truncated);
        tokio::spawn(async move {
            while let Some(chunk) = body_stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        if let Err(e) = stdin.write_all(&bytes).await {
                            if is_expected_stdin_close(e.kind()) {
                                debug!("git closed its stdin before the body ended: {}", e);
                            } else {
                                warn!("Error writing to git process stdin: {}", e);
                            }
                            break;
                        }
                    }
                    Err(e) => {
                        body_truncated.store(true, Ordering::SeqCst);
                        debug!("request body did not arrive in full: {}", e);
                        break;
                    }
                }
            }
        });
    }

    if let Some(stderr) = process.stderr.take() {
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                warn!("git http-backend stderr: {}", line);
            }
        });
    }

    let stdout = process
        .stdout
        .take()
        .ok_or_else(|| GitStoreError::Other(anyhow::anyhow!("no stdout on git process")))?;

    let mut reaper = BackendReaper::new(process);
    let mut reader = tokio::io::BufReader::new(stdout);

    let mut response_headers = HeaderMap::new();
    let mut status_code = StatusCode::OK;
    let mut content_length: Option<usize> = None;

    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .map_err(|e| GitStoreError::Other(anyhow::anyhow!("read git response: {}", e)))?;

        if line.trim().is_empty() {
            break;
        }

        if let Some((key, value)) = line.trim().split_once(':') {
            let key = key.trim().to_string();
            let value = value.trim().to_string();

            if key.eq_ignore_ascii_case("status") {
                // CGI `Status: 200 OK` -> HTTP status line.
                if let Some(code_str) = value.split_whitespace().next() {
                    if let Ok(code) = code_str.parse::<u16>() {
                        status_code = StatusCode::from_u16(code).unwrap_or(StatusCode::OK);
                    }
                }
                continue;
            }
            // Don't echo hop-by-hop headers back to the client; axum
            // will set them according to the response body's framing.
            if is_hop_by_hop(&key) {
                continue;
            }
            if key.eq_ignore_ascii_case("content-length") {
                content_length = value.parse().ok();
                if let Ok(header_value) = value.parse::<http::HeaderValue>() {
                    response_headers.insert(header::CONTENT_LENGTH, header_value);
                }
            } else if let Ok(header_name) = key.parse::<http::HeaderName>() {
                if let Ok(header_value) = value.parse::<http::HeaderValue>() {
                    response_headers.insert(header_name, header_value);
                }
            }
        }
    }

    // Buffer the body: streaming via `Body::from_stream` triggered
    // "Transport error: Connection closed early" on dulwich/breezy
    // clients because they expected either Content-Length or clean
    // chunked framing.
    let body_data = if let Some(length) = content_length {
        let mut buf = vec![0u8; length];
        reader
            .read_exact(&mut buf)
            .await
            .map_err(|e| GitStoreError::Other(anyhow::anyhow!("read git body: {}", e)))?;
        buf
    } else {
        let mut buf = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut buf)
            .await
            .map_err(|e| GitStoreError::Other(anyhow::anyhow!("read git body: {}", e)))?;
        buf
    };

    let backend_failed = reaper.exit_status().is_some_and(|s| !s.success());
    if is_incomplete_request_body(
        body_truncated.load(Ordering::SeqCst),
        body_data.is_empty(),
        backend_failed,
        status_code,
    ) {
        return Err(GitStoreError::IncompleteRequestBody);
    }

    let mut response = Response::builder().status(status_code);
    for (name, value) in response_headers.iter() {
        response = response.header(name, value);
    }

    Ok(response.body(Body::from(body_data))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_incomplete_request_body_needs_a_non_zero_exit() {
        assert!(!is_incomplete_request_body(
            true,
            true,
            false,
            StatusCode::OK
        ));
        assert!(is_incomplete_request_body(true, true, true, StatusCode::OK));
    }

    #[test]
    fn test_is_incomplete_request_body_requires_every_condition() {
        assert!(!is_incomplete_request_body(
            false,
            true,
            true,
            StatusCode::OK
        ));
        assert!(!is_incomplete_request_body(
            true,
            false,
            true,
            StatusCode::OK
        ));
        assert!(!is_incomplete_request_body(
            true,
            true,
            true,
            StatusCode::BAD_REQUEST
        ));
    }

    #[test]
    fn test_is_expected_stdin_close_accepts_the_close_kinds() {
        for kind in [
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::UnexpectedEof,
        ] {
            assert!(
                is_expected_stdin_close(kind),
                "{:?} should be treated as an expected stdin close",
                kind
            );
        }
    }

    #[test]
    fn test_is_expected_stdin_close_rejects_other_kinds() {
        for kind in [
            std::io::ErrorKind::Other,
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::WriteZero,
        ] {
            assert!(
                !is_expected_stdin_close(kind),
                "{:?} should not be treated as an expected stdin close",
                kind
            );
        }
    }

    #[test]
    fn test_diff_query_parsing() {
        let query = "old=abc123&new=def456&path=src/main.rs";
        let params: DiffQuery = serde_urlencoded::from_str(query).unwrap();
        assert_eq!(params.old.as_deref(), Some("abc123"));
        assert_eq!(params.new.as_deref(), Some("def456"));
        assert_eq!(params.path, Some("src/main.rs".to_string()));
    }

    #[test]
    fn test_forward_request_headers_strips_content_encoding() {
        let mut headers = HeaderMap::new();
        headers.insert("content-encoding", "gzip".parse().unwrap());
        headers.insert(
            "content-type",
            "application/x-git-receive-pack-request".parse().unwrap(),
        );
        headers.insert("user-agent", "git/2.55".parse().unwrap());

        let mut env = HashMap::new();
        forward_request_headers(&headers, &mut env);

        assert!(
            !env.contains_key("HTTP_CONTENT_ENCODING"),
            "Content-Encoding must not reach the CGI env (would cause double-decode): {:?}",
            env
        );
        assert_eq!(
            env.get("HTTP_CONTENT_TYPE").map(String::as_str),
            Some("application/x-git-receive-pack-request")
        );
        assert_eq!(
            env.get("HTTP_USER_AGENT").map(String::as_str),
            Some("git/2.55")
        );
    }

    #[test]
    fn test_forward_request_headers_strips_hop_by_hop() {
        let mut headers = HeaderMap::new();
        headers.insert("connection", "close".parse().unwrap());
        headers.insert("transfer-encoding", "chunked".parse().unwrap());
        headers.insert("upgrade", "h2c".parse().unwrap());
        headers.insert("authorization", "Basic dXNlcjpwYXNz".parse().unwrap());

        let mut env = HashMap::new();
        forward_request_headers(&headers, &mut env);

        for banned in ["HTTP_CONNECTION", "HTTP_TRANSFER_ENCODING", "HTTP_UPGRADE"] {
            assert!(
                !env.contains_key(banned),
                "{} is hop-by-hop and must not be forwarded: {:?}",
                banned,
                env
            );
        }
        assert_eq!(
            env.get("HTTP_AUTHORIZATION").map(String::as_str),
            Some("Basic dXNlcjpwYXNz")
        );
    }

    #[test]
    fn test_is_hop_by_hop() {
        for h in [
            "Connection",
            "connection",
            "Keep-Alive",
            "Proxy-Authenticate",
            "Proxy-Authorization",
            "TE",
            "Trailer",
            "Transfer-Encoding",
            "Upgrade",
        ] {
            assert!(is_hop_by_hop(h), "should be hop-by-hop: {}", h);
        }
        for h in [
            "Content-Type",
            "Content-Length",
            "Authorization",
            "User-Agent",
        ] {
            assert!(!is_hop_by_hop(h), "should not be hop-by-hop: {}", h);
        }
    }

    #[test]
    fn test_is_git_http_subpath_whitelist() {
        use axum::http::Method;
        let get = Method::GET;
        let post = Method::POST;
        // Smart protocol
        assert!(is_git_http_subpath(&post, "git-upload-pack"));
        assert!(is_git_http_subpath(&post, "git-receive-pack"));
        assert!(!is_git_http_subpath(&get, "git-upload-pack"));
        // Refs discovery / HEAD
        assert!(is_git_http_subpath(&get, "info/refs"));
        assert!(is_git_http_subpath(&get, "HEAD"));
        // Dumb-HTTP metadata
        assert!(is_git_http_subpath(&get, "objects/info/alternates"));
        assert!(is_git_http_subpath(&get, "objects/info/http-alternates"));
        assert!(is_git_http_subpath(&get, "objects/info/packs"));
        // Loose object: 2 hex + '/' + 38 hex (SHA-1 object).
        assert!(is_git_http_subpath(
            &get,
            "objects/ab/23456789abcdef0123456789abcdef01234567"
        ));
        assert!(!is_git_http_subpath(
            &get,
            "objects/zz/23456789abcdef0123456789abcdef01234567"
        ));
        assert!(!is_git_http_subpath(&get, "objects/ab/short"));
        // Pack files
        assert!(is_git_http_subpath(
            &get,
            "objects/pack/pack-0123456789abcdef0123456789abcdef01234567.pack"
        ));
        assert!(is_git_http_subpath(
            &get,
            "objects/pack/pack-0123456789abcdef0123456789abcdef01234567.idx"
        ));
        assert!(!is_git_http_subpath(
            &get,
            "objects/pack/pack-notvalid.pack"
        ));
        // Rejections
        assert!(!is_git_http_subpath(&get, "config"));
        assert!(!is_git_http_subpath(&get, "../etc/passwd"));
        assert!(!is_git_http_subpath(&get, ""));
    }

    #[test]
    fn test_is_valid_hexsha() {
        assert!(is_valid_hexsha("0123456789abcdef0123456789abcdef01234567"));
        assert!(is_valid_hexsha("ABCDEF0123456789ABCDEF0123456789ABCDEF01"));
        assert!(!is_valid_hexsha("tooshort"));
        assert!(!is_valid_hexsha("0123456789abcdef0123456789abcdef0123456g"));
        assert!(!is_valid_hexsha(""));
    }

    #[test]
    fn test_revision_info_query_parsing() {
        let query = "old=abc123&new=def456";
        let params: RevisionInfoQuery = serde_urlencoded::from_str(query).unwrap();
        assert_eq!(params.old.as_deref(), Some("abc123"));
        assert_eq!(params.new.as_deref(), Some("def456"));
    }

    /// Missing `old` or `new` on the query string must surface as a
    /// 400 with Python's "need both old and new" text, not axum's
    /// default 422 rejection body.
    #[test]
    fn test_revision_info_query_missing() {
        let params: RevisionInfoQuery = serde_urlencoded::from_str("new=def456").unwrap();
        assert!(params.old.is_none());
    }

    fn writer_ctx() -> AuthContext {
        AuthContext {
            worker_name: Some("worker-1".to_string()),
            allow_writes: true,
        }
    }

    fn reader_ctx() -> AuthContext {
        AuthContext {
            worker_name: None,
            allow_writes: false,
        }
    }

    #[test]
    fn test_validate_git_command_rejects_dot_dot() {
        let result = validate_git_command(
            Some(&"git-upload-pack".to_string()),
            "../etc",
            &reader_ctx(),
        );
        assert!(matches!(result, Err(GitStoreError::HttpError(_))));
    }

    #[test]
    fn test_validate_git_command_rejects_double_slash() {
        let result = validate_git_command(
            Some(&"git-upload-pack".to_string()),
            "foo//bar",
            &reader_ctx(),
        );
        assert!(matches!(result, Err(GitStoreError::HttpError(_))));
    }

    #[test]
    fn test_validate_git_command_rejects_absolute_path() {
        let result = validate_git_command(
            Some(&"git-upload-pack".to_string()),
            "/etc/passwd",
            &reader_ctx(),
        );
        assert!(matches!(result, Err(GitStoreError::HttpError(_))));
    }

    #[test]
    fn test_validate_git_command_accepts_normal_path() {
        let result = validate_git_command(
            Some(&"git-upload-pack".to_string()),
            "info/refs",
            &reader_ctx(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_git_command_accepts_no_service() {
        let result = validate_git_command(None, "info/refs", &reader_ctx());
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_git_service_upload_pack_always_allowed() {
        assert!(validate_git_service("git-upload-pack", &reader_ctx()).is_ok());
        assert!(validate_git_service("git-upload-pack", &writer_ctx()).is_ok());
    }

    /// Receive-pack yields 401 (not 403) when unauthenticated, so
    /// breezy/dulwich retry with URL userinfo.
    #[test]
    fn test_validate_git_service_receive_pack_requires_writer() {
        let result = validate_git_service("git-receive-pack", &reader_ctx());
        assert!(matches!(result, Err(GitStoreError::AuthenticationFailed)));

        let result = validate_git_service("git-receive-pack", &writer_ctx());
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_git_service_unknown_rejected() {
        let result = validate_git_service("git-future-service", &writer_ctx());
        assert!(matches!(result, Err(GitStoreError::PermissionDenied)));

        let result = validate_git_service("", &writer_ctx());
        assert!(matches!(result, Err(GitStoreError::PermissionDenied)));
    }

    #[test]
    fn test_validate_git_command_path_check_runs_before_service() {
        let result = validate_git_command(
            Some(&"git-upload-pack".to_string()),
            "../escape",
            &writer_ctx(),
        );
        assert!(matches!(result, Err(GitStoreError::HttpError(_))));
    }
}
