//! Admin and public web apps.
//!
//! Modelled after `git-store/src/web.rs`. The admin app is
//! unauthenticated and mounted at the repo root; the public app lives
//! under `/bzr/` and gates writes on HTTP Basic worker credentials.

use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use tera::{Context, Tera};
use tokio::fs;
use tracing::warn;

use crate::api_types::{
    RemoteConfiguredResponse, RemoteInfo, RemotesListResponse, RepositoryCreatedResponse,
    RevisionInfoResponse,
};
use crate::bzr_browse::BrowseRouters;
use crate::config::Config;
use crate::database::DatabaseManager;
use crate::error::{BzrError, Result};
use crate::repository::{PyO3RepositoryManager, RepositoryManager, RepositoryPath};
use crate::smart_protocol::{
    smart_protocol_campaign_handler, smart_protocol_codebase_handler, smart_protocol_dispatch,
    smart_protocol_role_handler,
};
use crate::web_utils::{
    apply_standard_middleware, health_handler, metrics_handler, ready_handler, HealthChecker,
};

/// Which app this `AppState` belongs to. Governs write policy: `Admin`
/// always writes, `Public` requires HTTP Basic worker auth. Matches
/// `py/janitor/bzr_store.py::create_web_app` where the admin interface
/// is unauthenticated on port 9929 and the public interface on 9930 is
/// what gets worker-write gating.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppRole {
    /// Admin interface: unauthenticated and always read/write.
    Admin,
    /// Public interface: read-only unless HTTP Basic auth matches a
    /// row in the `worker` table.
    Public,
}

/// Application state shared between handlers.
#[derive(Clone)]
pub struct AppState {
    /// Application configuration.
    pub config: Config,
    /// Database connection manager for authentication and validation.
    pub database: DatabaseManager,
    /// Repository management interface for Bazaar operations.
    pub repository_manager: Arc<dyn RepositoryManager>,
    /// Template engine for rendering HTML responses.
    pub templates: Arc<Tera>,
    /// Shared health checker; same instance backs admin and public apps.
    pub health_checker: Arc<HealthChecker>,
    /// Which app this state drives (admin vs public).
    pub role: AppRole,
    /// Parsed `janitor.conf`: used to validate campaign names on smart
    /// protocol requests, matching Python's `get_campaign_config(...)`
    /// call.
    pub janitor_config: Arc<janitor::config::Config>,
    /// Insert-once cache of per-codebase loggerhead browse routers.
    pub browse_routers: BrowseRouters,
    /// Filesystem path to loggerhead's bundled CSS/JS/images. When
    /// `None`, the `/bzr/<codebase>` browse routes return 503.
    pub loggerhead_static_dir: Option<std::path::PathBuf>,
}

impl AppState {
    /// Resolve whether the current request is permitted to do writes on
    /// the smart protocol. `Admin` role always writes; `Public` role
    /// consults HTTP Basic Authorization against the `worker` table.
    pub async fn resolve_allow_writes(&self, headers: &HeaderMap) -> Result<bool> {
        match self.role {
            AppRole::Admin => Ok(true),
            AppRole::Public => Ok(is_worker_request(&self.database, headers).await?.is_some()),
        }
    }
}

