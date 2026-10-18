//! axum handlers: serve `/dists/...` files, expose `/publish`, `/ready`,
//! `/last-publish`, `/pgp_keys`, `/gpg-key`, `/metrics`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Form, Path, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use janitor::{Campaign, InvalidIdentifier};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tower::ServiceExt;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use tracing::{debug, info, warn};

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

/// Body accepted by `POST /publish`. Plain form body with an
/// optional `campaign` field. Absent campaign means "trigger every
/// apt_repository that consumes a debian_build campaign".
#[derive(Debug, Default, Deserialize)]
pub struct PublishRequest {
    /// Optional campaign filter. When set, only apt_repositories that
    /// select this campaign are triggered.
    pub campaign: Option<Campaign>,
}

/// Response payload for `POST /publish`. Returns `{}` on success.
#[derive(Debug, Default, Serialize)]
pub struct PublishResponse {}

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
        Router::new()
            // Plain "ok" health endpoint. No health aggregation --
            // the periodic services keep their own /health state
            // internally.
            .route("/health", get(shared::health_ok))
            // /ready returns 500 with a list of apt_repository
            // suites that have never been published yet, plaintext
            // body listing whatever *has* been published. Handled
            // here rather than by the shared readiness handler
            // because the shared one only knows about generic
            // health checks and can't tell that a suite is still
            // missing its first Release file.
            .route("/ready", get(archive_ready_handler))
            // Repository serving endpoints
            .route("/dists/{suite}/Release", get(serve_suite_file))
            .route("/dists/{suite}/Release.gpg", get(serve_suite_file))
            .route("/dists/{suite}/InRelease", get(serve_suite_file))
            .route(
                "/dists/{suite}/{component}/{binary_arch}/Packages",
                get(serve_suite_file),
            )
            .route(
                "/dists/{suite}/{component}/{binary_arch}/Packages.gz",
                get(serve_suite_file),
            )
            .route(
                "/dists/{suite}/{component}/{binary_arch}/Packages.bz2",
                get(serve_suite_file),
            )
            .route(
                "/dists/{suite}/{component}/source/Sources",
                get(serve_suite_file),
            )
            .route(
                "/dists/{suite}/{component}/source/Sources.gz",
                get(serve_suite_file),
            )
            .route(
                "/dists/{suite}/{component}/source/Sources.bz2",
                get(serve_suite_file),
            )
            // By-hash serving
            // Either an index of a suite or a component file, like
            // Contents-<arch>, of an on-demand repository.
            .route(
                "/dists/{a}/{b}/{c}/by-hash/{algo}/{hash}",
                get(serve_by_hash_file),
            )
            .route(
                "/dists/{suite}/{component}/source/by-hash/{algo}/{hash}",
                get(serve_suite_file),
            )
            .route(
                "/dists/{suite}/{component}/by-hash/{algo}/{hash}",
                get(serve_suite_file),
            )
            // On-demand dists: /dists/{kind=cs|run|<campaign>}/{id}/...
            // Either the Release of an on-demand repository or a
            // component file of a suite, like Contents-<arch>.
            .route("/dists/{kind}/{id}/{file}", get(serve_dists_file))
            .route(
                "/dists/{kind}/{id}/{component}/{file}",
                get(serve_on_demand_file),
            )
            .route(
                "/dists/{kind}/{id}/{component}/{binary_arch}/{file}",
                get(serve_on_demand_file),
            )
            .route(
                "/dists/{kind}/{id}/{component}/source/{file}",
                get(serve_on_demand_file),
            )
            .route(
                "/dists/{kind}/{id}/{component}/{binary_arch}/by-hash/{algo}/{hash}",
                get(serve_on_demand_file),
            )
            .route(
                "/dists/{kind}/{id}/{component}/source/by-hash/{algo}/{hash}",
                get(serve_on_demand_file),
            )
            // Publishing and management endpoints
            .route("/publish", post(publish_repository))
            .route("/last-publish", get(last_publish_status))
            .route("/gpg-key", get(serve_gpg_key))
            .route("/pgp_keys", get(handle_pgp_keys))
            .route("/metrics", get(shared::metrics_ok))
            // One span per request, exported when zipkin_address is set.
            .layer(TraceLayer::new_for_http())
            .with_state(self.state.clone())
    }

    /// Start the web service on the specified address.
    pub async fn serve(&self, bind_address: &str) -> ArchiveResult<()> {
        let app = self.router();

        let listener = tokio::net::TcpListener::bind(bind_address)
            .await
            .map_err(|e| {
                ArchiveError::InvalidConfiguration(format!(
                    "Failed to bind to {}: {}",
                    bind_address, e
                ))
            })?;

        info!("Archive web service listening on {}", bind_address);

        axum::serve(listener, app)
            .await
            .map_err(|e| ArchiveError::InvalidConfiguration(format!("Server error: {}", e)))?;

        Ok(())
    }
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

async fn publish_repository(
    State(state): State<AppState>,
    Form(request): Form<PublishRequest>,
) -> Result<Json<PublishResponse>, StatusCode> {
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

/// `GET /pgp_keys` -- return the signing PGP public keys as a JSON
/// array of armored strings. When no GPG is configured, returns an
/// empty JSON array.
async fn handle_pgp_keys(State(state): State<AppState>) -> Response {
    let Some(gpg_config) = &state.config.gpg else {
        return Json(Vec::<String>::new()).into_response();
    };
    match crate::sign::export_public_keys(gpg_config).await {
        Ok(key_data) => Json(vec![key_data]).into_response(),
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
    // Fall back to `main` + default_architectures when we can't match.
    let matching_repo = state.config.repositories.values().find(|r| r.suite == kind);
    let (components, arches) = if let Some(repo) = matching_repo {
        (repo.components.clone(), repo.architectures.clone())
    } else {
        let fallback_arches = if state.config.default_architectures.is_empty() {
            vec!["amd64".to_string()]
        } else {
            state.config.default_architectures.clone()
        };
        (vec!["main".to_string()], fallback_arches)
    };
    let origin = state
        .config
        .runtime_config
        .as_deref()
        .map(|cfg| cfg.origin())
        .unwrap_or_default();

    match crate::on_demand::refresh_on_demand_dists(
        &dists_dir,
        state.database.as_ref(),
        state.scanner.clone(),
        origin,
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
