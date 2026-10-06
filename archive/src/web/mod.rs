//! axum handlers: serve `/dists/...` files, expose `/publish`, `/ready`,
//! `/last-publish`, `/pgp_keys`, `/gpg-key`, `/metrics`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    body::{Body, Bytes},
    extract::{Path, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri},
    response::{IntoResponse, Redirect, Response},
    routing::{get, MethodRouter},
    Json, Router,
};
use janitor_types::{Campaign, InvalidIdentifier};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tower::ServiceExt;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use tracing::{debug, info, warn};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::config::ArchiveConfig;
use crate::database::BuildManager;
use crate::error::{ArchiveError, ArchiveResult};
use crate::manager::GeneratorManager;
use crate::on_demand::OnDemandTarget;
use crate::repository::RepositoryGenerator;
use crate::scanner::PackageScanner;

mod shared;

/// Shared map of `apt_repository.name -> last successful publish
/// time`. Populated by the generator manager when a publish task
/// completes; consumed by `/ready` (missing entries -> 500) and
/// `/last-publish` (returned as a `{suite: iso8601}` object).
pub type LastPublishTimes = Arc<RwLock<HashMap<String, chrono::DateTime<chrono::Utc>>>>;

/// Construct an empty [`LastPublishTimes`] handle. Exported so the
/// service entry point in `main.rs` can share the same map between
/// the web service, the generator manager, and the periodic
/// republish loop.
pub fn new_last_publish_times() -> LastPublishTimes {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Shared state cloned into every axum handler.
///
/// `generator_manager` is optional so tests and one-shot generation
/// paths (which don't want a background scheduler) can still build
/// an `AppState`. `last_publish_times` is populated by the manager
/// when a publish job completes.
#[derive(Clone)]
#[allow(missing_docs)]
pub struct AppState {
    pub config: Arc<ArchiveConfig>,
    pub generator: Arc<RepositoryGenerator>,
    pub scanner: Arc<PackageScanner>,
    pub database: Arc<BuildManager>,
    pub generator_manager: Option<Arc<GeneratorManager>>,
    pub last_publish_times: LastPublishTimes,
}

/// Body accepted by `POST /publish`. Form body with an optional
/// `campaign` field. Absent campaign means "trigger every
/// apt_repository that consumes a debian_build campaign".
#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
pub struct PublishRequest {
    /// Optional campaign filter. When set, only apt_repositories that
    /// select this campaign are triggered.
    pub campaign: Option<Campaign>,
}

/// Response payload for `POST /publish`. Returns `{}` on success.
#[derive(Debug, Default, Serialize, utoipa::ToSchema)]
pub struct PublishResponse {}

#[derive(utoipa::OpenApi)]
struct ArchiveApi;

/// Routes for managing the archive, as opposed to the apt repositories
/// it serves. `/pgp_keys` is only routed when `signing` is set.
fn management_routes(signing: bool) -> OpenApiRouter<AppState> {
    let router = OpenApiRouter::with_openapi(<ArchiveApi as utoipa::OpenApi>::openapi())
        // Plain "ok" health endpoint. No health aggregation --
        // the periodic services keep their own /health state
        // internally.
        .routes(routes!(shared::health_ok))
        // /ready returns 500 with a list of apt_repository
        // suites that have never been published yet, plaintext
        // body listing whatever *has* been published. Handled
        // here rather than by the shared readiness handler
        // because the shared one only knows about generic
        // health checks and can't tell that a suite is still
        // missing its first Release file.
        .routes(routes!(archive_ready_handler))
        .routes(routes!(publish_repository))
        .routes(routes!(last_publish_status))
        .routes(routes!(serve_gpg_key))
        .routes(routes!(shared::metrics_ok));
    if signing {
        router.routes(routes!(handle_pgp_keys))
    } else {
        router
    }
}

/// OpenAPI document for the archive's management API. The apt
/// repositories under `/dists` are not included.
pub fn openapi() -> utoipa::openapi::OpenApi {
    management_routes(true).into_openapi()
}

/// Archive web service.
pub struct ArchiveWebService {
    state: AppState,
}

impl ArchiveWebService {
    /// Create a new archive web service. No generator manager and an
    /// empty last-publish map; suited for standalone tooling that
    /// only serves files.
    pub async fn new(
        config: ArchiveConfig,
        generator: RepositoryGenerator,
        scanner: PackageScanner,
        database: BuildManager,
    ) -> ArchiveResult<Self> {
        Self::build(
            config,
            generator,
            scanner,
            database,
            None,
            new_last_publish_times(),
        )
        .await
    }

    /// Create a web service wired to a shared generator manager and
    /// the shared [`LastPublishTimes`] map. Used by the main server
    /// entry point so `/publish`, `/ready`, `/last-publish` all
    /// observe the same publish state.
    pub async fn with_publish_observer(
        config: ArchiveConfig,
        generator: RepositoryGenerator,
        scanner: PackageScanner,
        database: BuildManager,
        generator_manager: Arc<GeneratorManager>,
        last_publish_times: LastPublishTimes,
    ) -> ArchiveResult<Self> {
        Self::build(
            config,
            generator,
            scanner,
            database,
            Some(generator_manager),
            last_publish_times,
        )
        .await
    }

    async fn build(
        config: ArchiveConfig,
        generator: RepositoryGenerator,
        scanner: PackageScanner,
        database: BuildManager,
        generator_manager: Option<Arc<GeneratorManager>>,
        last_publish_times: LastPublishTimes,
    ) -> ArchiveResult<Self> {
        let state = AppState {
            config: Arc::new(config),
            generator: Arc::new(generator),
            scanner: Arc::new(scanner),
            database: Arc::new(database),
            generator_manager,
            last_publish_times,
        };

        Ok(Self { state })
    }

    /// Create the Axum router with all routes.
    pub fn router(&self) -> Router {
        let (mut router, openapi) =
            management_routes(self.state.config.gpg.is_some()).split_for_parts();
        // Every route with the methods it accepts, to tell which
        // normalized paths to redirect to.
        let mut known = matchit::Router::new();
        for (path, item) in &openapi.paths.paths {
            known
                .insert(path.clone(), path_item_methods(item))
                .expect("OpenAPI paths are valid route patterns");
        }
        let spec = openapi
            .to_json()
            .expect("OpenAPI document can be serialized");
        let mut get_routes: Vec<(&str, MethodRouter<AppState>)> = vec![
            ("/", get(index)),
            (
                "/openapi.json",
                get(move || async move { ([("content-type", "application/json")], spec) }),
            ),
        ];
        get_routes.extend(Self::public_routes());
        for (path, handler) in get_routes {
            router = router.route(path, handler);
            known
                .insert(path, vec![Method::GET])
                .expect("route patterns accepted by axum are valid for matchit");
        }
        with_path_normalization(router, known)
            // One span per request, exported when zipkin_address is set.
            .layer(TraceLayer::new_for_http())
            .with_state(self.state.clone())
    }

    /// Create the router for the public port: the dists trees.
    pub fn public_router(&self) -> Router {
        let mut router = Router::new();
        let mut known = matchit::Router::new();
        for (path, handler) in Self::public_routes() {
            router = router.route(path, handler);
            known
                .insert(path, vec![Method::GET])
                .expect("route patterns accepted by axum are valid for matchit");
        }
        with_path_normalization(router, known)
            .layer(TraceLayer::new_for_http())
            .with_state(self.state.clone())
    }

    /// The routes of the public port, all for GET.
    fn public_routes() -> Vec<(&'static str, MethodRouter<AppState>)> {
        vec![
            // Repository serving endpoints
            ("/dists/{suite}/Release", get(serve_suite_file)),
            ("/dists/{suite}/Release.gpg", get(serve_suite_file)),
            ("/dists/{suite}/InRelease", get(serve_suite_file)),
            (
                "/dists/{suite}/{component}/{binary_arch}/Packages",
                get(serve_suite_file),
            ),
            (
                "/dists/{suite}/{component}/{binary_arch}/Packages.gz",
                get(serve_suite_file),
            ),
            (
                "/dists/{suite}/{component}/{binary_arch}/Packages.bz2",
                get(serve_suite_file),
            ),
            (
                "/dists/{suite}/{component}/source/Sources",
                get(serve_suite_file),
            ),
            (
                "/dists/{suite}/{component}/source/Sources.gz",
                get(serve_suite_file),
            ),
            (
                "/dists/{suite}/{component}/source/Sources.bz2",
                get(serve_suite_file),
            ),
            // By-hash serving
            // Either an index of a suite or a component file, like
            // Contents-<arch>, of an on-demand repository.
            (
                "/dists/{a}/{b}/{c}/by-hash/{algo}/{hash}",
                get(serve_by_hash_file),
            ),
            (
                "/dists/{suite}/{component}/source/by-hash/{algo}/{hash}",
                get(serve_suite_file),
            ),
            (
                "/dists/{suite}/{component}/by-hash/{algo}/{hash}",
                get(serve_suite_file),
            ),
            // On-demand dists: /dists/{kind=cs|run|<campaign>}/{id}/...
            // Either the Release of an on-demand repository or a
            // component file of a suite, like Contents-<arch>.
            ("/dists/{kind}/{id}/{file}", get(serve_dists_file)),
            (
                "/dists/{kind}/{id}/{component}/{file}",
                get(serve_on_demand_file),
            ),
            (
                "/dists/{kind}/{id}/{component}/{binary_arch}/{file}",
                get(serve_on_demand_file),
            ),
            (
                "/dists/{kind}/{id}/{component}/source/{file}",
                get(serve_on_demand_file),
            ),
            (
                "/dists/{kind}/{id}/{component}/{binary_arch}/by-hash/{algo}/{hash}",
                get(serve_on_demand_file),
            ),
            (
                "/dists/{kind}/{id}/{component}/source/by-hash/{algo}/{hash}",
                get(serve_on_demand_file),
            ),
        ]
    }

    /// Serve all routes on `bind_address` and only the public ones on `public_bind_address`.
    pub async fn serve(&self, bind_address: &str, public_bind_address: &str) -> ArchiveResult<()> {
        let listener = bind(bind_address).await?;
        let public_listener = bind(public_bind_address).await?;

        info!("Archive web service listening on {}", bind_address);
        info!("Public archive listening on {}", public_bind_address);

        tokio::try_join!(
            axum::serve(listener, self.router()),
            axum::serve(public_listener, self.public_router()),
        )
        .map_err(|e| ArchiveError::InvalidConfiguration(format!("Server error: {}", e)))?;

        Ok(())
    }
}

async fn bind(address: &str) -> ArchiveResult<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(address).await.map_err(|e| {
        ArchiveError::InvalidConfiguration(format!("Failed to bind to {}: {}", address, e))
    })
}