/// Decode the HTTP Basic `Authorization` header and, if present, call
/// `DatabaseManager::is_worker`. Returns the worker name on success.
/// Mirrors `py/janitor/worker_creds.py::is_worker`.
pub(crate) async fn is_worker_request(
    database: &DatabaseManager,
    headers: &HeaderMap,
) -> Result<Option<String>> {
    let auth_header = match headers.get(header::AUTHORIZATION) {
        Some(h) => h,
        None => return Ok(None),
    };
    let auth_str = match auth_header.to_str() {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    let encoded = match auth_str.strip_prefix("Basic ") {
        Some(e) => e,
        None => return Ok(None),
    };
    let decoded = match BASE64.decode(encoded) {
        Ok(d) => d,
        Err(_) => return Ok(None),
    };
    let creds = match std::str::from_utf8(&decoded) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    let (username, password) = match creds.split_once(':') {
        Some(pair) => pair,
        None => return Ok(None),
    };
    database.is_worker(username, password).await
}

/// Query parameters for the diff endpoint.
#[derive(Debug, Deserialize)]
pub struct DiffQuery {
    /// Old revision identifier to diff from.
    pub old: String,
    /// New revision identifier to diff to.
    pub new: String,
}

/// Query parameters for the revision-info endpoint.
#[derive(Debug, Deserialize)]
pub struct RevisionInfoQuery {
    /// Starting revision identifier for the range.
    pub old: String,
    /// Ending revision identifier for the range.
    pub new: String,
}

/// Response shape for the admin repository listing.
#[derive(Debug, Serialize)]
pub struct RepositoryListResponse {
    /// List of repository paths in the format codebase/campaign/role.
    pub repositories: Vec<String>,
    /// Total number of repositories found.
    pub count: usize,
}

/// Build both admin and public applications.
///
/// `janitor_config` is the parsed `janitor.conf`; it's used to validate
/// campaign names on smart protocol requests (matches the Python
/// `get_campaign_config(app["config"], name)` check).
pub async fn create_applications(
    config: Config,
    janitor_config: Arc<janitor::config::Config>,
) -> Result<(Router, Router)> {
    let database = DatabaseManager::new(&config).await?;

    let repository_manager: Arc<dyn RepositoryManager> = Arc::new(PyO3RepositoryManager::new(
        config.repository_path.clone(),
        database.clone(),
        true,
    ));

    let templates = Arc::new(init_templates());

    let health_checker = Arc::new(HealthChecker::new().with_db(database.pool().clone()));

    let browse_routers = BrowseRouters::new();
    let loggerhead_static_dir = default_loggerhead_static_dir();
    if loggerhead_static_dir.is_none() {
        warn!(
            "LOGGERHEAD_STATIC_DIR not set and the crate's bundled static dir \
             could not be located; /bzr/<codebase> will return 503"
        );
    }

    let admin_state = AppState {
        config: config.clone(),
        database: database.clone(),
        repository_manager: repository_manager.clone(),
        templates: templates.clone(),
        health_checker: health_checker.clone(),
        role: AppRole::Admin,
        janitor_config: janitor_config.clone(),
        browse_routers: browse_routers.clone(),
        loggerhead_static_dir: loggerhead_static_dir.clone(),
    };
    let public_state = AppState {
        config,
        database,
        repository_manager,
        templates,
        health_checker,
        role: AppRole::Public,
        janitor_config,
        browse_routers,
        loggerhead_static_dir,
    };

    let client_max_size: usize = usize::MAX;
    let admin_app = create_admin_app(admin_state, client_max_size);
    let public_app = create_public_app(public_state, client_max_size);
    Ok((admin_app, public_app))
}

/// Figure out where loggerhead's bundled `static/` directory lives.
///
/// Checked in order:
/// 1. `LOGGERHEAD_STATIC_DIR` env var (lets packagers point us at
///    `/usr/share/loggerhead/static` or similar).
/// 2. `${CARGO_MANIFEST_DIR}/../target/..` guess so `cargo run` works
///    out of a checked-out source tree.
fn default_loggerhead_static_dir() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("LOGGERHEAD_STATIC_DIR") {
        let pb = std::path::PathBuf::from(p);
        if pb.is_dir() {
            return Some(pb);
        }
    }
    // Fallback: search cargo's registry for the loggerhead crate's
    // bundled assets. This isn't guaranteed to resolve in production
    // installs but makes `cargo run` and `cargo test` work out of the
    // box.
    let home = std::env::var("CARGO_HOME")
        .ok()
        .or_else(|| std::env::var("HOME").ok().map(|h| format!("{}/.cargo", h)))?;
    let candidates = std::fs::read_dir(format!("{}/registry/src", home)).ok()?;
    for entry in candidates.flatten() {
        let sub = entry.path();
        if let Ok(index) = std::fs::read_dir(&sub) {
            for lh in index.flatten() {
                let name = lh.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("loggerhead-") {
                    let s = lh.path().join("static");
                    if s.is_dir() {
                        return Some(s);
                    }
                }
            }
        }
    }
    None
}

