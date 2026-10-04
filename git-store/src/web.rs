//! Admin and public web apps.
//!
//! The admin app is unauthenticated with full read/write. The public
//! app is mounted under `/git/` and requires HTTP Basic worker
//! credentials for writes.

use crate::web_utils::{
    apply_standard_middleware, health_handler, metrics_handler, ready_handler, HealthChecker,
};
use crate::{
    database::DatabaseManager,
    error::{GitStoreError, Result},
    git_http::{git_backend, git_diff, git_log, revision_info},
    repository::RepositoryManager,
    Config,
};
use axum::{
    extract::{DefaultBodyLimit, Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use std::sync::Arc;
use tera::Tera;

/// Governs write policy: `Admin` always writes, `Public` requires
/// worker auth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppRole {
    Admin,
    Public,
}

#[derive(Clone)]
pub struct AppState {
    pub repo_manager: Arc<RepositoryManager>,
    pub config: Arc<Config>,
    pub tera: Arc<Tera>,
    pub db_manager: Arc<DatabaseManager>,
    pub health_checker: Arc<HealthChecker>,
    pub role: AppRole,
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

/// Decode HTTP Basic auth and check it against the `worker` table.
/// `None` for missing/malformed/invalid credentials.
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

/// Admin app: unauthenticated, mounted at the repo root.
///
/// `client_max_size` bounds request bodies in bytes; 0 means unlimited
/// (matches Python's `web.Application(client_max_size=...)`).
pub fn create_admin_app(state: AppState, client_max_size: usize) -> Router {
    let health = health_router(state.health_checker.clone());
    let router = Router::new()
        .route("/", get(list_repositories))
        .route("/{codebase}/diff", get(git_diff))
        .route("/{codebase}/revision-info", get(revision_info))
        .route("/{codebase}/log", get(git_log))
        .route("/{codebase}/remotes/{name}", post(set_remote))
        // Smart-protocol: GET for the handshake, POST for the pack
        // transfer. Dumb-HTTP metadata lives under {*path} with a
        // whitelist enforced in `git_backend`.
        .route("/{codebase}/git-upload-pack", post(git_backend))
        .route("/{codebase}/git-receive-pack", post(git_backend))
        .route("/{codebase}/info/refs", get(git_backend))
        .route("/{codebase}/HEAD", get(git_backend))
        .route("/{codebase}/{*path}", get(git_backend))
        .with_state(state)
        .merge(health)
        .layer(body_limit_layer(client_max_size));

    apply_standard_middleware(router, &web_config(client_max_size))
}

/// `client_max_size == 0` matches Python's "unlimited" convention.
fn body_limit_layer(client_max_size: usize) -> DefaultBodyLimit {
    if client_max_size == 0 {
        DefaultBodyLimit::disable()
    } else {
        DefaultBodyLimit::max(client_max_size)
    }
}

fn web_config(client_max_size: usize) -> janitor::shared_config::WebConfig {
    janitor::shared_config::WebConfig {
        enable_compression: true,
        enable_request_logging: true,
        enable_cors: true,
        // git-receive-pack on a large repo routinely runs longer than
        // the 30s default while pack-receive parses incoming packs.
        request_timeout_seconds: 600,
        // 0 (unlimited in Python) -> usize::MAX so RequestBodyLimitLayer
        // is effectively a no-op.
        max_request_size_bytes: if client_max_size == 0 {
            usize::MAX
        } else {
            client_max_size
        },
        ..janitor::shared_config::WebConfig::default()
    }
}

/// Public app: mounted under `/git/`, writes gated by worker auth.
///
/// `/health`, `/ready`, and `/metrics` are **not** exposed here —
/// those stay on the admin app, matching the Python route table in
/// `py/janitor/git_store.py`. Deployments typically only let a
/// reverse proxy at this port, and leaking `/metrics` externally is
/// an information-disclosure risk.
pub fn create_public_app(state: AppState, client_max_size: usize) -> Router {
    let router = Router::new()
        .route("/", get(home_handler))
        .route("/git/", get(list_repositories))
        // Smart-protocol: POST for the pack transfer, GET for refs.
        .route("/git/{codebase}/git-upload-pack", post(git_backend))
        .route("/git/{codebase}/git-receive-pack", post(git_backend))
        .route("/git/{codebase}/info/refs", get(git_backend))
        // axum's `{*path}` needs at least one segment, so route the
        // bare `/git/{codebase}` and `/git/{codebase}/` explicitly.
        .route(
            "/git/{codebase}",
            get(klaus_or_git_backend_root).post(klaus_or_git_backend_root),
        )
        .route(
            "/git/{codebase}/",
            get(klaus_or_git_backend_root).post(klaus_or_git_backend_root),
        )
        .route(
            "/git/{codebase}/{*path}",
            get(klaus_or_git_backend).post(klaus_or_git_backend),
        )
        .with_state(state);

    // Serve klaus's static assets from a single shared path so
    // browsers reuse the cache across repos.
    let router = match crate::klaus::klaus_static_dir() {
        Ok(dir) if dir.exists() => {
            router.nest_service("/git/_static", tower_http::services::ServeDir::new(dir))
        }
        Ok(dir) => {
            tracing::warn!(
                "klaus static dir {} missing, /git/_static disabled",
                dir.display()
            );
            router
        }
        Err(e) => {
            tracing::warn!(
                "klaus static dir lookup failed: {}, /git/_static disabled",
                e
            );
            router
        }
    };
    let router = router.layer(body_limit_layer(client_max_size));

    apply_standard_middleware(router, &web_config(client_max_size))
}

async fn home_handler() -> Response {
    (StatusCode::OK, "").into_response()
}

/// Returns None when the Accept header matches none of our supported
/// content types, matching Python's `mimeparse.best_match` returning
/// an empty string. Callers should surface that as 406 Not Acceptable.
///
/// Python passes `["text/html", "text/plain", "application/json"]` to
/// `best_match` as the supported list, which breaks ties by preference
/// order; we mirror that so a client sending `Accept: */*` gets HTML
/// and `Accept: text/plain, application/json` gets plain (not JSON).
fn negotiate_content_type(accept_header: Option<&str>) -> Option<ContentType> {
    let accept = accept_header.unwrap_or("*/*");
    let has_wildcard = accept.contains("*/*");
    let has_text_wildcard = accept.contains("text/*");
    let has_html = accept.contains("text/html");
    let has_plain = accept.contains("text/plain");
    let has_json = accept.contains("application/json");

    if has_html || has_wildcard || has_text_wildcard {
        Some(ContentType::Html)
    } else if has_plain {
        Some(ContentType::Plain)
    } else if has_json {
        Some(ContentType::Json)
    } else {
        None
    }
}

#[derive(Debug)]
enum ContentType {
    Json,
    Html,
    Plain,
}

/// Dispatch between the git smart-protocol backend and klaus.
async fn klaus_or_git_backend(
    State(state): State<AppState>,
    path: Path<(String, String)>,
    req: axum::extract::Request,
) -> Result<Response> {
    if crate::klaus::is_git_client_request(&req) {
        crate::git_http::git_backend(State(state), req).await
    } else {
        crate::klaus::handle_klaus(State(state), path, req).await
    }
}

/// Bare-codebase variant of [`klaus_or_git_backend`] with an empty
/// sub-path.
async fn klaus_or_git_backend_root(
    State(state): State<AppState>,
    Path(codebase): Path<String>,
    req: axum::extract::Request,
) -> Result<Response> {
    if crate::klaus::is_git_client_request(&req) {
        crate::git_http::git_backend(State(state), req).await
    } else {
        crate::klaus::handle_klaus(State(state), Path((codebase, String::new())), req).await
    }
}

async fn list_repositories(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Response> {
    let repos = state.repo_manager.list_repositories()?;
    let accept_header = headers.get(header::ACCEPT).and_then(|h| h.to_str().ok());

    let Some(kind) = negotiate_content_type(accept_header) else {
        return Ok((StatusCode::NOT_ACCEPTABLE, "").into_response());
    };
    match kind {
        ContentType::Json => Ok(Json(repos).into_response()),
        ContentType::Plain => {
            let text = repos
                .into_iter()
                .map(|n| format!("{}\n", n))
                .collect::<String>();
            Ok(([(header::CONTENT_TYPE, "text/plain")], text).into_response())
        }
        ContentType::Html => {
            let mut context = tera::Context::new();
            context.insert("vcs", "git");
            context.insert("repositories", &repos);
            let html = state
                .tera
                .render("index.html", &context)
                .map_err(|e| GitStoreError::Other(anyhow::anyhow!("Template error: {}", e)))?;
            Ok(Html(html).into_response())
        }
    }
}

/// Body is form-encoded with a `url` field.
///
/// Matches Python's `handle_set_git_remote` -> `_git_open_repo`:
/// unknown codebases must 404 (`no such codebase`) rather than get an
/// auto-created bare repo. The `url` field is required.
async fn set_remote(
    State(state): State<AppState>,
    Path((codebase, name)): Path<(String, String)>,
    body: String,
) -> Result<StatusCode> {
    if !state.db_manager.codebase_exists(&codebase).await? {
        return Err(GitStoreError::RepositoryNotFound(codebase));
    }
    let params: std::collections::HashMap<String, String> = serde_urlencoded::from_str(&body)
        .map_err(|e| GitStoreError::BadRequest(format!("invalid form body: {}", e)))?;
    let url = params
        .get("url")
        .ok_or_else(|| GitStoreError::BadRequest("missing 'url' field".to_string()))?;
    state.repo_manager.set_remote(&codebase, &name, url)?;
    Ok(StatusCode::OK)
}

pub fn init_templates(templates_path: Option<&std::path::Path>) -> Result<Tera> {
    let tera = if let Some(path) = templates_path {
        let pattern = path.join("**/*.html").display().to_string();
        Tera::new(&pattern)?
    } else {
        let mut tera = Tera::default();

        tera.add_raw_template(
            "base.html",
            r#"<!DOCTYPE html>
<html>
<head>
    <title>{% block title %}Git Store{% endblock %}</title>
    <meta charset="utf-8">
    <style>
        body { font-family: sans-serif; margin: 20px; }
        .repo-list { list-style: none; padding: 0; }
        .repo-item { padding: 10px; border-bottom: 1px solid #ccc; }
    </style>
</head>
<body>
    <h1>Git Store</h1>
    {% block content %}{% endblock %}
</body>
</html>"#,
        )?;

        tera.add_raw_template(
            "index.html",
            // Trailing slash matters: klaus's WSGI app dispatches off
            // SCRIPT_NAME ending at the repo root. Without it,
            // `/git/<repo>` redirects (308) to add the slash.
            r#"{% extends "base.html" %}
{% block title %}Git Store - Repositories{% endblock %}
{% block content %}
    <h2>Repositories</h2>
    <ul class="repo-list">
    {% for repo in repositories %}
        <li class="repo-item">
            <a href="/git/{{ repo }}/">{{ repo }}</a>
        </li>
    {% endfor %}
    </ul>
{% endblock %}"#,
        )?;

        tera
    };

    Ok(tera)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_templates() {
        let tera = init_templates(None).unwrap();
        assert!(tera.get_template_names().any(|name| name == "base.html"));
        assert!(tera.get_template_names().any(|name| name == "index.html"));
    }

    /// Regression for janitor.debian.net#112: links must include
    /// the `/git/` prefix and a trailing slash.
    #[test]
    fn test_index_template_links_to_git_repo_root() {
        let tera = init_templates(None).unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("repositories", &vec!["a52dec", "abi-tracker"]);
        let html = tera.render("index.html", &ctx).unwrap();

        for repo in ["a52dec", "abi-tracker"] {
            let canonical = format!("href=\"/git/{}/\"", repo);
            assert!(html.contains(&canonical), "missing {}", canonical);
            assert!(!html.contains(&format!("href=\"/{}\"", repo)));
            assert!(!html.contains(&format!("href=\"/git/{}\"", repo)));
        }
    }

    #[test]
    fn test_negotiate_content_type_json() {
        assert!(matches!(
            negotiate_content_type(Some("application/json")),
            Some(ContentType::Json)
        ));
    }

    #[test]
    fn test_negotiate_content_type_plain() {
        assert!(matches!(
            negotiate_content_type(Some("text/plain")),
            Some(ContentType::Plain)
        ));
    }

    #[test]
    fn test_negotiate_content_type_html_default() {
        assert!(matches!(
            negotiate_content_type(Some("*/*")),
            Some(ContentType::Html)
        ));
        assert!(matches!(
            negotiate_content_type(None),
            Some(ContentType::Html)
        ));
        assert!(matches!(
            negotiate_content_type(Some("text/html,application/xhtml+xml")),
            Some(ContentType::Html)
        ));
    }

    #[test]
    fn test_negotiate_content_type_unmatchable_returns_none() {
        assert!(negotiate_content_type(Some("image/png")).is_none());
        assert!(negotiate_content_type(Some("application/xml")).is_none());
    }

    /// Python's mimeparse breaks ties by position in the supported
    /// list (html > plain > json). `Accept: text/plain, application/json`
    /// must pick plain, not json.
    #[test]
    fn test_negotiate_content_type_prefers_plain_over_json_on_tie() {
        assert!(matches!(
            negotiate_content_type(Some("text/plain, application/json")),
            Some(ContentType::Plain)
        ));
    }

    /// Explicit html wins even when json is also offered.
    #[test]
    fn test_negotiate_content_type_prefers_html_over_json() {
        assert!(matches!(
            negotiate_content_type(Some("text/html,application/json")),
            Some(ContentType::Html)
        ));
    }
}