/// `GET /` -- empty body.
async fn index() -> &'static str {
    ""
}

/// Methods with an operation in `item`.
fn path_item_methods(item: &utoipa::openapi::path::PathItem) -> Vec<Method> {
    [
        (&item.get, Method::GET),
        (&item.put, Method::PUT),
        (&item.post, Method::POST),
        (&item.delete, Method::DELETE),
        (&item.options, Method::OPTIONS),
        (&item.head, Method::HEAD),
        (&item.patch, Method::PATCH),
        (&item.trace, Method::TRACE),
    ]
    .into_iter()
    .filter(|(operation, _)| operation.is_some())
    .map(|(_, method)| method)
    .collect()
}

/// Paths to try when a request matches no route, in the order
/// aiohttp's `normalize_path_middleware(append_slash=True)` tries
/// them: duplicate slashes merged, a slash appended, or both.
fn normalized_path_candidates(path: &str) -> Vec<String> {
    fn merge_slashes(path: &str) -> String {
        let mut out = String::with_capacity(path.len());
        for c in path.chars() {
            if !(c == '/' && out.ends_with('/')) {
                out.push(c);
            }
        }
        out
    }
    let mut candidates = vec![merge_slashes(path)];
    if !path.ends_with('/') {
        candidates.push(format!("{}/", path));
    }
    candidates.push(merge_slashes(&format!("{}/", path)));
    candidates
        .into_iter()
        .map(|p| format!("/{}", p.trim_start_matches('/')))
        .collect()
}