fn web_config(client_max_size: usize) -> janitor::shared_config::WebConfig {
    janitor::shared_config::WebConfig {
        enable_compression: true,
        enable_request_logging: true,
        enable_cors: false,
        // Bzr smart-protocol pushes can run long on big fetches.
        request_timeout_seconds: 600,
        max_request_size_bytes: if client_max_size == 0 {
            usize::MAX
        } else {
            client_max_size
        },
        ..janitor::shared_config::WebConfig::default()
    }
}

fn body_limit_layer(client_max_size: usize) -> DefaultBodyLimit {
    if client_max_size == 0 {
        DefaultBodyLimit::disable()
    } else {
        DefaultBodyLimit::max(client_max_size)
    }
}

/// Sub-router carrying the health / ready / metrics endpoints under
/// their own `Arc<HealthChecker>` state.
fn health_router(checker: Arc<HealthChecker>) -> Router {
    Router::new()
        .route("/health", get(health_handler))
        .route("/ready", get(ready_handler))
        .route("/metrics", get(metrics_handler))
        .with_state(checker)
}

/// Create the admin application with full access.
///
/// Matches `py/janitor/bzr_store.py::create_web_app` admin-side: no
/// authentication middleware. Writes are always allowed on this app;
/// the Python codebase exposes the admin interface on a private
/// address (9929) and the public one (9930) is what gets
/// worker-write gating.
pub fn create_admin_app(state: AppState, client_max_size: usize) -> Router {
    let health = health_router(state.health_checker.clone());
    let router = Router::new()
        .route("/", get(public_repo_list_handler))
        .route("/repositories", get(list_repositories_handler))
        .route("/repositories/{codebase}", post(create_repository_handler))
        .route("/{codebase}/info", get(repository_info_handler))
        .route("/{codebase}/diff", get(diff_handler))
        .route("/{codebase}/revision-info", get(revision_info_handler))
        .route(
            "/{codebase}/remotes/{remote}",
            post(configure_remote_handler),
        )
        .route("/{codebase}/remotes", get(list_remotes_handler))
        .route(
            "/{codebase}/.bzr/smart",
            post(smart_protocol_codebase_handler),
        )
        .route(
            "/{codebase}/{campaign}/.bzr/smart",
            post(smart_protocol_campaign_handler),
        )
        .route(
            "/{codebase}/{campaign}/{role}/.bzr/smart",
            post(smart_protocol_role_handler),
        )
        .with_state(state)
        .merge(health)
        .layer(body_limit_layer(client_max_size));

    apply_standard_middleware(router, &web_config(client_max_size))
}

