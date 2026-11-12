//! Admin and public web apps.
//!
//! The admin app is unauthenticated with full read/write. The public
//! app is mounted under `/git/` and requires HTTP Basic worker
//! credentials for writes.

use crate::web_utils::{apply_standard_middleware, HealthChecker};
use crate::{
    database::DatabaseManager,
    error::{GitStoreError, Result},
    git_http::git_backend,
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
use janitor_types::CodebaseName;
use std::sync::Arc;
use tera::Tera;
use utoipa::openapi::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

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
fn health_routes() -> OpenApiRouter<Arc<HealthChecker>> {
    OpenApiRouter::new()
        .routes(routes!(crate::web_utils::health_handler))
        .routes(routes!(crate::web_utils::ready_handler))
        .routes(routes!(crate::web_utils::metrics_handler))
}

/// Base of the admin app's OpenAPI document. `CodebaseName` is listed
/// explicitly because it is only used in path parameters, whose schemas
/// utoipa does not collect.
#[derive(utoipa::OpenApi)]
#[openapi(components(schemas(CodebaseName)))]
struct AdminApi;

/// REST endpoints of the admin app; the git protocol routes are added
/// by [`create_admin_app`].
fn admin_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::with_openapi(<AdminApi as utoipa::OpenApi>::openapi())
        .routes(routes!(list_repositories))
        .routes(routes!(crate::git_http::git_diff))
        .routes(routes!(crate::git_http::revision_info))
        .routes(routes!(crate::git_http::git_log))
        .routes(routes!(set_remote))
}

/// OpenAPI document for the admin app. The git protocol routes are not
/// included.
pub fn admin_openapi() -> OpenApi {
    let mut openapi = admin_routes().into_openapi();
    openapi.merge(health_routes().into_openapi());
    openapi
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
    let spec = admin_openapi()
        .to_json()
        .expect("OpenAPI document can be serialized");
    let health = health_routes().with_state(state.health_checker.clone());
    let (router, _) = admin_routes()
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
        .split_for_parts();
    let router = router
        .route(
            "/openapi.json",
            get(move || async move { ([("content-type", "application/json")], spec) }),
        )
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

fn negotiate_content_type(accept_header: Option<&str>) -> Option<ContentType> {
    match janitor::content_negotiation::best_match(
        &["text/html", "application/json", "text/plain"],
        accept_header,
    )? {
        "text/html" => Some(ContentType::Html),
        "application/json" => Some(ContentType::Json),
        _ => Some(ContentType::Plain),
    }
}

#[derive(Debug)]
enum ContentType {
    Json,
    Html,
    Plain,
}

/// Dispatch between the git smart-protocol backend and klaus.
///
/// The codebase segment is taken raw because git clients may append a
/// `,branch=<role>` parameter that `git_backend` strips before
/// validating; klaus requests are validated here.
async fn klaus_or_git_backend(
    State(state): State<AppState>,
    Path((codebase, sub_path)): Path<(String, String)>,
    req: axum::extract::Request,
) -> Result<Response> {
    if crate::klaus::is_git_client_request(&req) {
        crate::git_http::git_backend(State(state), req).await
    } else {
        let codebase = parse_codebase(codebase)?;
        crate::klaus::handle_klaus(State(state), Path((codebase, sub_path)), req).await
    }
}

pub(crate) fn parse_codebase(codebase: String) -> Result<CodebaseName> {
    CodebaseName::new(codebase).map_err(|e| GitStoreError::BadRequest(e.to_string()))
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
        let codebase = parse_codebase(codebase)?;
        crate::klaus::handle_klaus(State(state), Path((codebase, String::new())), req).await
    }
}

#[utoipa::path(
    get,
    path = "/",
    responses(
        (status = 200, description = "Names of the repositories; the format depends on the Accept header",
         content((Vec<String> = "application/json"), (String = "text/plain"), (String = "text/html"))),
        (status = 406, description = "None of the offered content types is acceptable"),
    )
)]
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

/// Form body of `POST /{codebase}/remotes/{name}`; only used to describe
/// the request in the OpenAPI document, since the handler checks that
/// the codebase exists before parsing the body.
#[derive(utoipa::ToSchema)]
#[allow(dead_code)]
struct SetRemoteForm {
    /// URL of the remote.
    url: String,
}

/// Body is form-encoded with a `url` field.
///
/// Matches Python's `handle_set_git_remote` -> `_git_open_repo`:
/// unknown codebases must 404 (`no such codebase`) rather than get an
/// auto-created bare repo. The `url` field is required.
#[utoipa::path(
    post,
    path = "/{codebase}/remotes/{name}",
    params(
        ("codebase" = CodebaseName, Path, description = "Codebase name"),
        ("name" = String, Path, description = "Name of the remote"),
    ),
    request_body(content = SetRemoteForm, content_type = "application/x-www-form-urlencoded"),
    responses(
        (status = 200, description = "The remote was set"),
        (status = 400, description = "Invalid form body or missing `url`", body = String, content_type = "text/plain", headers(("X-Janitor-Error" = String, description = "Class of the error"))),
        (status = 404, description = "No such codebase", body = String, content_type = "text/plain", headers(("X-Janitor-Error" = String, description = "Class of the error"))),
        (status = 500, description = "Internal error", body = String, content_type = "text/plain", headers(("X-Janitor-Error" = String, description = "Class of the error"))),
    )
)]
async fn set_remote(
    State(state): State<AppState>,
    Path((codebase, name)): Path<(CodebaseName, String)>,
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

    fn collect_refs<'a>(value: &'a serde_json::Value, refs: &mut Vec<&'a str>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, value) in map {
                    match value {
                        serde_json::Value::String(target) if key == "$ref" => refs.push(target),
                        _ => collect_refs(value, refs),
                    }
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    collect_refs(item, refs);
                }
            }
            _ => {}
        }
    }

    /// Schemas only end up in the document when utoipa can see them from
    /// a path or the `OpenApi` derive; a `$ref` to anything else would
    /// leave clients with a broken document.
    #[test]
    fn test_openapi_refs_resolve() {
        let doc = serde_json::to_value(admin_openapi()).unwrap();
        let mut refs = vec![];
        collect_refs(&doc, &mut refs);
        assert!(!refs.is_empty());
        let missing: Vec<&str> = refs
            .into_iter()
            .filter(|r| {
                let name = r
                    .strip_prefix("#/components/schemas/")
                    .unwrap_or_else(|| panic!("unexpected $ref {r}"));
                doc["components"]["schemas"].get(name).is_none()
            })
            .collect();
        assert_eq!(missing, Vec::<&str>::new());
    }

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
    fn test_negotiate_content_type() {
        let cases = [
            (None, Some("html")),
            (Some("*/*"), Some("html")),
            (Some("text/plain"), Some("plain")),
            (Some("text/plain, application/json"), Some("json")),
            (Some("application/json, text/html;q=0.1"), Some("json")),
            (Some("image/png"), None),
        ];
        for (accept, expected) in cases {
            let actual = negotiate_content_type(accept).map(|kind| match kind {
                ContentType::Html => "html",
                ContentType::Json => "json",
                ContentType::Plain => "plain",
            });
            assert_eq!(actual, expected, "Accept: {:?}", accept);
        }
    }
}