/// Add a fallback to `router` that redirects requests matching no route
/// to a normalized path among `known`, if there is one.
fn with_path_normalization(
    router: Router<AppState>,
    known: matchit::Router<Vec<Method>>,
) -> Router<AppState> {
    let known = Arc::new(known);
    router.fallback(move |method: Method, uri: Uri| {
        let known = known.clone();
        async move { normalize_path(&known, &method, &uri) }
    })
}

/// Fallback for requests that match no route: redirect to a
/// normalized path if that one does match, else 404.
fn normalize_path(known: &matchit::Router<Vec<Method>>, method: &Method, uri: &Uri) -> Response {
    let allowed = |route_methods: &Vec<Method>| {
        route_methods
            .iter()
            .any(|m| m == method || (*m == Method::GET && *method == Method::HEAD))
    };
    let target = normalized_path_candidates(uri.path())
        .into_iter()
        .find(|candidate| known.at(candidate).is_ok_and(|m| allowed(m.value)));
    let Some(mut target) = target else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(query) = uri.query() {
        target.push('?');
        target.push_str(query);
    }
    Redirect::permanent(&target).into_response()
}

/// Serve a file from the dists tree of a configured suite. Files that
/// have not been generated yet give a 404.
async fn serve_suite_file(
    Path(params): Path<HashMap<String, String>>,
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, StatusCode> {
    let suite = params.get("suite").ok_or(StatusCode::BAD_REQUEST)?;
    serve_from_suite(&state, suite, request).await
}

async fn serve_from_suite(
    state: &AppState,
    suite: &str,
    request: Request,
) -> Result<Response, StatusCode> {
    let repo_config = state
        .config
        .repositories
        .get(suite)
        .ok_or(StatusCode::NOT_FOUND)?;
    serve_from_dir(repo_config.suite_path(), 2, request).await
}

/// Serve `/dists/{a}/{b}/{file}`. On-demand repositories only have
/// their Release files at that level, so anything else is a component
/// file of suite `a`, like `Contents-amd64` in component `b`.
async fn serve_dists_file(
    Path((a, b, file)): Path<(String, String, String)>,
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, StatusCode> {
    if matches!(file.as_str(), "Release" | "InRelease" | "Release.gpg") {
        serve_from_on_demand(&state, &a, &b, request).await
    } else {
        serve_from_suite(&state, &a, request).await
    }
}

/// Serve the file named by the request path, minus its first `skip`
/// segments, from `dir`.
///
/// `ServeDir` decodes the path itself and only accepts plain file
/// names, so `..` or an encoded `/` can not escape `dir`.
async fn serve_from_dir(
    dir: PathBuf,
    skip: usize,
    mut request: Request,
) -> Result<Response, StatusCode> {
    let rest = request
        .uri()
        .path()
        .splitn(skip + 2, '/')
        .nth(skip + 1)
        .ok_or(StatusCode::NOT_FOUND)?;
    *request.uri_mut() = format!("/{}", rest)
        .parse()
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let Ok(response) = ServeDir::new(dir)
        .append_index_html_on_directories(false)
        .oneshot(request)
        .await;
    Ok(response.map(Body::new))
}

/// `POST /publish` -- trigger a republish across every configured
/// apt_repository:
///
/// - reads an optional `campaign` field from the form body
/// - iterates every `config.campaign` with a `debian_build` block
/// - filters by `campaign` when supplied
/// - dispatches to `generator_manager.trigger_campaign` for each match
/// - returns `{}` on success
/// Enumerate the campaigns `/publish` should iterate over. Walks
/// `runtime_config.campaign` and skips those without a
/// `debian_build` block. When no protobuf runtime config is
/// available we fall back to the manager's fan-out map keys so
/// env-only deployments still have something to iterate over.
pub(crate) fn publish_campaign_candidates(
    runtime_config: Option<&janitor::config::Config>,
    fallback_campaigns: impl IntoIterator<Item = Campaign>,
) -> Result<Vec<Campaign>, InvalidIdentifier> {
    if let Some(rt) = runtime_config {
        rt.campaign
            .iter()
            .filter(|c| c.has_debian_build())
            .filter_map(|c| c.name.as_deref())
            .map(Campaign::new)
            .collect()
    } else {
        Ok(fallback_campaigns.into_iter().collect())
    }
}

/// Apply the optional `campaign` filter from a `/publish` request
/// body. Returns the list of campaign names to actually trigger.
/// Absent filter -> pass through; present filter keeps only the
/// candidate whose name matches exactly.
pub(crate) fn publish_apply_filter<'a>(
    candidates: &'a [Campaign],
    filter: Option<&Campaign>,
) -> Vec<&'a Campaign> {
    candidates
        .iter()
        .filter(|name| match filter {
            Some(f) => f == *name,
            None => true,
        })
        .collect()
}