/// Create the public application.
///
/// Matches `py/janitor/bzr_store.py::create_web_app` public-side:
/// routes live under the `/bzr/` prefix, `GET /` returns an empty home
/// response, and `GET /bzr/` content-negotiates a repo list
/// (JSON / text / HTML). The smart protocol is always mounted — the
/// *write* part is gated by `is_worker` inside the handler via
/// `AppState::resolve_allow_writes`.
///
/// `/health`, `/ready`, and `/metrics` are **not** exposed here —
/// those stay on the admin app, matching the Python route table.
/// Deployments typically only let a reverse proxy at this port, and
/// leaking `/metrics` externally is an information-disclosure risk.
///
/// New vs. Python: `/bzr/<codebase>` and sub-paths that aren't a
/// smart-protocol `.bzr/smart` POST are routed to a per-codebase
/// loggerhead browser (replaces the external WSGI loggerhead the
/// Python deployment relied on).
pub fn create_public_app(state: AppState, client_max_size: usize) -> Router {
    let router = Router::new()
        .route("/", get(home_handler))
        .route("/bzr/", get(public_repo_list_handler))
        .route("/bzr/{codebase}", get(crate::bzr_browse::bzr_browse_root))
        .route("/bzr/{codebase}/", get(crate::bzr_browse::bzr_browse_root))
        // Unified wildcard: POST dispatches to the smart protocol,
        // GET to the per-branch loggerhead router. Specific POST
        // sub-routes (/bzr/{codebase}/.bzr/smart etc.) cannot coexist
        // with the wildcard in axum 0.8 even when methods differ, so
        // we re-parse the captured sub-path here.
        .route(
            "/bzr/{codebase}/{*sub_path}",
            get(crate::bzr_browse::bzr_browse_handler).post(bzr_smart_sub_dispatcher),
        )
        .with_state(state)
        .layer(body_limit_layer(client_max_size));

    apply_standard_middleware(router, &web_config(client_max_size))
}