/// Read the `/publish` form body the way aiohttp's `request.post()`
/// does: urlencoded bodies are parsed, and bodies of any other type
/// (including none at all) carry no fields.
pub(crate) fn parse_publish_request(
    content_type: Option<&str>,
    body: &[u8],
) -> Result<PublishRequest, StatusCode> {
    // aiohttp treats a missing Content-Type as application/octet-stream.
    let mime = content_type
        .map(|ct| {
            ct.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .unwrap_or_else(|| "application/octet-stream".to_string());
    match mime.as_str() {
        "" | "application/x-www-form-urlencoded" => {
            let campaign = form_urlencoded::parse(body)
                .find(|(key, _)| key == "campaign")
                .map(|(_, value)| Campaign::new(value.into_owned()))
                .transpose()
                .map_err(|e| {
                    debug!("Rejecting /publish request: {}", e);
                    StatusCode::UNPROCESSABLE_ENTITY
                })?;
            Ok(PublishRequest { campaign })
        }
        // TODO: parse multipart bodies, which aiohttp accepts too.
        "multipart/form-data" => Err(StatusCode::UNSUPPORTED_MEDIA_TYPE),
        _ => Ok(PublishRequest::default()),
    }
}

#[utoipa::path(
    post,
    path = "/publish",
    request_body(content = PublishRequest, content_type = "application/x-www-form-urlencoded"),
    responses(
        (status = 200, description = "Publishing was triggered", body = PublishResponse),
        (status = 500, description = "Invalid campaign in the configuration"),
        (status = 503, description = "Publishing is not available in this deployment"),
    )
)]
async fn publish_repository(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<PublishResponse>, StatusCode> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().map_err(|_| StatusCode::BAD_REQUEST))
        .transpose()?;
    let request = parse_publish_request(content_type, &body)?;
    info!("Repository publish request: {:?}", request);

    let Some(manager) = state.generator_manager.as_ref() else {
        warn!("/publish invoked but no GeneratorManager is wired up");
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };

    let fallback: Vec<Campaign> = if state.config.runtime_config.is_some() {
        Vec::new()
    } else {
        manager.get_campaign_mapping().await.into_keys().collect()
    };
    let candidates = publish_campaign_candidates(state.config.runtime_config.as_deref(), fallback)
        .map_err(|e| {
            warn!("Invalid campaign in configuration: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    for campaign_name in publish_apply_filter(&candidates, request.campaign.as_ref()) {
        if let Err(e) = manager.trigger_campaign(campaign_name).await {
            warn!(
                "trigger_campaign({}) failed during /publish: {}",
                campaign_name, e
            );
        }
    }

    Ok(Json(PublishResponse::default()))
}

/// Serialize the shared last-publish map into the `{suite:
/// iso8601}` shape `/last-publish` returns. Kept pure so tests
/// don't need to construct an entire AppState.
pub(crate) fn last_publish_json(
    times: &HashMap<String, chrono::DateTime<chrono::Utc>>,
) -> HashMap<String, String> {
    times
        .iter()
        .map(|(name, dt)| (name.clone(), dt.to_rfc3339()))
        .collect()
}

/// `GET /last-publish` -- return the last successful publish time
/// for each apt_repository as a JSON object `{suite: iso8601}`.
#[utoipa::path(
    get,
    path = "/last-publish",
    responses((status = 200, description = "Last successful publish time (RFC 3339) of each apt repository", body = HashMap<String, String>))
)]
async fn last_publish_status(State(state): State<AppState>) -> Json<HashMap<String, String>> {
    let times = state.last_publish_times.read().await;
    Json(last_publish_json(&times))
}

/// Compute the (status, body) pair for `/ready` from the current
/// last-publish map and the configured repository list. `Ok(body)`
/// -> 200; `Err(body)` -> 500. Extracted so tests can assert on
/// exact response shape without constructing an entire AppState.
pub(crate) fn compute_ready_response<'a>(
    repositories: impl IntoIterator<Item = &'a str>,
    times: &HashMap<String, chrono::DateTime<chrono::Utc>>,
) -> Result<String, String> {
    let mut missing: Vec<&str> = Vec::new();
    for name in repositories {
        if !times.contains_key(name) {
            missing.push(name);
        }
    }

    let mut status_body = String::new();
    for (name, dt) in times.iter() {
        status_body.push_str(&format!("{}: {}\n", name, dt.to_rfc3339()));
    }

    if !missing.is_empty() {
        Err(format!(
            "missing: {}\n\npresent:\n{}",
            missing.join(", "),
            status_body
        ))
    } else {
        Ok(status_body)
    }
}

/// `GET /ready` -- 500 with a list of apt_repositories that have
/// never been published, plaintext body listing those that have.
#[utoipa::path(
    get,
    path = "/ready",
    responses(
        (status = 200, description = "Every apt repository has been published; lists the publish times", body = String, content_type = "text/plain"),
        (status = 500, description = "Some apt repositories have not been published yet", body = String, content_type = "text/plain"),
    )
)]
async fn archive_ready_handler(State(state): State<AppState>) -> Response {
    let times = state.last_publish_times.read().await;
    match compute_ready_response(
        state.config.repositories.values().map(|r| r.name.as_str()),
        &times,
    ) {
        Ok(body) => (StatusCode::OK, body).into_response(),
        Err(body) => (StatusCode::INTERNAL_SERVER_ERROR, body).into_response(),
    }
}

/// Serve GPG public key.
#[utoipa::path(
    get,
    path = "/gpg-key",
    responses(
        (status = 200, description = "The signing key", body = String, content_type = "application/pgp-keys"),
        (status = 404, description = "No signing key is configured"),
        (status = 500, description = "The key could not be exported"),
    )
)]
async fn serve_gpg_key(State(state): State<AppState>) -> Result<Response, StatusCode> {
    let Some(gpg_config) = &state.config.gpg else {
        debug!("No GPG configuration available");
        return Err(StatusCode::NOT_FOUND);
    };
    let key_data = crate::sign::export_public_keys(gpg_config)
        .await
        .map_err(|e| {
            warn!("Failed to export GPG public key: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        HeaderValue::from_static("application/pgp-keys"),
    );
    headers.insert(
        "Cache-Control",
        HeaderValue::from_static("public, max-age=86400"), // 24 hours
    );
    Ok((headers, key_data).into_response())
}

/// `GET /pgp_keys` -- a JSON array with a minimal armored export of
/// every key in the keyring that has a secret key. Only routed when
/// signing is enabled.
#[utoipa::path(
    get,
    path = "/pgp_keys",
    responses(
        (status = 200, description = "Minimal ASCII-armored export of each key with a secret key in the keyring; only served when signing is enabled", body = Vec<String>),
        (status = 500, description = "The keys could not be exported", body = String, content_type = "text/plain"),
    )
)]
async fn handle_pgp_keys(State(state): State<AppState>) -> Response {
    let Some(gpg_config) = &state.config.gpg else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match crate::sign::export_minimal_public_keys(gpg_config).await {
        Ok(keys) => Json(keys).into_response(),
        Err(e) => {
            warn!("Failed to export GPG public keys: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// Return true when `kind` is a recognized on-demand dispatcher
/// segment. `cs` and `run` are always allowed; anything else must
/// match a configured campaign.
///
/// Extracted so tests can hit the same logic the handler uses
/// without spinning up an axum router.
pub(crate) fn is_valid_on_demand_kind(
    kind: &str,
    runtime_config: Option<&janitor::config::Config>,
    archive_config: &ArchiveConfig,
) -> bool {
    if kind.is_empty() {
        return false;
    }
    if matches!(kind, "cs" | "run") {
        return true;
    }
    if let Some(rt) = runtime_config {
        return rt.campaign.iter().any(|c| c.name.as_deref() == Some(kind));
    }
    // Env-only deployments: fall back to the repository suite set.
    archive_config
        .repositories
        .values()
        .any(|r| r.suite == kind)
}

/// Ensure on-demand dists exist for `(kind, id)`, returning the
/// directory under which generated files should live. Returns
/// `Ok(None)` on database NotFound so the caller can emit 404.
async fn prepare_on_demand(
    state: &AppState,
    kind: &str,
    id: &str,
) -> Result<Option<PathBuf>, StatusCode> {
    let dists_dir = state.config.archive_path.join("dists");

    if !is_valid_on_demand_kind(kind, state.config.runtime_config.as_deref(), &state.config) {
        debug!("Rejecting on-demand kind {}: not a known campaign", kind);
        return Ok(None);
    }
    let target = OnDemandTarget::parse(kind, id).map_err(|e| {
        debug!("Rejecting on-demand request: {}", e);
        StatusCode::BAD_REQUEST
    })?;

    // Prefer the repository config whose `suite` matches the
    // caller's `kind` (campaign name); honour the per-repository
    // components/architectures on AptRepositoryConfig when it does.
    // Fall back to the first repository entry (origin only) plus
    // `main` + default_architectures when we can't match.
    let matching_repo = state.config.repositories.values().find(|r| r.suite == kind);
    let (components, arches, origin) = if let Some(repo) = matching_repo {
        (
            repo.components.clone(),
            repo.architectures.clone(),
            repo.origin.clone(),
        )
    } else {
        let fallback_arches = if state.config.default_architectures.is_empty() {
            vec!["amd64".to_string()]
        } else {
            state.config.default_architectures.clone()
        };
        let fallback_origin = state
            .config
            .repositories
            .values()
            .next()
            .map(|r| r.origin.clone())
            .unwrap_or_else(|| "janitor".to_string());
        (vec!["main".to_string()], fallback_arches, fallback_origin)
    };

    match crate::on_demand::refresh_on_demand_dists(
        &dists_dir,
        state.database.as_ref(),
        state.scanner.clone(),
        &origin,
        &components,
        &arches,
        &target,
        state.config.gpg.as_ref(),
        state.config.runtime_config.as_deref(),
    )
    .await
    {
        Ok(()) => Ok(Some(dists_dir.join(kind).join(id))),
        Err(ArchiveError::NotFound(msg)) => {
            debug!("on-demand dists not found: {}", msg);
            Ok(None)
        }
        Err(e) => {
            warn!(
                "Failed to refresh on-demand dists for {}/{}: {}",
                kind, id, e
            );
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Serve a file from an on-demand dists tree, generating the tree first
/// if it is missing or stale.
async fn serve_on_demand_file(
    Path(params): Path<HashMap<String, String>>,
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, StatusCode> {
    let kind = params.get("kind").ok_or(StatusCode::BAD_REQUEST)?;
    let id = params.get("id").ok_or(StatusCode::BAD_REQUEST)?;
    serve_from_on_demand(&state, kind, id, request).await
}

/// Serve `/dists/{a}/{b}/{c}/by-hash/{algo}/{hash}`. Index directories
/// are `source` or `binary-<arch>`, which no component is called, so
/// for those `a` is a suite; otherwise `a/b` is an on-demand repository
/// and `c` one of its components.
async fn serve_by_hash_file(
    Path((a, b, c, _algo, _hash)): Path<(String, String, String, String, String)>,
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, StatusCode> {
    if c == "source" || c.starts_with("binary-") {
        serve_from_suite(&state, &a, request).await
    } else {
        serve_from_on_demand(&state, &a, &b, request).await
    }
}

async fn serve_from_on_demand(
    state: &AppState,
    kind: &str,
    id: &str,
    request: Request,
) -> Result<Response, StatusCode> {
    let base = prepare_on_demand(state, kind, id)
        .await?
        .ok_or(StatusCode::NOT_FOUND)?;
    serve_from_dir(base, 3, request).await
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
        let doc = serde_json::to_value(openapi()).unwrap();
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

    #[tokio::test]
    async fn test_health_ok_returns_ok_body() {
        // /health returns text "ok" -- some deployment probes
        // assert on the exact body.
        use axum::body::to_bytes;
        let response = shared::health_ok().await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64).await.unwrap();
        assert_eq!(&body[..], b"ok");
    }

    /// `/last-publish` returns `{suite: iso8601}` (RFC 3339). Empty
    /// map yields an empty object; a single entry yields one key.
    #[test]
    fn last_publish_json_maps_utc_to_rfc3339() {
        use chrono::{DateTime, Utc};
        let times: HashMap<String, DateTime<Utc>> = HashMap::new();
        assert!(last_publish_json(&times).is_empty());

        let mut times: HashMap<String, DateTime<Utc>> = HashMap::new();
        let ts: DateTime<Utc> = "2026-01-15T12:34:56+00:00".parse().unwrap();
        times.insert("unstable".to_string(), ts);
        let out = last_publish_json(&times);
        assert_eq!(out.len(), 1);
        // to_rfc3339 with a UTC timestamp yields `+00:00`, not `Z`.
        assert_eq!(out.get("unstable").unwrap(), "2026-01-15T12:34:56+00:00");
    }

    /// `/ready` returns 500 with a `missing: ...` header when at
    /// least one configured suite hasn't been published yet.
    #[test]
    fn compute_ready_response_missing_suite_returns_err() {
        let times = HashMap::new();
        let result = compute_ready_response(vec!["unstable", "stable"], &times);
        let body = result.expect_err("expected 500 body");
        assert!(body.starts_with("missing: "));
        // Order isn't guaranteed by config.repositories.values() so
        // check both names appear.
        assert!(body.contains("unstable"));
        assert!(body.contains("stable"));
        assert!(body.contains("\n\npresent:\n"));
    }

    /// When every suite has a publish time recorded, `/ready`
    /// returns 200 with the present-list as body.
    #[test]
    fn compute_ready_response_all_published_returns_ok() {
        use chrono::{DateTime, Utc};
        let ts: DateTime<Utc> = "2026-01-15T12:34:56+00:00".parse().unwrap();
        let mut times = HashMap::new();
        times.insert("unstable".to_string(), ts);
        times.insert("stable".to_string(), ts);
        let body = compute_ready_response(vec!["unstable", "stable"], &times).unwrap();
        assert!(body.contains("unstable: "));
        assert!(body.contains("stable: "));
        assert!(!body.starts_with("missing:"));
    }

    /// Partial publish state: one suite present, one missing. Must
    /// 500 and mention only the missing one in the `missing:`
    /// header while still listing the present one below.
    #[test]
    fn compute_ready_response_partial_missing() {
        use chrono::{DateTime, Utc};
        let ts: DateTime<Utc> = "2026-01-15T12:34:56+00:00".parse().unwrap();
        let mut times = HashMap::new();
        times.insert("stable".to_string(), ts);
        let body = compute_ready_response(vec!["unstable", "stable"], &times)
            .expect_err("expected 500 body");
        // Only unstable is missing.
        let missing_line = body.lines().next().unwrap();
        assert_eq!(missing_line, "missing: unstable");
        assert!(body.contains("stable: 2026-01-15T12:34:56+00:00"));
    }

    /// Empty repository list + empty times: 200 with empty body.
    /// A deployment with no apt_repositories configured is still
    /// considered ready -- the loop over configured repositories
    /// yields nothing, so `missing` stays empty.
    #[test]
    fn compute_ready_response_no_repos_is_ready() {
        let times = HashMap::new();
        let body = compute_ready_response(Vec::<&str>::new(), &times).unwrap();
        assert!(body.is_empty());
    }

    async fn test_router(config: ArchiveConfig) -> Router {
        let pool = sqlx::PgPool::connect_lazy("postgresql://localhost/unused").unwrap();
        let generator = || {
            let pool = pool.clone();
            async move {
                RepositoryGenerator::new(
                    Arc::new(PackageScanner::new("memory:///").await.unwrap()),
                    Arc::new(BuildManager::new(pool)),
                    crate::repository::RepositoryGenerationConfig::default(),
                )
            }
        };
        let manager = GeneratorManager::new(
            config.clone(),
            generator().await,
            PackageScanner::new("memory:///").await.unwrap(),
            BuildManager::new(pool.clone()),
            crate::manager::GeneratorManagerConfig::default(),
        )
        .await
        .unwrap();
        ArchiveWebService::with_publish_observer(
            config,
            generator().await,
            PackageScanner::new("memory:///").await.unwrap(),
            BuildManager::new(pool),
            Arc::new(manager),
            new_last_publish_times(),
        )
        .await
        .unwrap()
        .router()
    }

    async fn request(router: Router, method: Method, uri: &str) -> Response {
        use tower::ServiceExt;
        router
            .oneshot(
                axum::http::Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_index_returns_empty_body() {
        use axum::body::to_bytes;
        let response = request(test_router(empty_archive_config()).await, Method::GET, "/").await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64).await.unwrap();
        assert_eq!(&body[..], b"");
    }

    #[tokio::test]
    async fn test_publish_without_body() {
        use axum::body::to_bytes;
        let response = request(
            test_router(empty_archive_config()).await,
            Method::POST,
            "/publish",
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64).await.unwrap();
        assert_eq!(&body[..], b"{}");
    }

    #[test]
    fn test_parse_publish_request() {
        let form = Some("application/x-www-form-urlencoded");
        assert_eq!(
            parse_publish_request(form, b"campaign=lintian-fixes")
                .unwrap()
                .campaign
                .as_deref(),
            Some("lintian-fixes")
        );
        assert_eq!(parse_publish_request(form, b"").unwrap().campaign, None);
        assert_eq!(
            parse_publish_request(form, b"campaign=..%2Fx").unwrap_err(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            parse_publish_request(Some("text/plain"), b"campaign=x")
                .unwrap()
                .campaign,
            None
        );
        assert_eq!(
            parse_publish_request(None, b"campaign=x").unwrap().campaign,
            None
        );
        assert_eq!(
            parse_publish_request(Some("multipart/form-data; boundary=x"), b"").unwrap_err(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
    }

    #[tokio::test]
    async fn test_duplicate_slashes_redirect() {
        let response = request(
            test_router(empty_archive_config()).await,
            Method::GET,
            "//health?x=1",
        )
        .await;
        assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            response.headers().get(header::LOCATION).unwrap(),
            "/health?x=1"
        );
    }

    #[tokio::test]
    async fn test_unknown_path_not_found() {
        let router = test_router(empty_archive_config()).await;
        let response = request(router.clone(), Method::GET, "//nonexistent").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        // /publish only accepts POST.
        let response = request(router, Method::GET, "//publish").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn test_normalized_path_candidates() {
        assert_eq!(
            normalized_path_candidates("//dists//x"),
            vec!["/dists/x", "/dists//x/", "/dists/x/"]
        );
        assert_eq!(normalized_path_candidates("/a/"), vec!["/a/", "/a/"]);
    }

    #[tokio::test]
    async fn test_pgp_keys_not_routed_without_gpg() {
        let response = request(
            test_router(empty_archive_config()).await,
            Method::GET,
            "/pgp_keys",
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_pgp_keys_routed_with_gpg() {
        use axum::body::to_bytes;
        let gpg_home = tempfile::tempdir().unwrap();
        let config = ArchiveConfig {
            gpg: Some(crate::config::GpgConfig {
                gpg_home: Some(gpg_home.path().to_path_buf()),
                ..crate::config::GpgConfig::new(None)
            }),
            ..empty_archive_config()
        };
        let response = request(test_router(config).await, Method::GET, "/pgp_keys").await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64).await.unwrap();
        assert_eq!(&body[..], b"[]");
    }

    fn empty_archive_config() -> ArchiveConfig {
        ArchiveConfig {
            archive_path: PathBuf::from("/tmp/archive"),
            ..Default::default()
        }
    }

    /// `cs` and `run` are unconditionally valid on-demand kinds.
    /// Must hold even when there's no runtime config.
    #[test]
    fn is_valid_on_demand_kind_cs_and_run_always_valid() {
        let ac = empty_archive_config();
        assert!(is_valid_on_demand_kind("cs", None, &ac));
        assert!(is_valid_on_demand_kind("run", None, &ac));
    }

    /// Empty kind (`""`) is rejected.
    #[test]
    fn is_valid_on_demand_kind_empty_rejected() {
        let ac = empty_archive_config();
        assert!(!is_valid_on_demand_kind("", None, &ac));
    }

    /// Campaign names declared in the runtime protobuf config are
    /// valid. Anything else is rejected. Guards against the
    /// original bug where the port accepted any string and deferred
    /// validation to a downstream SQL query that quietly returned
    /// an empty set.
    #[test]
    fn is_valid_on_demand_kind_matches_runtime_campaigns() {
        let cfg = janitor::config::read_string(
            r#"
                campaign { name: "lintian-fixes" }
                campaign { name: "fresh-releases" }
            "#,
        )
        .unwrap();
        let ac = empty_archive_config();
        assert!(is_valid_on_demand_kind("lintian-fixes", Some(&cfg), &ac));
        assert!(is_valid_on_demand_kind("fresh-releases", Some(&cfg), &ac));
        assert!(!is_valid_on_demand_kind(
            "unknown-campaign",
            Some(&cfg),
            &ac
        ));
        assert!(!is_valid_on_demand_kind("../etc/passwd", Some(&cfg), &ac));
    }

    /// `/publish` candidate list must skip campaigns that lack a
    /// `debian_build { ... }` block -- only debian_build campaigns
    /// are iterated.
    #[test]
    fn publish_campaign_candidates_skips_non_debian() {
        let cfg = janitor::config::read_string(
            r#"
                campaign {
                    name: "lintian-fixes"
                    debian_build { base_distribution: "unstable" }
                }
                campaign { name: "no-debian-build" }
                campaign {
                    name: "fresh-releases"
                    debian_build { base_distribution: "unstable" }
                }
            "#,
        )
        .unwrap();
        let mut got = publish_campaign_candidates(Some(&cfg), std::iter::empty()).unwrap();
        got.sort();
        assert_eq!(got, vec!["fresh-releases", "lintian-fixes"]);
    }

    /// Without runtime config, candidates come from the fallback
    /// iterator (typically the manager's campaign->repo mapping
    /// keys). This is the env-only deployment path.
    #[test]
    fn publish_campaign_candidates_uses_fallback_when_no_runtime() {
        let fallback = campaigns(&["lintian-fixes", "unstable"]);
        let got = publish_campaign_candidates(None, fallback).unwrap();
        // Order preserved from the fallback iterator.
        assert_eq!(got, vec!["lintian-fixes", "unstable"]);
    }

    /// `?campaign=X` restricts the iteration to a single campaign
    /// -- only the exact-match candidate survives.
    #[test]
    fn publish_apply_filter_selects_named_campaign() {
        let candidates = campaigns(&["lintian-fixes", "fresh-releases", "unchanged"]);
        let filter = Campaign::new("fresh-releases").unwrap();
        let filtered = publish_apply_filter(&candidates, Some(&filter));
        assert_eq!(filtered, vec!["fresh-releases"]);
    }

    /// A `campaign` filter that doesn't match any candidate must
    /// yield an empty list -- no trigger fires.
    #[test]
    fn publish_apply_filter_no_match_returns_empty() {
        let candidates = campaigns(&["lintian-fixes"]);
        let filter = Campaign::new("unknown").unwrap();
        let filtered = publish_apply_filter(&candidates, Some(&filter));
        assert!(filtered.is_empty());
    }

    /// Absent filter (`None`) returns every candidate. This is the
    /// default when the form body has no `campaign` field.
    #[test]
    fn publish_apply_filter_none_returns_all() {
        let candidates = campaigns(&["aa", "bb", "cc"]);
        let filtered = publish_apply_filter(&candidates, None);
        assert_eq!(filtered, vec!["aa", "bb", "cc"]);
    }

    fn campaigns(names: &[&str]) -> Vec<Campaign> {
        names.iter().map(|n| Campaign::new(*n).unwrap()).collect()
    }

    #[test]
    fn publish_campaign_candidates_rejects_invalid_name() {
        let cfg = janitor::config::read_string(
            r#"
                campaign {
                    name: "Lintian_Fixes"
                    debian_build { base_distribution: "unstable" }
                }
            "#,
        )
        .unwrap();
        assert!(publish_campaign_candidates(Some(&cfg), std::iter::empty()).is_err());
    }

    /// Without a runtime config we fall back to the repository
    /// suite set. Deployments that ship apt_repositories via env
    /// vars only should still get campaign-aware validation.
    #[test]
    fn is_valid_on_demand_kind_fallback_uses_repo_suites() {
        let mut ac = empty_archive_config();
        ac.repositories.insert(
            "lintian-fixes".to_string(),
            crate::config::AptRepositoryConfig {
                name: "lintian-fixes".to_string(),
                description: "".to_string(),
                origin: "Janitor".to_string(),
                label: "lintian-fixes".to_string(),
                suite: "lintian-fixes".to_string(),
                codename: "lintian-fixes".to_string(),
                architectures: vec!["amd64".to_string()],
                components: vec!["main".to_string()],
                base_path: PathBuf::from("/x"),
                by_hash: true,
            },
        );
        assert!(is_valid_on_demand_kind("lintian-fixes", None, &ac));
        assert!(!is_valid_on_demand_kind("fresh-releases", None, &ac));
    }
}