/// `POST /bzr/{codebase}/{*sub_path}` — route smart protocol requests
/// to the correct handler by inspecting the captured `sub_path`
/// suffix. Handles the three URL shapes the Python app supported:
/// `/.bzr/smart` (codebase), `/{campaign}/.bzr/smart`,
/// `/{campaign}/{role}/.bzr/smart`.
async fn bzr_smart_sub_dispatcher(
    State(state): State<AppState>,
    Path((codebase, sub_path)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    let parts: Vec<&str> = sub_path.trim_end_matches('/').splitn(4, '/').collect();
    match parts.as_slice() {
        [".bzr", "smart"] => {
            smart_protocol_dispatch(state, codebase, None, None, headers, body).await
        }
        [campaign, ".bzr", "smart"] => {
            smart_protocol_dispatch(
                state,
                codebase,
                Some(campaign.to_string()),
                None,
                headers,
                body,
            )
            .await
        }
        [campaign, role, ".bzr", "smart"] => {
            smart_protocol_dispatch(
                state,
                codebase,
                Some(campaign.to_string()),
                Some(role.to_string()),
                headers,
                body,
            )
            .await
        }
        _ => Ok((StatusCode::NOT_FOUND, "Not found").into_response()),
    }
}

/// `GET /` on the public app — matches
/// `py/janitor/bzr_store.py::handle_home`: an empty 200 response,
/// useful for reverse-proxy root checks.
async fn home_handler() -> Response {
    (StatusCode::OK, "").into_response()
}

/// `GET /bzr/` — content-negotiated repository listing. Returns sorted
/// codebase names in whichever of JSON, plain text, or HTML the client
/// prefers. Matches `py/janitor/bzr_store.py::handle_repo_list`.
async fn public_repo_list_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response> {
    let mut names = read_repo_names(&state.config.repository_path).await?;
    names.sort();

    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("*/*");
    match negotiate_content_type(accept) {
        Some(AcceptMatch::Json) => Ok(Json(names).into_response()),
        Some(AcceptMatch::Text) => {
            let body = names
                .into_iter()
                .map(|n| format!("{}\n", n))
                .collect::<String>();
            Ok(([(header::CONTENT_TYPE, "text/plain")], body).into_response())
        }
        Some(AcceptMatch::Html) => {
            let mut ctx = Context::new();
            ctx.insert("vcs", "bzr");
            ctx.insert("repositories", &names);
            let html = state
                .templates
                .render("repo-list.html", &ctx)
                .unwrap_or_else(|_| render_default_repo_list(&names));
            Ok(Html(html).into_response())
        }
        None => Ok(StatusCode::NOT_ACCEPTABLE.into_response()),
    }
}

enum AcceptMatch {
    Json,
    Text,
    Html,
}

/// Replicates the shape of `mimeparse.best_match(["text/html",
/// "application/json", "text/plain"], accept)` closely enough for this
/// one route. Python's mimeparse breaks ties by position in the
/// supported list (html > json > plain); `Accept: */*` or `text/*`
/// returns HTML, matching the default a browser prefers.
///
/// Returns `None` when no supported type matched — the caller should
/// respond with 406 Not Acceptable, as Python's `web.HTTPNotAcceptable`
/// does.
fn negotiate_content_type(accept: &str) -> Option<AcceptMatch> {
    let normalized: Vec<&str> = accept.split(',').map(|s| s.trim()).collect();
    let mut json = false;
    let mut text = false;
    let mut html = false;
    let mut star = false;
    for entry in &normalized {
        // Ignore `;q=` parameters; we don't rank by quality.
        let m = entry.split(';').next().unwrap_or("").trim();
        match m {
            "application/json" => json = true,
            "text/plain" => text = true,
            "text/html" => html = true,
            "*/*" | "text/*" => star = true,
            _ => {}
        }
    }
    if html || star {
        Some(AcceptMatch::Html)
    } else if json {
        Some(AcceptMatch::Json)
    } else if text {
        Some(AcceptMatch::Text)
    } else {
        None
    }
}

async fn read_repo_names(vcs_path: &std::path::Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    if !vcs_path.exists() {
        return Ok(out);
    }
    let mut entries = fs::read_dir(vcs_path).await?;
    while let Some(e) = entries.next_entry().await? {
        if let Ok(name) = e.file_name().into_string() {
            out.push(name);
        }
    }
    Ok(out)
}

fn render_default_repo_list(names: &[String]) -> String {
    let mut body = String::from(
        "<!DOCTYPE html><html><head><title>Bazaar Repositories</title></head><body><h1>Bazaar Repositories</h1><ul>",
    );
    for n in names {
        body.push_str(&format!("<li>{}</li>", n));
    }
    body.push_str("</ul></body></html>");
    body
}

/// `GET /` on the admin app — list repositories as HTML.
async fn list_repositories_handler(State(state): State<AppState>) -> Result<Response> {
    let repositories = state.repository_manager.list_repositories().await?;

    let mut context = Context::new();
    context.insert("repositories", &repositories);

    let html = state
        .templates
        .render("repositories.html", &context)
        .unwrap_or_else(|e| format!("Template error: {}", e));

    Ok(Html(html).into_response())
}

/// `POST /repositories/{codebase}` — admin: ensure the shared bzr
/// repository for `codebase` exists on disk.
async fn create_repository_handler(
    State(state): State<AppState>,
    Path(codebase): Path<String>,
) -> Result<Json<RepositoryCreatedResponse>> {
    let repo_path = RepositoryPath::codebase_only(codebase);
    let path = state
        .repository_manager
        .ensure_repository(&repo_path)
        .await?;

    let response = RepositoryCreatedResponse {
        status: "created".to_string(),
        path: repo_path.relative_path(),
        full_path: path.to_string_lossy().to_string(),
    };
    Ok(Json(response))
}

/// `GET /{codebase}/info` — admin repository metadata.
async fn repository_info_handler(
    State(state): State<AppState>,
    Path(codebase): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let repo_path = RepositoryPath::codebase_only(codebase);
    let info = state
        .repository_manager
        .get_repository_info(&repo_path)
        .await?;

    Ok(Json(serde_json::to_value(info)?))
}

/// `GET /{codebase}/diff?old=...&new=...` — admin diff endpoint.
/// Matches `py/janitor/bzr_store.py::bzr_diff_request`: produces a
/// unified diff between two revids in text/x-diff.
async fn diff_handler(
    State(state): State<AppState>,
    Path(codebase): Path<String>,
    Query(query): Query<DiffQuery>,
) -> Result<Response> {
    let repo_path = RepositoryPath::codebase_only(codebase);
    let diff = state
        .repository_manager
        .get_diff(&repo_path, &query.old, &query.new)
        .await?;

    Ok((StatusCode::OK, [("content-type", "text/x-diff")], diff).into_response())
}

/// `GET /{codebase}/revision-info?old=...&new=...` — admin revision
/// range info. Matches `py/janitor/bzr_store.py::bzr_revision_info_request`.
async fn revision_info_handler(
    State(state): State<AppState>,
    Path(codebase): Path<String>,
    Query(query): Query<RevisionInfoQuery>,
) -> Result<Json<RevisionInfoResponse>> {
    let repo_path = RepositoryPath::codebase_only(codebase);
    let revisions = state
        .repository_manager
        .get_revision_info(&repo_path, &query.old, &query.new)
        .await?;

    let response = RevisionInfoResponse { revisions };
    Ok(Json(response))
}

/// `POST /{codebase}/remotes/{remote}` — set the parent location of
/// the campaign-less branch under `<vcs_path>/<codebase>/<remote>`.
/// Matches `py/janitor/bzr_store.py::handle_set_bzr_remote`: body is
/// urlencoded with a `url` field.
async fn configure_remote_handler(
    State(state): State<AppState>,
    Path((codebase, remote)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<RemoteConfiguredResponse>> {
    // Match Python: the branch is at `<vcs_path>/<codebase>/<remote>`,
    // not the shared repo root. Model this with a RepositoryPath that
    // treats `remote` as the campaign slot (the on-disk layout is the
    // same).
    let repo_path = RepositoryPath::new(codebase, Some(remote.clone()), None);

    let url = extract_remote_url(&headers, &body)?;

    state
        .repository_manager
        .configure_remote(&repo_path, &url)
        .await?;

    let response = RemoteConfiguredResponse {
        status: "configured".to_string(),
        remote_url: url,
    };
    Ok(Json(response))
}

/// Pull the `url` parameter out of either a urlencoded form body
/// (Python's `aiohttp request.post()` default) or a JSON body with a
/// `remote_url`/`url` field. Returns a BadRequest error when neither
/// yields a URL.
fn extract_remote_url(headers: &HeaderMap, body: &Bytes) -> Result<String> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if content_type.starts_with("application/x-www-form-urlencoded") || content_type.is_empty() {
        if let Ok(params) =
            serde_urlencoded::from_bytes::<std::collections::HashMap<String, String>>(body)
        {
            if let Some(u) = params.get("url").or_else(|| params.get("remote_url")) {
                return Ok(u.clone());
            }
        }
    }
    if content_type.starts_with("application/json") {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
            if let Some(u) = v
                .get("url")
                .or_else(|| v.get("remote_url"))
                .and_then(|x| x.as_str())
            {
                return Ok(u.to_string());
            }
        }
    }
    Err(BzrError::invalid_request(
        "missing 'url' field in request body",
    ))
}

/// `GET /{codebase}/remotes` — list configured remotes. The current
/// implementation returns at most one, matching Python's single
/// `parent_location` config entry per branch.
async fn list_remotes_handler(
    State(state): State<AppState>,
    Path(codebase): Path<String>,
) -> Result<Json<RemotesListResponse>> {
    let repo_path = RepositoryPath::codebase_only(codebase);
    let fs_path = state.config.repository_path.join(&repo_path.codebase);

    if !fs_path.exists() {
        return Err(BzrError::PathNotFound {
            path: repo_path.relative_path(),
        });
    }

    let output = tokio::process::Command::new("brz")
        .args(["config", "parent_location"])
        .current_dir(&fs_path)
        .output()
        .await;

    let parent_location = match output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            stdout.trim().to_string()
        }
        _ => String::new(),
    };

    let mut remotes = Vec::new();
    if !parent_location.is_empty() {
        remotes.push(RemoteInfo {
            name: "parent".to_string(),
            url: parent_location,
        });
    }

    let response = RemotesListResponse {
        repository: repo_path.relative_path(),
        remotes,
    };
    Ok(Json(response))
}

/// Build the Tera template registry.
///
/// Called from `create_applications`; also useful in tests. Keeps
/// `repo-list.html` and `repositories.html` in-tree so a plain `cargo
/// test` run doesn't need an external templates directory.
pub fn init_templates() -> Tera {
    let mut tera = Tera::default();

    tera.add_raw_template(
        "repo-list.html",
        r#"<!DOCTYPE html>
<html>
<head><title>Bazaar Repositories</title></head>
<body>
    <h1>Bazaar Repositories</h1>
    <ul>
    {% for repo in repositories %}
        <li><a href="/bzr/{{ repo }}/">{{ repo }}</a></li>
    {% endfor %}
    </ul>
</body>
</html>"#,
    )
    .expect("add repo-list.html");

    tera.add_raw_template(
        "repositories.html",
        r#"<!DOCTYPE html>
<html>
<head><title>BZR Repositories</title></head>
<body>
    <h1>Bazaar Repositories</h1>
    <ul>
    {% for repo in repositories %}
        <li>{{ repo.path.codebase }}{% if repo.exists %} (present){% else %} (missing){% endif %}</li>
    {% endfor %}
    </ul>
    <p>Total: {{ repositories | length }}</p>
</body>
</html>"#,
    )
    .expect("add repositories.html");

    tera
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiate_wildcard_prefers_html() {
        assert!(matches!(
            negotiate_content_type("*/*"),
            Some(AcceptMatch::Html)
        ));
    }

    #[test]
    fn negotiate_text_wildcard_prefers_html() {
        assert!(matches!(
            negotiate_content_type("text/*"),
            Some(AcceptMatch::Html)
        ));
    }

    #[test]
    fn negotiate_explicit_json() {
        assert!(matches!(
            negotiate_content_type("application/json"),
            Some(AcceptMatch::Json)
        ));
    }

    #[test]
    fn negotiate_explicit_plain() {
        assert!(matches!(
            negotiate_content_type("text/plain"),
            Some(AcceptMatch::Text)
        ));
    }

    #[test]
    fn negotiate_html_wins_over_json() {
        assert!(matches!(
            negotiate_content_type("text/html, application/json"),
            Some(AcceptMatch::Html)
        ));
    }

    #[test]
    fn negotiate_returns_none_on_unmatched() {
        assert!(negotiate_content_type("application/xml").is_none());
    }

    #[test]
    fn repo_list_template_renders() {
        let tera = init_templates();
        let mut ctx = Context::new();
        ctx.insert("vcs", "bzr");
        ctx.insert("repositories", &vec!["foo".to_string(), "bar".to_string()]);
        let out = tera.render("repo-list.html", &ctx).unwrap();
        assert!(out.contains("foo"));
        assert!(out.contains("bar"));
        assert!(out.contains("Bazaar Repositories"));
    }

    #[test]
    fn extract_remote_url_from_form_urlencoded() {
        let mut h = HeaderMap::new();
        h.insert(
            header::CONTENT_TYPE,
            "application/x-www-form-urlencoded".parse().unwrap(),
        );
        let body = Bytes::from_static(b"url=bzr%2Bssh%3A%2F%2Fexample.invalid%2Ffoo");
        let out = extract_remote_url(&h, &body).unwrap();
        assert_eq!(out, "bzr+ssh://example.invalid/foo");
    }

    #[test]
    fn extract_remote_url_from_json() {
        let mut h = HeaderMap::new();
        h.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        let body = Bytes::from_static(br#"{"url":"bzr://example.invalid/foo"}"#);
        let out = extract_remote_url(&h, &body).unwrap();
        assert_eq!(out, "bzr://example.invalid/foo");
    }

    #[test]
    fn extract_remote_url_missing_errors() {
        let h = HeaderMap::new();
        let body = Bytes::from_static(b"");
        assert!(extract_remote_url(&h, &body).is_err());
    }
}
